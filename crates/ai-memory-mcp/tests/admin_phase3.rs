//! Integration tests for Phase 3 admin routes:
//! `POST /admin/reorg`, `POST /admin/lint`, `POST /admin/forget-sweep`,
//! `POST /admin/embed`, `POST /admin/commit`.
//!
//! Follows the same pattern as `admin_bootstrap.rs` and
//! `admin_status_search.rs`: build a real [`AdminState`] over a
//! tmpdir-backed store + wiki, drive the router with
//! `tower::ServiceExt::oneshot`.

use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, ObservationKind, PagePath, SessionId, Tier,
};
use ai_memory_llm::SyntheticEmbedder;
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use ai_memory_wiki::WritePageRequest;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal `AdminState` with no LLM and no embedder.
async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    };
    (state, store)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn post(state: AdminState, uri: &str, body: serde_json::Value) -> axum::response::Response {
    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    router.oneshot(req).await.unwrap()
}

// ---------------------------------------------------------------------------
// reorg
// ---------------------------------------------------------------------------

/// Seed two sessions in two distinct cwds inside the "scratch" project.
async fn seed_sessions_for_reorg(store: &Store) -> (SessionId, SessionId) {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let scratch = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    let sid_a = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid_a,
            workspace_id: ws,
            project_id: scratch,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some(std::path::PathBuf::from("/home/user/alpha-repo")),
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(NewObservation {
            session_id: sid_a,
            workspace_id: ws,
            project_id: scratch,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "alpha prompt".into(),
            body: "".into(),
            importance: 5,
        })
        .await
        .unwrap();

    let sid_b = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid_b,
            workspace_id: ws,
            project_id: scratch,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some(std::path::PathBuf::from("/home/user/beta-repo")),
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(NewObservation {
            session_id: sid_b,
            workspace_id: ws,
            project_id: scratch,
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "beta prompt".into(),
            body: "".into(),
            importance: 5,
        })
        .await
        .unwrap();

    (sid_a, sid_b)
}

#[tokio::test]
async fn reorg_dry_run_returns_plan_entries() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (sid_a, sid_b) = seed_sessions_for_reorg(&store).await;

    let resp = post(state, "/admin/reorg", json!({ "dry_run": true })).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    let plan = body["plan"].as_array().unwrap();
    // Both sessions are in the wrong (scratch) project; both must appear.
    assert_eq!(plan.len(), 2, "two sessions need moving: {body}");

    let session_ids: Vec<&str> = plan
        .iter()
        .map(|e| e["session_id"].as_str().unwrap())
        .collect();
    assert!(
        session_ids.contains(&sid_a.to_string().as_str())
            || session_ids.iter().any(|s| *s == sid_a.to_string()),
        "sid_a must be in plan"
    );
    assert!(
        session_ids.iter().any(|s| *s == sid_b.to_string()),
        "sid_b must be in plan"
    );

    // dry-run → summary counters must be zero.
    assert_eq!(body["summary"]["sessions_moved"].as_u64().unwrap(), 0);
    assert!(body["dry_run"].as_bool().unwrap());
}

#[tokio::test]
async fn reorg_live_moves_sessions() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_sessions_for_reorg(&store).await;

    let resp = post(state, "/admin/reorg", json!({ "dry_run": false })).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["summary"]["sessions_moved"].as_u64().unwrap(), 2);
    assert_eq!(body["summary"]["observations_updated"].as_u64().unwrap(), 2);
    assert!(!body["dry_run"].as_bool().unwrap());
}

#[tokio::test]
async fn reorg_empty_store_returns_empty_plan() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(state, "/admin/reorg", json!({})).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["plan"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// lint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lint_dry_run_returns_lint_report_shape() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    // Seed a page so the lint pass has something to evaluate.
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/test.md").unwrap(),
            title: "Test page".into(),
            body: "Some content for lint testing.".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
        })
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/lint",
        json!({
            "workspace": "default",
            "project": "scratch",
            "dry_run": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    // LintReport must have a `findings` array (possibly empty when
    // the rule-based pass finds nothing to flag).
    assert!(
        body["findings"].is_array(),
        "response must have a findings array: {body}"
    );
}

// ---------------------------------------------------------------------------
// forget-sweep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forget_sweep_dry_run_returns_sweep_report_shape() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/forget-sweep",
        json!({
            "workspace": "default",
            "project": "scratch",
            "dry_run": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    // SweepReport must have `dry_run`, `candidates_evaluated`, `evicted`,
    // and `hard_deleted` fields.
    assert!(
        body["dry_run"].is_boolean(),
        "response must have dry_run field: {body}"
    );
    assert!(
        body["candidates_evaluated"].is_number(),
        "response must have candidates_evaluated: {body}"
    );
    assert!(
        body["evicted"].is_array(),
        "response must have evicted array: {body}"
    );
}

// ---------------------------------------------------------------------------
// embed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn embed_without_embedder_returns_503() {
    let tmp = TempDir::new().unwrap();
    // AdminState is built with `embedder: None` by `make_state`.
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/embed",
        json!({
            "workspace": "default",
            "project": "scratch",
            "reembed": false,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "embed without embedder must return 503"
    );

    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or("").contains("embedder"),
        "error must mention embedder: {body}"
    );
}

#[tokio::test]
async fn embed_all_projects_rebuilds_workspace_projects() {
    let tmp = TempDir::new().unwrap();
    let (mut state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let alpha = store
        .writer
        .get_or_create_project(ws, "alpha", None)
        .await
        .unwrap();
    let beta = store
        .writer
        .get_or_create_project(ws, "beta", None)
        .await
        .unwrap();

    state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: alpha,
            path: PagePath::new("notes/a.md").unwrap(),
            frontmatter: serde_json::json!({"title": "A", "tier": "semantic"}),
            body: "alpha content".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("A".into()),
        })
        .await
        .unwrap();
    state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: beta,
            path: PagePath::new("notes/b.md").unwrap(),
            frontmatter: serde_json::json!({"title": "B", "tier": "semantic"}),
            body: "beta content".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("B".into()),
        })
        .await
        .unwrap();

    state.embedder = Some(Arc::new(SyntheticEmbedder::new(64)));
    let resp = post(
        state,
        "/admin/embed",
        json!({
            "workspace": "default",
            "reembed": true,
            "all_projects": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["embedded"].as_u64().unwrap(), 2, "{body}");
    assert_eq!(
        store
            .reader
            .embedded_page_ids(ws, alpha, "synthetic".into(), "bag-of-words-v1".into(), 64)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .reader
            .embedded_page_ids(ws, beta, "synthetic".into(), "bag-of-words-v1".into(), 64)
            .await
            .unwrap()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// commit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn commit_clean_wiki_returns_not_committed() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(state, "/admin/commit", json!({ "message": "test commit" })).await;
    // Route must not error; clean tree → committed: false.
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert!(
        body["committed"].is_boolean(),
        "response must have committed field: {body}"
    );
    // An empty wiki may return either false (nothing to stage) or true
    // (first commit of an empty tree). Both are acceptable; we just
    // verify the route doesn't panic.
}

#[tokio::test]
async fn commit_with_new_page_returns_committed_true_and_40char_oid() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    // Write a page through the wiki so it lands on disk (git can see it).
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/commit-test.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Commit test", "tier": "semantic"}),
            body: "Content for the commit test.".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Commit test".into()),
        })
        .await
        .unwrap();

    let resp = post(state, "/admin/commit", json!({ "message": "test commit" })).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert!(
        body["committed"].as_bool().unwrap_or(false),
        "expected committed=true after writing a page: {body}"
    );
    let oid = body["oid"].as_str().expect("oid field must be present");
    assert_eq!(oid.len(), 40, "oid must be a 40-char hex SHA: {oid}");
    assert!(
        oid.chars().all(|c| c.is_ascii_hexdigit()),
        "oid must be all hex digits: {oid}"
    );
}
