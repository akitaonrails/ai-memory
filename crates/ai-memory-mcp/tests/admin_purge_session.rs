//! Integration tests for `POST /admin/purge-session`.

use ai_memory_core::{
    AgentKind, NewHandoff, NewObservation, NewSession, ObservationKind, PagePath, ProjectId,
    Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
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

/// Seed `default/radar` with one session that has 4 observations, a summary
/// page (`sessions/<sid>.md` on disk), and a handoff authored by the session.
/// Returns `(workspace_id, project_id, session_id)`.
async fn seed_session(store: &Store, wiki: &Wiki) -> (WorkspaceId, ProjectId, SessionId) {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "radar", None)
        .await
        .unwrap();

    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
        })
        .await
        .unwrap();

    for i in 0..4u8 {
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
                    title: format!("obs {i}"),
                    body: "body".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
    }

    // Summary page on disk at sessions/<sid>.md.
    let page_id = wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(format!("sessions/{sid}.md")).unwrap(),
            frontmatter: serde_json::json!({"title": "session summary"}),
            body: "summary".into(),
            tier: Tier::Episodic,
            pinned: false,
            title: Some("session summary".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();
    store.writer.end_session(sid, Some(page_id)).await.unwrap();

    // A handoff authored by this session.
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: Some(sid),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "radar handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        })
        .await
        .unwrap();

    (ws, proj, sid)
}

/// Absolute on-disk path of a session's summary page file.
fn summary_file(
    tmp: &TempDir,
    ws: WorkspaceId,
    proj: ProjectId,
    sid: SessionId,
) -> std::path::PathBuf {
    tmp.path()
        .join("wiki")
        .join(ws.to_string())
        .join(proj.to_string())
        .join("sessions")
        .join(format!("{sid}.md"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `purge-session` does not require `confirm` (it is scoped to a single
/// session, unlike `purge-project`). A request without `confirm` still
/// succeeds.
#[tokio::test]
async fn purge_session_without_confirm_succeeds() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (_ws, _proj, sid) = seed_session(&store, &state.wiki).await;

    let resp = post(
        state,
        "/admin/purge-session",
        json!({ "id": sid.to_string() }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// A malformed session id must return 400.
#[tokio::test]
async fn purge_session_malformed_id_returns_400() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(state, "/admin/purge-session", json!({ "id": "not-a-uuid" })).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Purging a non-existent session must return 404.
#[tokio::test]
async fn purge_session_nonexistent_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;
    let missing = SessionId::new();

    let resp = post(
        state,
        "/admin/purge-session",
        json!({ "id": missing.to_string() }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The happy path: deletes observations, summary page, handoff, the session
/// row, and the on-disk summary file — leaving the project intact.
#[tokio::test]
async fn purge_session_deletes_data_and_file() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (ws, proj, sid) = seed_session(&store, &state.wiki).await;

    // Preconditions.
    assert!(
        summary_file(&tmp, ws, proj, sid).exists(),
        "summary file present before"
    );

    let resp = post(
        state,
        "/admin/purge-session",
        json!({ "id": sid.to_string() }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["session_id"].as_str(), Some(sid.to_string().as_str()));
    assert_eq!(body["observations_deleted"].as_u64(), Some(4));
    assert_eq!(body["handoffs_deleted"].as_u64(), Some(1));
    assert_eq!(body["pages_deleted"].as_u64(), Some(1));
    assert_eq!(body["file_deleted"].as_bool(), Some(true));
    assert!(body["file_failed"].is_null(), "no file failure reported");

    // DB rows gone.
    let obs: u64 = store
        .reader
        .with_conn(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
                [sid.as_bytes()],
                |r| r.get(0),
            )
            .map_err(ai_memory_store::StoreError::from)
        })
        .await
        .unwrap();
    assert_eq!(obs, 0, "observations deleted");

    let session_exists: bool = store
        .reader
        .with_conn(move |conn| {
            let n: u64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                    [sid.as_bytes()],
                    |r| r.get(0),
                )
                .map_err(ai_memory_store::StoreError::from)?;
            Ok(n > 0)
        })
        .await
        .unwrap();
    assert!(!session_exists, "session row deleted");

    let handoffs: u64 = store
        .reader
        .with_conn(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM handoffs WHERE from_session_id = ?1",
                [sid.as_bytes()],
                |r| r.get(0),
            )
            .map_err(ai_memory_store::StoreError::from)
        })
        .await
        .unwrap();
    assert_eq!(handoffs, 0, "session-authored handoff deleted");

    // On-disk file gone.
    assert!(
        !summary_file(&tmp, ws, proj, sid).exists(),
        "summary page file removed from disk"
    );

    // Project + workspace are reaped (orphaned by the session purge).
    let proj_exists: bool = store
        .reader
        .with_conn(move |conn| {
            let n: u64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM projects WHERE id = ?1",
                    [proj.as_bytes()],
                    |r| r.get(0),
                )
                .map_err(ai_memory_store::StoreError::from)?;
            Ok(n > 0)
        })
        .await
        .unwrap();
    assert!(!proj_exists, "orphaned project reaped by session purge");
}

/// A session with no summary page still purges cleanly (no file removal
/// attempted, no error).
#[tokio::test]
async fn purge_session_without_summary_page_is_clean() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "nosummary", None)
        .await
        .unwrap();
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
        })
        .await
        .unwrap();
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
                title: "t".into(),
                body: "b".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/purge-session",
        json!({ "id": sid.to_string() }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["observations_deleted"].as_u64(), Some(1));
    assert_eq!(body["pages_deleted"].as_u64(), Some(0));
    assert_eq!(body["file_deleted"].as_bool(), Some(false));
}

/// Purging the same session twice: the second call must return 404 (the
/// first removal is permanent, not a silent no-op).
#[tokio::test]
async fn purge_session_idempotent_second_call_is_404() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let (_ws, _proj, sid) = seed_session(&store, &state.wiki).await;

    let first = post(
        state,
        "/admin/purge-session",
        json!({ "id": sid.to_string() }),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);

    // Rebuild a fresh AdminState over the same store for the second call.
    let (state2, _store) = make_state(&tmp).await;
    let second = post(
        state2,
        "/admin/purge-session",
        json!({ "id": sid.to_string() }),
    )
    .await;
    assert_eq!(second.status(), StatusCode::NOT_FOUND);
}
