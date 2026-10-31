//! `POST /admin/llm-test` (#1639).
//!
//! The health report's retry hint used to name a client-side `llm-test`, which
//! builds its provider from the caller's environment: with the server in a
//! container or on another host, the keys exist only there, so the advice
//! failed while the server was healthy. The probe now runs on the server, and
//! these pin what it reports — including that a failure never echoes the
//! provider's body or the reply.

use std::sync::Arc;

use ai_memory_llm::{ChatRequest, ChatResponse, LlmError, LlmProvider, LlmResult};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

/// A provider that answers a fixed reply, or fails the way a missing key does.
struct ProbeProvider {
    reply: Result<&'static str, ()>,
}

#[async_trait::async_trait]
impl LlmProvider for ProbeProvider {
    fn name(&self) -> &'static str {
        "openai-compat"
    }

    fn model(&self) -> &str {
        "gpt-test"
    }

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        match self.reply {
            Ok(text) => Ok(ChatResponse {
                text: text.to_owned(),
                usage: None,
                model: "gpt-test".to_owned(),
            }),
            Err(()) => Err(LlmError::Provider {
                status: 401,
                // Composed at runtime: the shape has to look like a key for the
                // scrub to catch it, and a key-shaped literal in the source is
                // a gitleaks finding even when it is fake.
                body: format!("Incorrect API key provided: {}", fake_key()),
                retry_after: None,
            }),
        }
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.complete(ChatRequest::user_prompt(String::new()))
            .await
            .map(|response| json!({ "text": response.text }))
    }
}

/// A key-shaped value assembled from parts, so no scanner reads it as one.
fn fake_key() -> String {
    format!("sk-{}-{}", "live", "0123456789abcdef")
}

async fn make_admin_state(tmp: &TempDir, llm: Option<Arc<dyn LlmProvider>>) -> AdminState {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    AdminState {
        ingest_metrics: Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    }
}

async fn probe(state: AdminState) -> (StatusCode, Value) {
    let response = admin_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/llm-test")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn a_healthy_server_reports_its_configured_provider() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp, Some(Arc::new(ProbeProvider { reply: Ok("pong") }))).await;

    let (status, body) = probe(state).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["provider"], json!("openai-compat"));
    assert_eq!(body["model"], json!("gpt-test"));
    assert_eq!(
        body["reply_chars"],
        json!(4),
        "the reply is measured, never echoed"
    );
    assert!(body.get("reply").is_none(), "{body}");
    assert!(body["latency_ms"].is_u64(), "{body}");
}

/// The failure class and a bounded message, like the health report: an
/// operator probing from another host must not have the provider's body (which
/// can quote the key) printed into their terminal or their logs.
#[tokio::test]
async fn a_failing_probe_reports_the_class_without_the_provider_body() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp, Some(Arc::new(ProbeProvider { reply: Err(()) }))).await;

    let (status, body) = probe(state).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["status"], json!(401));
    assert_eq!(body["provider"], json!("openai-compat"));
    let message = body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("Incorrect API key provided"),
        "the class is useful: {message}"
    );
    assert!(
        !message.contains(&fake_key()),
        "the key must not reach the operator: {message}"
    );
}

/// A server with no provider configured says so, instead of reporting a probe
/// failure the operator would read as a broken provider.
#[tokio::test]
async fn a_server_without_a_provider_answers_service_unavailable() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp, None).await;

    let (status, body) = probe(state).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no LLM provider configured"),
        "{body}"
    );
}
