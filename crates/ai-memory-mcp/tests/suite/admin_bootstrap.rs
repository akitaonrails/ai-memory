//! Integration tests for `POST /admin/bootstrap`.
//!
//! Exercises the dry-run path (no LLM required) end-to-end through the
//! axum router: marshal a synthetic [`BootstrapSource`] bundle, POST it
//! to the router, and assert the response shape matches [`BootstrapOutcome`].
//! The LLM path is covered by the consolidate-crate unit tests.

use ai_memory_consolidate::{BootstrapOutcome, BootstrapSource, SourceKind};
use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

struct EmptyBootstrapLlm;

#[async_trait::async_trait]
impl LlmProvider for EmptyBootstrapLlm {
    fn name(&self) -> &'static str {
        "empty-bootstrap"
    }

    fn model(&self) -> &str {
        "empty-bootstrap"
    }

    async fn complete(&self, _request: ChatRequest) -> ai_memory_llm::LlmResult<ChatResponse> {
        unreachable!("bootstrap uses structured completion")
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> ai_memory_llm::LlmResult<serde_json::Value> {
        Ok(json!({ "pages": [], "rationale": "ok" }))
    }
}

/// Records how many structured completions overlap. Each call waits at a
/// barrier sized for the overlap a test expects, so a gate that admits that
/// many runs passes deterministically, while a stricter gate leaves the
/// first call alone at the barrier until `rendezvous` times out.
struct OverlapLlm {
    in_flight: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    barrier: tokio::sync::Barrier,
    rendezvous: std::time::Duration,
}

impl OverlapLlm {
    fn new(expected_overlap: usize, rendezvous: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
            barrier: tokio::sync::Barrier::new(expected_overlap),
            rendezvous,
        })
    }

    fn peak(&self) -> usize {
        self.peak.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LlmProvider for OverlapLlm {
    fn name(&self) -> &'static str {
        "overlap"
    }

    fn model(&self) -> &str {
        "overlap"
    }

    async fn complete(&self, _request: ChatRequest) -> ai_memory_llm::LlmResult<ChatResponse> {
        unreachable!("bootstrap uses structured completion")
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> ai_memory_llm::LlmResult<serde_json::Value> {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let _ = tokio::time::timeout(self.rendezvous, self.barrier.wait()).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(json!({ "pages": [], "rationale": "ok" }))
    }
}

fn live_bootstrap_body(project: &str) -> serde_json::Value {
    json!({
        "workspace": "default", "project": project, "sources": synthetic_sources(),
        "max_input_tokens": 50_000, "dry_run": false, "force": false
    })
}

/// Run two live bootstraps at the same time.
async fn concurrent_bootstraps(state: AdminState, projects: [&str; 2]) -> Vec<StatusCode> {
    let (first, second) = tokio::join!(
        post_bootstrap(state.clone(), live_bootstrap_body(projects[0])),
        post_bootstrap(state, live_bootstrap_body(projects[1])),
    );
    vec![first.status(), second.status()]
}

/// `bootstrap_max_concurrent = 2` lets runs for two different projects call
/// the LLM at the same time. Under the old server-wide mutex the peak was 1.
#[tokio::test]
async fn distinct_projects_bootstrap_concurrently_up_to_the_limit() {
    let tmp = TempDir::new().unwrap();
    let mut state = make_admin_state(&tmp).await;
    let llm = OverlapLlm::new(2, std::time::Duration::from_secs(10));
    state.llm = Some(llm.clone());
    state.bootstrap_gate = ai_memory_mcp::BootstrapGate::new(2);

    let statuses = concurrent_bootstraps(state, ["alpha", "beta"]).await;

    assert_eq!(statuses, vec![StatusCode::OK, StatusCode::OK]);
    assert_eq!(
        llm.peak(),
        2,
        "both runs must be in their LLM call together"
    );
}

/// Control: the default gate keeps the previous one-at-a-time behaviour.
#[tokio::test]
async fn default_gate_still_runs_one_bootstrap_at_a_time() {
    let tmp = TempDir::new().unwrap();
    let mut state = make_admin_state(&tmp).await;
    let llm = OverlapLlm::new(2, std::time::Duration::from_millis(200));
    state.llm = Some(llm.clone());

    let statuses = concurrent_bootstraps(state, ["alpha", "beta"]).await;

    assert_eq!(statuses, vec![StatusCode::OK, StatusCode::OK]);
    assert_eq!(llm.peak(), 1, "default bootstrap_max_concurrent is 1");
}

/// Two runs for the same project never overlap, however many slots are
/// free: the second waits, finds the first run's manifest and is refused
/// as already bootstrapped instead of paying for the LLM calls again.
#[tokio::test]
async fn same_project_bootstraps_stay_serialised_with_free_slots() {
    let tmp = TempDir::new().unwrap();
    let mut state = make_admin_state(&tmp).await;
    let llm = OverlapLlm::new(2, std::time::Duration::from_millis(200));
    state.llm = Some(llm.clone());
    state.bootstrap_gate = ai_memory_mcp::BootstrapGate::new(4);

    let mut statuses = concurrent_bootstraps(state, ["alpha", "alpha"]).await;
    statuses.sort_by_key(StatusCode::as_u16);

    assert_eq!(
        statuses,
        vec![StatusCode::OK, StatusCode::UNPROCESSABLE_ENTITY],
        "exactly one run writes the project; the other sees its manifest"
    );
    assert_eq!(llm.peak(), 1, "a project's runs must not overlap");
}

/// Build a minimal `AdminState` backed by a real on-disk store + wiki.
async fn make_admin_state(tmp: &TempDir) -> AdminState {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let db_path = store.db_path().to_path_buf();
    AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None, // no LLM — bootstrap report path only
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

/// Synthetic sources that represent a small project.
fn synthetic_sources() -> Vec<BootstrapSource> {
    vec![
        BootstrapSource {
            kind: SourceKind::Readme,
            label: "README (README.md)".into(),
            text: "# my-project\n\nA toy project used for testing bootstrap ingestion.".into(),
        },
        BootstrapSource {
            kind: SourceKind::ProjectRules,
            label: "rules: CLAUDE.md".into(),
            text: "## Rules\n\n- Always write tests first.\n- Prefer composition.".into(),
        },
        BootstrapSource {
            kind: SourceKind::GitCommit,
            label: "git: feat: add initial storage layer".into(),
            text: "Commit abc12345\nAuthor: Test\nDate: 2026-01-01\n\nfeat: add initial storage layer\n\nAdded SQLite-backed store with WAL mode.".into(),
        },
    ]
}

/// POST a JSON body to the admin router and return the response.
async fn post_bootstrap(state: AdminState, body: serde_json::Value) -> axum::response::Response {
    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/admin/bootstrap")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    router.oneshot(req).await.unwrap()
}

/// Dry-run with no LLM returns a valid [`BootstrapOutcome`] with
/// `dry_run: true` and no pages written.
#[tokio::test]
async fn dry_run_returns_outcome_without_llm() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp).await;

    let body = json!({
        "workspace": "test-ws",
        "project": "test-proj",
        "sources": synthetic_sources(),
        "max_input_tokens": 50_000,
        "dry_run": true,
        "force": false,
    });

    let resp = post_bootstrap(state, body).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let outcome: BootstrapOutcome = serde_json::from_slice(&bytes).unwrap();

    assert!(outcome.dry_run, "must be a dry-run outcome");
    assert_eq!(outcome.pages_written.len(), 0, "dry-run writes no pages");
    assert_eq!(outcome.sources_collected, 3, "three sources were supplied");
    assert!(outcome.sources_sent <= outcome.sources_collected);
    assert_eq!(outcome.llm_chunks, 1, "small bundle fits one chunk");
}

/// Client-side pre-prune count is preserved in the outcome.
#[tokio::test]
async fn dry_run_honours_sources_collected_hint() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp).await;

    let body = json!({
        "workspace": "test-ws",
        "project": "test-proj",
        "sources": synthetic_sources(),
        "sources_collected": 9_999,
        "max_input_tokens": 50_000,
        "dry_run": true,
    });

    let resp = post_bootstrap(state, body).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let outcome: BootstrapOutcome = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(outcome.sources_collected, 9_999);
    assert_eq!(
        outcome.sources_dropped,
        9_999 - outcome.sources_sent,
        "dropped must be collected minus sent"
    );
}

/// When no LLM is configured and `dry_run=false`, the server returns
/// 503 with a descriptive error body — not a panic.
#[tokio::test]
async fn bootstrap_surfaces_manifest_failure_after_promotion() {
    let tmp = TempDir::new().unwrap();
    let mut state = make_admin_state(&tmp).await;
    state.llm = Some(Arc::new(EmptyBootstrapLlm));
    let workspace = state
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let (project, _) = state
        .writer
        .resolve_project_by_identity(
            workspace,
            RepositoryIdentity {
                identity: "github.com/acme/api".into(),
                source: IdentitySource::GitRemote,
            },
            IdentityStyle::HostPath,
            "api",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    state.wiki.backfill_scope_manifests().await.unwrap();
    let manifest = tmp
        .path()
        .join("wiki")
        .join(workspace.to_string())
        .join(project.to_string())
        .join("_meta.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let resp = post_bootstrap(
        state,
        json!({
            "workspace": "default", "project": "acme-api", "sources": synthetic_sources(),
            "max_input_tokens": 50_000, "dry_run": false, "force": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let outcome: BootstrapOutcome = serde_json::from_slice(&bytes).unwrap();
    assert!(
        outcome
            .manifest_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("committed"))
    );
}

#[tokio::test]
async fn non_dry_run_without_llm_returns_503() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp).await;

    let body = json!({
        "workspace": "test-ws",
        "project": "test-proj",
        "sources": synthetic_sources(),
        "max_input_tokens": 50_000,
        "dry_run": false,
        "force": false,
    });

    let resp = post_bootstrap(state, body).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let val: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let error_msg = val["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("LLM") || error_msg.contains("provider"),
        "error message must mention LLM/provider: {error_msg}"
    );
}

/// Empty source list returns an error — the server must not panic or
/// produce a misleading success response.
#[tokio::test]
async fn empty_sources_returns_error() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp).await;

    let body = json!({
        "workspace": "test-ws",
        "project": "test-proj",
        "sources": [],
        "dry_run": true,
    });

    let resp = post_bootstrap(state, body).await;
    // Empty sources with dry_run=true goes through process_sources which
    // returns BootstrapError::NoSources -> UNPROCESSABLE_ENTITY.
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "empty sources must not succeed"
    );
}

/// A non-empty request that gets fully pruned by an impossibly small budget is
/// still a validation failure, not a successful no-op bootstrap.
#[tokio::test]
async fn fully_pruned_sources_return_error() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp).await;

    let body = json!({
        "workspace": "test-ws",
        "project": "test-proj",
        "sources": synthetic_sources(),
        "max_input_tokens": 1,
        "dry_run": true,
    });

    let resp = post_bootstrap(state, body).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// The workspace + project are auto-created by the handler; posting the
/// same workspace/project a second time with `force=true` must not error
/// (idempotent upsert).
#[tokio::test]
async fn workspace_and_project_auto_created_idempotent() {
    let tmp = TempDir::new().unwrap();
    let state = make_admin_state(&tmp).await;

    let body = json!({
        "workspace": "idempotent-ws",
        "project": "idempotent-proj",
        "sources": synthetic_sources(),
        "dry_run": true,
        "force": true,
    });

    // First call.
    let resp1 = post_bootstrap(state.clone(), body.clone()).await;
    assert_eq!(resp1.status(), StatusCode::OK);

    // Second call — workspace + project already exist; must still succeed.
    let resp2 = post_bootstrap(state, body).await;
    assert_eq!(resp2.status(), StatusCode::OK);
}
