//! Integration tests for `POST /admin/bootstrap`.
//!
//! Exercises the dry-run path (no LLM required) end-to-end through the
//! axum router: marshal a synthetic [`BootstrapSource`] bundle, POST it
//! to the router, and assert the response shape matches [`BootstrapOutcome`].
//! The LLM path is covered by the consolidate-crate unit tests.

use ai_memory_consolidate::{BootstrapOutcome, BootstrapSource, SourceKind};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

/// Build a minimal `AdminState` backed by a real on-disk store + wiki.
async fn make_admin_state(tmp: &TempDir) -> AdminState {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None, // no LLM — dry-run path only
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
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
