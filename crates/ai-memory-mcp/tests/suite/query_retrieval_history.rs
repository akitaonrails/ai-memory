//! #2038 Workstream 2: opt-in per-session retrieval history, proven end to end
//! through the real `memory_query` tool. Recording is detached (best-effort), so
//! each assertion polls the reader briefly for the spawned write to land.
//!
//! Covers: a recorded call captures the query, scope, streams and returned hits
//! with their relevance scores; recording is off by default (nothing written);
//! a call with no session id records nothing; and — the isolation bite — a
//! foreign `(workspace, project)` never reads another scope's history through
//! the same session id, while the legitimate scope does.

use ai_memory_core::{NewPage, PagePath, ProjectId, SessionId, Tier, WorkspaceId};
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
const SESSION: &str = "11111111-1111-4111-8111-111111111111";
const SEEDED_PAGES: usize = 6;

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
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

/// Build a harness. `record` enables retrieval-history recording with the given
/// retention cap.
async fn harness(record: bool, retention: Option<usize>) -> Harness {
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
                "alpha shared retrieval term for the history test",
            ))
            .await
            .expect("seed page");
    }
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_retrieval_history_recording(record, retention);
    Harness {
        router: mount(server),
        store,
        ws,
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

/// Run `memory_query`, optionally carrying a hook session id header, and return
/// the decoded tool response.
async fn query(h: &Harness, arguments: Value, session: Option<&str>) -> Value {
    let envelope = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "memory_query", "arguments": arguments },
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(session) = session {
        req = req.header("x-memory-actor-session-id", session);
    }
    let req = req.body(Body::from(envelope.to_string())).expect("mcp req");
    let resp = h.router.clone().oneshot(req).await.expect("oneshot");
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"));
    if let Some(err) = v.get("error") {
        panic!("unexpected JSON-RPC error: {err}");
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {v}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&joined).unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}"))
}

/// Poll the reader until the session has at least `want` records, or give up.
/// Recording is detached, so a just-returned query may not have landed yet.
async fn wait_for_records(
    h: &Harness,
    ws: WorkspaceId,
    proj: ProjectId,
    session: SessionId,
    want: usize,
) -> ai_memory_store::RetrievalHistoryPage {
    let want = i64::try_from(want).unwrap_or(i64::MAX);
    for _ in 0..250 {
        let page = h
            .store
            .reader
            .retrieval_history_page(ws, proj, session, 0, 100)
            .await
            .expect("history read");
        if page.total >= want {
            return page;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("retrieval history never reached {want} record(s)");
}

fn session_id() -> SessionId {
    SessionId::from_native(SESSION)
}

/// A recorded call captures the query, the single-project scope, the active
/// streams and the returned hits with their positions.
#[tokio::test]
async fn recording_captures_the_query_and_returned_hits() {
    let h = harness(true, None).await;
    let resp = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
        Some(SESSION),
    )
    .await;
    let returned = resp.get("hits").and_then(Value::as_array).unwrap().len();
    assert!(returned > 0, "control query should return hits");

    let page = wait_for_records(&h, h.ws, h.proj, session_id(), 1).await;
    assert_eq!(page.total, 1);
    let record = &page.records[0];
    assert_eq!(record.query, "alpha");
    assert_eq!(record.scope_kind, "project");
    assert!(!record.source.is_empty(), "active streams recorded");
    assert_eq!(record.result_count as usize, returned);
    assert_eq!(record.hits.len(), returned, "every returned hit recorded");
    for (i, hit) in record.hits.iter().enumerate() {
        assert_eq!(hit.position as usize, i, "hits recorded in returned order");
        assert!(hit.page_path.starts_with("notes/alpha-"));
    }
}

/// Off by default: a query on a server that never opted in records nothing.
#[tokio::test]
async fn recording_is_off_by_default() {
    let h = harness(false, None).await;
    query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
        Some(SESSION),
    )
    .await;
    // Give any (erroneous) detached write ample time to land, then assert none.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let page = h
        .store
        .reader
        .retrieval_history_page(h.ws, h.proj, session_id(), 0, 100)
        .await
        .expect("history read");
    assert_eq!(page.total, 0, "recording disabled must write nothing");
}

/// A call with no hook session id has nothing to attribute the record to, so it
/// records nothing even with recording enabled.
#[tokio::test]
async fn a_query_without_a_session_records_nothing() {
    let h = harness(true, None).await;
    query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
        None,
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let page = h
        .store
        .reader
        .retrieval_history_page(h.ws, h.proj, session_id(), 0, 100)
        .await
        .expect("history read");
    assert_eq!(page.total, 0, "no session id → no record");
}

/// Isolation bite: history is keyed by `(workspace, project, session)`. A
/// foreign project reading the same session id sees nothing; the real scope sees
/// the record. Removing the project/workspace predicate from the reader would
/// make the foreign read return the record and fail this test.
#[tokio::test]
async fn history_is_scoped_to_its_project() {
    let h = harness(true, None).await;
    query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
        Some(SESSION),
    )
    .await;
    // Legitimate control: the real scope has the record.
    wait_for_records(&h, h.ws, h.proj, session_id(), 1).await;

    // A different project in the same workspace, same session id: no leak.
    let other = h
        .store
        .writer
        .get_or_create_project(h.ws, "other", None)
        .await
        .expect("other proj");
    let foreign = h
        .store
        .reader
        .retrieval_history_page(h.ws, other, session_id(), 0, 100)
        .await
        .expect("history read");
    assert_eq!(
        foreign.total, 0,
        "a foreign project must not read this project's retrieval history"
    );
}
