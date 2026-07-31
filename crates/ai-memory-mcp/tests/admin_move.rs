//! Integration tests for `POST /admin/move-project`.
//!
//! Follows the same pattern as `admin_rename.rs` / `admin_purge.rs`: build
//! a real [`AdminState`] over a tmpdir-backed store + wiki, drive the
//! router with `tower::ServiceExt::oneshot`.
//!
//! move-project has two paths. To a FRESH destination (no same-named project)
//! it does a lossless TRUE-MOVE: re-stamp the project's workspace_id in place,
//! keeping the same project_id, sessions, observations, handoffs, page history
//! and embeddings. To an EXISTING same-named project it does a COPY-PURGE
//! merge: copy the source's latest pages in through the normal write path
//! (carrying embeddings over verbatim, de-duplicating any conflicting path so
//! both versions survive), then purge the source — there the episodic rows
//! (sessions/observations/handoffs) are dropped by the purge.

use ai_memory_core::{AgentKind, PagePath, Sanitized, Sanitizer, Tier};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, PrepareWorkstreamRun, Store, WorkstreamSelection};
use ai_memory_wiki::{
    AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki, WritePageRequest,
};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post as axum_post;
use axum::{Json, Router};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
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

async fn make_state_with_chain(tmp: &TempDir, chain: AdmissionChain) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_admission_chain(chain)
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

/// Seed `<ws>/<project>/<path>` with one page carrying `body`.
async fn seed_page(store: &Store, wiki: &Wiki, ws: &str, project: &str, path: &str, body: &str) {
    seed_page_with_metadata(
        store,
        wiki,
        ws,
        project,
        path,
        body,
        SeedPageOptions {
            frontmatter: serde_json::json!({"title": path}),
            title: Some(path.into()),
            tier: Tier::Semantic,
            pinned: false,
        },
    )
    .await;
}

struct SeedPageOptions {
    frontmatter: serde_json::Value,
    title: Option<String>,
    tier: Tier,
    pinned: bool,
}

async fn seed_page_with_metadata(
    store: &Store,
    wiki: &Wiki,
    ws: &str,
    project: &str,
    path: &str,
    body: &str,
    options: SeedPageOptions,
) {
    let ws_id = store.writer.get_or_create_workspace(ws).await.unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws_id, project, None)
        .await
        .unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws_id,
        project_id: proj,
        path: PagePath::new(path.to_string()).unwrap(),
        frontmatter: options.frontmatter,
        body: body.to_string(),
        tier: options.tier,
        pinned: options.pinned,
        title: options.title,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    })
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path with a FRESH destination (no same-named project there): this is
/// a lossless TRUE MOVE — the project_id is re-stamped into `dst`, nothing is
/// purged, and the on-disk dir is renamed. Assert the pages now live under
/// `dst/proj`, the source name no longer resolves, and the dir moved.
#[tokio::test]
async fn move_project_true_move_into_fresh_dest() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "decisions/0001.md",
        "decision body",
    )
    .await;
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "gotchas/x.md",
        "see [[decisions/0001]] for the call",
    )
    .await;

    // Capture the source on-disk dir before `state` is consumed by `post`.
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .expect("src workspace exists");
    let src_proj = store
        .reader
        .find_project(src_ws, "proj".to_string())
        .await
        .unwrap()
        .expect("src project exists");
    let managed = store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            workspace_id: src_ws,
            project_id: src_proj,
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
    let src_dir = state.wiki.project_root(src_ws, src_proj);
    assert!(src_dir.exists(), "source dir must exist before move");

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "move must succeed");

    let body = body_json(resp).await;
    assert_eq!(body["pages_copied"].as_u64().unwrap_or(0), 2, "{body}");
    assert_eq!(body["workstreams_moved"].as_u64().unwrap_or(0), 1, "{body}");
    assert_eq!(body["moved_via"], "true-move", "{body}");
    // Nothing is purged in a true move — the rows are re-stamped, not copied.
    assert_eq!(body["source_purged"], false, "{body}");
    assert_eq!(body["merged_into_existing"], false, "{body}");

    // project_id is preserved: the dst project IS the former src project.
    let dst_ws_check = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    let dst_proj_check = store
        .reader
        .find_project(dst_ws_check, "proj".to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        dst_proj_check, src_proj,
        "true move keeps the same project_id"
    );
    assert!(
        store
            .reader
            .managed_run_status(managed.run_id)
            .await
            .unwrap()
            .is_some(),
        "the existing managed run must remain attached after the move"
    );

    // Both pages now belong to dst/proj (latest), content preserved.
    let dst_pages = store.reader.list_pages("dst", "proj").await.unwrap();
    let mut dst_paths: Vec<String> = dst_pages.into_iter().map(|p| p.path).collect();
    dst_paths.sort();
    assert_eq!(dst_paths, vec!["decisions/0001.md", "gotchas/x.md"]);

    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .expect("dst workspace exists");
    let dst_proj = store
        .reader
        .find_project(dst_ws, "proj".to_string())
        .await
        .unwrap()
        .expect("dst project exists");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let read = wiki
        .read_page(
            dst_ws,
            dst_proj,
            &PagePath::new("decisions/0001.md".to_string()).unwrap(),
        )
        .unwrap();
    assert!(
        read.body.contains("decision body"),
        "moved body must round-trip; got {:?}",
        read.body
    );

    // Source project row and on-disk dir are gone.
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_none(),
        "source project row must be gone"
    );
    assert!(!src_dir.exists(), "source dir must be removed after move");
}

/// Without `confirm: true` the server returns 400 and leaves the source intact.
#[tokio::test]
async fn move_project_requires_confirm() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": false }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Source untouched.
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_some(),
        "source project must still exist after a rejected move"
    );
}

/// A move from a nonexistent source project returns 404.
#[tokio::test]
async fn move_project_404_on_unknown_source() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "nope", "project": "ghost", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Moving into a workspace that already has a same-named project MERGES:
/// the destination ends up with both the pre-existing and the moved pages.
#[tokio::test]
async fn move_project_merges_into_existing_dest() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/b.md", "body b").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert_eq!(body["merged_into_existing"], true, "{body}");
    // A same-named project in the dest forces copy+purge (can't re-stamp two
    // project_ids into one).
    assert_eq!(body["moved_via"], "copy-purge", "{body}");
    assert_eq!(body["source_purged"], true, "{body}");

    // Destination holds BOTH the pre-existing and the moved page.
    let mut dst_paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    dst_paths.sort();
    assert_eq!(dst_paths, vec!["notes/a.md", "notes/b.md"]);

    // Source gone.
    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_none(),
        "source project must be purged after merge-move"
    );
}

/// A same-workspace move is rejected with 422 (use rename-project instead).
#[tokio::test]
async fn move_project_same_workspace_rejected() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "w", "project": "proj", "to_workspace": "w", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// The move carries the source page's existing embedding over instead of
/// recomputing it. Proven by overwriting the source embedding with a
/// recognisable marker vector: if the move re-embedded, the destination would
/// hold the synthetic bag-of-words vector, not the marker.
#[tokio::test]
async fn move_project_carries_source_embedding() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let embedder: std::sync::Arc<dyn ai_memory_llm::Embedder> =
        std::sync::Arc::new(ai_memory_llm::SyntheticEmbedder::new(8));
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(embedder.clone());
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: Some(embedder),
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

    // Seed source page (gets a synthetic embedding on write).
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "notes/a.md",
        "hello world embed",
    )
    .await;

    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    let src_proj = store
        .reader
        .find_project(src_ws, "proj".to_string())
        .await
        .unwrap()
        .unwrap();
    let src = store
        .reader
        .load_embeddings(
            src_ws,
            src_proj,
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();
    assert_eq!(src.len(), 1, "source page must have an embedding to carry");

    // Overwrite with a recognisable marker vector.
    let marker: Vec<f32> = vec![9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0];
    store
        .writer
        .store_embedding(
            src[0].id,
            ai_memory_store::f32_vec_to_bytes(&marker),
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The destination page must carry the MARKER (not a recomputed vector).
    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    let dst_proj = store
        .reader
        .find_project(dst_ws, "proj".to_string())
        .await
        .unwrap()
        .unwrap();
    let dst = store
        .reader
        .load_embeddings(
            dst_ws,
            dst_proj,
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();
    assert_eq!(dst.len(), 1, "dest page must carry an embedding");
    for (got, want) in dst[0].vector.iter().zip(marker.iter()) {
        assert!(
            (got - want).abs() < 1e-4,
            "carried embedding must equal the source marker (not a recompute); got {:?}",
            dst[0].vector
        );
    }
}

/// The whole point of the true move over copy+purge: a session and its
/// observation — episodic rows that copy+purge would DROP — survive a move
/// into a fresh destination, re-stamped to the new workspace.
#[tokio::test]
async fn move_project_true_move_preserves_sessions_and_observations() {
    use ai_memory_core::{AgentKind, NewObservation, NewSession, ObservationKind, SessionId};

    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let src_ws = store
        .reader
        .find_workspace("src".to_string())
        .await
        .unwrap()
        .unwrap();
    let src_proj = store
        .reader
        .find_project(src_ws, "proj".to_string())
        .await
        .unwrap()
        .unwrap();

    // Seed an episodic session + observation under src/proj.
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: src_ws,
            project_id: src_proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid,
                workspace_id: src_ws,
                project_id: src_proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "do the thing".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["moved_via"], "true-move", "{body}");
    assert_eq!(body["source_purged"], false, "{body}");

    // The session followed the project to the new workspace (same session_id,
    // same project_id, new workspace_id).
    let dst_ws = store
        .reader
        .find_workspace("dst".to_string())
        .await
        .unwrap()
        .unwrap();
    let (sess_ws, sess_proj) = store
        .reader
        .session_project_ids(sid)
        .await
        .unwrap()
        .expect("session must still exist after the move");
    assert_eq!(sess_ws, dst_ws, "session re-stamped to dst workspace");
    assert_eq!(sess_proj, src_proj, "session keeps its project_id");

    // The observation survived and re-stamped too.
    let obs = store.reader.observations_for_session(sid).await.unwrap();
    assert_eq!(obs.len(), 1, "observation must survive the move");
    assert_eq!(obs[0].workspace_id, dst_ws, "observation re-stamped to dst");
}

#[tokio::test]
async fn true_move_stale_source_write_fails_before_creating_file() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let (src_ws, src_proj) = ids(&store, "src", "proj").await;
    let stale_wiki = state.wiki.clone();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let stale_path = PagePath::new("notes/stale.md".to_string()).unwrap();
    let err = stale_wiki
        .write_page(WritePageRequest {
            workspace_id: src_ws,
            project_id: src_proj,
            path: stale_path.clone(),
            frontmatter: serde_json::json!({"title": "stale"}),
            body: "must not land".to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("stale".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("does not belong to workspace"),
        "stale write should fail at the pair validator: {err}"
    );
    assert!(
        !stale_wiki.abs_path(src_ws, src_proj, &stale_path).exists(),
        "stale write must not create an orphan file under the old workspace"
    );
}

#[tokio::test]
async fn true_move_notifies_admission_with_destination_names() {
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_for_route = seen.clone();
    let app = Router::new().route(
        "/hook",
        axum_post(
            move |headers: HeaderMap, Json(payload): Json<serde_json::Value>| {
                let seen = seen_for_route.clone();
                async move {
                    let mut payload = payload;
                    payload["op_header"] = headers
                        .get("X-Memory-Op")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .into();
                    seen.lock().unwrap().push(payload);
                    StatusCode::NO_CONTENT
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::MoveProject],
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
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let rec = seen.lock().unwrap();
    assert_eq!(rec.len(), 1, "move_project admission must fire once");
    assert_eq!(rec[0]["op_header"], "move_project");
    assert_eq!(rec[0]["ctx"]["workspace"], "src");
    assert_eq!(rec[0]["ctx"]["project"], "proj");
    assert_eq!(rec[0]["ctx"]["destination_workspace"], "dst");
    assert_eq!(rec[0]["ctx"]["destination_project"], "proj");
    assert_eq!(rec[0]["page"]["path"], "");
}

// ---------------------------------------------------------------------------
// Rework (PR #60 review): failure model, live-session guard, copy-purge
// conflict/partial/idempotency, copy-purge embedding carry-over.
// ---------------------------------------------------------------------------

/// Build an AdminState with a synthetic embedder (for copy-purge embedding
/// carry-over) and a published active project pointer.
async fn make_state_with_embedder(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let embedder: std::sync::Arc<dyn ai_memory_llm::Embedder> =
        std::sync::Arc::new(ai_memory_llm::SyntheticEmbedder::new(8));
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(embedder.clone());
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: Some(embedder),
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

async fn ids(
    store: &Store,
    ws: &str,
    proj: &str,
) -> (ai_memory_core::WorkspaceId, ai_memory_core::ProjectId) {
    let ws_id = store
        .reader
        .find_workspace(ws.to_string())
        .await
        .unwrap()
        .unwrap();
    let proj_id = store
        .reader
        .find_project(ws_id, proj.to_string())
        .await
        .unwrap()
        .unwrap();
    (ws_id, proj_id)
}

/// W1: when the on-disk dir rename fails mid true-move, the SQL re-stamp is
/// rolled back so NOTHING moves (no DB-ahead-of-disk split-brain). We force the
/// rename to fail by planting a FILE where the destination workspace dir would
/// be created.
#[tokio::test]
async fn true_move_rolls_back_when_dir_rename_fails() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    // Pre-create the destination workspace, then plant a FILE at its wiki dir
    // so `create_dir_all` during the rename fails. The pre-check passes because
    // the project subdir cannot exist under a file.
    let dst_ws = store
        .writer
        .get_or_create_workspace("dst".to_string())
        .await
        .unwrap();
    let dst_ws_dir = tmp.path().join("wiki").join(dst_ws.to_string());
    std::fs::write(&dst_ws_dir, b"not a dir").unwrap();

    let (src_ws, src_proj) = ids(&store, "src", "proj").await;
    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("nothing changed"),
        "{body}"
    );

    // The project must still live in the SOURCE workspace (rollback worked).
    let still_src = store
        .reader
        .find_project(src_ws, "proj".to_string())
        .await
        .unwrap();
    assert_eq!(
        still_src,
        Some(src_proj),
        "project rolled back to source ws"
    );
    assert!(
        store
            .reader
            .find_project(dst_ws, "proj".to_string())
            .await
            .unwrap()
            .is_none(),
        "destination must hold no project after rollback"
    );
}

/// W3: moving the project the hook router has published as ACTIVE is refused
/// (409) without `force`.
#[tokio::test]
async fn move_refuses_active_project_without_force() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    let (src_ws, src_proj) = ids(&store, "src", "proj").await;
    state.active_project.set(src_ws, src_proj);

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

/// W3: `force: true` overrides the active-project guard, and the move
/// republishes the active pointer under the destination workspace.
#[tokio::test]
async fn move_active_project_with_force_succeeds_and_republishes() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    let (src_ws, src_proj) = ids(&store, "src", "proj").await;
    let active = state.active_project.clone();
    active.set(src_ws, src_proj);

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst",
                "confirm": true, "force": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The active pointer now points at the SAME project in the destination ws.
    let (dst_ws, _) = ids(&store, "dst", "proj").await;
    assert_eq!(
        active.get(),
        Some((dst_ws, src_proj)),
        "active republished to dst"
    );
}

#[tokio::test]
async fn copy_purge_force_never_deletes_a_live_managed_workstream() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "source").await;
    seed_page(
        &store,
        &state.wiki,
        "dst",
        "proj",
        "notes/keep.md",
        "destination",
    )
    .await;
    let (src_ws, src_proj) = ids(&store, "src", "proj").await;
    state.active_project.set(src_ws, src_proj);
    let prepared = store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            workspace_id: src_ws,
            project_id: src_proj,
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

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "src",
            "project": "proj",
            "to_workspace": "dst",
            "confirm": true,
            "force": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("--force cannot delete a live managed workstream"),
        "the force boundary must be explicit: {body}"
    );
    assert_eq!(
        store
            .reader
            .find_project(src_ws, "proj".into())
            .await
            .unwrap(),
        Some(src_proj),
        "the source must remain intact"
    );
    assert!(
        store
            .reader
            .managed_run_status(prepared.run_id)
            .await
            .unwrap()
            .is_some(),
        "the managed lease row must survive"
    );
}

/// Build an AdminState over an already-open store (lets one test drive two
/// routers — e.g. a move then a re-run — against the same SQLite file).
fn build_state(store: &Store, tmp: &TempDir) -> AdminState {
    AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: Wiki::new(tmp.path(), store.writer.clone()).unwrap(),
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
    }
}

/// on_conflict=duplicate: a same-path conflict de-duplicates the source page so
/// BOTH versions survive, and the remap is reported.
#[tokio::test]
async fn copy_purge_conflict_duplicates_keeping_both() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body SRC").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/a.md", "body DST").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst",
                "confirm": true, "on_conflict": "duplicate" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["moved_via"], "copy-purge", "{body}");

    let conflicts = body["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{body}");
    assert_eq!(conflicts[0]["path"], "notes/a.md");
    assert_eq!(conflicts[0]["moved_to"], "notes/a-from-src.md");

    let mut paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["notes/a-from-src.md", "notes/a.md"]);

    let (dw, dp) = ids(&store, "dst", "proj").await;
    let kept = store
        .reader
        .page_body_by_ids(dw, dp, "notes/a.md")
        .await
        .unwrap()
        .unwrap();
    let moved = store
        .reader
        .page_body_by_ids(dw, dp, "notes/a-from-src.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.body, "body DST", "destination's original page is kept");
    assert_eq!(
        moved.body, "body SRC",
        "source page lands under the de-duped path"
    );
}

/// on_conflict=block (the DEFAULT): a same-path conflict aborts the whole move
/// with 409, lists the conflicting paths, and leaves the source intact —
/// nothing is copied to the destination.
#[tokio::test]
async fn copy_purge_conflict_blocks_by_default() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body SRC").await;
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "notes/clean.md",
        "only in src",
    )
    .await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/a.md", "body DST").await;

    // No on_conflict → defaults to block.
    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    let conflicts = body["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{body}");
    assert_eq!(conflicts[0], "notes/a.md");

    // Source untouched (NOT purged) and the destination got NONE of the pages.
    let (src_ws, _) = ids(&store, "src", "proj").await;
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_some(),
        "block must leave the source intact"
    );
    let dst_paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    assert_eq!(
        dst_paths,
        vec!["notes/a.md"],
        "destination unchanged — nothing copied"
    );
}

/// `on_conflict=block` must treat metadata differences as conflicts too. A
/// same body/title/frontmatter with a different pinned bit would otherwise be
/// silently overwritten by the copy path.
#[tokio::test]
async fn copy_purge_conflict_blocks_metadata_difference_by_default() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let fm = serde_json::json!({"title": "same"});
    seed_page_with_metadata(
        &store,
        &state.wiki,
        "src",
        "proj",
        "notes/a.md",
        "same body",
        SeedPageOptions {
            frontmatter: fm.clone(),
            title: Some("same".into()),
            tier: Tier::Semantic,
            pinned: false,
        },
    )
    .await;
    seed_page_with_metadata(
        &store,
        &state.wiki,
        "dst",
        "proj",
        "notes/a.md",
        "same body",
        SeedPageOptions {
            frontmatter: fm,
            title: Some("same".into()),
            tier: Tier::Semantic,
            pinned: true,
        },
    )
    .await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["conflicts"], json!(["notes/a.md"]), "{body}");

    let (src_ws, _) = ids(&store, "src", "proj").await;
    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_some(),
        "metadata conflict must leave the source intact"
    );
}

/// on_conflict=overwrite: the source page supersedes the destination page at the
/// same path; the destination ends with the source's content and no duplicate.
#[tokio::test]
async fn copy_purge_conflict_overwrites() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body SRC").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/a.md", "body DST").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst",
                "confirm": true, "on_conflict": "overwrite" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["source_purged"], true, "{body}");
    // Conflict reported, mapped to the same path (supersede, not duplicate).
    let conflicts = body["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{body}");
    assert_eq!(conflicts[0]["path"], "notes/a.md");
    assert_eq!(conflicts[0]["moved_to"], "notes/a.md");

    // Destination has exactly ONE page at the path, carrying the SOURCE content.
    let dst_paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    assert_eq!(dst_paths, vec!["notes/a.md"], "no duplicate path");
    let (dw, dp) = ids(&store, "dst", "proj").await;
    let page = store
        .reader
        .page_body_by_ids(dw, dp, "notes/a.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.body, "body SRC", "source superseded the destination");
}

/// on_conflict=duplicate edge case: a source page whose path collides with the
/// destination AND another source page whose natural path equals the first's
/// de-duplicated target. All three distinct contents must survive — the natural
/// path must not clobber the slot the de-duplication already claimed.
#[tokio::test]
async fn copy_purge_duplicate_avoids_clobbering_dedup_slot() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    // `notes/dup.md` conflicts with the destination; its dedup target is
    // `notes/dup-from-src.md` — which ALSO exists as a source page.
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "notes/dup.md",
        "SRC-dup",
    )
    .await;
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "notes/dup-from-src.md",
        "SRC-natural",
    )
    .await;
    seed_page(
        &store,
        &state.wiki,
        "dst",
        "proj",
        "notes/dup.md",
        "DST-dup",
    )
    .await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst",
                "confirm": true, "on_conflict": "duplicate" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // All three distinct contents survive at three distinct paths (nothing
    // clobbered). Assert as a set so the test is order-independent.
    let (dw, dp) = ids(&store, "dst", "proj").await;
    let paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    assert_eq!(
        paths.len(),
        3,
        "three distinct destination paths: {paths:?}"
    );
    let mut bodies: Vec<String> = Vec::new();
    for p in &paths {
        bodies.push(
            store
                .reader
                .page_body_by_ids(dw, dp, p)
                .await
                .unwrap()
                .unwrap()
                .body
                .trim()
                .to_string(),
        );
    }
    bodies.sort();
    assert_eq!(
        bodies,
        vec!["DST-dup", "SRC-dup", "SRC-natural"],
        "every version survives; none clobbered"
    );
}

/// W6: the copy-purge (merge) path carries the source embedding verbatim onto
/// the new destination page id — not a recompute. (The existing embedding test
/// exercised only the true-move path.)
#[tokio::test]
async fn copy_purge_carries_source_embedding() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state_with_embedder(&tmp).await;
    seed_page(
        &store,
        &state.wiki,
        "src",
        "proj",
        "notes/a.md",
        "hello world embed",
    )
    .await;
    // A pre-existing same-named dst project forces the copy-purge path.
    seed_page(&store, &state.wiki, "dst", "proj", "notes/keep.md", "keep").await;

    let (src_ws, src_proj) = ids(&store, "src", "proj").await;
    let src = store
        .reader
        .load_embeddings(
            src_ws,
            src_proj,
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();
    assert_eq!(src.len(), 1);
    let marker: Vec<f32> = vec![9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0];
    store
        .writer
        .store_embedding(
            src[0].id,
            ai_memory_store::f32_vec_to_bytes(&marker),
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["moved_via"], "copy-purge", "{body}");

    let (dst_ws, dst_proj) = ids(&store, "dst", "proj").await;
    let dst = store
        .reader
        .load_embeddings(
            dst_ws,
            dst_proj,
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            8,
        )
        .await
        .unwrap();
    let moved = dst
        .iter()
        .find(|e| e.path.as_str() == "notes/a.md")
        .expect("moved page embedding");
    for (got, want) in moved.vector.iter().zip(marker.iter()) {
        assert!(
            (got - want).abs() < 1e-4,
            "carried marker, got {:?}",
            moved.vector
        );
    }
}

/// W6: an unreadable source page is skipped and the source is NOT purged, so a
/// fixed re-run is safe; the readable pages still land at the destination.
#[tokio::test]
async fn copy_purge_partial_skips_and_preserves_source() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/ok.md", "ok").await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/bad.md", "bad").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/keep.md", "keep").await;

    let (src_ws, src_proj) = ids(&store, "src", "proj").await;
    let bad = tmp
        .path()
        .join("wiki")
        .join(src_ws.to_string())
        .join(src_proj.to_string())
        .join("notes/bad.md");
    std::fs::remove_file(&bad).unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["source_purged"], false, "{body}");
    let skipped = body["pages_skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{body}");
    assert_eq!(skipped[0], "notes/bad.md");

    assert!(
        store
            .reader
            .find_project(src_ws, "proj".to_string())
            .await
            .unwrap()
            .is_some(),
        "source must NOT be purged when a page was skipped"
    );
    let dst_paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    assert!(
        dst_paths.contains(&"notes/ok.md".to_string()),
        "readable page copied"
    );
}

/// Reject-policy webhook returning 5xx must abort a true-move (cross-workspace
/// rename) before any DB or filesystem change. The original test only used a
/// 204-returning webhook, which never exercises the reject path.
#[tokio::test]
async fn true_move_aborts_when_admission_rejects() {
    let app = Router::new().route(
        "/hook",
        axum_post(
            |_headers: HeaderMap, Json(_payload): Json<serde_json::Value>| async move {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "mirror refuses the move".to_string(),
                )
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "rejecting-mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::MoveProject],
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
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "reject-policy webhook returning 5xx must abort the move"
    );

    // Source rows must be intact — admission ran before any destructive work.
    let src_pages = store.reader.list_pages("src", "proj").await.unwrap();
    assert_eq!(
        src_pages.len(),
        1,
        "source pages must survive when admission rejects the move"
    );
}

/// Audit BLOCKING #2 regression guard: copy-purge merge must run the purge
/// admission BEFORE writer.purge_project so a reject-policy webhook can still
/// abort the source-side destruction. The prior code ordered admit AFTER the
/// SQL purge — by the time admit ran the rows were gone and reject came too
/// late. Reject-on-purge here must leave source rows intact.
#[tokio::test]
async fn copy_purge_purge_admission_runs_before_db_destruction() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let purge_hits = std::sync::Arc::new(AtomicUsize::new(0));
    let purge_hits_for_route = purge_hits.clone();
    let app = Router::new().route(
        "/hook",
        axum_post(
            move |headers: HeaderMap, Json(_payload): Json<serde_json::Value>| {
                let purge_hits = purge_hits_for_route.clone();
                async move {
                    let op = headers
                        .get("X-Memory-Op")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    // Accept move_project so the copy leg can land; reject the
                    // PurgeProject step so the source SQL purge must not run.
                    match op {
                        "purge_project" => {
                            purge_hits.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::INTERNAL_SERVER_ERROR, "no purging".to_string())
                        }
                        _ => (StatusCode::NO_CONTENT, String::new()),
                    }
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::MoveProject, AdmissionOp::PurgeProject],
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
    // Pre-seed BOTH sides so move-project takes the copy-purge path (merge
    // into existing dst/proj), not the lossless true-move path.
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/keep.md", "keep").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "purge admission rejecting must surface as 500 from the move handler"
    );
    assert!(
        purge_hits.load(Ordering::SeqCst) >= 1,
        "purge admission webhook must have fired (admit runs before the DB purge)"
    );

    // The source rows must still be present: admission ran before the SQL
    // destruction, so the reject prevented `writer.purge_project` from running.
    let src_pages = store.reader.list_pages("src", "proj").await.unwrap();
    assert_eq!(
        src_pages.len(),
        1,
        "src/proj pages must survive when purge admission rejects: {src_pages:?}"
    );
}

/// A move copies frontmatter (including the contributors list) verbatim, so
/// the copy leg skips the `contributors` enrich webhook — re-enriching would
/// only add blocking per-page latency to a bulk move — while other hooks
/// (e.g. the git-mirror) still fire for the copied page.
#[tokio::test]
async fn move_copy_skips_contributors_webhook_but_runs_others() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let contrib_hits = std::sync::Arc::new(AtomicUsize::new(0));
    let mirror_hits = std::sync::Arc::new(AtomicUsize::new(0));
    let c = contrib_hits.clone();
    let m = mirror_hits.clone();
    let app = Router::new()
        .route(
            "/contributors",
            axum_post(move |Json(_p): Json<serde_json::Value>| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::NO_CONTENT, String::new())
                }
            }),
        )
        .route(
            "/mirror",
            axum_post(move |Json(_p): Json<serde_json::Value>| {
                let m = m.clone();
                async move {
                    m.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::NO_CONTENT, String::new())
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let chain = AdmissionChain::new(vec![
        WebhookConfig {
            name: "contributors".into(),
            url: format!("{base}/contributors"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::WritePage],
            blocking: true,
        },
        WebhookConfig {
            name: "mirror".into(),
            url: format!("{base}/mirror"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::WritePage],
            blocking: true,
        },
    ])
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
    // Pre-seed BOTH sides so the move takes the copy-purge (merge) path.
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/keep.md", "keep").await;

    let contrib_before = contrib_hits.load(Ordering::SeqCst);
    let mirror_before = mirror_hits.load(Ordering::SeqCst);

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "merge move should succeed");

    assert_eq!(
        contrib_hits.load(Ordering::SeqCst),
        contrib_before,
        "the contributors enrich webhook must be skipped on the move copy"
    );
    assert!(
        mirror_hits.load(Ordering::SeqCst) > mirror_before,
        "non-skipped webhooks must still fire for the copied page"
    );
}

/// W6: re-running a copy-purge merge is idempotent — re-copying an
/// already-present page is a no-op supersession, leaving no duplicates.
#[tokio::test]
async fn copy_purge_rerun_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    // First move: src/proj (a.md) into existing dst/proj (keep.md), purges src.
    let s1 = build_state(&store, &tmp);
    seed_page(&store, &s1.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &s1.wiki, "dst", "proj", "notes/keep.md", "keep").await;
    let r1 = post(
        s1,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(r1.status(), StatusCode::OK);

    // Re-create the source identically and run the merge again.
    let s2 = build_state(&store, &tmp);
    seed_page(&store, &s2.wiki, "src", "proj", "notes/a.md", "body a").await;
    let r2 = post(
        s2,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(r2.status(), StatusCode::OK);

    // Destination still holds exactly one of each path — no duplicates.
    let mut paths: Vec<String> = store
        .reader
        .list_pages("dst", "proj")
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.path)
        .collect();
    paths.sort();
    assert_eq!(paths, vec!["notes/a.md", "notes/keep.md"]);
}

#[tokio::test]
async fn delete_workspace_refuses_non_empty_without_force() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "victim", "proj", "notes/a.md", "body").await;

    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "victim" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "non-empty delete must be refused without force"
    );
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_some(),
        "a refused delete must not remove the workspace"
    );
}

#[tokio::test]
async fn delete_workspace_force_removes_everything() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "victim", "proj", "notes/a.md", "body").await;
    let workspace_id = store
        .reader
        .find_workspace("victim".into())
        .await
        .unwrap()
        .expect("workspace exists");
    let project_id = store
        .reader
        .find_project(workspace_id, "proj".into())
        .await
        .unwrap()
        .expect("project exists");
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
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "force": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["projects_deleted"].as_u64().unwrap_or(0), 1, "{body}");
    assert!(body["pages_deleted"].as_u64().unwrap_or(0) >= 1, "{body}");
    assert_eq!(body["workstreams_deleted"], 1, "{body}");
    assert_eq!(body["managed_runs_deleted"], 1, "{body}");
    assert_eq!(
        body["workstream_ids"],
        json!([prepared.workstream_id.to_string()]),
        "{body}"
    );
    assert!(
        !raw_dir.exists(),
        "workspace deletion must remove raw workstream segments"
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
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_none(),
        "workspace must be gone after a force delete"
    );
}

#[tokio::test]
async fn delete_workspace_unknown_is_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;
    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "ghost" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rename_workspace_ok_collision_and_unknown() {
    // OK: renames in place; old name no longer resolves, new one does.
    {
        let tmp = TempDir::new().unwrap();
        let (state, store) = make_state(&tmp).await;
        seed_page(&store, &state.wiki, "old", "proj", "notes/a.md", "b").await;
        let resp = post(
            state,
            "/admin/rename-workspace",
            json!({"from":"old","to":"new"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            store
                .reader
                .find_workspace("new".into())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .reader
                .find_workspace("old".into())
                .await
                .unwrap()
                .is_none()
        );
    }
    // 422: destination name already taken.
    {
        let tmp = TempDir::new().unwrap();
        let (state, store) = make_state(&tmp).await;
        seed_page(&store, &state.wiki, "old", "proj", "notes/a.md", "b").await;
        store
            .writer
            .get_or_create_workspace("occupied")
            .await
            .unwrap();
        let resp = post(
            state,
            "/admin/rename-workspace",
            json!({"from":"old","to":"occupied"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    // 404: unknown source workspace.
    {
        let tmp = TempDir::new().unwrap();
        let (state, _store) = make_state(&tmp).await;
        let resp = post(
            state,
            "/admin/rename-workspace",
            json!({"from":"ghost","to":"x"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn rename_workspace_refreshes_scope_manifests() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "old", "proj", "notes/a.md", "b").await;
    state.wiki.backfill_scope_manifests().await.unwrap();
    let ws_id = store
        .reader
        .find_workspace("old".into())
        .await
        .unwrap()
        .expect("workspace exists");

    let resp = post(
        state,
        "/admin/rename-workspace",
        json!({"from":"old","to":"new"}),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["manifests_refreshed"].as_u64().unwrap_or(0) >= 1,
        "rename should rewrite the workspace manifest: {body}"
    );
    let manifest = std::fs::read_to_string(
        tmp.path()
            .join("wiki")
            .join(ws_id.to_string())
            .join("_meta.md"),
    )
    .unwrap();
    assert!(manifest.contains("workspace: new"), "{manifest}");
    assert!(!manifest.contains("workspace: old"), "{manifest}");
}

#[tokio::test]
async fn rename_workspace_manifest_refresh_failure_is_nonfatal_after_db_rename() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "old", "proj", "notes/a.md", "b").await;
    state.wiki.backfill_scope_manifests().await.unwrap();
    let ws_id = store
        .reader
        .find_workspace("old".into())
        .await
        .unwrap()
        .expect("workspace exists");
    let manifest_path = tmp
        .path()
        .join("wiki")
        .join(ws_id.to_string())
        .join("_meta.md");
    std::fs::remove_file(&manifest_path).unwrap();
    std::fs::create_dir(&manifest_path).unwrap();

    let resp = post(
        state,
        "/admin/rename-workspace",
        json!({"from":"old","to":"new"}),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["from"], "old");
    assert_eq!(body["to"], "new");
    assert_eq!(body["manifests_refreshed"], 0);
    assert!(
        body["manifest_warning"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "response should disclose non-fatal manifest refresh failure: {body}"
    );
    assert!(
        store
            .reader
            .find_workspace("new".into())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .reader
            .find_workspace("old".into())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn delete_workspace_reject_policy_aborts_before_db_or_disk_destruction() {
    let app = Router::new().route(
        "/hook",
        axum_post(
            |headers: HeaderMap, Json(_payload): Json<serde_json::Value>| async move {
                let op = headers
                    .get("X-Memory-Op")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                assert_eq!(op, "purge_workspace");
                (StatusCode::INTERNAL_SERVER_ERROR, "no workspace purge")
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tmp = TempDir::new().unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "rejecting-mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::PurgeWorkspace],
        blocking: true,
    }])
    .unwrap();
    let (state, store) = make_state_with_chain(&tmp, chain).await;
    seed_page(&store, &state.wiki, "victim", "proj", "notes/a.md", "body").await;
    let ws_id = store
        .reader
        .find_workspace("victim".into())
        .await
        .unwrap()
        .expect("workspace exists");
    let ws_root = tmp.path().join("wiki").join(ws_id.to_string());

    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "force": true }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_some(),
        "rejecting admission must leave DB rows intact"
    );
    assert!(
        ws_root.exists(),
        "rejecting admission must leave disk intact"
    );
}

#[tokio::test]
async fn delete_workspace_reports_partial_disk_failure_and_dispatches_notification() {
    let (tx, rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let tx_for_route = tx.clone();
    let app = Router::new().route(
        "/hook",
        axum_post(move |Json(payload): Json<serde_json::Value>| {
            let tx_for_route = tx_for_route.clone();
            async move {
                if let Some(tx) = tx_for_route.lock().unwrap().take() {
                    let _ = tx.send(payload);
                }
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tmp = TempDir::new().unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "async-mirror".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::PurgeWorkspace],
        blocking: false,
    }])
    .unwrap();
    let (state, store) = make_state_with_chain(&tmp, chain).await;
    seed_page(&store, &state.wiki, "victim", "proj", "notes/a.md", "body").await;
    let ws_id = store
        .reader
        .find_workspace("victim".into())
        .await
        .unwrap()
        .expect("workspace exists");
    let ws_root = tmp.path().join("wiki").join(ws_id.to_string());
    std::fs::remove_dir_all(&ws_root).unwrap();
    std::fs::write(&ws_root, "not a directory").unwrap();

    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "force": true }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["files_deleted"].as_array().unwrap().is_empty(),
        "{body}"
    );
    assert_eq!(body["files_failed"].as_array().unwrap().len(), 1, "{body}");
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_none(),
        "DB delete should still commit and be reported as partial filesystem failure"
    );

    let payload = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
        .await
        .expect("async purge_workspace dispatch should fire")
        .unwrap();
    assert_eq!(payload["ctx"]["op"], "purge_workspace");
    assert_eq!(payload["ctx"]["workspace"], "victim");
    assert_eq!(payload["ctx"]["partial_failure"], true);
}
