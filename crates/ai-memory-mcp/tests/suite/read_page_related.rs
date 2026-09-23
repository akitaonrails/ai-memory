//! Opt-in related-pages graph walk on `memory_read_page`. By default the tool
//! returns one page and nothing else; with `include_related: true` it also
//! returns the pages reachable from that page through the link graph, out to
//! `related_depth` hops (default 1, hard-capped at 3). Each related entry
//! carries its identity plus how far and in which direction it was reached.
//!
//! Default-off must be byte-identical to the previous single-page response.

use ai_memory_core::{NewPage, PagePath, Tier, WorkspaceId};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const PROJ: &str = "scratch";
const SIBLING: &str = "lib";

struct Harness {
    router: Router,
    store: Store,
    ws: WorkspaceId,
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

/// Seed the graph through the real write path so links form from `[[wiki-links]]`
/// in the bodies:
///
/// ```text
///   notes/d.md ──▶ notes/a.md ──▶ notes/b.md ──▶ notes/c.md
///                                     └────────▶ lib:notes/x.md  (cross-project)
/// ```
async fn seed(h: &Harness) {
    // Cross-project sibling target, created directly so the [[lib:...]] link
    // resolves. This exercises the cross-project awareness of the walk.
    let lib = h
        .store
        .writer
        .get_or_create_project(h.ws, SIBLING, None)
        .await
        .expect("sibling proj");
    h.store
        .writer
        .upsert_page(NewPage {
            workspace_id: h.ws,
            project_id: lib,
            path: PagePath::new("notes/x.md").unwrap(),
            title: "Sibling X".into(),
            body: "cross-project leaf".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .expect("sibling page");

    write_page(&h.router, "notes/c.md", "# C\n\nleaf page").await;
    write_page(
        &h.router,
        "notes/b.md",
        "# B\n\nsee [[notes/c.md]] and [[lib:notes/x.md]]",
    )
    .await;
    write_page(&h.router, "notes/a.md", "# A\n\nsee [[notes/b.md]]").await;
    write_page(&h.router, "notes/d.md", "# D\n\nsee [[notes/a.md]]").await;
}

fn related(resp: &Value) -> &Vec<Value> {
    resp.get("related")
        .and_then(|r| r.as_array())
        .unwrap_or_else(|| panic!("no related array: {resp}"))
}

fn paths(nodes: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = nodes
        .iter()
        .map(|n| n.get("path").and_then(|p| p.as_str()).unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn read_args(extra: Value) -> Value {
    let mut base = json!({ "workspace": WS, "project": PROJ, "path": "notes/a.md" });
    let obj = base.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    base
}

#[tokio::test]
async fn default_off_is_byte_identical_and_has_no_related_block() {
    let h = harness().await;
    seed(&h).await;

    let plain = call(&h.router, "memory_read_page", read_args(json!({}))).await;
    assert!(
        plain.get("related").is_none(),
        "a plain read must not carry a related block: {plain}"
    );

    // include_related:false is exactly the same response as omitting it.
    let explicit_off = call(
        &h.router,
        "memory_read_page",
        read_args(json!({ "include_related": false })),
    )
    .await;
    assert_eq!(
        plain, explicit_off,
        "include_related:false must be byte-identical to the default read"
    );
}

#[tokio::test]
async fn include_related_default_depth_returns_direct_neighbours() {
    let h = harness().await;
    seed(&h).await;

    let resp = call(
        &h.router,
        "memory_read_page",
        read_args(json!({ "include_related": true })),
    )
    .await;

    // The single page is still returned unchanged.
    assert_eq!(
        resp.get("path").and_then(|p| p.as_str()),
        Some("notes/a.md")
    );

    let nodes = related(&resp);
    assert_eq!(
        paths(nodes),
        vec!["notes/b.md".to_string(), "notes/d.md".to_string()],
        "default depth 1 returns only direct neighbours (outgoing b, incoming d): {resp}"
    );

    let b = nodes
        .iter()
        .find(|n| n.get("path").and_then(|p| p.as_str()) == Some("notes/b.md"))
        .unwrap();
    assert_eq!(b.get("depth").and_then(Value::as_u64), Some(1));
    assert_eq!(b.get("direction").and_then(|d| d.as_str()), Some("link"));
    assert!(
        b.get("title").and_then(|t| t.as_str()).is_some(),
        "carries a title"
    );
    assert!(
        b.get("kind").and_then(|k| k.as_str()).is_some(),
        "carries a kind"
    );

    let d = nodes
        .iter()
        .find(|n| n.get("path").and_then(|p| p.as_str()) == Some("notes/d.md"))
        .unwrap();
    assert_eq!(
        d.get("direction").and_then(|x| x.as_str()),
        Some("backlink")
    );
}

#[tokio::test]
async fn related_depth_controls_how_far_the_walk_reaches() {
    let h = harness().await;
    seed(&h).await;

    let depth2 = call(
        &h.router,
        "memory_read_page",
        read_args(json!({ "include_related": true, "related_depth": 2 })),
    )
    .await;

    let nodes = related(&depth2);
    assert_eq!(
        paths(nodes),
        vec![
            "notes/b.md".to_string(),
            "notes/c.md".to_string(),
            "notes/d.md".to_string(),
            "notes/x.md".to_string(),
        ],
        "depth 2 adds c (via b) and the cross-project lib:x (via b): {depth2}"
    );

    let c = nodes
        .iter()
        .find(|n| n.get("path").and_then(|p| p.as_str()) == Some("notes/c.md"))
        .unwrap();
    assert_eq!(
        c.get("depth").and_then(Value::as_u64),
        Some(2),
        "c is two hops out"
    );

    let x = nodes
        .iter()
        .find(|n| n.get("path").and_then(|p| p.as_str()) == Some("notes/x.md"))
        .unwrap();
    assert_eq!(
        x.get("project").and_then(|p| p.as_str()),
        Some(SIBLING),
        "the cross-project neighbour carries its real project"
    );
}

#[tokio::test]
async fn related_depth_is_clamped_to_the_hard_cap() {
    let h = harness().await;
    seed(&h).await;

    let at_cap = call(
        &h.router,
        "memory_read_page",
        read_args(json!({ "include_related": true, "related_depth": 3 })),
    )
    .await;
    let over_cap = call(
        &h.router,
        "memory_read_page",
        read_args(json!({ "include_related": true, "related_depth": 200 })),
    )
    .await;

    assert_eq!(
        paths(related(&over_cap)),
        paths(related(&at_cap)),
        "a related_depth beyond the hard cap is clamped, not honoured"
    );
}
