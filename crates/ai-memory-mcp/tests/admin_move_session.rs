//! Integration tests for `POST /admin/move-session` (#402).
//!
//! Same harness as `admin_move.rs`: a real [`AdminState`] over a
//! tmpdir-backed store + wiki, driven through `admin_router` with
//! `tower::ServiceExt::oneshot`. Two scopes, one session with everything a
//! move re-stamps (observations, a handoff it produced, a completed
//! consolidation job) and a `sessions/<id>.md` page written through the wiki
//! so a real file sits on disk.

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
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
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
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };
    (state, store)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn post(state: &AdminState, uri: &str, body: serde_json::Value) -> axum::response::Response {
    let router = admin_router(state.clone());
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    router.oneshot(req).await.unwrap()
}

async fn get(state: &AdminState, uri: &str) -> axum::response::Response {
    let router = admin_router(state.clone());
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    router.oneshot(req).await.unwrap()
}

/// Two projects in workspace `default`, `src` and `dst`, plus a workspace
/// `other` with a project `far` for the cross-workspace case.
struct Scopes {
    ws: WorkspaceId,
    src: ProjectId,
    dst: ProjectId,
    other_ws: WorkspaceId,
    far: ProjectId,
}

async fn seed_scopes(store: &Store) -> Scopes {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let src = store
        .writer
        .get_or_create_project(ws, "src", None)
        .await
        .unwrap();
    let dst = store
        .writer
        .get_or_create_project(ws, "dst", None)
        .await
        .unwrap();
    let other_ws = store.writer.get_or_create_workspace("other").await.unwrap();
    let far = store
        .writer
        .get_or_create_project(other_ws, "far", None)
        .await
        .unwrap();
    Scopes {
        ws,
        src,
        dst,
        other_ws,
        far,
    }
}

/// One ended session in `(ws, proj)` with two observations, a handoff it
/// produced, a completed consolidation job and a `sessions/<id>.md` page
/// written through the wiki (so the file exists on disk).
async fn seed_session(
    store: &Store,
    wiki: &Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
    cwd: &str,
) -> SessionId {
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some(cwd.into()),
            actor_user: None,
        })
        .await
        .unwrap();
    for n in 0..2 {
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
                    title: format!("prompt {n}"),
                    body: "do the thing".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
    }
    store.writer.end_session(sid, None).await.unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: Some(sid),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "handoff from the moved session".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();
    assert!(
        store
            .writer
            .enqueue_session_consolidation(ws, proj, sid)
            .await
            .unwrap()
    );
    let job = store
        .writer
        .claim_session_consolidation(i64::MAX / 2, 0)
        .await
        .unwrap()
        .expect("the queued job must be claimable");
    assert_eq!(job.session_id(), sid);
    store
        .writer
        .complete_session_consolidation(job)
        .await
        .unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: page_path(sid),
        frontmatter: json!({ "title": "Session summary" }),
        body: format!("consolidated body of {sid}"),
        tier: Tier::Episodic,
        pinned: false,
        title: Some("Session summary".into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    })
    .await
    .unwrap();
    sid
}

fn page_path(sid: SessionId) -> PagePath {
    PagePath::new(format!("sessions/{sid}.md")).unwrap()
}

/// `(sessions, observations, handoffs, consolidation_jobs)` rows of `sid`
/// sitting in `(ws, proj)`.
async fn rows_in_scope(
    store: &Store,
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
) -> (i64, i64, i64, i64) {
    // The mcp crate does not depend on rusqlite directly: params go in as
    // plain byte slices and the closure returns the store's error type.
    store
        .reader
        .with_conn(move |conn| {
            let params = [
                sid.as_bytes().as_slice(),
                ws.as_bytes().as_slice(),
                proj.as_bytes().as_slice(),
            ];
            let mut counts = [0i64; 4];
            for (slot, sql) in counts.iter_mut().zip([
                "SELECT COUNT(*) FROM sessions \
                 WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
                "SELECT COUNT(*) FROM observations \
                 WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
                "SELECT COUNT(*) FROM handoffs \
                 WHERE from_session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
                "SELECT COUNT(*) FROM session_consolidation_jobs \
                 WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            ]) {
                *slot = conn.query_row(sql, params, |r| r.get(0))?;
            }
            Ok((counts[0], counts[1], counts[2], counts[3]))
        })
        .await
        .unwrap()
}

async fn read_page(state: &AdminState, ws: &str, project: &str, sid: SessionId) -> StatusCode {
    get(
        state,
        &format!("/admin/read-page?workspace={ws}&project={project}&path=sessions/{sid}.md"),
    )
    .await
    .status()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn move_session_dry_run_reports_counts_and_changes_nothing() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": sid.to_string(), "project": "dst" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], true, "{body}");
    assert_eq!(body["from"]["project"], "src", "{body}");
    assert_eq!(body["to"]["workspace"], "default", "{body}");
    assert_eq!(body["to"]["project"], "dst", "{body}");
    assert_eq!(body["summary"]["observations"], 2, "{body}");
    assert_eq!(body["summary"]["handoffs"], 1, "{body}");
    assert_eq!(body["summary"]["consolidation_jobs"], 1, "{body}");
    assert_eq!(body["summary"]["page_versions_moved"], 1, "{body}");
    assert_eq!(body["page"], "moved", "{body}");
    assert_eq!(body["cwd"], "/repo/src", "{body}");
    assert!(
        body["cwd_warning"]
            .as_str()
            .is_some_and(|w| w.contains("'src'") && w.contains("'dst'")),
        "basename(cwd) = src differs from dst: {body}"
    );
    assert!(body.get("checkpoint").is_none(), "{body}");

    // Nothing moved: rows and file still in the source scope.
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (1, 2, 1, 1)
    );
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.dst).await,
        (0, 0, 0, 0)
    );
    let path = page_path(sid);
    assert!(state.wiki.abs_path(scopes.ws, scopes.src, &path).exists());
    assert!(!state.wiki.abs_path(scopes.ws, scopes.dst, &path).exists());
    assert_eq!(
        read_page(&state, "default", "src", sid).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn move_session_restamps_rows_moves_file_and_checkpoints() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    // cwd basename equals the destination: no warning expected.
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/dst").await;

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": sid.to_string(), "project": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], false, "{body}");
    assert_eq!(body["summary"]["observations"], 2, "{body}");
    assert_eq!(body["summary"]["page_versions_moved"], 1, "{body}");
    assert_eq!(body["page"], "moved", "{body}");
    assert!(body.get("cwd_warning").is_none(), "{body}");
    assert!(
        body["checkpoint"].as_str().is_some(),
        "a real move must checkpoint the wiki: {body}"
    );

    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.dst).await,
        (1, 2, 1, 1)
    );
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (0, 0, 0, 0)
    );
    let path = page_path(sid);
    assert!(!state.wiki.abs_path(scopes.ws, scopes.src, &path).exists());
    assert!(state.wiki.abs_path(scopes.ws, scopes.dst, &path).exists());
    assert_eq!(
        read_page(&state, "default", "dst", sid).await,
        StatusCode::OK
    );
    assert_eq!(
        read_page(&state, "default", "src", sid).await,
        StatusCode::NOT_FOUND
    );
    // The move landed in git history.
    let checkpoints = state.wiki.recent_checkpoints(5).unwrap();
    assert!(
        checkpoints
            .iter()
            .any(|c| c.summary.contains("move-session")),
        "{checkpoints:?}"
    );
}

#[tokio::test]
async fn move_session_across_workspaces_and_regenerate() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;

    let resp = post(
        &state,
        "/admin/move-session",
        json!({
            "session_id": sid.to_string(),
            "workspace": "other",
            "project": "far",
            "pages": "regenerate",
            "confirm": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["to"]["workspace"], "other", "{body}");
    assert_eq!(body["summary"]["pages_regenerated"], 1, "{body}");
    assert_eq!(body["summary"]["page_versions_moved"], 0, "{body}");
    assert_eq!(body["page"], "regenerated", "{body}");

    assert_eq!(
        rows_in_scope(&store, sid, scopes.other_ws, scopes.far).await,
        (1, 2, 1, 1)
    );
    let path = page_path(sid);
    assert!(
        !state.wiki.abs_path(scopes.ws, scopes.src, &path).exists(),
        "retired page file must be removed so the watcher cannot resurrect it"
    );
    assert!(
        !state
            .wiki
            .abs_path(scopes.other_ws, scopes.far, &path)
            .exists()
    );
    assert_eq!(
        read_page(&state, "default", "src", sid).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn move_session_page_collision_returns_409_and_leaves_source_intact() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;
    // A latest page at the same path in the destination, DB row only, so the
    // wiki's file step succeeds and the store refuses (PagePathTaken).
    store
        .writer
        .upsert_page(ai_memory_core::NewPage {
            workspace_id: scopes.ws,
            project_id: scopes.dst,
            path: page_path(sid),
            title: "taken".into(),
            body: "already here".into(),
            tier: Tier::Semantic,
            frontmatter_json: json!({}),
            pinned: false,
            links: vec![],
            author_id: None,
            expires_at: None,
            entities: vec![],
        })
        .await
        .unwrap();

    for confirm in [false, true] {
        let resp = post(
            &state,
            "/admin/move-session",
            json!({ "session_id": sid.to_string(), "project": "dst", "confirm": confirm }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "confirm={confirm}");
        let body = body_json(resp).await;
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|e| e.contains("regenerate")),
            "{body}"
        );
    }
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (1, 2, 1, 1)
    );
    let path = page_path(sid);
    assert!(
        state.wiki.abs_path(scopes.ws, scopes.src, &path).exists(),
        "the file must be back in the source after the store refused"
    );
    assert!(!state.wiki.abs_path(scopes.ws, scopes.dst, &path).exists());

    // Regenerate side-steps the collision.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({
            "session_id": sid.to_string(),
            "project": "dst",
            "pages": "regenerate",
            "confirm": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.dst).await,
        (1, 2, 1, 1)
    );
}

#[tokio::test]
async fn move_session_pending_job_or_open_session_needs_force() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;
    // A second generation waiting in the queue.
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid,
                workspace_id: scopes.ws,
                project_id: scopes.src,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "late prompt".into(),
                body: "one more".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    assert!(
        store
            .writer
            .enqueue_session_consolidation(scopes.ws, scopes.src, sid)
            .await
            .unwrap()
    );

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": sid.to_string(), "project": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("consolidation job")),
        "{body}"
    );
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (1, 3, 1, 2)
    );

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": sid.to_string(), "project": "dst", "confirm": true, "force": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.dst).await,
        (1, 3, 1, 2)
    );

    // An open session (no session end) is refused the same way.
    let open = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: open,
            workspace_id: scopes.ws,
            project_id: scopes.src,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": open.to_string(), "project": "dst" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("still open")),
        "{body}"
    );
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": open.to_string(), "project": "dst", "force": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "dry run with force passes the guard"
    );
}

#[tokio::test]
async fn move_session_batch_from_project_empties_the_source() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let a = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;
    let b = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;

    // Dry run first: counts, nothing written.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], true, "{body}");
    assert_eq!(body["total"], 2, "{body}");
    assert_eq!(body["moved"], 2, "{body}");
    assert_eq!(body["sessions"].as_array().map(Vec::len), Some(2), "{body}");
    assert_eq!(
        rows_in_scope(&store, a, scopes.ws, scopes.src).await,
        (1, 2, 1, 1)
    );

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], false, "{body}");
    assert_eq!(body["moved"], 2, "{body}");
    assert!(body["checkpoint"].as_str().is_some(), "{body}");
    for sid in [a, b] {
        assert_eq!(
            rows_in_scope(&store, sid, scopes.ws, scopes.dst).await,
            (1, 2, 1, 1)
        );
        assert_eq!(
            rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
            (0, 0, 0, 0)
        );
        assert!(
            state
                .wiki
                .abs_path(scopes.ws, scopes.dst, &page_path(sid))
                .exists()
        );
        assert_eq!(
            read_page(&state, "default", "dst", sid).await,
            StatusCode::OK
        );
    }
    // Source is empty now: a second batch has nothing to do.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["total"], 0, "{body}");
}

#[tokio::test]
async fn move_session_batch_stops_at_first_error_and_reports_progress() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let first = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;
    // Second session is still open: refused without force.
    let open = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: open,
            workspace_id: scopes.ws,
            project_id: scopes.src,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["session_id"], open.to_string(), "{body}");
    assert_eq!(body["total"], 2, "{body}");
    assert_eq!(body["moved"], 1, "{body}");
    assert_eq!(body["sessions"].as_array().map(Vec::len), Some(1), "{body}");
    // The first session stayed moved (its own transaction), the open one did not.
    assert_eq!(
        rows_in_scope(&store, first, scopes.ws, scopes.dst).await,
        (1, 2, 1, 1)
    );
    assert_eq!(
        rows_in_scope(&store, open, scopes.ws, scopes.src).await.0,
        1
    );
}

#[tokio::test]
async fn move_session_target_404_unless_create() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": sid.to_string(), "project": "brand-new", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (1, 2, 1, 1)
    );

    // Dry run with create: the full plan is reported, nothing is created.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({
            "session_id": sid.to_string(),
            "workspace": "fresh-ws",
            "project": "brand-new",
            "create": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], true, "{body}");
    assert_eq!(body["would_create_project"], true, "{body}");
    assert_eq!(body["session_moved"], true, "{body}");
    assert_eq!(body["summary"]["observations"], 2, "{body}");
    assert_eq!(body["summary"]["handoffs"], 1, "{body}");
    assert_eq!(body["summary"]["consolidation_jobs"], 1, "{body}");
    assert_eq!(body["summary"]["page_versions_moved"], 1, "{body}");
    assert_eq!(body["page"], "moved", "{body}");
    assert!(
        store
            .reader
            .find_workspace("fresh-ws".to_string())
            .await
            .unwrap()
            .is_none(),
        "a dry run must not create the workspace"
    );
    let dst_projects: i64 = store
        .reader
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM projects WHERE name = 'brand-new'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(dst_projects, 0, "a dry run must not create the project row");
    // Batch dry run with create behaves the same.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "brand-new", "create": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["would_create_project"], true, "{body}");
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["sessions"][0]["would_create_project"], true, "{body}");
    assert!(
        store
            .reader
            .find_project(scopes.ws, "brand-new".to_string())
            .await
            .unwrap()
            .is_none()
    );

    let resp = post(
        &state,
        "/admin/move-session",
        json!({
            "session_id": sid.to_string(),
            "workspace": "fresh-ws",
            "project": "brand-new",
            "create": true,
            "confirm": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["to"]["workspace"], "fresh-ws", "{body}");
    assert!(body.get("would_create_project").is_none(), "{body}");
    let new_ws = store
        .reader
        .find_workspace("fresh-ws".to_string())
        .await
        .unwrap()
        .expect("workspace created");
    let new_proj = store
        .reader
        .find_project(new_ws, "brand-new".to_string())
        .await
        .unwrap()
        .expect("project created");
    assert_eq!(
        rows_in_scope(&store, sid, new_ws, new_proj).await,
        (1, 2, 1, 1)
    );
    assert!(
        state
            .wiki
            .abs_path(new_ws, new_proj, &page_path(sid))
            .exists()
    );
    // The new project dir got its scope manifest.
    assert!(
        state
            .wiki
            .project_root(new_ws, new_proj)
            .join("_meta.md")
            .exists()
    );
    assert_eq!(
        read_page(&state, "fresh-ws", "brand-new", sid).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn move_session_rejects_bad_shapes() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.src, "/repo/src").await;

    // Neither session_id nor from_project.
    let resp = post(&state, "/admin/move-session", json!({ "project": "dst" })).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Both at once.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": sid.to_string(), "from_project": "src", "project": "dst" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Unknown session.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": SessionId::new().to_string(), "project": "dst" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // Same scope in the single form is a re-home, not an error: nothing is
    // stray here, so every count is zero and the row stays.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "session_id": sid.to_string(), "project": "src", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["session_moved"], false, "{body}");
    assert_eq!(body["summary"]["observations"], 0, "{body}");
    assert_eq!(body["page"], "already in destination", "{body}");
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (1, 2, 1, 1)
    );
    // Same scope in the batch form stays a 422.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "src" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Scatter `n` observations of `sid` into `(ws, proj)` the way pre-sticky
/// mid-session routing did: rows stamped into a project the session row
/// never belonged to.
async fn scatter_observations(
    store: &Store,
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
    n: usize,
) {
    for i in 0..n {
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id: sid,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::PostToolUse,
                    extension: None,
                    source_event: None,
                    title: format!("stray {i}"),
                    body: "landed in the wrong bucket".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn move_session_batch_empties_phantom_bucket_with_only_observations() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    // The session is rooted in `dst` (row, 2 observations, handoff, job,
    // page all there); 4 of its observations landed in `src`, which has no
    // `sessions` row at all: the phantom bucket.
    let sid = seed_session(&store, &state.wiki, scopes.ws, scopes.dst, "/repo/dst").await;
    scatter_observations(&store, sid, scopes.ws, scopes.src, 4).await;
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (0, 4, 0, 0)
    );

    // Dry run sees the session through its observations and reports them.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], true, "{body}");
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["moved"], 1, "{body}");
    let s = &body["sessions"][0];
    assert_eq!(s["session_id"], sid.to_string(), "{body}");
    assert_eq!(s["session_moved"], false, "{body}");
    assert_eq!(s["summary"]["observations"], 4, "{body}");
    assert_eq!(s["summary"]["handoffs"], 0, "{body}");
    assert_eq!(s["page"], "already in destination", "{body}");
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (0, 4, 0, 0)
    );

    // Confirm: the strays are gathered, the bucket is empty, the row and the
    // page never moved.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], false, "{body}");
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["moved"], 1, "{body}");
    assert_eq!(body["sessions"][0]["session_moved"], false, "{body}");
    assert_eq!(body["sessions"][0]["summary"]["observations"], 4, "{body}");
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.src).await,
        (0, 0, 0, 0)
    );
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.dst).await,
        (1, 6, 1, 1)
    );
    assert!(
        state
            .wiki
            .abs_path(scopes.ws, scopes.dst, &page_path(sid))
            .exists()
    );
    assert_eq!(
        read_page(&state, "default", "dst", sid).await,
        StatusCode::OK
    );
    // Nothing left touching the bucket.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst" }),
    )
    .await;
    assert_eq!(body_json(resp).await["total"], 0);
}

/// An open session whose row already sits in the destination is re-homed
/// without `force`: the row does not move, so the open-session guard does not
/// apply (a pending job still does).
#[tokio::test]
async fn move_session_rehome_of_open_session_needs_no_force() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let scopes = seed_scopes(&store).await;
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: scopes.ws,
            project_id: scopes.dst,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some("/repo/dst".into()),
            actor_user: None,
        })
        .await
        .unwrap();
    scatter_observations(&store, sid, scopes.ws, scopes.src, 2).await;

    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "src", "project": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["moved"], 1, "{body}");
    assert_eq!(body["sessions"][0]["session_moved"], false, "{body}");
    assert_eq!(body["sessions"][0]["summary"]["observations"], 2, "{body}");
    assert_eq!(body["sessions"][0]["page"], "none", "{body}");
    assert_eq!(
        rows_in_scope(&store, sid, scopes.ws, scopes.dst).await,
        (1, 2, 0, 0)
    );

    // The same open session, batch-moved out of its own scope, still needs
    // force: the row would move.
    let resp = post(
        &state,
        "/admin/move-session",
        json!({ "from_project": "dst", "project": "src", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}
