//! `POST /admin/reclaim-ledger-versions` is a new destructive entry point, so
//! it gets the adversarial treatment [`docs/security-boundaries.md`] requires:
//! attempt the violation, assert the refusal, and include a legitimate
//! control.
//!
//! The guard under test is the confirm gate. Unlike `compact`, this route
//! removes page rows, so a request that deletes without saying so must be
//! refused — and a request that only measures must be allowed through without
//! `confirm`, because a dry run is precisely the step that lets an operator
//! decide.

use super::common::{get, post};
use ai_memory_consolidate::{DEFAULT_CONTRADICTION_SIM_HIGH, DEFAULT_CONTRADICTION_SIM_LOW};
use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_mcp::AdminState;
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::http::StatusCode;
use serde_json::json;
use tempfile::TempDir;

async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
        db_path,
    };
    (state, store)
}

/// Total `pages` rows, read back through the public reader rather than the
/// handle the writer used, so a passing assertion is not the writer agreeing
/// with itself.
async fn page_rows(store: &Store) -> u64 {
    store.reader.status_counts().await.unwrap().pages_all
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// Seed a ledger with two versions, so there is residue to reclaim.
async fn seed_ledger(store: &Store) -> ai_memory_core::ProjectId {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    for stamp in ["2026-09-05T00:00:00Z", "2026-09-06T00:00:00Z"] {
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new("log-2026-09.md").unwrap(),
                title: "t".into(),
                body: format!("## [{stamp}] tool Bash: cargo test\n"),
                tier: Tier::Episodic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
    }
    proj
}

#[tokio::test]
async fn deleting_without_confirm_is_refused_and_changes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_ledger(&store).await;
    let before = page_rows(&store).await;
    assert_eq!(before, 2);

    let resp = post(
        state,
        "/admin/reclaim-ledger-versions",
        json!({ "confirm": false }),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a deleting run must say so"
    );
    assert_eq!(
        page_rows(&store).await,
        before,
        "the refusal has to be a real refusal, not an error raised after the fact",
    );
}

/// `confirm` has no serde default, so a body that omits it is rejected by the
/// extractor before the handler runs — the same fail-closed shape
/// `/admin/compact` uses. A request that never states its intent cannot
/// delete.
#[tokio::test]
async fn omitting_confirm_entirely_is_refused() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_ledger(&store).await;

    let resp = post(state, "/admin/reclaim-ledger-versions", json!({})).await;

    assert!(
        resp.status().is_client_error(),
        "expected a client error, got {}",
        resp.status(),
    );
    assert_eq!(page_rows(&store).await, 2);
}

/// Legitimate control: the measuring run needs no `confirm`, because it
/// changes nothing — and that is the step the CLI puts in front of `--confirm`.
#[tokio::test]
async fn a_dry_run_is_allowed_without_confirm_and_reports_the_residue() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_ledger(&store).await;

    let resp = post(
        state,
        "/admin/reclaim-ledger-versions",
        json!({ "confirm": false, "dry_run": true }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["ledger_paths"], 1);
    assert_eq!(body["pages_deleted"], 1);
    assert!(body["bytes_deleted"].as_u64().unwrap() > 0);
    assert_eq!(page_rows(&store).await, 2, "a dry run deletes nothing");
}

/// Legitimate control: the confirmed run goes through, and takes the superseded
/// version only.
#[tokio::test]
async fn the_confirmed_run_reclaims_the_superseded_version() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_ledger(&store).await;

    let resp = post(
        state,
        "/admin/reclaim-ledger-versions",
        json!({ "confirm": true }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["pages_deleted"], 1);
    assert_eq!(body["dropped_latest"], false);
    assert_eq!(page_rows(&store).await, 1);
}

#[tokio::test]
async fn get_is_not_a_valid_method_for_this_route() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = get(state, "/admin/reclaim-ledger-versions").await;
    assert!(
        resp.status().is_client_error(),
        "the route is POST-only, got {}",
        resp.status(),
    );
}
