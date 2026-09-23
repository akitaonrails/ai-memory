//! 2.4 opt-in `reasoning` tier on the `memory_query(answer=true)` synthesis
//! path (borrowed from Honcho's reasoning-effort ladder). The knob is a serde
//! enum `{minimal, low, medium, high, max}` (default `minimal`).
//!
//! `ChatRequest` carries no per-request reasoning/effort field today (the
//! provider-level `reasoning_effort` is fixed at construction from config), so
//! the honest per-request mapping is a per-tier max-token budget. These tests
//! capture the outgoing `ChatRequest.max_tokens` and prove:
//!
//! 1. `reasoning:high` forwards a strictly larger token budget than
//!    `reasoning:minimal`.
//! 2. Omitting `reasoning` is byte-identical to `reasoning:minimal`, and both
//!    equal the pre-B5 default budget (2000) — the default path is unchanged.
//! 3. An invalid `reasoning` value is rejected by the schema enum (the tool
//!    call errors cleanly rather than silently degrading).

use std::sync::Arc;
use std::sync::Mutex;

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider, LlmResult};
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

/// The pre-B5 answer-synthesis token budget. `reasoning:minimal` (and omitting
/// `reasoning`) must forward exactly this so the default path is unchanged.
const MINIMAL_ANSWER_BUDGET: u32 = 2_000;

/// A fake `LlmProvider` that records the `max_tokens` of every structured
/// request it receives, so a test can assert how the tier scaled the budget.
struct CapturingLlm {
    seen_max_tokens: Arc<Mutex<Vec<u32>>>,
}

#[async_trait::async_trait]
impl LlmProvider for CapturingLlm {
    fn name(&self) -> &'static str {
        "capture"
    }

    fn model(&self) -> &str {
        "capture-1"
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        self.seen_max_tokens
            .lock()
            .unwrap()
            .push(request.max_tokens);
        Ok(ChatResponse {
            text: "unused".into(),
            usage: None,
            model: "capture-1".into(),
        })
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        _schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.seen_max_tokens
            .lock()
            .unwrap()
            .push(request.max_tokens);
        Ok(json!({
            "answer": "The widget subsystem stores its needle in notes/topic.md.",
            "citations": ["notes/topic.md"],
        }))
    }
}

struct Harness {
    router: Router,
    store: Store,
    ws: WorkspaceId,
    proj: ProjectId,
    seen_max_tokens: Arc<Mutex<Vec<u32>>>,
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
    let seen_max_tokens = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        seen_max_tokens: seen_max_tokens.clone(),
    });
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_llm(provider);

    Harness {
        router: mount(server),
        store,
        ws,
        proj,
        seen_max_tokens,
        _tmp: tmp,
    }
}

/// Full JSON-RPC envelope (so a test can inspect `error` on a rejected call).
async fn call_raw(router: &Router, name: &str, arguments: Value) -> Value {
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
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"))
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

async fn seed_needle(h: &Harness) {
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "notes/topic.md",
            "the widget subsystem needle lives here",
        ))
        .await
        .unwrap();
}

async fn query(h: &Harness, extra: Value) {
    let resp = call_raw(&h.router, "memory_query", query_args("needle", extra)).await;
    assert!(
        resp.get("error").is_none(),
        "unexpected JSON-RPC error: {resp}"
    );
}

/// 1. A higher tier forwards a strictly larger token budget than minimal.
#[tokio::test]
async fn higher_reasoning_tier_widens_token_budget() {
    let h = harness().await;
    seed_needle(&h).await;

    query(&h, json!({ "answer": true, "reasoning": "minimal" })).await;
    query(&h, json!({ "answer": true, "reasoning": "high" })).await;

    let seen = h.seen_max_tokens.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        2,
        "each answer=true call must reach the provider once: {seen:?}"
    );
    let (minimal, high) = (seen[0], seen[1]);
    assert!(
        high > minimal,
        "reasoning:high must forward a larger token budget than minimal \
         (high={high}, minimal={minimal})"
    );
}

/// 2. Omitting `reasoning` == `reasoning:minimal`, and both equal the pre-B5
///    default budget. The default path stays byte-identical.
#[tokio::test]
async fn default_reasoning_equals_minimal_and_is_unchanged() {
    let h = harness().await;
    seed_needle(&h).await;

    query(&h, json!({ "answer": true })).await;
    query(&h, json!({ "answer": true, "reasoning": "minimal" })).await;

    let seen = h.seen_max_tokens.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "two answer=true calls expected: {seen:?}");
    assert_eq!(
        seen[0], seen[1],
        "omitting reasoning must equal reasoning:minimal: {seen:?}"
    );
    assert_eq!(
        seen[0], MINIMAL_ANSWER_BUDGET,
        "the default/minimal budget must match the pre-B5 value ({MINIMAL_ANSWER_BUDGET})"
    );
}

/// 3. An invalid `reasoning` value is rejected by the schema enum — the call
///    errors cleanly and never reaches the provider.
#[tokio::test]
async fn invalid_reasoning_value_is_rejected() {
    let h = harness().await;
    seed_needle(&h).await;

    let resp = call_raw(
        &h.router,
        "memory_query",
        query_args("needle", json!({ "answer": true, "reasoning": "supreme" })),
    )
    .await;

    assert!(
        resp.get("error").is_some() || resp.pointer("/result/isError") == Some(&json!(true)),
        "an invalid reasoning value must be rejected: {resp}"
    );
    assert!(
        h.seen_max_tokens.lock().unwrap().is_empty(),
        "a rejected call must never reach the provider"
    );
}
