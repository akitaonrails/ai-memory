//! Integration tests for `POST /admin/purge-project`.
//!
//! Follows the same pattern as `admin_phase3.rs`: build a real
//! [`AdminState`] over a tmpdir-backed store + wiki, drive the router
//! with `tower::ServiceExt::oneshot`.

use ai_memory_core::{
    AgentKind, NewHandoff, NewObservation, NewSession, ObservationKind, PagePath, ProjectId,
    Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, PrepareWorkstreamRun, Store, WorkstreamSelection};
use ai_memory_wiki::{
    AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki, WritePageRequest,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post as route_post;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
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

/// Seed two projects (`default/keep` and `default/doomed`), each with one
/// page, one session, some observations, and a handoff. Returns IDs for
/// both projects so callers can construct the per-project wiki paths.
async fn seed_two_projects(store: &Store, wiki: &Wiki) -> (WorkspaceId, ProjectId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let keep = store
        .writer
        .get_or_create_project(ws, "keep", None)
        .await
        .unwrap();
    let doomed = store
        .writer
        .get_or_create_project(ws, "doomed", None)
        .await
        .unwrap();

    // Write pages through the wiki so they land on disk in per-project dirs.
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: keep,
        path: PagePath::new("notes/keep.md").unwrap(),
        frontmatter: serde_json::json!({"title": "Keep page"}),
        body: "This page must survive the purge.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: Some("Keep page".into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    })
    .await
    .unwrap();

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: doomed,
        path: PagePath::new("notes/doomed.md").unwrap(),
        frontmatter: serde_json::json!({"title": "Doomed page"}),
        body: "This page will be destroyed.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: Some("Doomed page".into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    })
    .await
    .unwrap();

    // Sessions + observations for both projects.
    for (proj, label) in [(keep, "keep"), (doomed, "doomed")] {
        let sid = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        for i in 0..3u8 {
            store
                .writer
                .insert_observation(Sanitized::new(
                    NewObservation {
                        session_id: sid,
                        workspace_id: ws,
                        project_id: proj,
                        kind: ObservationKind::UserPrompt,
                        extension: None,
                        source_event: None,
                        title: format!("{label} obs {i}"),
                        body: "body".into(),
                        importance: 5,
                    },
                    &Sanitizer::builtin(),
                ))
                .await
                .unwrap();
        }
        // Handoff for the doomed project only.
        if label == "doomed" {
            store
                .writer
                .insert_handoff(NewHandoff {
                    workspace_id: ws,
                    project_id: proj,
                    from_session_id: Some(sid),
                    from_agent: AgentKind::ClaudeCode,
                    to_agent: None,
                    cwd: None,
                    summary: "doomed handoff".into(),
                    open_questions: vec![],
                    next_steps: vec![],
                    files_touched: vec![],
                    owner_user: None,
                })
                .await
                .unwrap();
        }
    }

    (ws, keep, doomed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Missing `confirm: true` must return 400.
#[tokio::test]
async fn purge_project_without_confirm_returns_400() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "any", "confirm": false }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("confirm=true"),
        "error must mention confirm=true: {body}"
    );
}

/// Non-existent project must return 404.
#[tokio::test]
async fn purge_project_nonexistent_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    // Ensure the workspace exists so the 404 is about the project.
    store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "nonexistent", "confirm": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "nonexistent project must 404"
    );

    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "error must say 'not found': {body}"
    );
}

/// Non-existent workspace must also return 404.
#[tokio::test]
async fn purge_project_nonexistent_workspace_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "ghost-workspace", "project": "x", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Happy path: purge the doomed project, verify counts and directory removal.
#[tokio::test]
async fn purge_project_deletes_data_and_files() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;

    // The per-project directory must exist before purge.
    let proj_dir = state.wiki.project_root(ws, doomed);
    assert!(proj_dir.exists(), "project dir must exist before purge");

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "purge must succeed");

    let body = body_json(resp).await;
    assert_eq!(
        body["label"].as_str().unwrap_or(""),
        "default/doomed",
        "label must match: {body}"
    );
    assert_eq!(
        body["pages_deleted"].as_u64().unwrap_or(0),
        1,
        "one page deleted: {body}"
    );
    assert_eq!(
        body["sessions_deleted"].as_u64().unwrap_or(0),
        1,
        "one session deleted: {body}"
    );
    assert_eq!(
        body["observations_deleted"].as_u64().unwrap_or(0),
        3,
        "three observations deleted: {body}"
    );
    assert_eq!(
        body["handoffs_deleted"].as_u64().unwrap_or(0),
        1,
        "one handoff deleted: {body}"
    );

    // The entire project directory must be gone.
    assert!(
        !proj_dir.exists(),
        "project directory must be removed after purge"
    );
}

#[tokio::test]
async fn purge_project_removes_raw_workstream_segments() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (workspace_id, _keep, project_id) = seed_two_projects(&store, &state.wiki).await;
    let prepared = store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            workspace_id,
            project_id,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: vec![AgentKind::Codex],
            selection: WorkstreamSelection::Current,
            lease_owner: "test".into(),
        })
        .await
        .unwrap();
    assert!(
        store
            .writer
            .cancel_managed_run(prepared.run_id)
            .await
            .unwrap()
    );
    let raw_dir = tmp
        .path()
        .join("raw/workstreams")
        .join(prepared.workstream_id.to_string());
    std::fs::create_dir_all(&raw_dir).unwrap();
    std::fs::write(raw_dir.join("000001.jsonl"), "event\n").unwrap();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    assert!(
        !raw_dir.exists(),
        "raw workstream directory must be removed"
    );
    assert_eq!(body["workstreams_deleted"], 1);
    assert_eq!(body["managed_runs_deleted"], 1);
    assert_eq!(
        body["workstream_ids"],
        json!([prepared.workstream_id.to_string()])
    );
    let raw_suffix = Path::new("raw")
        .join("workstreams")
        .join(prepared.workstream_id.to_string());
    assert!(
        body["files_deleted"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|path| path.as_str())
            .any(|path| Path::new(path).ends_with(&raw_suffix)),
        "raw cleanup must be visible in the report: {body}"
    );
}

/// A reject-policy purge webhook must be able to abort before DB rows or files
/// are deleted. This guards the destructive-operation ordering.
#[tokio::test]
async fn purge_project_rejecting_admission_leaves_source_intact() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let app = Router::new().route(
        "/guard",
        route_post(|| async { (StatusCode::FORBIDDEN, "blocked") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "purge-guard".into(),
        url: format!("http://{addr}/guard"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::PurgeProject],
        blocking: true,
    }])
    .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_admission_chain(chain)
        .with_store_reader(store.reader.clone());
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
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };

    let (ws, _keep, doomed) = seed_two_projects(&store, &state.wiki).await;
    let doomed_dir = state.wiki.project_root(ws, doomed);

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

    assert!(
        store
            .reader
            .find_project(ws, "doomed".to_string())
            .await
            .unwrap()
            .is_some(),
        "rejecting admission must leave project row intact"
    );
    assert!(
        doomed_dir.exists(),
        "rejecting admission must leave files intact"
    );
    assert_eq!(
        store
            .reader
            .list_pages("default", "doomed")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// After purging `doomed`, `keep` must still have all its data and files intact.
#[tokio::test]
async fn purge_project_preserves_sibling_project() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let (ws, keep, _doomed) = seed_two_projects(&store, &state.wiki).await;
    // Clone the wiki handle before state is consumed by `post`.
    let wiki = state.wiki.clone();

    let resp = post(
        state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The `keep` project's data must be untouched.
    let counts = store.reader.status_counts().await.unwrap();
    // 1 page (keep), 1 session (keep), 3 observations (keep).
    assert_eq!(
        counts.pages_latest, 1,
        "keep's page must survive: {counts:?}"
    );
    assert_eq!(
        counts.sessions, 1,
        "keep's session must survive: {counts:?}"
    );
    assert_eq!(
        counts.observations, 3,
        "keep's observations must survive: {counts:?}"
    );

    // keep's wiki directory must still exist with its page.
    let keep_dir = wiki.project_root(ws, keep);
    assert!(keep_dir.exists(), "keep's project dir must survive");
    let keep_file = keep_dir.join("notes/keep.md");
    assert!(keep_file.exists(), "keep's wiki file must survive");
}

/// Purging the same project twice: second call returns 404 (already gone).
#[tokio::test]
async fn purge_project_idempotent_second_call_is_404() {
    let tmp = TempDir::new().unwrap();
    let (state_a, store) = make_state(&tmp).await;
    let state_b = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: state_a.wiki.clone(),
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };

    seed_two_projects(&store, &state_a.wiki).await;

    // First purge succeeds.
    let r1 = post(
        state_a,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(r1.status(), StatusCode::OK);

    // Second purge: project is gone → 404.
    let r2 = post(
        state_b,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "doomed", "confirm": true }),
    )
    .await;
    assert_eq!(
        r2.status(),
        StatusCode::NOT_FOUND,
        "second purge must 404 because project is already gone"
    );
}
