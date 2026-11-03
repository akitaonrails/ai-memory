//! `[retrieval] observation_stream` (R1), proven end to end through the real
//! `memory_query` tool over JSON-RPC.
//!
//! The use case: a finished session's zero-LLM page keeps only a short
//! excerpt of each turn, so the detail a later question asks about lives only
//! in the session's observations. With the stream on, `memory_query` returns
//! the session page (hits stay pages), explains the vote with an
//! `observation` stream entry, and shows the matching observation excerpt.
//! With it off, the same query finds no page and falls back to `raw_hits`.
//! The stream is on by default.

use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, ObservationKind, PagePath, ProjectId,
    Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::{RetrievalTuning, Store};
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

struct Seeded {
    _tmp: TempDir,
    router: Router,
    session_id: SessionId,
    observation_id: String,
}

async fn seeded(observation_stream: bool) -> Seeded {
    seeded_with(Some(observation_stream), true).await
}

/// `observation_stream: None` keeps the store's default tuning.
async fn seeded_with(observation_stream: Option<bool>, session_page: bool) -> Seeded {
    let tmp = TempDir::new().expect("tempdir");
    let mut store = Store::open(tmp.path()).expect("store");
    // The server reads its tuning once at construction, as `serve` does from
    // `Config::load`.
    if let Some(observation_stream) = observation_stream {
        store.reader.set_retrieval_tuning(RetrievalTuning {
            observation_stream,
            ..RetrievalTuning::default()
        });
    }
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let (session_id, observation_id) = seed_session(&store, ws, proj, session_page).await;
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Seeded {
        _tmp: tmp,
        router: Router::new().nest_service("/mcp", service),
        session_id,
        observation_id,
    }
}

async fn seed_session(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    session_page: bool,
) -> (SessionId, String) {
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some("/repo".into()),
            actor_user: None,
        })
        .await
        .expect("session");
    let observation_id = store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                occurred_at: None,
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::Stop,
                extension: None,
                source_event: None,
                title: "assistant".into(),
                body: "We pinned the flaky integration test to the zircon runner pool \
                       because the default pool lacks the GPU driver."
                    .into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .expect("observation");
    // The session's compiled page, shaped like the zero-LLM synth page: a
    // short excerpt that does not carry the answer. A live session has none.
    if !session_page {
        return (session_id, observation_id.to_string());
    }
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
            title: "Session: CI fixes".into(),
            body: "User asked why CI was red.".into(),
            tier: Tier::Episodic,
            frontmatter_json: json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .expect("session page");
    // A distractor page that shares one query word, so the stream has to
    // rank, not merely be the only source.
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/runners.md").unwrap(),
            title: "Runners".into(),
            body: "The default runner pool is shared.".into(),
            tier: Tier::Semantic,
            frontmatter_json: json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .expect("distractor");
    (session_id, observation_id.to_string())
}

async fn query(router: &Router, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "memory_query", "arguments": arguments },
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
    let v: Value = serde_json::from_slice(&bytes).expect("json-rpc");
    assert!(v.get("error").is_none(), "JSON-RPC error: {v}");
    let text = v
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing tool text: {v}"));
    serde_json::from_str(text).expect("tool payload")
}

fn args() -> Value {
    json!({
        "query": "zircon runner",
        "workspace": WS,
        "project": PROJ,
        "explain": true,
        "limit": 5,
    })
}

#[tokio::test]
async fn memory_query_finds_a_session_page_by_its_observation_evidence() {
    let seeded = seeded(true).await;
    let payload = query(&seeded.router, args()).await;

    let hits = payload["hits"].as_array().expect("hits");
    let session_path = format!("sessions/{}.md", seeded.session_id);
    let hit = hits
        .iter()
        .find(|h| h["path"] == session_path.as_str())
        .unwrap_or_else(|| panic!("session page missing: {payload}"));
    // Hits are still pages; the excerpt shows the evidence that matched.
    assert_eq!(hit["title"], "Session: CI fixes");
    let snippet = hit["snippet"].as_str().unwrap();
    assert!(snippet.contains("<mark>zircon</mark>"), "{snippet}");
    let details = &hit["score_details"];
    assert_eq!(details["observation_rank"], 1, "{details}");
    assert_eq!(
        details["observation_ids"],
        json!([seeded.observation_id]),
        "{details}"
    );
    assert!(details["rrf"]["observation"].as_f64().unwrap() > 0.0);
    let streams = payload["streams_active"].as_array().expect("streams");
    assert!(streams.contains(&json!("observation")), "{streams:?}");
    // A page hit means the raw fallback stays quiet.
    assert!(
        payload
            .get("raw_hits")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty),
        "{payload}"
    );
    // The distractor still competes on its own FTS match and carries no
    // observation vote.
    if let Some(distractor) = hits.iter().find(|h| h["path"] == "notes/runners.md") {
        assert!(
            distractor["score_details"]
                .get("observation_rank")
                .is_none()
        );
    }
}

#[tokio::test]
async fn memory_query_ranks_with_the_observation_stream_by_default() {
    let seeded = seeded_with(None, true).await;
    let payload = query(&seeded.router, args()).await;

    let session_path = format!("sessions/{}.md", seeded.session_id);
    let hit = payload["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|h| h["path"] == session_path.as_str())
        .unwrap_or_else(|| panic!("session page missing: {payload}"));
    assert_eq!(hit["score_details"]["observation_rank"], 1, "{payload}");
    let streams = payload["streams_active"].as_array().expect("streams");
    assert!(streams.contains(&json!("observation")), "{streams:?}");
}

#[tokio::test]
async fn memory_query_without_the_stream_never_reports_an_observation_vote() {
    let seeded = seeded(false).await;
    let payload = query(&seeded.router, args()).await;

    let session_path = format!("sessions/{}.md", seeded.session_id);
    let hits = payload["hits"].as_array().expect("hits");
    assert!(
        hits.iter().all(|h| h["path"] != session_path.as_str()),
        "{payload}"
    );
    assert!(
        hits.iter()
            .all(|h| h["score_details"].get("observation_rank").is_none()),
        "{payload}"
    );
    let streams = payload["streams_active"].as_array().expect("streams");
    assert!(!streams.contains(&json!("observation")), "{streams:?}");
}

#[tokio::test]
async fn a_live_session_without_a_page_still_reaches_the_raw_fallback() {
    let seeded = seeded_with(Some(true), false).await;
    let payload = query(
        &seeded.router,
        json!({
            "query": "zircon",
            "workspace": WS,
            "project": PROJ,
            "explain": true,
        }),
    )
    .await;
    assert_eq!(payload["hits"], json!([]), "{payload}");
    let raw = payload["raw_hits"].as_array().expect("raw_hits");
    assert_eq!(raw.len(), 1, "{payload}");
    assert_eq!(raw[0]["id"], seeded.observation_id.as_str());
}

/// `explain=true` times the observation stream exactly when it runs, and a
/// query without explain carries no timings at all.
#[tokio::test]
async fn explain_timings_follow_the_observation_stream() {
    let on = seeded(true).await;
    let payload = query(&on.router, args()).await;
    assert!(
        payload["timings"]["observation_ms"]
            .as_f64()
            .is_some_and(|ms| ms >= 0.0),
        "{payload}"
    );
    let mut quiet = args();
    quiet["explain"] = json!(false);
    let payload = query(&on.router, quiet).await;
    assert!(payload.get("timings").is_none(), "{payload}");

    let off = seeded(false).await;
    let payload = query(&off.router, args()).await;
    let timings = payload["timings"].as_object().expect("timings");
    assert!(timings.contains_key("fts_ms"), "{payload}");
    assert!(!timings.contains_key("observation_ms"), "{payload}");
}
