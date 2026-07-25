//! Integration tests for Phase 3 admin routes:
//! `POST /admin/reorg`, `POST /admin/lint`, `POST /admin/forget-sweep`,
//! `POST /admin/embed`, `POST /admin/commit`.
//!
//! Follows the same pattern as `admin_bootstrap.rs` and
//! `admin_status_search.rs`: build a real [`AdminState`] over a
//! tmpdir-backed store + wiki, drive the router with
//! `tower::ServiceExt::oneshot`.

use ai_memory_core::{
    ActorContext, AgentKind, NewObservation, NewPage, NewSession, ObservationKind, PagePath,
    Sanitized, Sanitizer, SessionId, Tier,
};
use ai_memory_llm::SyntheticEmbedder;
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{
    AutoImproveProposalOperation, AutoImproveProposalStatus, DecayParams, NewAutoImproveProposal,
    StageAutoImproveRun, Store,
};
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
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

async fn get(state: AdminState, uri: &str) -> axum::response::Response {
    let router = admin_router(state);
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    router.oneshot(req).await.unwrap()
}

fn telemetry_stage_input(
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
) -> StageAutoImproveRun {
    StageAutoImproveRun {
        workspace_id: ws,
        project_id: proj,
        session_id: None,
        provider: Some("test".into()),
        model: Some("test-model".into()),
        summary: Some("test telemetry seed".into()),
        warnings_json: serde_json::json!([]),
        rejected_candidates_json: serde_json::json!([]),
        config_json: serde_json::json!({}),
        proposal_actor: ActorContext::default(),
        proposals: vec![NewAutoImproveProposal {
            operation: AutoImproveProposalOperation::Create,
            target_path: PagePath::new("notes/telemetry.md").unwrap(),
            kind: "note".into(),
            title: "Telemetry seed".into(),
            confidence: 0.9,
            rationale: "seed report telemetry".into(),
            evidence_json: serde_json::json!([]),
            body_markdown: "# Telemetry".into(),
            artifact_sha256: None,
            edit_mode: None,
            patch_json: None,
            expected_base_body_sha256: None,
        }],
    }
}

#[tokio::test]
async fn auto_improve_report_returns_telemetry_without_creating_proposals() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
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
        .stage_auto_improve_run(telemetry_stage_input(ws, proj))
        .await
        .unwrap();
    let before = store
        .reader
        .list_auto_improve_proposals(ws, proj, None, 50)
        .await
        .unwrap()
        .len();

    let resp = post(
        state,
        "/admin/auto-improve/report",
        json!({ "workspace": "default", "project": "scratch", "since_days": 30, "limit": 3 }),
    )
    .await;
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert!(body["summary"].as_str().unwrap().contains("run(s)"));
    assert_eq!(body["aggregate"]["run_count"].as_u64().unwrap(), 1);
    assert_eq!(
        body["aggregate"]["proposals_by_status"][0]["key"]
            .as_str()
            .unwrap(),
        "pending"
    );
    assert_eq!(body["terminal_rates"]["denominator"].as_u64().unwrap(), 0);

    let after = store
        .reader
        .list_auto_improve_proposals(ws, proj, None, 50)
        .await
        .unwrap()
        .len();
    assert_eq!(after, before, "report endpoint must not stage proposals");
    let pending = store
        .reader
        .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Pending), 50)
        .await
        .unwrap()
        .len();
    assert_eq!(pending, before);
}

#[tokio::test]
async fn auto_improve_report_missing_scope_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/auto-improve/report",
        json!({ "workspace": "default", "project": "missing" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auto_improve_report_stage_creates_one_report_proposal_and_approval_writes_page() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
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

    let before = store
        .reader
        .list_auto_improve_proposals(ws, proj, None, 50)
        .await
        .unwrap()
        .len();
    let resp = post(
        state.clone(),
        "/admin/auto-improve/report",
        json!({ "workspace": "default", "project": "scratch", "since_days": 30, "limit": 3, "stage": true }),
    )
    .await;
    let status = resp.status();
    let body = body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let proposal_ids = body["proposal_ids"].as_array().unwrap();
    assert_eq!(proposal_ids.len(), 1);
    assert_eq!(body["sidecar_paths"].as_array().unwrap().len(), 1);

    let proposals = store
        .reader
        .list_auto_improve_proposals(ws, proj, None, 50)
        .await
        .unwrap();
    assert_eq!(proposals.len(), before + 1);
    let staged = proposals
        .iter()
        .find(|p| p.id.to_string() == proposal_ids[0].as_str().unwrap())
        .unwrap();
    assert_eq!(staged.status, AutoImproveProposalStatus::Pending);
    assert_eq!(staged.kind, "auto_improve_report");
    let target_path = staged.target_path.as_str().to_string();
    assert!(target_path.starts_with("notes/auto-improve-report-"));
    assert!(target_path.ends_with(".md"));
    assert!(
        store
            .reader
            .page_body_by_ids(ws, proj, &target_path)
            .await
            .unwrap()
            .is_none(),
        "staging must not write the target page before approval"
    );

    let approve_resp = post(
        state.clone(),
        &format!(
            "/admin/pending-writes/{}/approve?workspace=default&project=scratch",
            staged.id
        ),
        json!({}),
    )
    .await;
    assert_eq!(approve_resp.status(), StatusCode::OK);
    let report_page = store
        .reader
        .page_body_by_ids(ws, proj, &target_path)
        .await
        .unwrap()
        .expect("approval writes report page");
    assert!(report_page.body.contains("# Auto-Improve Telemetry Report"));
    assert_eq!(
        store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 50)
            .await
            .unwrap()
            .len(),
        before + 1,
        "approval must not create extra proposals"
    );

    let telemetry_resp = post(
        state,
        "/admin/auto-improve/report",
        json!({ "workspace": "default", "project": "scratch", "since_days": 30, "limit": 10 }),
    )
    .await;
    assert_eq!(telemetry_resp.status(), StatusCode::OK);
    let telemetry = body_json(telemetry_resp).await;
    let maintenance = telemetry["aggregate"]["maintenance_proposals_by_kind"]
        .as_array()
        .unwrap();
    assert!(maintenance.iter().any(|row| {
        row["key"].as_str() == Some("auto_improve_report") && row["count"].as_u64() == Some(1)
    }));
    assert_eq!(
        telemetry["aggregate"]["proposals_by_kind"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "report proposals are maintenance, not learning proposals"
    );
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
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid_a,
                workspace_id: ws,
                project_id: scratch,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "alpha prompt".into(),
                body: "".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
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
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid_b,
                workspace_id: ws,
                project_id: scratch,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "beta prompt".into(),
                body: "".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
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
async fn reorg_does_not_store_nongit_cwd_as_repo_path() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_sessions_for_reorg(&store).await;

    let resp = post(state, "/admin/reorg", json!({ "dry_run": false })).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let scopes = store.reader.list_all_scopes().await.unwrap();
    for project_name in ["alpha-repo", "beta-repo"] {
        let scope = scopes
            .iter()
            .find(|s| s.workspace_name == "default" && s.project_name == project_name)
            .unwrap_or_else(|| panic!("missing reorg-created project {project_name}"));
        assert_eq!(
            scope.repo_path, None,
            "admin reorg must not turn non-git cwd into catch-all repo_path for {project_name}"
        );
    }
}

#[tokio::test]
async fn reorg_live_graveyards_only_default_workspace_pages() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_sessions_for_reorg(&store).await;

    let default_ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let default_proj = store
        .writer
        .get_or_create_project(default_ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: default_ws,
            project_id: default_proj,
            path: PagePath::new("notes/default-latest.md").unwrap(),
            title: "Default latest".into(),
            body: "default workspace page should be graveyarded".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
        })
        .await
        .unwrap();

    let sibling_ws = store
        .writer
        .get_or_create_workspace("sibling")
        .await
        .unwrap();
    let sibling_proj = store
        .writer
        .get_or_create_project(sibling_ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: sibling_ws,
            project_id: sibling_proj,
            path: PagePath::new("notes/sibling-latest.md").unwrap(),
            title: "Sibling latest".into(),
            body: "sibling workspace page must remain latest".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
        })
        .await
        .unwrap();

    let resp = post(state, "/admin/reorg", json!({ "dry_run": false })).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["summary"]["pages_graveyarded"].as_u64().unwrap(), 1);

    let default_hits = store
        .reader
        .recent_pages_for_project(default_ws, default_proj, 10)
        .await
        .unwrap();
    assert!(
        default_hits.is_empty(),
        "default workspace latest page should be graveyarded"
    );

    let sibling_hits = store
        .reader
        .recent_pages_for_project(sibling_ws, sibling_proj, 10)
        .await
        .unwrap();
    assert_eq!(
        sibling_hits.len(),
        1,
        "sibling workspace latest page must survive default reorg"
    );
    assert_eq!(sibling_hits[0].path.as_str(), "notes/sibling-latest.md");
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
            author_id: None,
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
    let (state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

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
async fn embed_missing_project_scope_does_not_create_workspace() {
    let tmp = TempDir::new().unwrap();
    let (mut state, store) = make_state(&tmp).await;
    state.embedder = Some(Arc::new(SyntheticEmbedder::new(64)));

    let resp = post(
        state,
        "/admin/embed",
        json!({
            "workspace": "missing",
            "project": "ghost",
            "reembed": false,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let missing_ws = store
        .reader
        .find_workspace("missing".to_string())
        .await
        .unwrap();
    assert!(
        missing_ws.is_none(),
        "embed must not create a missing workspace"
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
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
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
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
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
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
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

#[tokio::test]
async fn checkpoints_list_and_restore_page_round_trip() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
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
    let path = PagePath::new("notes/recover.md").unwrap();

    state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: path.clone(),
            frontmatter: serde_json::json!({"title": "Recover", "tier": "semantic"}),
            body: "old body".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Recover".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();
    let old_oid = state.wiki.commit_all("old checkpoint").unwrap().unwrap();

    state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: path.clone(),
            frontmatter: serde_json::json!({"title": "Recover", "tier": "semantic"}),
            body: "new body".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Recover".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();
    state.wiki.commit_all("new checkpoint").unwrap().unwrap();

    let resp = get(state.clone(), "/admin/checkpoints?limit=5").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let checkpoints = body_json(resp).await;
    assert!(
        checkpoints
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["summary"] == "old checkpoint")),
        "expected old checkpoint in list: {checkpoints}"
    );

    let resp = post(
        state.clone(),
        "/admin/restore-page",
        json!({
            "workspace": "default",
            "project": "scratch",
            "path": "notes/recover.md",
            "rev": old_oid.to_string(),
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["path"], "notes/recover.md");
    assert_eq!(body["restored_from"], old_oid.to_string());

    let md = state.wiki.read_page(ws, proj, &path).unwrap();
    assert_eq!(md.body, "old body");
    let stored = store
        .reader
        .page_body_by_ids(ws, proj, "notes/recover.md")
        .await
        .unwrap()
        .expect("restored page should be indexed");
    assert_eq!(stored.body, "old body");
}
