//! #2038 default result-count precedence, proven end to end through the real
//! `memory_query` tool: an explicit per-call `limit` wins, then the queried
//! project's stored `default_result_limit`, then the server-wide global
//! default, then the built-in 10. Each layer is set the way production sets it
//! — the project column through the store write path, the global default
//! through the server builder — and the assertion is the decoded hit count.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::{RetrievalCriteria, Store};
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
/// More candidates than any limit under test, so the hit count is always the
/// limit, never the supply of matching pages.
const SEEDED_PAGES: usize = 24;

struct Harness {
    router: Router,
    store: Store,
    proj: ProjectId,
    _tmp: TempDir,
}

fn mount(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

/// Build a harness whose server carries `global_default` as its server-wide
/// `[retrieval] default_result_limit`, then seed `SEEDED_PAGES` pages that all
/// match the query term.
async fn harness(global_default: Option<usize>) -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    for i in 0..SEEDED_PAGES {
        store
            .writer
            .upsert_page(page(
                ws,
                proj,
                &format!("notes/alpha-{i:02}.md"),
                "alpha shared retrieval term for the default limit test",
            ))
            .await
            .expect("seed page");
    }
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_default_limit(global_default);
    Harness {
        router: mount(server),
        store,
        proj,
        _tmp: tmp,
    }
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
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

/// The number of hits `memory_query("alpha")` returns for this project, with an
/// optional explicit `limit`.
async fn hit_count(h: &Harness, limit: Option<usize>) -> usize {
    let mut args = json!({ "query": "alpha", "workspace": WS, "project": PROJ });
    if let Some(limit) = limit {
        args.as_object_mut()
            .unwrap()
            .insert("limit".into(), json!(limit));
    }
    let resp = call(&h.router, "memory_query", args).await;
    resp.get("hits")
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("no hits array: {resp}"))
        .len()
}

async fn set_project_limit(h: &Harness, limit: Option<u32>) {
    h.store
        .writer
        .set_project_retrieval_criteria(
            h.proj,
            RetrievalCriteria {
                default_result_limit: limit,
            },
        )
        .await
        .unwrap();
}

/// No global default, no project default, no call `limit`: the built-in 10.
#[tokio::test]
async fn falls_back_to_the_builtin_default() {
    let h = harness(None).await;
    assert_eq!(hit_count(&h, None).await, 10);
}

/// The global `[retrieval] default_result_limit` replaces the built-in when a
/// call gives no `limit` and the project has none.
#[tokio::test]
async fn global_default_applies_when_project_and_call_are_silent() {
    let h = harness(Some(5)).await;
    assert_eq!(hit_count(&h, None).await, 5);
}

/// The project's own stored default overrides the global one.
#[tokio::test]
async fn project_default_overrides_global() {
    let h = harness(Some(5)).await;
    set_project_limit(&h, Some(3)).await;
    assert_eq!(
        hit_count(&h, None).await,
        3,
        "the project's stored default must win over the global default"
    );
    // Clearing the project default falls back to the global one again.
    set_project_limit(&h, None).await;
    assert_eq!(hit_count(&h, None).await, 5);
}

/// An explicit per-call `limit` wins over both the project and global defaults.
#[tokio::test]
async fn explicit_call_limit_wins_over_every_default() {
    let h = harness(Some(5)).await;
    set_project_limit(&h, Some(3)).await;
    assert_eq!(
        hit_count(&h, Some(7)).await,
        7,
        "an explicit call limit must override the project and global defaults"
    );
}
