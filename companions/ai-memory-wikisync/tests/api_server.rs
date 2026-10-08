//! Integration tests against a fixture `/api/v1` server (axum, temp port):
//! 200/ETag/304/401/404 and incremental-cursor pagination, plus the
//! end-to-end plan/export/local-edit-refusal flow through `run`.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ai_memory_wikisync::bidi::{self, Prefer, SyncArgs};
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
            path: path.to_string(),
            body: body.to_string(),
            title: format!("title {path}"),
            tier: "semantic".to_string(),
            pinned: false,
            frontmatter: json!({}),
        }
    }
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
    reads_by_path: Mutex<BTreeMap<String, usize>>,
    recent_calls: AtomicUsize,
    page_reads_200: AtomicUsize,
    page_reads_304: AtomicUsize,
    mcp_writes: AtomicUsize,
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
            reads_by_path: Mutex::new(BTreeMap::new()),
            recent_calls: AtomicUsize::new(0),
            page_reads_200: AtomicUsize::new(0),
            page_reads_304: AtomicUsize::new(0),
            mcp_writes: AtomicUsize::new(0),
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
    let document = json!({
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

/// `POST /mcp` `tools/call memory_write_page` with the server's replace
/// semantics: every field the call omits is cleared, and the stored body
/// passes a redaction step like the server's sanitizer.
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
    let args = &request["params"]["arguments"];
    if request["params"]["name"] != "memory_write_page"
        || args["workspace"] != "demo"
        || args["project"] != "app"
    {
        return Json(json!({"jsonrpc": "2.0", "id": request["id"],
            "result": {"isError": true, "content": [{"type": "text", "text": "bad call"}]}}))
        .into_response();
    }
    fixture.mcp_writes.fetch_add(1, Ordering::SeqCst);
    let path = args["path"].as_str().unwrap_or_default().to_string();
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
    {
        let mut pages = fixture.pages.lock().unwrap();
        pages.retain(|page| page.path != path);
        pages.push(written);
    }
    Json(json!({"jsonrpc": "2.0", "id": request["id"],
        "result": {"isError": false, "content": [{"type": "text", "text": "{}"}]}}))
    .into_response()
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
    }
}

async fn sync(addr: SocketAddr, dest: &std::path::Path) -> anyhow::Result<()> {
    bidi::run(&sync_args(addr, dest, None), true).await
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
    bidi::run(&sync_args(addr, &dest, None), false)
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

    bidi::run(&sync_args(addr, &dest, Some(Prefer::Server)), true)
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
    bidi::run(&sync_args(addr, &dest, Some(Prefer::Repo)), true)
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
async fn sync_stops_when_the_server_page_changes_before_the_write() {
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision()])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");
    fs::write(
        dest.join("decisions/db.md"),
        DECISION_FILE.replace("Use Postgres.", "Repo edit."),
    )
    .unwrap();

    // Reads so far: 1 by the first sync. The second sync classifies with
    // read 2 (a 304) and 3 (the full page for the import); read 4 is the
    // re-check right before the write, and lands after an edit.
    let fixture_with_race = Fixture {
        edit_on_read: Some(("decisions/db.md", 4)),
        ..Fixture::with_pages(vec![fixture.page("decisions/db.md").unwrap()])
    };
    let (race_addr, raced) = serve(fixture_with_race).await;
    // Seed the race server's read counter like the first server's.
    raced
        .reads_by_path
        .lock()
        .unwrap()
        .insert("decisions/db.md".into(), 1);
    let err = sync(race_addr, &dest).await.expect_err("race detected");
    assert!(
        format!("{err:#}").contains("changed on the server during this run"),
        "{err:#}"
    );
    assert_eq!(
        raced.mcp_writes.load(Ordering::SeqCst),
        0,
        "the write never happened"
    );
}

#[tokio::test]
async fn sync_reports_deletes_and_never_propagates_them() {
    let mut other = decision();
    other.path = "decisions/other.md".to_string();
    let (addr, fixture) = serve(Fixture::with_pages(vec![decision(), other])).await;
    let (_tmp, dest) = temp_dest();
    sync(addr, &dest).await.expect("first sync");

    fixture.remove("decisions/db.md");
    fs::remove_file(dest.join("decisions/other.md")).unwrap();
    sync(addr, &dest)
        .await
        .expect("deletes are notices, not refusals");
    assert!(
        dest.join("decisions/db.md").exists(),
        "server delete not propagated"
    );
    assert!(
        fixture.page("decisions/other.md").is_some(),
        "repo delete not propagated"
    );
    assert_eq!(fixture.mcp_writes.load(Ordering::SeqCst), 0);

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
