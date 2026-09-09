//! #692: a 5xx built from an upstream provider failure must also reach the
//! server's own log.
//!
//! The failure that motivated this reached the operator as
//! `502 Bad Gateway: {"error":"provider error 404: 404 page not found"}` and
//! nowhere else: the server logged the bootstrap starting, then nothing. The
//! response body is the client's copy of the diagnosis; the log is the
//! operator's, and only the log survives the CLI process exiting.

use std::io;
use std::sync::{Arc, Mutex};

use ai_memory_consolidate::{BootstrapSource, SourceKind};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmError, LlmProvider, LlmResult};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

/// A provider that fails the way a misdirected base URL does: a real HTTP
/// 404 whose body is Ollama's plain-text `404 page not found` rather than the
/// vendor's structured error.
struct FourOhFourProvider;

#[async_trait::async_trait]
impl LlmProvider for FourOhFourProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn model(&self) -> &str {
        "gemini-3.5-flash-lite"
    }

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Err(provider_404())
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        Err(provider_404())
    }
}

fn provider_404() -> LlmError {
    LlmError::Provider {
        status: 404,
        body: "404 page not found".into(),
    }
}

/// Collects everything a subscriber writes so a test can assert on it.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn make_admin_state(tmp: &TempDir, llm: Option<Arc<dyn LlmProvider>>) -> AdminState {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    AdminState {
        ingest_metrics: Arc::new(ai_memory_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    }
}

fn synthetic_sources() -> Vec<BootstrapSource> {
    vec![BootstrapSource {
        kind: SourceKind::Readme,
        label: "README (README.md)".into(),
        text: "# my-project\n\nA toy project used for testing bootstrap ingestion.".into(),
    }]
}

/// The operator's only durable record of *why* bootstrap returned 502 is the
/// server log, so the provider status and body must appear there — not just in
/// the response the CLI prints once and forgets.
#[tokio::test]
async fn a_bootstrap_provider_failure_is_logged_and_not_only_returned() {
    let captured = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(Level::WARN)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp, Some(Arc::new(FourOhFourProvider))).await;

    let body = json!({
        "workspace": "test-ws",
        "project": "test-proj",
        "sources": synthetic_sources(),
        "max_input_tokens": 50_000,
        "dry_run": false,
        "force": false,
    });

    let resp = admin_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/bootstrap")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let logged = captured.text();
    assert!(
        logged.contains("404 page not found"),
        "the provider error must reach the server log, not only the response body; \
         captured log was: {logged:?}"
    );
}

/// The converse, and the reason the rule is "server error" rather than "any
/// error": a rejected request is the caller's problem and was answered in the
/// response. Logging those too would bury the 5xx this file exists to surface
/// under noise any client can generate at will.
#[tokio::test]
async fn a_rejected_request_is_answered_without_a_server_log_line() {
    let captured = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(Level::WARN)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp, Some(Arc::new(FourOhFourProvider))).await;

    let body = json!({
        "workspace": "test-ws",
        "project": "test-proj",
        "sources": Vec::<BootstrapSource>::new(),
        "max_input_tokens": 50_000,
        "dry_run": false,
        "force": false,
    });

    let resp = admin_router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/bootstrap")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(
        resp.status().is_client_error(),
        "no sources is a validation failure, got {}",
        resp.status()
    );
    assert_eq!(
        captured.text(),
        "",
        "a 4xx must not write to the server log"
    );
}
