//! Access reinforcement on the read paths that previously reinforced nothing
//! (design-memory-aging.md, bucket C1). A page a client opens by
//! `memory_read_page`, a page the link graph surfaces through the
//! `include_related` walk, and the pages `memory_explore` surfaces should each
//! bump `access_count` + `last_accessed_at` exactly as a `memory_query` /
//! `memory_recent` hit already does — the M8 reinforcement term that feeds the
//! decay formula's `access_term`.
//!
//! The bump reuses the sanctioned `spawn_access_bump` path: fire-and-forget on
//! the single-writer actor, throttled to ≤1 per (page, operator) per
//! `ACCESS_BUMP_COOLDOWN`, FTS-exempt. It is strictly additive — the response
//! payloads must stay byte-identical.

use ai_memory_core::WorkspaceId;
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const PROJ: &str = "scratch";

struct Harness {
    router: Router,
    store: Store,
    ws: WorkspaceId,
    proj: ai_memory_core::ProjectId,
    _tmp: TempDir,
}

fn mount(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

async fn harness() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj).with_wiki(wiki);

    Harness {
        router: mount(server),
        store,
        ws,
        proj,
        _tmp: tmp,
    }
}

async fn call(router: &Router, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .expect("mcp req");
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"));
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}\nfull: {text}");
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {text}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&joined).unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}"))
}

async fn write_page(router: &Router, path: &str, body: &str) {
    let resp = call(
        router,
        "memory_write_page",
        json!({ "workspace": WS, "project": PROJ, "path": path, "body": body }),
    )
    .await;
    assert!(resp.get("page_id").is_some(), "page not written: {resp}");
}

/// Read the current `access_count` for one page via the same read path the
/// forget sweep uses, so the assertion sees the committed single-writer state.
async fn access_count(h: &Harness, path: &str) -> u32 {
    let cands = h
        .store
        .reader
        .decay_candidates(h.ws, h.proj)
        .await
        .expect("decay_candidates");
    cands
        .into_iter()
        .find(|c| c.path.as_str() == path)
        .unwrap_or_else(|| panic!("page {path} not among decay candidates"))
        .access_count
}

/// Poll until `access_count` for `path` reaches `want`, or give up. The bump is
/// spawned fire-and-forget onto the writer actor, so a read is not synchronous —
/// mirror how the writer lands asynchronously by polling the committed value.
async fn wait_for_access(h: &Harness, path: &str, want: u32) -> u32 {
    for _ in 0..200 {
        let got = access_count(h, path).await;
        if got >= want {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    access_count(h, path).await
}

/// A direct by-path `memory_read_page` reinforces the page it returns.
#[tokio::test]
async fn read_page_bumps_access_count() {
    let h = harness().await;
    write_page(&h.router, "notes/a.md", "# A\n\nbody").await;
    assert_eq!(
        access_count(&h, "notes/a.md").await,
        0,
        "seeded page starts at 0"
    );

    let resp = call(
        &h.router,
        "memory_read_page",
        json!({ "workspace": WS, "project": PROJ, "path": "notes/a.md" }),
    )
    .await;
    assert_eq!(
        resp.get("path").and_then(|p| p.as_str()),
        Some("notes/a.md")
    );

    let got = wait_for_access(&h, "notes/a.md", 1).await;
    assert_eq!(got, 1, "a by-path read must reinforce the page it returns");
}

/// The `include_related` walk reinforces the seed AND the walked neighbours —
/// a graph-surfaced page was used and should resist decay too.
#[tokio::test]
async fn read_page_include_related_bumps_related() {
    let h = harness().await;
    write_page(&h.router, "notes/b.md", "# B\n\nleaf").await;
    write_page(&h.router, "notes/a.md", "# A\n\nsee [[notes/b.md]]").await;

    let resp = call(
        &h.router,
        "memory_read_page",
        json!({
            "workspace": WS, "project": PROJ, "path": "notes/a.md",
            "include_related": true,
        }),
    )
    .await;
    let related = resp
        .get("related")
        .and_then(|r| r.as_array())
        .expect("related array present");
    assert!(
        related
            .iter()
            .any(|n| n.get("path").and_then(|p| p.as_str()) == Some("notes/b.md")),
        "walk should reach notes/b.md: {resp}"
    );

    assert_eq!(
        wait_for_access(&h, "notes/a.md", 1).await,
        1,
        "seed reinforced"
    );
    assert_eq!(
        wait_for_access(&h, "notes/b.md", 1).await,
        1,
        "the walked neighbour must be reinforced, not just the seed"
    );
}

/// `memory_explore` surfaces pages (via the briefing snapshot); those pages are
/// reinforced. With no LLM configured the tool returns the structured briefing,
/// which is enough to exercise the reinforcement.
#[tokio::test]
async fn explore_bumps_surfaced_pages() {
    let h = harness().await;
    write_page(&h.router, "notes/topic.md", "# Topic\n\nrecent work").await;
    assert_eq!(access_count(&h, "notes/topic.md").await, 0);

    let resp = call(
        &h.router,
        "memory_explore",
        json!({ "workspace": WS, "project": PROJ }),
    )
    .await;
    // No LLM in tests → structured briefing is returned unchanged.
    assert!(
        resp.get("briefing").is_some(),
        "explore returns briefing: {resp}"
    );

    let got = wait_for_access(&h, "notes/topic.md", 1).await;
    assert_eq!(got, 1, "explore must reinforce the pages it surfaced");
}

/// Two reads inside the cooldown window bump only once — the per-(page,operator)
/// throttle inside `spawn_access_bump` bounds read-amplification.
#[tokio::test]
async fn read_page_throttle_no_double_bump() {
    let h = harness().await;
    write_page(&h.router, "notes/a.md", "# A\n\nbody").await;

    call(
        &h.router,
        "memory_read_page",
        json!({ "workspace": WS, "project": PROJ, "path": "notes/a.md" }),
    )
    .await;
    assert_eq!(wait_for_access(&h, "notes/a.md", 1).await, 1);

    // A second read within the 60s cooldown must not bump again.
    call(
        &h.router,
        "memory_read_page",
        json!({ "workspace": WS, "project": PROJ, "path": "notes/a.md" }),
    )
    .await;
    // Give any (erroneously) spawned second bump time to land, then assert it
    // did not.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        access_count(&h, "notes/a.md").await,
        1,
        "the cooldown must suppress a double bump within the window"
    );
}

/// The bump is additive: a plain read's response payload is exactly the four
/// documented fields, unchanged by reinforcement.
#[tokio::test]
async fn read_page_response_payload_unchanged() {
    let h = harness().await;
    write_page(&h.router, "notes/a.md", "# A\n\nbody text").await;

    let resp = call(
        &h.router,
        "memory_read_page",
        json!({ "workspace": WS, "project": PROJ, "path": "notes/a.md" }),
    )
    .await;
    let obj = resp.as_object().expect("object response");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["body", "frontmatter", "path", "title"],
        "reinforcement must not add fields to the read_page payload: {resp}"
    );
}
