//! Multi-actor concurrency stress tests for the per-session / per-actor
//! [`ActiveProject`] isolation modes.
//!
//! Unit tests in `ai_memory_core::active_project` cover the map's
//! single-threaded contract. These tests exercise the *production wiring*
//! — `/hook` → `process_envelope` → `set_for(actor)` interleaved with
//! `/mcp tools/call` → `actor_key_from_parts` → `get_for(actor)` — under
//! genuine `tokio::spawn` concurrency, so a race in the mutex, the LRU
//! eviction order, or the actor-key extraction surfaces here rather than
//! in prod.
//!
//! The router is assembled in-process (axum `Router` + tower
//! `ServiceExt::oneshot`), the auth middleware is replaced by a fake that
//! injects `ActorContext` from `x-test-actor-user` / `x-test-actor-session-id`
//! request headers — exactly the field the production middleware fills.
//! Session id is also accepted via the production `x-memory-actor-session-id`
//! header on /mcp so the "header is cache key, not credential" scenario can
//! exercise the same fallback the real server walks.

use ai_memory_core::{ActiveProject, ActiveProjectMode, ActorContext};
use ai_memory_hooks::{
    DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, HookState, ProjectCacheStore, SubagentSessionSet,
    hook_router,
};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

/// All scenarios share a server with this many seeded projects, each with a
/// `marker.md` page whose body uniquely identifies the project. Reads after a
/// hook event must find the marker for the project the hook published — not
/// any other.
const SEEDED_PROJECTS: usize = 8;

/// English number words, indexed from 1 — used as space-separated FTS5
/// tokens so we can both find any marker via the shared `markerstream`
/// word and tell which project a given hit came from. Indexed 1..=8.
fn number_word(i: usize) -> &'static str {
    match i {
        1 => "alphaone",
        2 => "alphatwo",
        3 => "alphathree",
        4 => "alphafour",
        5 => "alphafive",
        6 => "alphasix",
        7 => "alphaseven",
        8 => "alphaeight",
        _ => panic!("number_word: only 1..=8 supported, got {i}"),
    }
}

/// Per-actor map config used by most scenarios — large enough not to evict
/// under normal concurrency, small enough that the burst test can overflow it.
const TEST_TTL: Duration = Duration::from_secs(60);

struct Harness {
    router: Router,
    active_project: ActiveProject,
    _store: Store,
    _wiki: Wiki,
    _tmp: TempDir,
}

impl Harness {
    /// Build a complete in-process server: store + wiki + N seeded projects
    /// + MCP service + hook router + actor-injecting fake-auth middleware.
    async fn build(mode: ActiveProjectMode, ttl: Duration, max_entries: usize) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let store = Store::open(tmp.path()).expect("store");
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .expect("ws");
        // Baked default project; hook events that set their own cwd take
        // precedence via the per-actor map.
        let baked_proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .expect("baked proj");

        let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");

        // Seed N distinct projects, each with a unique marker page so MCP
        // reads can verify they resolved into the right active_project.
        //
        // The FTS5 tokenizer is `unicode61 ... tokenchars '/_-'`, so
        // `_` is part of a token (NOT a separator). A body like
        // "PROJ_1_BODY" tokenizes as a single token and a `PROJ` query
        // misses it. Use space-separated words instead: a shared word
        // "markerstream" that every marker carries, plus a per-project
        // unique english-number word so we can both find any marker and
        // tell which project it came from.
        for i in 1..=SEEDED_PROJECTS {
            let proj = store
                .writer
                .get_or_create_project(ws, format!("proj-{i}"), None)
                .await
                .expect("seed proj");
            store
                .writer
                .upsert_page(ai_memory_core::NewPage {
                    workspace_id: ws,
                    project_id: proj,
                    path: ai_memory_core::PagePath::new("marker.md").expect("path"),
                    title: format!("Marker {i}"),
                    body: format!("markerstream {tag}", tag = number_word(i)),
                    tier: ai_memory_core::Tier::Semantic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                })
                .await
                .expect("seed page");
        }

        let active_project = ActiveProject::with_config(mode, ttl, max_entries);

        let server =
            AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked_proj)
                .with_wiki(wiki.clone())
                .with_active_project(active_project.clone());

        let mcp_service = StreamableHttpService::new(
            move || Ok(server.clone()),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true),
        );

        let project_cache: ai_memory_hooks::ProjectCache =
            Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default()));
        let hooks = hook_router(HookState {
            workspace_id: ws,
            project_id: baked_proj,
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki: wiki.clone(),
            consolidator: None,
            sanitizer: ai_memory_core::Sanitizer::default(),
            project_cache,
            active_project: active_project.clone(),
            ingest_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
            )),
            ingest_gates: ai_memory_hooks::IngestGates::default(),
            consolidate_on_session_end: false,
            capture_assistant_enabled: false,
            subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
            ingest_rate: Arc::new(tokio::sync::Mutex::new(
                ai_memory_hooks::IngestRateLimiter::disabled(),
            )),
            home_dir: None,
        });

        let router = Router::new()
            .nest_service("/mcp", mcp_service)
            .merge(hooks)
            // Fake auth: read test headers, inject ActorContext exactly the
            // way the production auth middleware (rung 1 / rung 2) does.
            // This is the seam under test — the engine's per-actor map keys
            // off the *extension*, never the raw header value, for `user`.
            .layer(axum::middleware::from_fn(inject_actor_from_test_headers));

        Self {
            router,
            active_project,
            _store: store,
            _wiki: wiki,
            _tmp: tmp,
        }
    }
}

/// Test-only middleware that mimics production auth: read `x-test-actor-user`
/// and `x-test-actor-session-id`, inject them as an [`ActorContext`] extension.
async fn inject_actor_from_test_headers(
    mut req: Request<Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let user = req
        .headers()
        .get("x-test-actor-user")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let session_id = req
        .headers()
        .get("x-test-actor-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if user.is_some() || session_id.is_some() {
        req.extensions_mut().insert(ActorContext {
            user,
            session_id,
            ..ActorContext::default()
        });
    }
    next.run(req).await
}

/// Build a `POST /hook` request publishing `cwd → proj-{i}` for the given
/// `session_id` (and optional `user` for per-actor mode). The body
/// `session_id` field is what the engine actually keys the map by — we mirror
/// it in the header so the MCP read in the same actor's lane lines up.
fn hook_request(user: Option<&str>, session_id: &str, project_index: usize) -> Request<Body> {
    let body = json!({
        "event": "session-start",
        "session_id": session_id,
        "cwd": format!("/tmp/aim-test/proj-{project_index}"),
    });
    let mut builder = Request::builder()
        .method("POST")
        .uri(
            "/hook?event=session-start&agent=claude-code&project=proj-{i}"
                .replace("{i}", &project_index.to_string()),
        )
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("x-test-actor-session-id", session_id);
    if let Some(u) = user {
        builder = builder.header("x-test-actor-user", u);
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("hook req")
}

/// Build a `POST /mcp` tools/call request for `memory_query` matching the
/// shared `markerstream` token every seeded marker carries. The
/// assertion downstream is on WHICH marker (and therefore which
/// project's unique `alpha*` word) the per-actor scope resolves to.
fn mcp_query_request(user: Option<&str>, session_id: Option<&str>) -> Request<Body> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_query",
            "arguments": { "query": "markerstream", "limit": 20 }
        }
    });
    build_mcp_request(body.to_string(), user, session_id)
}

/// Build a `POST /mcp` tools/call request for `memory_write_page`. The page
/// path is unique per call so concurrent writes don't collide on the same
/// row; the per-actor map decides which *project* the write lands in.
fn mcp_write_request(
    user: Option<&str>,
    session_id: Option<&str>,
    page_path: &str,
    body_text: &str,
) -> Request<Body> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "memory_write_page",
            "arguments": {
                "path": page_path,
                "body": format!("# Title\n\n{body_text}"),
            }
        }
    });
    build_mcp_request(body.to_string(), user, session_id)
}

fn build_mcp_request(body: String, user: Option<&str>, session_id: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(u) = user {
        builder = builder.header("x-test-actor-user", u);
    }
    if let Some(s) = session_id {
        // Send BOTH the test-only header (which the fake middleware lifts
        // into the ActorContext extension) AND the production
        // `x-memory-actor-session-id` header (which the MCP server reads
        // directly from raw headers as the rung-4 fallback). Setting both
        // mirrors what an authenticated browser session would look like
        // and avoids depending on which one the server prefers.
        builder = builder
            .header("x-test-actor-session-id", s)
            .header("x-memory-actor-session-id", s);
    }
    builder.body(Body::from(body)).expect("mcp req")
}

async fn response_body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body bytes");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

/// Parse a JSON-RPC tools/call response and return the joined text of all
/// `content[]` text items. Panics with the raw body on protocol errors so
/// failures are debuggable.
fn extract_tool_text(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("non-JSON response: {body}\nerr: {e}"));
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}\nfull: {body}");
    }
    let content = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {body}"));
    content
        .iter()
        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Fire a hook synchronously and wait for the `/hook` 202 ACCEPTED + a
/// brief settle so the spawned `process_envelope` task lands its write to
/// the active-project map before the caller continues. This matches the
/// production "hooks are fire-and-forget" contract — tests need an explicit
/// barrier because they assert on what the map looks like immediately after.
async fn fire_hook_and_settle(
    router: &Router,
    user: Option<&str>,
    session_id: &str,
    project_index: usize,
) {
    let resp = router
        .clone()
        .oneshot(hook_request(user, session_id, project_index))
        .await
        .expect("hook oneshot");
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "hook must accept");
    // Drain the response body before settling so the connection releases.
    let _ = response_body_text(resp).await;
    // `process_envelope` is `tokio::spawn`-ed; poll a read for THIS session
    // until its write is actually visible, instead of guessing a fixed settle
    // time. A fixed sleep races under heavy load — the observed flake was a
    // loaded full-workspace run where a session's active-project write hadn't
    // landed yet, so its read saw `{hits: []}`. Polling waits exactly as long
    // as needed (normally one iteration), with a generous cap that only burns
    // if the write genuinely never lands (a real bug the downstream assert
    // then reports).
    let expected = number_word(project_index);
    for _ in 0..200 {
        tokio::task::yield_now().await;
        let probe = router
            .clone()
            .oneshot(mcp_query_request(user, Some(session_id)))
            .await
            .expect("settle probe");
        if probe.status() == StatusCode::OK
            && extract_tool_text(&response_body_text(probe).await).contains(expected)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 1 — per_session isolates concurrent reads after one hook each.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_session_isolates_concurrent_reads_after_hook() {
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;

    // Each session i sets its active project to proj-i via one hook event.
    for i in 1..=SEEDED_PROJECTS {
        fire_hook_and_settle(&h.router, None, &format!("sess-{i}"), i).await;
    }

    // Now fan out: each session does M parallel reads. Every read must
    // resolve to its own project's marker, not some neighbour's.
    const READS_PER_SESSION: usize = 20;
    let mut handles = Vec::new();
    for i in 1..=SEEDED_PROJECTS {
        for _ in 0..READS_PER_SESSION {
            let router = h.router.clone();
            let session = format!("sess-{i}");
            handles.push(tokio::spawn(async move {
                let resp = router
                    .oneshot(mcp_query_request(None, Some(&session)))
                    .await
                    .expect("mcp oneshot");
                assert_eq!(resp.status(), StatusCode::OK, "session {i} read status");
                let body = response_body_text(resp).await;
                let text = extract_tool_text(&body);
                (i, text)
            }));
        }
    }

    for h in handles {
        let (i, text) = h.await.expect("join");
        let own = number_word(i);
        assert!(
            text.contains(own),
            "session {i} did NOT see its own marker. Expected {own:?} in result text:\n{text}"
        );
        // Every other project's marker must be ABSENT — that's the
        // isolation guarantee. Reads must not bleed across sessions.
        for j in 1..=SEEDED_PROJECTS {
            if j == i {
                continue;
            }
            let foreign = number_word(j);
            assert!(
                !text.contains(foreign),
                "session {i} leaked into sibling proj-{j}.\n  saw: {foreign:?}\n  in:  {text}"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 2 — per_session isolates concurrent writes.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_session_isolates_concurrent_writes() {
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;

    for i in 1..=SEEDED_PROJECTS {
        fire_hook_and_settle(&h.router, None, &format!("sess-{i}"), i).await;
    }

    // Each session writes a uniquely-named page concurrently.
    let mut write_handles = Vec::new();
    for i in 1..=SEEDED_PROJECTS {
        let router = h.router.clone();
        let session = format!("sess-{i}");
        let page_path = format!("writes/from-sess-{i}.md");
        // Underscore-free body word so the FTS5 tokenizer (which treats
        // `_` as a TOKEN character, not a separator) keeps the marker
        // searchable. Each session gets a unique english-number-tagged
        // word so the round-trip read can distinguish own vs leaked.
        let body = format!("writefromsess{tag}", tag = number_word(i));
        write_handles.push(tokio::spawn(async move {
            let resp = router
                .oneshot(mcp_write_request(None, Some(&session), &page_path, &body))
                .await
                .expect("mcp write oneshot");
            assert_eq!(resp.status(), StatusCode::OK, "write {i} status");
            let body = response_body_text(resp).await;
            // Ensure the call did NOT error; we don't need to parse the
            // detailed result here — the round-trip read below is the
            // real assertion.
            let _ = extract_tool_text(&body);
            i
        }));
    }
    for w in write_handles {
        w.await.expect("write join");
    }

    // Round-trip: each session reads back its own page. Because the per-
    // session active_project still points at proj-i, memory_query for
    // "WRITE_FROM_SESS_" must find ONLY the row that landed in proj-i.
    let mut read_handles = Vec::new();
    for i in 1..=SEEDED_PROJECTS {
        let router = h.router.clone();
        let session = format!("sess-{i}");
        read_handles.push(tokio::spawn(async move {
            let body_q = json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "memory_query",
                    "arguments": { "query": "writefromsess*", "limit": 20 }
                }
            });
            let resp = router
                .oneshot(build_mcp_request(body_q.to_string(), None, Some(&session)))
                .await
                .expect("mcp read oneshot");
            let body = response_body_text(resp).await;
            (i, extract_tool_text(&body))
        }));
    }

    for r in read_handles {
        let (i, text) = r.await.expect("read join");
        let own = format!("writefromsess{tag}", tag = number_word(i));
        assert!(
            text.contains(&own),
            "sess-{i} write did not round-trip in its own scope:\n{text}"
        );
        // Strict isolation: no other session's payload should leak. The
        // per-session map keeps each write in its own project_id, so a
        // memory_query scoped by the same session cannot see siblings.
        for j in 1..=SEEDED_PROJECTS {
            if j == i {
                continue;
            }
            let foreign = format!("writefromsess{tag}", tag = number_word(j));
            assert!(
                !text.contains(&foreign),
                "sess-{i} read leaked write from sess-{j}:\n{text}"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 3 — per_actor: users sharing a session_id are still isolated.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_actor_isolates_users_sharing_session_id() {
    let h = Harness::build(
        ActiveProjectMode::PerActor,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;

    // Alice and Bob send hooks with the SAME `session_id` but DIFFERENT
    // users. In `per_actor` mode they must land in distinct map slots
    // because the key is `(user, session_id)`, not `session_id` alone.
    let shared_sess = "shared-session-x";
    fire_hook_and_settle(&h.router, Some("alice"), shared_sess, 1).await;
    fire_hook_and_settle(&h.router, Some("bob"), shared_sess, 5).await;

    // Alice reads — must see proj-1's marker, never proj-5's.
    let resp = h
        .router
        .clone()
        .oneshot(mcp_query_request(Some("alice"), Some(shared_sess)))
        .await
        .expect("alice read");
    let alice_text = extract_tool_text(&response_body_text(resp).await);
    assert!(
        alice_text.contains(number_word(1)),
        "alice did not see her own proj-1 marker:\n{alice_text}"
    );
    assert!(
        !alice_text.contains(number_word(5)),
        "alice leaked into bob's proj-5:\n{alice_text}"
    );

    // Bob reads — must see proj-5's marker, never proj-1's.
    let resp = h
        .router
        .clone()
        .oneshot(mcp_query_request(Some("bob"), Some(shared_sess)))
        .await
        .expect("bob read");
    let bob_text = extract_tool_text(&response_body_text(resp).await);
    assert!(
        bob_text.contains(number_word(5)),
        "bob did not see his own proj-5 marker:\n{bob_text}"
    );
    assert!(
        !bob_text.contains(number_word(1)),
        "bob leaked into alice's proj-1:\n{bob_text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_actor_unknown_mcp_session_does_not_use_latest_user_slot() {
    let h = Harness::build(
        ActiveProjectMode::PerActor,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;

    // Issue #97: HTTP MCP requests carry their own MCP session id, not the
    // hook payload's session id. If the keyed lookup misses, falling back to
    // Alice's user-only "latest project" slot makes one repo read another.
    fire_hook_and_settle(&h.router, Some("alice"), "hook-session-a", 1).await;
    fire_hook_and_settle(&h.router, Some("alice"), "hook-session-b", 4).await;

    let resp = h
        .router
        .clone()
        .oneshot(mcp_query_request(
            Some("alice"),
            Some("mcp-session-from-http"),
        ))
        .await
        .expect("mismatched mcp-session read");
    assert_eq!(resp.status(), StatusCode::OK);
    let text = extract_tool_text(&response_body_text(resp).await);

    for leaked in [number_word(1), number_word(4)] {
        assert!(
            !text.contains(leaked),
            "unmatched MCP session must not fall back to Alice's hook-published latest project {leaked:?}:\n{text}"
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 4 — burst above max_entries: engine survives, no panic / no 5xx.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_above_max_entries_does_not_corrupt() {
    // max_entries deliberately tiny so the burst is sure to exceed it.
    let h = Harness::build(ActiveProjectMode::PerSession, TEST_TTL, 8).await;

    const BURST_SIZE: usize = 64;

    // Fan out: BURST_SIZE distinct sessions all hooking in parallel. The
    // map can hold only 8, so the LRU eviction path is under contention.
    let mut hook_handles = Vec::new();
    for i in 0..BURST_SIZE {
        let router = h.router.clone();
        let sess = format!("burst-{i}");
        // Cycle through the seeded projects so every hook resolves a real
        // (workspace, project) pair — the eviction code shouldn't care
        // about WHICH ids, just that inserting under contention doesn't
        // tear the map.
        let proj_idx = (i % SEEDED_PROJECTS) + 1;
        hook_handles.push(tokio::spawn(async move {
            let resp = router
                .oneshot(hook_request(None, &sess, proj_idx))
                .await
                .expect("burst hook oneshot");
            assert_eq!(
                resp.status(),
                StatusCode::ACCEPTED,
                "burst hook {i} must accept under load"
            );
        }));
    }
    for h in hook_handles {
        h.await.expect("burst hook join");
    }

    // Settle: every spawned process_envelope must drain.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now BURST_SIZE parallel reads. Every read must return a JSON-RPC
    // 200 — no panic, no 500. The CONTENT may be any of the projects
    // we hooked (LRU may have evicted), but the *server* must never
    // tear.
    let mut read_handles = Vec::new();
    for i in 0..BURST_SIZE {
        let router = h.router.clone();
        let sess = format!("burst-{i}");
        read_handles.push(tokio::spawn(async move {
            let resp = router
                .oneshot(mcp_query_request(None, Some(&sess)))
                .await
                .expect("burst read oneshot");
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "burst read {i} status must be 200"
            );
            let body = response_body_text(resp).await;
            // Any JSON-RPC response that is NOT an error is acceptable;
            // we just need to confirm the server didn't crash mid-burst.
            let v: serde_json::Value = serde_json::from_str(&body)
                .unwrap_or_else(|e| panic!("non-JSON burst read {i}: {body}\nerr: {e}"));
            assert!(
                v.get("error").is_none(),
                "burst read {i} produced JSON-RPC error: {body}"
            );
        }));
    }
    for h in read_handles {
        h.await.expect("burst read join");
    }

    // Invariant check on the map itself: the LRU cap must be enforced
    // even after the burst. We can't easily count entries from outside
    // (no public accessor), but we can verify the single-slot fallback
    // still resolves — that's the graceful-degradation contract.
    assert!(
        h.active_project.get().is_some(),
        "after a burst, the single-slot fallback must still be populated"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 5 — TTL eviction under concurrent insertion (no zombie reads).
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttl_eviction_under_concurrent_insertion() {
    // Short TTL so the test runs in seconds; large max so eviction is
    // strictly TTL-driven, not capacity-driven.
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        Duration::from_millis(150),
        4096,
    )
    .await;

    // Phase 1: insert N distinct sessions, read them back immediately —
    // each must see its own project.
    const N: usize = 12;
    for i in 0..N {
        fire_hook_and_settle(
            &h.router,
            None,
            &format!("ttl-{i}"),
            (i % SEEDED_PROJECTS) + 1,
        )
        .await;
    }

    // Phase 2: wait past the TTL.
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Phase 3: parallel reads after expiry. The per-session entries are
    // gone; reads must NOT panic and must NOT return cross-actor data.
    // Missing keyed entries fail closed to the baked default, so what's not
    // acceptable is a 500, a JSON-RPC error, or a foreign marker.
    let mut handles = Vec::new();
    for i in 0..N {
        let router = h.router.clone();
        let sess = format!("ttl-{i}");
        handles.push(tokio::spawn(async move {
            let resp = router
                .oneshot(mcp_query_request(None, Some(&sess)))
                .await
                .expect("ttl read oneshot");
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "post-TTL read {i} status must be 200"
            );
            let body = response_body_text(resp).await;
            let text = extract_tool_text(&body);
            for j in 1..=SEEDED_PROJECTS {
                let foreign = number_word(j);
                assert!(
                    !text.contains(foreign),
                    "expired session {i} leaked seeded project marker {foreign:?}:\n{text}"
                );
            }
        }));
    }
    for h in handles {
        h.await.expect("ttl read join");
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 6 — header session_id is a cache key, NOT a credential.
// ────────────────────────────────────────────────────────────────────────────
//
// In per_actor mode, the *user* component of the actor key is read ONLY from
// the auth middleware's [`ActorContext`] extension — never from a client-
// supplied header. So if Bob sends the `x-memory-actor-session-id` header
// with Alice's session id, the engine still keys by `(Bob, alice-session)`
// — a fresh slot, not Alice's slot. This is the documented trust boundary;
// pin it explicitly so a refactor can't quietly downgrade `user` to the
// header path.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn header_session_id_is_cache_key_not_credential() {
    let h = Harness::build(
        ActiveProjectMode::PerActor,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;

    // Alice publishes her active project (proj-2) via hook.
    fire_hook_and_settle(&h.router, Some("alice"), "alice-session", 2).await;

    // Bob attempts to escalate: he sends an MCP read with Alice's session
    // id in the header. The MIDDLEWARE-injected `user` (from his own
    // `x-test-actor-user`) is "bob"; the *session_id* is Alice's. Per-
    // actor key becomes `(bob, alice-session)` — distinct from
    // `(alice, alice-session)`, so Bob's read must NOT resolve into
    // proj-2.
    let resp = h
        .router
        .clone()
        .oneshot(mcp_query_request(Some("bob"), Some("alice-session")))
        .await
        .expect("bob spoof read");
    let bob_text = extract_tool_text(&response_body_text(resp).await);

    // Bob hooked nothing, so his per-actor slot is empty. Make Bob also hook
    // to a different project; the spoofed request still keys by the unmatched
    // `(bob, alice-session)` tuple and must not fall back to either Alice's
    // project or Bob's latest project.
    fire_hook_and_settle(&h.router, Some("bob"), "bob-own-session", 7).await;
    let resp = h
        .router
        .clone()
        .oneshot(mcp_query_request(Some("bob"), Some("alice-session")))
        .await
        .expect("bob spoof read 2");
    let bob_text_after_own_hook = extract_tool_text(&response_body_text(resp).await);
    // The legitimate Bob slot is keyed by `(bob, bob-own-session)`. The spoof
    // request keys `(bob, alice-session)` — a fresh slot — and must fail closed
    // instead of falling through to the single/user latest slot.
    let _ = bob_text; // first probe kept for diagnostics
    assert!(
        !bob_text_after_own_hook.contains(number_word(2))
            && !bob_text_after_own_hook.contains(number_word(7)),
        "header forgery must not fall back to alice's proj-2 or bob's latest proj-7.\nGot: {bob_text_after_own_hook}"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 7 — sustained-rate stress.
//
// One-shot bursts (scenarios 1–6) prove the map handles a *spike* of
// concurrent ops; this scenario proves it handles a *sustained flow*
// over time without:
//   - latency creeping up (lock contention growing unboundedly),
//   - cross-actor reads slipping through under steady-state churn,
//   - dropped /hook 202s under continuous load.
//
// Duration is short enough for CI (~5s) but long enough to dwarf the
// per-op latency, so a flapping invariant would surface here even with
// only thousands of cycles. Crank `STRESS_DURATION_SECS` env var to
// repro a flake locally over a longer window.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sustained_per_session_isolation_holds_under_continuous_traffic() {
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;

    // 4 distinct sessions; each one owns its own seeded project. Every
    // iteration in the loop re-hooks (set active project) and then
    // reads, asserting the read returns the OWN marker and NEVER a
    // sibling's. The hook+read pair is the smallest cycle that touches
    // every code path in the per-session map: set_for, purge_expired,
    // enforce_cap, get_for.
    const SESSIONS: usize = 4;
    let duration_secs: u64 = std::env::var("STRESS_DURATION_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let duration = Duration::from_secs(duration_secs);

    // Prime each session once and wait until its own keyed slot is visible.
    // The sustained loop below intentionally does not sleep between hook and
    // read, but without this initial barrier op #0 can race before the first
    // asynchronous hook processor has ever published that session's entry and
    // legitimately miss its own marker before the keyed slot exists.
    for i in 1..=SESSIONS {
        let session = format!("sustained-sess-{i}");
        let resp = h
            .router
            .clone()
            .oneshot(hook_request(None, &session, i))
            .await
            .expect("initial sustained hook oneshot");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let _ = response_body_text(resp).await;

        let own = number_word(i);
        let mut settled = false;
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            let resp = h
                .router
                .clone()
                .oneshot(mcp_query_request(None, Some(&session)))
                .await
                .expect("initial sustained read oneshot");
            assert_eq!(resp.status(), StatusCode::OK);
            let text = extract_tool_text(&response_body_text(resp).await);
            if text.contains(own) {
                settled = true;
                break;
            }
        }
        assert!(
            settled,
            "session {session} did not publish its initial {own:?} marker before stress loop"
        );
    }

    // One driver task per session, all running concurrently. Each
    // returns its op count + the first cross-actor leak it observed
    // (None means clean).
    let mut drivers = Vec::new();
    for i in 1..=SESSIONS {
        let router = h.router.clone();
        let session = format!("sustained-sess-{i}");
        let deadline = std::time::Instant::now() + duration;
        drivers.push(tokio::spawn(async move {
            let mut ops = 0_u64;
            let mut first_leak: Option<String> = None;
            while std::time::Instant::now() < deadline {
                // 1. Hook to pin active project = proj-i for this session.
                let resp = router
                    .clone()
                    .oneshot(hook_request(None, &session, i))
                    .await
                    .expect("sustained hook oneshot");
                if resp.status() != StatusCode::ACCEPTED {
                    return (ops, Some(format!("hook returned {}", resp.status())));
                }
                let _ = response_body_text(resp).await;

                // 2. Read; assert own marker present, no sibling marker.
                //    Skip a hard barrier sleep between hook and read —
                //    the spawned process_envelope is short, and any
                //    lag just shows up as a temporary cross-actor read
                //    (which IS what we want to catch).
                tokio::task::yield_now().await;
                let resp = router
                    .clone()
                    .oneshot(mcp_query_request(None, Some(&session)))
                    .await
                    .expect("sustained read oneshot");
                if resp.status() != StatusCode::OK {
                    return (ops, Some(format!("read returned {}", resp.status())));
                }
                let text = extract_tool_text(&response_body_text(resp).await);
                let own = number_word(i);

                // OWN marker may take a hook cycle to surface (the hook
                // is spawned async); ABSENCE of own is acceptable ONLY
                // if no foreign marker is present either. Foreign marker
                // present is a HARD leak.
                for j in 1..=SESSIONS {
                    if j == i {
                        continue;
                    }
                    let foreign = number_word(j);
                    if text.contains(foreign) && first_leak.is_none() {
                        first_leak = Some(format!(
                            "sess-{i} saw sess-{j}'s marker {foreign:?} (own={own}) on op #{ops}:\n{text}"
                        ));
                        break;
                    }
                }
                ops += 1;
            }
            (ops, first_leak)
        }));
    }

    let mut total_ops = 0_u64;
    let mut leaks = Vec::new();
    for d in drivers {
        let (ops, leak) = d.await.expect("driver join");
        total_ops += ops;
        if let Some(msg) = leak {
            leaks.push(msg);
        }
    }

    assert!(
        leaks.is_empty(),
        "sustained-rate stress observed {} cross-actor leak(s):\n{}",
        leaks.len(),
        leaks.join("\n---\n")
    );

    // Lower bound on completed ops — a generous floor that would only
    // fail if the engine seized up entirely under load. The real signal
    // is the leak assertion; this is just a "did we actually exercise
    // anything" check so a future regression making each op take 5s
    // doesn't pass a no-op test.
    let min_ops = (SESSIONS as u64) * 50;
    assert!(
        total_ops >= min_ops,
        "sustained-rate stress completed only {total_ops} ops across {SESSIONS} \
         sessions in {duration_secs}s (expected >= {min_ops}). \
         Either the harness is broken or perf regressed catastrophically."
    );
    println!("sustained stress: {total_ops} ops in {duration_secs}s ({SESSIONS} sessions)");
}

// ────────────────────────────────────────────────────────────────────────────
// Scenario 8 — real agent payload shapes (Claude / OpenCode / Codex).
//
// `HookEnvelope::from_query_and_body` extracts `session_id` and `cwd`
// from a list of well-known keys. The autoscope per-session map keys
// on the *raw extracted session_id*, so each agent's wire shape must
// reach the same map slot when the session id value matches — even if
// it lives at a different JSON path.
//
// The tests below send hand-crafted payloads in the shape each agent
// CLI actually emits (per `payload.rs::from_query_and_body`'s key
// list), then assert that a per-session MCP read keyed by the *same
// session_id value* finds the project the hook published. A
// regression in either the extractor OR the per-session resolution
// would break exactly one of these.
// ────────────────────────────────────────────────────────────────────────────

fn hook_with_raw_body(
    body: serde_json::Value,
    project_index: usize,
    session_id: &str,
) -> Request<Body> {
    // Same `project` query arg as the rest of the suite so the hook
    // resolves directly to the seeded `proj-{i}` row regardless of how
    // the agent shape encodes cwd.
    let uri = format!("/hook?event=session-start&agent=claude-code&project=proj-{project_index}");
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", "localhost")
        .header("content-type", "application/json")
        // The MCP read side will need the same session id we want the
        // hook to extract; mirror it on the header too so the read's
        // `actor_key_from_parts` lines up with the hook's body extract.
        .header("x-test-actor-session-id", session_id)
        .body(Body::from(body.to_string()))
        .expect("agent-shape hook req")
}

async fn fire_raw_hook_and_settle(
    router: &Router,
    body: serde_json::Value,
    project_index: usize,
    session_id: &str,
) {
    let resp = router
        .clone()
        .oneshot(hook_with_raw_body(body, project_index, session_id))
        .await
        .expect("agent-shape hook oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "agent-shape hook must accept"
    );
    let _ = response_body_text(resp).await;
    let expected = number_word(project_index);
    for _ in 0..200 {
        tokio::task::yield_now().await;
        let probe = router
            .clone()
            .oneshot(mcp_query_request(None, Some(session_id)))
            .await
            .expect("agent-shape settle probe");
        if probe.status() == StatusCode::OK
            && extract_tool_text(&response_body_text(probe).await).contains(expected)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn read_session_marker(router: &Router, session_id: &str) -> String {
    let resp = router
        .clone()
        .oneshot(mcp_query_request(None, Some(session_id)))
        .await
        .expect("agent-shape mcp oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    extract_tool_text(&response_body_text(resp).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_code_payload_shape_routes_per_session() {
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;
    // Claude Code's session-start hook (per `from_query_and_body`'s key
    // list): top-level `session_id`, top-level `cwd`,
    // `hook_event_name` discriminator.
    let body = json!({
        "hook_event_name": "SessionStart",
        "session_id": "claude-sess-A",
        "cwd": "/tmp/aim-test/proj-3",
        "tools": ["Read", "Write"],
        "version": "1.0.0",
    });
    fire_raw_hook_and_settle(&h.router, body, 3, "claude-sess-A").await;
    let text = read_session_marker(&h.router, "claude-sess-A").await;
    assert!(
        text.contains(number_word(3)),
        "Claude Code shape did not route to proj-3:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_payload_shape_routes_per_session() {
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;
    // OpenCode's SDK on `session.idle` / `tool.execute.*` events uses
    // `sessionID` (capital ID) nested under `properties` for some
    // events, and at the top level for others. The extractor accepts
    // BOTH; this test pins the top-level form. The nested form is
    // covered by the next test.
    let body = json!({
        "sessionID": "oc-sess-B",
        "cwd": "/tmp/aim-test/proj-6",
        "event": "session-start",
    });
    fire_raw_hook_and_settle(&h.router, body, 6, "oc-sess-B").await;
    let text = read_session_marker(&h.router, "oc-sess-B").await;
    assert!(
        text.contains(number_word(6)),
        "OpenCode top-level sessionID shape did not route to proj-6:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opencode_nested_properties_shape_routes_per_session() {
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;
    // OpenCode `tool.execute.before` and friends wrap the session id
    // (and the directory hint) inside a `properties.info` block. The
    // extractor's nested path list covers this; if the key list ever
    // changes upstream, this test will be the warning shot.
    let body = json!({
        "type": "tool.execute.before",
        "properties": {
            "sessionID": "oc-nested-C",
            "info": {
                "directory": "/tmp/aim-test/proj-2",
                "id": "ignored-tool-id"
            }
        }
    });
    fire_raw_hook_and_settle(&h.router, body, 2, "oc-nested-C").await;
    let text = read_session_marker(&h.router, "oc-nested-C").await;
    assert!(
        text.contains(number_word(2)),
        "OpenCode nested properties shape did not route to proj-2:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_payload_shape_routes_per_session() {
    let h = Harness::build(
        ActiveProjectMode::PerSession,
        TEST_TTL,
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    )
    .await;
    // Codex CLI: camelCase `sessionId`, `cwd` at top level. Differs
    // from Claude (snake_case) and OpenCode (capital ID) by one
    // letter each; the extractor must accept all three case variants
    // because the same engine serves all the agents.
    let body = json!({
        "sessionId": "codex-sess-D",
        "cwd": "/tmp/aim-test/proj-4",
        "model": "gpt-4",
    });
    fire_raw_hook_and_settle(&h.router, body, 4, "codex-sess-D").await;
    let text = read_session_marker(&h.router, "codex-sess-D").await;
    assert!(
        text.contains(number_word(4)),
        "Codex sessionId shape did not route to proj-4:\n{text}"
    );
}
