//! Integration tests against a fixture `/api/v1` server (axum, temp port):
//! 200/ETag/304/401/404 and incremental-cursor pagination, plus the
//! end-to-end plan/export/local-edit-refusal flow through `run`, and the
//! sync flow through the fixture's `POST /mcp`, which enforces the
//! server's conditional-write contract.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ai_memory_wikisync::bidi::{self, CheckStatus, Outcome, Prefer, SyncArgs, SyncMode};
use ai_memory_wikisync::state::{self, SyncState};
use ai_memory_wikisync::sync::{Mode, RunArgs, run};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;

/// One fixture page: what the real server's page read returns, reduced to
/// the fields wikisync reads.
#[derive(Clone)]
struct FixturePage {
    /// Version id: a new one for every write, like the server's page id.
    id: String,
    path: String,
    body: String,
    title: String,
    tier: String,
    pinned: bool,
    frontmatter: serde_json::Value,
}

impl FixturePage {
    fn new(path: &str, body: &str) -> Self {
        Self {
            id: next_version(),
            path: path.to_string(),
            body: body.to_string(),
            title: format!("title {path}"),
            tier: "semantic".to_string(),
            pinned: false,
            frontmatter: json!({}),
        }
    }
}

/// A fresh version id. Global, so ids never repeat across fixture servers.
fn next_version() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    format!(
        "00000000-0000-7000-8000-{:012}",
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// Fixture server state: pages, optional bearer token, call counters.
struct Fixture {
    /// Pages in listing order.
    pages: Mutex<Vec<FixturePage>>,
    token: Option<&'static str>,
    project_exists: bool,
    /// Page advertised by the listing but missing on read, to simulate a
    /// delete/expiry racing the run.
    unreachable: Option<&'static str>,
    /// Rewrite this page's body on its Nth read (1-based), to simulate an
    /// edit landing between a sync's classification and its write.
    edit_on_read: Option<(&'static str, usize)>,
    /// Text the fixture's write path redacts, like the server's sanitizer.
    redact: Option<&'static str>,
    /// Edit this page right before an MCP call on it is handled, to simulate
    /// another writer landing between a sync's classification and its write.
    edit_before_mcp: Option<&'static str>,
    /// A server older than 2.7: no `expected_page_id` in the tool schema, no
    /// `id` on pages, and preconditions silently ignored.
    old_server: bool,
    /// Arguments of every MCP tool call, in order.
    mcp_calls: Mutex<Vec<serde_json::Value>>,
    reads_by_path: Mutex<BTreeMap<String, usize>>,
    recent_calls: AtomicUsize,
    page_reads_200: AtomicUsize,
    page_reads_304: AtomicUsize,
    mcp_writes: AtomicUsize,
    mcp_deletes: AtomicUsize,
}

impl Fixture {
    fn new(pages: Vec<(String, String)>) -> Self {
        Self::with_pages(
            pages
                .iter()
                .map(|(path, body)| FixturePage::new(path, body))
                .collect(),
        )
    }

    fn with_pages(pages: Vec<FixturePage>) -> Self {
        Self {
            pages: Mutex::new(pages),
            token: None,
            project_exists: true,
            unreachable: None,
            edit_on_read: None,
            redact: None,
            edit_before_mcp: None,
            old_server: false,
            mcp_calls: Mutex::new(Vec::new()),
            reads_by_path: Mutex::new(BTreeMap::new()),
            recent_calls: AtomicUsize::new(0),
            page_reads_200: AtomicUsize::new(0),
            page_reads_304: AtomicUsize::new(0),
            mcp_writes: AtomicUsize::new(0),
            mcp_deletes: AtomicUsize::new(0),
        }
    }

    fn with_token(mut self, token: &'static str) -> Self {
        self.token = Some(token);
        self
    }

    fn missing_project(mut self) -> Self {
        self.project_exists = false;
        self
    }

    fn with_unreachable(mut self, path: &'static str) -> Self {
        self.unreachable = Some(path);
        self
    }

    fn page(&self, path: &str) -> Option<FixturePage> {
        self.pages
            .lock()
            .unwrap()
            .iter()
            .find(|page| page.path == path)
            .cloned()
    }

    fn update(&self, path: &str, edit: impl FnOnce(&mut FixturePage)) {
        let mut pages = self.pages.lock().unwrap();
        let page = pages
            .iter_mut()
            .find(|page| page.path == path)
            .expect("fixture page");
        edit(page);
        page.id = next_version();
    }

    fn remove(&self, path: &str) {
        self.pages.lock().unwrap().retain(|page| page.path != path);
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), Box<axum::response::Response>> {
        let expected = match self.token {
            None => return Ok(()),
            Some(token) => format!("Bearer {token}"),
        };
        match headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        {
            Some(value) if value == expected => Ok(()),
            _ => Err(Box::new(error_json(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ))),
        }
    }
}

fn error_json(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// Incremental `recent` listing with a page size of 2, so tests exercise
/// cursor pagination. The cursor is an opaque index string.
async fn recent_handler(
    State(fixture): State<Arc<Fixture>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
    headers: HeaderMap,
) -> axum::response::Response {
    fixture.recent_calls.fetch_add(1, Ordering::SeqCst);
    if let Err(response) = fixture.authorize(&headers) {
        return *response;
    }
    if !fixture.project_exists || workspace != "demo" || project != "app" {
        return error_json(StatusCode::NOT_FOUND, "workspace or project not found");
    }
    // Ignore updated_since; the fixture serves everything after the epoch.
    let start: usize = query
        .get("cursor")
        .and_then(|value| value.strip_prefix("idx:"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let all = fixture.pages.lock().unwrap().clone();
    let end = (start + 2).min(all.len());
    let pages: Vec<serde_json::Value> = all[start..end]
        .iter()
        .enumerate()
        .map(|(offset, page)| {
            json!({
                "path": page.path,
                "title": page.title,
                "kind": "note",
                "tier": "semantic",
                "updated_at": format!("2026-10-0{}T00:00:00Z", start + offset + 1),
            })
        })
        .collect();
    let next_cursor = if end < all.len() {
        json!(format!("idx:{end}"))
    } else {
        json!(null)
    };
    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(json!({ "pages": pages, "next_cursor": next_cursor })),
    )
        .into_response()
}

/// Page read with the server's exact-ETag 304 contract.
async fn page_handler(
    State(fixture): State<Arc<Fixture>>,
    Path((workspace, project, path)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(response) = fixture.authorize(&headers) {
        return *response;
    }
    if !fixture.project_exists || workspace != "demo" || project != "app" {
        return error_json(StatusCode::NOT_FOUND, "workspace or project not found");
    }
    if fixture.unreachable == Some(path.as_str()) {
        return error_json(StatusCode::NOT_FOUND, "page not found");
    }
    let read_count = {
        let mut reads = fixture.reads_by_path.lock().unwrap();
        let count = reads.entry(path.clone()).or_insert(0);
        *count += 1;
        *count
    };
    if let Some((target, nth)) = fixture.edit_on_read
        && target == path
        && nth == read_count
    {
        fixture.update(&path, |page| page.body.push_str("edited concurrently\n"));
    }
    let Some(page) = fixture.page(&path) else {
        return error_json(StatusCode::NOT_FOUND, "page not found");
    };
    let mut frontmatter = page.frontmatter.clone();
    frontmatter["tier"] = json!(page.tier);
    if page.pinned {
        frontmatter["pinned"] = json!(true);
    }
    let mut document = json!({
        "id": page.id,
        "project": project,
        "workspace": workspace,
        "path": path,
        "title": page.title,
        "kind": "note",
        "tier": page.tier,
        "pinned": page.pinned,
        "created_at": "2026-10-01T00:00:00Z",
        "updated_at": "2026-10-01T00:00:00Z",
        "supersedes": null,
        "frontmatter": frontmatter,
        "body_markdown": page.body,
        "links": [],
        "backlinks": [],
    });
    if fixture.old_server {
        document.as_object_mut().unwrap().remove("id");
    }
    // Like the server: the ETag covers the whole projection, so a change to
    // metadata alone also changes it.
    let etag = format!("\"{}\"", state::sha256_hex(document.to_string().as_bytes()));
    if let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && if_none_match == etag
    {
        fixture.page_reads_304.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag)],
            axum::body::Body::empty(),
        )
            .into_response();
    }
    fixture.page_reads_200.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::OK,
        [
            (header::ETAG, etag),
            (header::CONTENT_TYPE, "application/json".to_string()),
        ],
        Json(document),
    )
        .into_response()
}

fn rpc_result(id: &serde_json::Value, result: serde_json::Value) -> axum::response::Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn tool_text(id: &serde_json::Value, is_error: bool, text: String) -> axum::response::Response {
    rpc_result(
        id,
        json!({"isError": is_error, "content": [{"type": "text", "text": text}]}),
    )
}

/// The server's precondition failure: `invalid_request` with typed data.
fn precondition_failed(
    id: &serde_json::Value,
    path: &str,
    expected: Option<&str>,
    current: Option<&str>,
) -> axum::response::Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {
        "code": -32600,
        "message": format!("precondition failed for {path}; nothing was changed"),
        "data": {
            "reason": "precondition_failed",
            "path": path,
            "expected_page_id": expected,
            "current_page_id": current,
        },
    }}))
    .into_response()
}

/// `POST /mcp`: `tools/list`, and `tools/call` of `memory_write_page` (the
/// server's replace semantics: every field the call omits is cleared, and the
/// stored body passes a redaction step like the server's sanitizer) and
/// `memory_delete_page`, both honouring `expected_page_id` / `create_only`.
async fn mcp_handler(
    State(fixture): State<Arc<Fixture>>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> axum::response::Response {
    if let Err(response) = fixture.authorize(&headers) {
        return *response;
    }
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !(accept.contains("application/json") && accept.contains("text/event-stream")) {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let id = &request["id"];
    if request["method"] == "tools/list" {
        let mut properties = json!({"path": {"type": "string"}, "body": {"type": "string"}});
        if !fixture.old_server {
            properties["expected_page_id"] = json!({"type": "string"});
            properties["create_only"] = json!({"type": "boolean"});
        }
        return rpc_result(
            id,
            json!({"tools": [
                {"name": "memory_read_page", "inputSchema": {"type": "object"}},
                {"name": "memory_write_page",
                 "inputSchema": {"type": "object", "properties": properties}},
            ]}),
        );
    }
    let name = request["params"]["name"].as_str().unwrap_or_default();
    let args = &request["params"]["arguments"];
    if request["method"] != "tools/call"
        || !matches!(name, "memory_write_page" | "memory_delete_page")
        || args["workspace"] != "demo"
        || args["project"] != "app"
    {
        return tool_text(id, true, "bad call".to_string());
    }
    fixture.mcp_calls.lock().unwrap().push(json!({
        "name": name,
        "arguments": args,
    }));
    let path = args["path"].as_str().unwrap_or_default().to_string();
    if fixture.edit_before_mcp == Some(path.as_str()) && fixture.page(&path).is_some() {
        fixture.update(&path, |page| page.body.push_str("edited concurrently\n"));
    }
    let current = fixture.page(&path).map(|page| page.id);
    let expected = args["expected_page_id"].as_str();
    let create_only = args["create_only"].as_bool() == Some(true);
    if !fixture.old_server {
        let holds = match (expected, create_only) {
            (Some(expected), _) => current.as_deref() == Some(expected),
            (None, true) => current.is_none(),
            (None, false) => true,
        };
        if !holds {
            return precondition_failed(id, &path, expected, current.as_deref());
        }
    }
    if name == "memory_delete_page" {
        if current.is_none() {
            return tool_text(id, true, format!("no page at {path}"));
        }
        fixture.remove(&path);
        fixture.mcp_deletes.fetch_add(1, Ordering::SeqCst);
        return tool_text(
            id,
            false,
            json!({"path": path, "deleted": true}).to_string(),
        );
    }
    fixture.mcp_writes.fetch_add(1, Ordering::SeqCst);
    let mut body = args["body"].as_str().unwrap_or_default().to_string();
    if let Some(secret) = fixture.redact {
        body = body.replace(secret, "[REDACTED]");
    }
    let mut written = FixturePage::new(&path, &body);
    if let Some(title) = args["title"].as_str() {
        written.title = title.to_string();
    }
    if let Some(tier) = args["tier"].as_str() {
        written.tier = tier.to_string();
    }
    written.pinned = args["pinned"].as_bool().unwrap_or(false);
    if let Some(tags) = args["tags"].as_array().filter(|tags| !tags.is_empty()) {
        written.frontmatter = json!({ "tags": tags });
    }
    let page_id = written.id.clone();
    {
        let mut pages = fixture.pages.lock().unwrap();
        pages.retain(|page| page.path != path);
        pages.push(written);
    }
    tool_text(
        id,
        false,
        json!({"page_id": page_id, "path": path}).to_string(),
    )
}

async fn serve(fixture: Fixture) -> (SocketAddr, Arc<Fixture>) {
    let shared = Arc::new(fixture);
    let app = Router::new()
        .route(
            "/api/v1/workspaces/{workspace}/projects/{project}/recent",
            get(recent_handler),
        )
        .route(
            "/api/v1/workspaces/{workspace}/projects/{project}/pages/{*path}",
            get(page_handler),
        )
        .route("/mcp", post(mcp_handler))
        .with_state(shared.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture server");
    let addr = listener.local_addr().expect("fixture addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fixture server");
    });
    (addr, shared)
}

fn args(addr: SocketAddr, dest: &std::path::Path, families: &[&str]) -> RunArgs {
    RunArgs {
        server: format!("http://{addr}"),
        token: None,
        workspace: "demo".to_string(),
        project: "app".to_string(),
        dest: dest.to_path_buf(),
        include: families.iter().map(|f| f.to_string()).collect(),
        force: false,
    }
}

fn fixture_pages() -> Vec<(String, String)> {
    vec![
        (
            "_rules/postgres.md".to_string(),
            "# Postgres only\nUse Postgres.\n".to_string(),
        ),
        (
            "decisions/0001-db.md".to_string(),
            "# Standardised on Postgres\nBody.\n".to_string(),
        ),
        ("notes/a.md".to_string(), "# A\nalpha\n".to_string()),
        ("notes/b.md".to_string(), "# B\nbeta\n".to_string()),
        ("notes/c.md".to_string(), "# C\ngamma\n".to_string()),
    ]
}

/// The file export writes for a fixture page: the fixture's title as
/// frontmatter, then the body verbatim.
fn file(path: &str, body: &str) -> Vec<u8> {
    format!("---\ntitle: \"title {path}\"\n---\n{body}").into_bytes()
}

fn temp_dest() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Canonicalize so the symlink-free destination guard passes on every OS.
    let root = tmp.path().canonicalize().unwrap().join("wiki");
    (tmp, root)
}

#[tokio::test]
async fn plan_lists_actions_without_writing() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Plan)
        .await
        .expect("plan succeeds");
    assert!(!dest.exists(), "plan must not create the destination");
}

#[tokio::test]
async fn export_apply_writes_files_and_state() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();

    run(&args(addr, &dest, &["notes", "_rules"]), Mode::Apply)
        .await
        .expect("first export");

    assert_eq!(
        fs::read(dest.join("notes/a.md")).unwrap(),
        file("notes/a.md", "# A\nalpha\n")
    );
    assert_eq!(
        fs::read(dest.join("_rules/postgres.md")).unwrap(),
        file("_rules/postgres.md", "# Postgres only\nUse Postgres.\n")
    );
    // Only allowlisted families land on disk.
    assert!(!dest.join("decisions/0001-db.md").exists());

    let state = state::load(&dest).unwrap();
    assert_eq!(state.pages.len(), 4, "notes/* plus _rules/postgres.md");
    for path in [
        "notes/a.md",
        "notes/b.md",
        "notes/c.md",
        "_rules/postgres.md",
    ] {
        let entry = state.pages.get(path).expect(path);
        assert!(entry.etag.is_some(), "etag recorded for {path}");
    }
}

#[tokio::test]
async fn reexport_revalidates_with_etag_and_writes_nothing() {
    let (addr, fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("first export");
    let before = fs::read(dest.join("notes/a.md")).unwrap();
    let reads_before = fixture.page_reads_200.load(Ordering::SeqCst);

    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("second export");
    assert!(
        fixture.page_reads_304.load(Ordering::SeqCst) >= 3,
        "every unchanged page revalidated via 304"
    );
    assert_eq!(
        fixture.page_reads_200.load(Ordering::SeqCst),
        reads_before,
        "no full page body was fetched again"
    );
    assert_eq!(fs::read(dest.join("notes/a.md")).unwrap(), before);
}

#[tokio::test]
async fn local_edits_are_refused_until_forced() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("first export");

    fs::write(dest.join("notes/b.md"), b"# B\nlocally edited\n").unwrap();
    // A brand-new local file inside an allowlisted family is unknown, not
    // ours to clobber.
    fs::write(dest.join("notes/zz-new.md"), b"# local\n").unwrap();

    let refused = run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect_err("must refuse when files diverged");
    let message = refused.to_string();
    assert!(message.contains("diverged"), "{message}");
    assert_eq!(
        fs::read(dest.join("notes/b.md")).unwrap(),
        b"# B\nlocally edited\n",
        "diverged file is untouched"
    );
    assert_eq!(
        fs::read(dest.join("notes/a.md")).unwrap(),
        file("notes/a.md", "# A\nalpha\n")
    );
    assert_eq!(
        fs::read(dest.join("notes/zz-new.md")).unwrap(),
        b"# local\n",
        "unknown local file is untouched"
    );

    let mut force_args = args(addr, &dest, &["notes"]);
    force_args.force = true;
    run(&force_args, Mode::Apply)
        .await
        .expect("--force exports");
    assert_eq!(
        fs::read(dest.join("notes/b.md")).unwrap(),
        file("notes/b.md", "# B\nbeta\n")
    );
    assert_eq!(
        fs::read(dest.join("notes/zz-new.md")).unwrap(),
        b"# local\n",
        "--force overwrites divergent server pages; it still never deletes"
    );
    // State reflects the forced page.
    let state = state::load(&dest).unwrap();
    assert_eq!(
        state.pages["notes/b.md"].hash,
        state::sha256_hex(&file("notes/b.md", "# B\nbeta\n"))
    );
}

#[tokio::test]
async fn unauthorized_is_a_clear_error() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages()).with_token("sekrit")).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Plan)
        .await
        .expect_err("must fail");
    let message = err.to_string();
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("AI_MEMORY_AUTH_TOKEN"), "{message}");
    assert!(!message.contains("sekrit"), "token never leaks: {message}");

    let mut authed = args(addr, &dest, &["notes"]);
    authed.token = Some("sekrit".to_string());
    run(&authed, Mode::Plan).await.expect("authorized plan");
}

#[tokio::test]
async fn unknown_scope_is_a_clear_404() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages()).missing_project()).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Plan)
        .await
        .expect_err("must fail");
    let message = err.to_string();
    assert!(message.contains("404"), "{message}");
    assert!(message.contains("--workspace/--project"), "{message}");
}

#[tokio::test]
async fn listing_follows_cursors_to_the_last_page() {
    // Five pages with a fixture page size of 2 means three listing rounds.
    let (addr, fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("export");
    assert!(
        fixture.recent_calls.load(Ordering::SeqCst) >= 3,
        "cursor pagination was exercised"
    );
    assert!(dest.join("notes/c.md").exists(), "last page reached");
}

#[tokio::test]
async fn server_page_paths_escaping_dest_are_refused() {
    let hostile = vec![
        ("notes/ok.md".to_string(), "# ok\n".to_string()),
        (
            "notes/../../escape.md".to_string(),
            "# escape\n".to_string(),
        ),
    ];
    let (addr, _fixture) = serve(Fixture::new(hostile)).await;
    let (tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect_err("hostile listing refused");
    assert!(err.to_string().contains("refused"), "{err}");
    // The batch failed during selection: no page files, nothing outside dest.
    assert!(!dest.join("notes/ok.md").exists());
    assert!(
        !tmp.path().join("escape.md").exists(),
        "nothing outside dest"
    );
}

#[tokio::test]
async fn case_fold_collisions_from_server_are_refused() {
    let colliding = vec![
        ("notes/a.md".to_string(), "# a\n".to_string()),
        ("Notes/A.md".to_string(), "# A\n".to_string()),
    ];
    let (addr, _fixture) = serve(Fixture::new(colliding)).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes", "Notes"]), Mode::Apply)
        .await
        .expect_err("collision refused");
    assert!(err.to_string().contains("case-fold"), "{}", err);
}

#[tokio::test]
async fn allowlist_rules_apply_end_to_end() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();

    let none = RunArgs {
        include: vec![],
        ..args(addr, &dest, &[])
    };
    let err = run(&none, Mode::Plan).await.expect_err("empty allowlist");
    assert!(err.to_string().contains("must be explicit"), "{err}");

    let star = RunArgs {
        include: vec!["*".to_string()],
        ..args(addr, &dest, &[])
    };
    let err = run(&star, Mode::Plan).await.expect_err("bare star refused");
    assert!(err.to_string().contains("'*'"), "{err}");
}

#[tokio::test]
async fn page_missing_on_read_fails_loudly() {
    // The listing advertises a page the read cannot find (deleted or
    // expired mid-run): the run must fail instead of skipping silently.
    let mut pages = fixture_pages();
    pages.push(("notes/ghost.md".to_string(), "# ghost\n".to_string()));
    let (addr, _fixture) = serve(Fixture::new(pages).with_unreachable("notes/ghost.md")).await;
    let (_tmp, dest) = temp_dest();
    let err = run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect_err("ghost page must fail the run");
    assert!(
        err.to_string().contains("404") || err.to_string().contains("ghost"),
        "{err}"
    );
    assert!(
        !dest.join("notes/a.md").exists(),
        "a failed batch writes nothing"
    );
}

#[tokio::test]
async fn empty_state_after_user_deletes_local_file_recreates_it() {
    let (addr, _fixture) = serve(Fixture::new(fixture_pages())).await;
    let (_tmp, dest) = temp_dest();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("first export");
    fs::remove_file(dest.join("notes/b.md")).unwrap();
    run(&args(addr, &dest, &["notes"]), Mode::Apply)
        .await
        .expect("recreate");
    assert_eq!(
        fs::read(dest.join("notes/b.md")).unwrap(),
        file("notes/b.md", "# B\nbeta\n")
    );
    let state: SyncState = state::load(&dest).unwrap();
    assert!(state.pages.contains_key("notes/b.md"));
}

fn sync_args(addr: SocketAddr, dest: &std::path::Path, prefer: Option<Prefer>) -> SyncArgs {
    SyncArgs {
        run: args(addr, dest, &["decisions"]),
        prefer,
        propagate_deletes: false,
        max_deletes: bidi::DEFAULT_MAX_DELETES,
    }
}

fn deleting_args(addr: SocketAddr, dest: &std::path::Path, prefer: Option<Prefer>) -> SyncArgs {
    SyncArgs {
        propagate_deletes: true,
        ..sync_args(addr, dest, prefer)
    }
}

async fn sync(addr: SocketAddr, dest: &std::path::Path) -> anyhow::Result<Outcome> {
    bidi::run(&sync_args(addr, dest, None), SyncMode::Apply).await
}

fn decision() -> FixturePage {
    let mut page = FixturePage::new("decisions/db.md", "# DB\nUse Postgres.\n");
    page.title = "DB".to_string();
    page.frontmatter = json!({"tags": ["db"]});
    page.pinned = true;
    page
}

const DECISION_FILE: &str =
    "---\ntitle: \"DB\"\ntags: [\"db\"]\npinned: true\n---\n# DB\nUse Postgres.\n";

#[tokio::test]
async fn sync_exports_metadata_and_round_trips_without_writes() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    assert_eq!(
        fs::read_to_string(dest.join("decisions/db.md")).unwrap(),
        DECISION_FILE
    );

    sync(addr, &dest).await.expect("second sync");
    assert_eq!(
        fixture.mcp_writes.load(Ordering::SeqCst),
        0,
        "nothing to import"
    );
    assert_eq!(
        fs::read_to_string(dest.join("decisions/db.md")).unwrap(),
        DECISION_FILE
    );
}

#[tokio::test]
async fn sync_imports_a_repo_edit_with_its_metadata() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");

    let edited =
        "---\ntitle: \"DB\"\ntags: [\"db\", \"ops\"]\npinned: true\n---\n# DB\nUse Postgres 17.\n";
    fs::write(dest.join("decisions/db.md"), edited).unwrap();
    // Dry-run first: nothing reaches the server.
    bidi::run(&sync_args(addr, &dest, None), SyncMode::DryRun)
        .await
        .expect("dry run");
    assert_eq!(fixture.mcp_writes.load(Ordering::SeqCst), 0);

    sync(addr, &dest).await.expect("import");
    let page = fixture.page("decisions/db.md").unwrap();
    assert_eq!(page.body, "# DB\nUse Postgres 17.\n");
    assert!(
        page.pinned,
        "the write re-sent pinned, so the replace kept it"
    );
    assert_eq!(page.frontmatter["tags"], json!(["db", "ops"]));
    assert_eq!(
        fs::read_to_string(dest.join("decisions/db.md")).unwrap(),
        edited
    );

    sync(addr, &dest).await.expect("settled");
    assert_eq!(
        fixture.mcp_writes.load(Ordering::SeqCst),
        1,
        "a settled page is not written again"
    );
}

#[tokio::test]
async fn sync_creates_new_repo_pages_only_with_frontmatter() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");

    fs::write(dest.join("decisions/cache.md"), "# Cache\nUse Redis.\n").unwrap();
    let err = sync(addr, &dest)
        .await
        .expect_err("a file without frontmatter is refused");
    assert!(err.to_string().contains("refused"), "{err}");
    assert!(fixture.page("decisions/cache.md").is_none());
    assert_eq!(
        fixture.mcp_writes.load(Ordering::SeqCst),
        0,
        "a refusal blocks the whole batch"
    );

    fs::write(
        dest.join("decisions/cache.md"),
        "---\ntitle: \"Cache\"\n---\n# Cache\nUse Redis.\n",
    )
    .unwrap();
    sync(addr, &dest).await.expect("create");
    let page = fixture
        .page("decisions/cache.md")
        .expect("created on the server");
    assert_eq!(page.title, "Cache");
    assert_eq!(page.body, "# Cache\nUse Redis.\n");
    // Pages outside the allowlist never travel.
    fs::create_dir_all(dest.join("notes")).unwrap();
    fs::write(dest.join("notes/x.md"), "---\ntitle: \"X\"\n---\nx\n").unwrap();
    sync(addr, &dest).await.expect("notes ignored");
    assert!(fixture.page("notes/x.md").is_none());
}

#[tokio::test]
async fn sync_conflicts_until_a_side_is_preferred() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");

    let repo_edit = DECISION_FILE.replace("Use Postgres.", "Use Postgres in the repo.");
    fs::write(dest.join("decisions/db.md"), &repo_edit).unwrap();
    fixture.update("decisions/db.md", |page| {
        page.body = "# DB\nUse Postgres on the server.\n".into()
    });

    let err = sync(addr, &dest).await.expect_err("conflict");
    assert!(err.to_string().contains("refused"), "{err}");
    assert_eq!(
        fs::read_to_string(dest.join("decisions/db.md")).unwrap(),
        repo_edit
    );
    assert_eq!(fixture.mcp_writes.load(Ordering::SeqCst), 0);

    bidi::run(
        &sync_args(addr, &dest, Some(Prefer::Server)),
        SyncMode::Apply,
    )
    .await
    .expect("prefer server");
    assert!(
        fs::read_to_string(dest.join("decisions/db.md"))
            .unwrap()
            .contains("on the server")
    );

    fs::write(dest.join("decisions/db.md"), &repo_edit).unwrap();
    fixture.update("decisions/db.md", |page| {
        page.body = "# DB\nAgain on the server.\n".into()
    });
    bidi::run(&sync_args(addr, &dest, Some(Prefer::Repo)), SyncMode::Apply)
        .await
        .expect("prefer repo");
    assert!(
        fixture
            .page("decisions/db.md")
            .unwrap()
            .body
            .contains("in the repo")
    );
}

#[tokio::test]
async fn sync_refuses_to_clear_metadata_it_cannot_round_trip() {
    let mut consolidated = decision();
    consolidated.frontmatter = json!({"tags": ["db"], "summary": "s", "sources": ["x"]});
    let (addr, fixture) = serve(Fixture::with_pages(vec![consolidated])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("export still works");

    let edited = fs::read_to_string(dest.join("decisions/db.md"))
        .unwrap()
        .replace("Use Postgres.", "Edited.");
    fs::write(dest.join("decisions/db.md"), edited).unwrap();
    let err = sync(addr, &dest).await.expect_err("import refused");
    assert!(err.to_string().contains("refused"), "{err}");
    assert_eq!(fixture.mcp_writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.page("decisions/db.md").unwrap().frontmatter["summary"],
        "s"
    );
}

#[tokio::test]
async fn sync_import_sends_the_classified_version_and_records_the_new_one() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    let exported = fixture.page("decisions/db.md").unwrap().id;
    assert_eq!(
        state::load(&dest).unwrap().pages["decisions/db.md"]
            .page_id
            .as_deref(),
        Some(exported.as_str()),
        "the export records the version id"
    );

    fs::write(
        dest.join("decisions/db.md"),
        DECISION_FILE.replace("Use Postgres.", "Repo edit."),
    )
    .unwrap();
    fs::write(
        dest.join("decisions/new.md"),
        "---\ntitle: \"New\"\n---\n# New\n",
    )
    .unwrap();
    sync(addr, &dest).await.expect("import");

    let calls = fixture.mcp_calls.lock().unwrap().clone();
    let call = |path: &str| {
        calls
            .iter()
            .find(|call| call["arguments"]["path"] == path)
            .unwrap_or_else(|| panic!("no MCP call for {path}"))
            .clone()
    };
    let update = call("decisions/db.md");
    assert_eq!(update["arguments"]["expected_page_id"], exported.as_str());
    assert!(update["arguments"].get("create_only").is_none());
    let create = call("decisions/new.md");
    assert_eq!(create["arguments"]["create_only"], true);
    assert!(create["arguments"].get("expected_page_id").is_none());

    let state = state::load(&dest).unwrap();
    for path in ["decisions/db.md", "decisions/new.md"] {
        assert_eq!(
            state.pages[path].page_id,
            Some(fixture.page(path).unwrap().id),
            "{path}: the written version is the new base"
        );
    }
}

/// The race slice 3 could only narrow: another writer lands between the
/// classification read and the write. The server refuses the stale version,
/// the run reports it and fails, and every other page still syncs. Without
/// `expected_page_id` the fixture would accept the write and this fails.
#[tokio::test]
async fn sync_reports_a_page_changed_during_the_run_and_applies_the_rest() {
    let mut other = decision();
    other.path = "decisions/other.md".to_string();
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision(), other])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    let base = state::load(&dest).unwrap().pages["decisions/db.md"].clone();
    let raced_edit = DECISION_FILE.replace("Use Postgres.", "Repo edit.");
    fs::write(dest.join("decisions/db.md"), &raced_edit).unwrap();
    fs::write(
        dest.join("decisions/other.md"),
        DECISION_FILE.replace("Use Postgres.", "Other edit."),
    )
    .unwrap();

    let racing = Fixture {
        edit_before_mcp: Some("decisions/db.md"),
        ..Fixture::with_pages(vec![
            fixture.page("decisions/db.md").unwrap(),
            fixture.page("decisions/other.md").unwrap(),
        ])
    };
    let (race_addr, raced) = serve(racing).await;
    let err = sync(race_addr, &dest).await.expect_err("race reported");
    let message = format!("{err:#}");
    assert!(message.contains("changed during this run"), "{message}");
    assert!(message.contains("decisions/db.md"), "{message}");

    let page = raced.page("decisions/db.md").unwrap();
    assert!(
        page.body.contains("edited concurrently") && !page.body.contains("Repo edit"),
        "the concurrent edit survives: {}",
        page.body
    );
    assert!(
        raced
            .page("decisions/other.md")
            .unwrap()
            .body
            .contains("Other edit"),
        "the other page still synced"
    );
    let state = state::load(&dest).unwrap();
    assert_eq!(
        state.pages["decisions/db.md"], base,
        "raced state untouched"
    );
    assert_eq!(
        fs::read_to_string(dest.join("decisions/db.md")).unwrap(),
        raced_edit,
        "the repository edit is kept for the next run"
    );
}

#[tokio::test]
async fn sync_apply_refuses_a_server_without_conditional_writes() {
    let old = Fixture {
        old_server: true,
        ..Fixture::with_pages(vec![decision()])
    };
    let (addr, fixture) = serve(old).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest)
        .await
        .expect("exports need no precondition");

    fs::write(
        dest.join("decisions/db.md"),
        DECISION_FILE.replace("Use Postgres.", "Repo edit."),
    )
    .unwrap();
    bidi::run(&sync_args(addr, &dest, None), SyncMode::DryRun)
        .await
        .expect("a dry run still plans against an old server");
    let err = sync(addr, &dest).await.expect_err("old server refused");
    assert!(
        format!("{err:#}").contains("server too old for sync --apply"),
        "{err:#}"
    );
    assert_eq!(fixture.mcp_writes.load(Ordering::SeqCst), 0);
    assert!(fixture.mcp_calls.lock().unwrap().is_empty());
}

/// Only the tool schema can tell an old server apart when the plan has
/// nothing but creates, which carry no version id.
#[tokio::test]
async fn sync_apply_checks_the_tool_schema_before_a_create() {
    let old = Fixture {
        old_server: true,
        ..Fixture::with_pages(vec![])
    };
    let (addr, fixture) = serve(old).await;
    let (_tmp, dest) = temp_dest();
    fs::create_dir_all(dest.join("decisions")).unwrap();
    fs::write(
        dest.join("decisions/new.md"),
        "---\ntitle: \"New\"\n---\n# New\n",
    )
    .unwrap();
    let err = sync(addr, &dest).await.expect_err("old server refused");
    assert!(format!("{err:#}").contains("ai-memory >= 2.7"), "{err:#}");
    assert!(fixture.page("decisions/new.md").is_none());
    assert!(fixture.mcp_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sync_only_reports_deletes_without_the_flag() {
    let mut other = decision();
    other.path = "decisions/other.md".to_string();
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision(), other])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    let before = state::load(&dest).unwrap();

    fixture.remove("decisions/db.md");
    fs::remove_file(dest.join("decisions/other.md")).unwrap();
    let outcome = sync(addr, &dest)
        .await
        .expect("deletes are notices, not refusals");
    assert_eq!(outcome.status(), CheckStatus::Drift);
    assert!(
        dest.join("decisions/db.md").exists(),
        "server delete not propagated"
    );
    assert!(
        fixture.page("decisions/other.md").is_some(),
        "repo delete not propagated"
    );
    assert!(fixture.mcp_calls.lock().unwrap().is_empty());
    assert_eq!(
        state::load(&dest).unwrap(),
        before,
        "an unpropagated delete keeps its base entry"
    );

    fs::remove_file(dest.join("decisions/db.md")).unwrap();
    sync(addr, &dest).await.expect("gone on both sides");
    assert!(
        !state::load(&dest)
            .unwrap()
            .pages
            .contains_key("decisions/db.md")
    );
}

#[tokio::test]
async fn sync_propagates_deletes_from_an_unchanged_side() {
    let mut other = decision();
    other.path = "decisions/other.md".to_string();
    other.pinned = false;
    let mut kept = decision();
    kept.path = "decisions/kept.md".to_string();
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision(), other, kept])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    let other_id = fixture.page("decisions/other.md").unwrap().id;

    fixture.remove("decisions/db.md");
    fs::remove_file(dest.join("decisions/other.md")).unwrap();
    // A dry run with the flag still changes nothing.
    bidi::run(&deleting_args(addr, &dest, None), SyncMode::DryRun)
        .await
        .expect("dry run");
    assert!(dest.join("decisions/db.md").exists());
    assert!(fixture.page("decisions/other.md").is_some());

    bidi::run(&deleting_args(addr, &dest, None), SyncMode::Apply)
        .await
        .expect("deletes propagate");
    assert!(
        !dest.join("decisions/db.md").exists(),
        "server delete reached the repository"
    );
    assert!(
        fixture.page("decisions/other.md").is_none(),
        "repository delete reached the server"
    );
    let calls = fixture.mcp_calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["name"], "memory_delete_page");
    assert_eq!(
        calls[0]["arguments"]["expected_page_id"],
        other_id.as_str(),
        "the delete is conditional on the classified version"
    );
    let state = state::load(&dest).unwrap();
    assert!(!state.pages.contains_key("decisions/db.md"));
    assert!(!state.pages.contains_key("decisions/other.md"));
    assert!(state.pages.contains_key("decisions/kept.md"));
    assert!(
        dest.join("decisions/kept.md").exists(),
        "untouched pages stay"
    );
}

#[tokio::test]
async fn sync_delete_conflicts_follow_prefer() {
    let mut other = decision();
    other.path = "decisions/other.md".to_string();
    other.pinned = false;
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision(), other.clone()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");

    // db.md: deleted in the repository, edited on the server.
    // other.md: deleted on the server, edited in the repository.
    let repo_edit = DECISION_FILE
        .replace("pinned: true\n", "")
        .replace("Use Postgres.", "Repo edit.");
    let setup = |fixture: &Fixture| {
        let _ = fs::remove_file(dest.join("decisions/db.md"));
        fixture.update("decisions/db.md", |page| {
            page.body = "# DB\nServer edit.\n".into()
        });
        fs::write(dest.join("decisions/other.md"), &repo_edit).unwrap();
        fixture.remove("decisions/other.md");
    };
    setup(&fixture);
    let err = bidi::run(&deleting_args(addr, &dest, None), SyncMode::Apply)
        .await
        .expect_err("conflicts refuse the batch");
    assert!(err.to_string().contains("refused"), "{err}");
    assert!(fixture.mcp_calls.lock().unwrap().is_empty());
    assert!(dest.join("decisions/other.md").exists());

    // --prefer server: the server's edit comes back, its delete wins.
    bidi::run(
        &deleting_args(addr, &dest, Some(Prefer::Server)),
        SyncMode::Apply,
    )
    .await
    .expect("prefer server");
    assert!(
        fs::read_to_string(dest.join("decisions/db.md"))
            .unwrap()
            .contains("Server edit"),
        "re-exported"
    );
    assert!(!dest.join("decisions/other.md").exists(), "file deleted");
    assert!(fixture.mcp_calls.lock().unwrap().is_empty());

    // --prefer repo: the repository's delete wins (pinned or not), and its
    // edit is re-created with create_only.
    fixture.pages.lock().unwrap().push(other);
    sync(addr, &dest).await.expect("resettle other.md");
    setup(&fixture);
    bidi::run(
        &deleting_args(addr, &dest, Some(Prefer::Repo)),
        SyncMode::Apply,
    )
    .await
    .expect("prefer repo");
    assert!(fixture.page("decisions/db.md").is_none(), "server deleted");
    let recreated = fixture.page("decisions/other.md").expect("re-created");
    assert!(recreated.body.contains("Repo edit"), "{}", recreated.body);
    let calls = fixture.mcp_calls.lock().unwrap().clone();
    let create = calls
        .iter()
        .find(|call| call["name"] == "memory_write_page")
        .expect("write");
    assert_eq!(create["arguments"]["create_only"], true);
}

#[tokio::test]
async fn sync_never_deletes_a_pinned_page_without_prefer_repo() {
    // decision() is pinned on the server.
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    fs::remove_file(dest.join("decisions/db.md")).unwrap();

    for prefer in [None, Some(Prefer::Server)] {
        let err = bidi::run(&deleting_args(addr, &dest, prefer), SyncMode::Apply)
            .await
            .expect_err("pinned page refused");
        assert!(err.to_string().contains("refused"), "{err}");
    }
    assert!(fixture.page("decisions/db.md").is_some());
    assert_eq!(fixture.mcp_deletes.load(Ordering::SeqCst), 0);

    bidi::run(
        &deleting_args(addr, &dest, Some(Prefer::Repo)),
        SyncMode::Apply,
    )
    .await
    .expect("--prefer repo deletes it");
    assert!(fixture.page("decisions/db.md").is_none());
}

#[tokio::test]
async fn sync_refuses_more_deletes_than_the_ceiling() {
    let pages: Vec<FixturePage> = (0..3)
        .map(|n| FixturePage::new(&format!("decisions/d{n}.md"), "# D\n"))
        .collect();
    let (addr, fixture) = serve(Fixture::with_pages(pages)).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    for n in 0..3 {
        fixture.remove(&format!("decisions/d{n}.md"));
    }

    let capped = SyncArgs {
        max_deletes: 2,
        ..deleting_args(addr, &dest, None)
    };
    let err = bidi::run(&capped, SyncMode::Apply)
        .await
        .expect_err("ceiling");
    assert!(err.to_string().contains("--max-deletes 2"), "{err}");
    for n in 0..3 {
        assert!(dest.join(format!("decisions/d{n}.md")).exists());
    }

    let raised = SyncArgs {
        max_deletes: 3,
        ..capped
    };
    bidi::run(&raised, SyncMode::Apply)
        .await
        .expect("within the ceiling");
    for n in 0..3 {
        assert!(!dest.join(format!("decisions/d{n}.md")).exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sync_never_deletes_through_a_symlink() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");

    let outside = tmp.path().join("outside.md");
    fs::write(&outside, DECISION_FILE).unwrap();
    fs::remove_file(dest.join("decisions/db.md")).unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("decisions/db.md")).unwrap();
    fixture.remove("decisions/db.md");

    let err = bidi::run(&deleting_args(addr, &dest, None), SyncMode::Apply)
        .await
        .expect_err("symlink refused");
    assert!(err.to_string().contains("refused"), "{err}");
    assert!(
        fs::symlink_metadata(dest.join("decisions/db.md"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link is left alone"
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), DECISION_FILE);
}

#[tokio::test]
async fn sync_adopts_the_server_rendering_after_an_import() {
    let fixture = Fixture {
        redact: Some("hunter2"),
        ..Fixture::with_pages(vec![decision()])
    };
    let (addr, fixture) = serve(fixture).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    fs::write(
        dest.join("decisions/db.md"),
        DECISION_FILE.replace("Use Postgres.", "password hunter2"),
    )
    .unwrap();
    sync(addr, &dest).await.expect("import");
    let file = fs::read_to_string(dest.join("decisions/db.md")).unwrap();
    assert!(
        file.contains("[REDACTED]") && !file.contains("hunter2"),
        "{file}"
    );

    sync(addr, &dest).await.expect("settled");
    assert_eq!(fixture.mcp_writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn sync_upgrades_a_slice_one_export_without_importing() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    // What slice 1 left behind: the bare body, and its hash as the base.
    fs::create_dir_all(dest.join("decisions")).unwrap();
    fs::write(dest.join("decisions/db.md"), "# DB\nUse Postgres.\n").unwrap();
    let mut old = SyncState::default();
    old.pages.insert(
        "decisions/db.md".into(),
        state::PageState {
            hash: state::sha256_hex(b"# DB\nUse Postgres.\n"),
            etag: None,
            page_id: None,
        },
    );
    state::save(&dest, &old).unwrap();

    sync(addr, &dest).await.expect("upgrade");
    assert_eq!(
        fixture.mcp_writes.load(Ordering::SeqCst),
        0,
        "an unedited file is not imported"
    );
    assert_eq!(
        fs::read_to_string(dest.join("decisions/db.md")).unwrap(),
        DECISION_FILE
    );
}
