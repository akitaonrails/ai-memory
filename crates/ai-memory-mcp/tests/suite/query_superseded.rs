//! `memory_query(include_superseded=true)` retrieval, proven end to end
//! through the real MCP tool. Superseded page versions are hidden by default;
//! the opt-in flag returns them too, each labelled `superseded: true` in the
//! user-visible JSON. Default-off behaviour must be unchanged.
//!
//! This mirrors the superseded-version setup in `retrieval_via_tools.rs`
//! (`upsert_page` twice on one path), but exercises the plain-query
//! `include_superseded` path rather than the `as_of` time-travel path.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
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

struct Harness {
    router: Router,
    store: Store,
    ws: WorkspaceId,
    proj: ProjectId,
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

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, title: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

fn hits(resp: &Value) -> &Vec<Value> {
    resp.get("hits")
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("no hits array: {resp}"))
}

fn hit_for<'a>(resp: &'a Value, path: &str) -> Option<&'a Value> {
    hits(resp)
        .iter()
        .find(|h| h.get("path").and_then(|p| p.as_str()) == Some(path))
}

fn query_args(query: &str, extra: Value) -> Value {
    let mut base = json!({
        "query": query,
        "workspace": WS,
        "project": PROJ,
        "limit": 10,
    });
    let obj = base.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    base
}

/// The superseded version carries a term the latest version dropped. A plain
/// query never returns it; `include_superseded=true` does, marked.
#[tokio::test]
async fn memory_query_include_superseded_returns_marked_historical_version() {
    let h = harness().await;

    // v1 mentions `postgres`; v2 supersedes it and drops the term.
    let mut p = page(h.ws, h.proj, "notes/db.md", "DB", "we use postgres");
    h.store.writer.upsert_page(p.clone()).await.unwrap();
    p.body = "we migrated to sqlite".into();
    h.store.writer.upsert_page(p).await.unwrap();

    // Default: the superseded-only term does not surface.
    let now = call(&h.router, "memory_query", query_args("postgres", json!({}))).await;
    assert!(
        hit_for(&now, "notes/db.md").is_none(),
        "a plain query must not return the superseded version: {now}"
    );

    // include_superseded=true: the historical version answers, labelled.
    let historical = call(
        &h.router,
        "memory_query",
        query_args("postgres", json!({ "include_superseded": true })),
    )
    .await;
    let hit = hit_for(&historical, "notes/db.md")
        .expect("the superseded version must answer when include_superseded=true");
    assert_eq!(
        hit.get("superseded").and_then(|s| s.as_bool()),
        Some(true),
        "the historical hit must be labelled superseded: {hit}"
    );
}

/// The latest version is unchanged and unmarked whether or not the flag is set;
/// the flag only adds the historical version alongside it.
#[tokio::test]
async fn memory_query_latest_version_is_never_marked_superseded() {
    let h = harness().await;

    // A shared token matches both versions.
    let mut p = page(h.ws, h.proj, "notes/db.md", "DB", "zebraquux postgres");
    h.store.writer.upsert_page(p.clone()).await.unwrap();
    p.body = "zebraquux sqlite".into();
    h.store.writer.upsert_page(p).await.unwrap();

    // Default: exactly one hit, the latest, not marked.
    let now = call(
        &h.router,
        "memory_query",
        query_args("zebraquux", json!({})),
    )
    .await;
    assert_eq!(
        hits(&now).len(),
        1,
        "default returns only the latest: {now}"
    );
    let latest = hit_for(&now, "notes/db.md").expect("latest must be present");
    // Absent (skipped) or explicitly false both mean "not superseded".
    assert_ne!(
        latest.get("superseded").and_then(|s| s.as_bool()),
        Some(true),
        "the latest version must not be marked superseded: {latest}"
    );

    // Opt-in: both versions present; the latest still unmarked, one marked.
    let both = call(
        &h.router,
        "memory_query",
        query_args("zebraquux", json!({ "include_superseded": true })),
    )
    .await;
    assert_eq!(
        hits(&both).len(),
        2,
        "include_superseded returns both versions: {both}"
    );
    let marked = hits(&both)
        .iter()
        .filter(|h| h.get("superseded").and_then(|s| s.as_bool()) == Some(true))
        .count();
    assert_eq!(
        marked, 1,
        "exactly one version is marked superseded: {both}"
    );
}
