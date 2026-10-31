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

use super::common::{post, spawn_capture_hook};
use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
use ai_memory_core::{
    AgentKind, NewHandoff, NewObservation, NewSession, ObservationKind, PagePath, Sanitized,
    Sanitizer, SessionId, Tier,
};
use ai_memory_mcp::AdminState;
use ai_memory_store::{DecayParams, PrepareWorkstreamRun, Store, WorkstreamSelection};
use ai_memory_wiki::{
    AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki, WritePageRequest,
};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post as axum_post;
use axum::{Json, Router};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

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
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
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
    };
    (state, store)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
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
        evidence: Vec::new(),
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
            scope_by_id: false,
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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

#[tokio::test]
async fn move_project_merge_surfaces_destination_promotion_manifest_warning() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(
        &store,
        &state.wiki,
        "src",
        "acme-api",
        "notes/a.md",
        "body a",
    )
    .await;
    let dst_ws = store.writer.get_or_create_workspace("dst").await.unwrap();
    let (dst_project, _) = store
        .writer
        .resolve_project_by_identity(
            dst_ws,
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
        .join(dst_ws.to_string())
        .join(dst_project.to_string())
        .join("_meta.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "acme-api", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["merged_into_existing"], true, "{body}");
    assert!(
        body["manifest_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("committed")),
        "{body}"
    );
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
        json!({ "from_workspace": "nope", "project": "ghost", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Moving into a workspace that already has a same-named project MERGES:
/// the destination ends up with both the pre-existing and the moved pages.
/// #1713: the copy+purge merge is not a move. A `--confirm` written for "move
/// this project" must not delete a source's sessions and observations just
/// because the destination happens to hold a project of the same name. The
/// refusal names the counts it would discard and leaves the source intact;
/// the same request with `allow_lossy_merge` proceeds.
#[tokio::test]
async fn a_copy_purge_merge_requires_its_own_acknowledgement() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/b.md", "body b").await;
    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: store
                .reader
                .find_workspace("src".to_string())
                .await
                .unwrap()
                .unwrap(),
            project_id: store
                .reader
                .find_project(
                    store
                        .reader
                        .find_workspace("src".to_string())
                        .await
                        .unwrap()
                        .unwrap(),
                    "proj".to_string(),
                )
                .await
                .unwrap()
                .unwrap(),
            agent_kind: AgentKind::Codex,
            cwd: Some("/src/proj".into()),
            actor_user: None,
        })
        .await
        .unwrap();

    let resp = post(
        state.clone(),
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["strategy"], "copy-purge", "{body}");
    assert_eq!(body["discarded"]["sessions"], 1, "{body}");
    assert_eq!(body["discarded"]["pages_copied"], 1, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(error.contains("--allow-lossy-merge"), "{error}");
    assert!(error.contains("1 session(s)"), "{error}");

    // The source is untouched: its page and its session are both still there.
    assert_eq!(
        store.reader.list_pages("src", "proj").await.unwrap().len(),
        1
    );
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
    assert_eq!(
        store
            .reader
            .project_record_counts(src_ws, src_proj)
            .await
            .unwrap()
            .sessions,
        1,
        "the refusal must not delete the source's history"
    );

    // The same request with the acknowledgement proceeds; the merge tests
    // below cover that path end to end.
    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "src", "project": "proj", "to_workspace": "dst",
            "confirm": true, "allow_lossy_merge": true
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// The acknowledgement refusal reports the source rows the merge would lose.
/// If those counts cannot be read, it must fail closed rather than displaying
/// zeros that could mislead the operator into approving the destructive path.
#[tokio::test]
async fn a_copy_purge_preflight_propagates_count_failures() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/b.md", "body b").await;
    rusqlite::Connection::open(store.db_path())
        .unwrap()
        .execute_batch("ALTER TABLE handoffs RENAME TO unavailable_handoffs")
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true }),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    let error = body["error"].as_str().unwrap_or_default();
    assert!(error.contains("handoffs"), "{body}");
    assert!(body.get("discarded").is_none(), "{body}");
    assert!(body.get("strategy").is_none(), "{body}");
    assert_eq!(
        store.reader.list_pages("src", "proj").await.unwrap().len(),
        1,
        "a failed preflight must leave the source untouched"
    );
}

#[tokio::test]
async fn move_project_merges_into_existing_dest() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;
    seed_page(&store, &state.wiki, "dst", "proj", "notes/b.md", "body b").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        json!({ "from_workspace": "w", "project": "proj", "to_workspace": "w", "confirm": true, "allow_lossy_merge": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// A private profile (`default/_profile.<user id>`) cannot be moved out of
/// the default workspace: its owner finds it there, so a move would orphan it.
/// The refusal comes before any lookup, so nothing is touched.
#[tokio::test]
async fn move_project_refuses_a_private_profile() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "default",
            "project": "_profile.00000000000000000000000000000001",
            "to_workspace": "elsewhere",
            "confirm": true, "allow_lossy_merge": true
        }),
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: Some(embedder),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
            occurred_at: None,
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
                occurred_at: None,
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
            evidence: Vec::new(),
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: Some(embedder),
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

/// A non-directory namespace component is refused before the project move, so
/// neither disk nor SQLite changes.
#[tokio::test]
async fn true_move_refuses_an_invalid_destination_namespace_without_db_changes() {
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("not a directory"),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
                "confirm": true, "allow_lossy_merge": true, "force": true }),
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
            scope_by_id: false,
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
            "confirm": true, "allow_lossy_merge": true,
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: Wiki::new(tmp.path(), store.writer.clone()).unwrap(),
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
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
                "confirm": true, "allow_lossy_merge": true, "on_conflict": "duplicate" }),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
                "confirm": true, "allow_lossy_merge": true, "on_conflict": "overwrite" }),
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
                "confirm": true, "allow_lossy_merge": true, "on_conflict": "duplicate" }),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };
    seed_page(&store, &state.wiki, "src", "proj", "notes/a.md", "body a").await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
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
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
    )
    .await;
    assert_eq!(r1.status(), StatusCode::OK);

    // Re-create the source identically and run the merge again — across
    // a full second boundary, so the re-seeded page's `generated.at`
    // provably differs from the copy the first merge landed. Identical
    // CONTENT with a different write instant must not be a merge
    // conflict (this was the timing flake: sub-second reruns passed,
    // second-crossing reruns 409'd).
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let s2 = build_state(&store, &tmp);
    seed_page(&store, &s2.wiki, "src", "proj", "notes/a.md", "body a").await;
    let r2 = post(
        s2,
        "/admin/move-project",
        json!({ "from_workspace": "src", "project": "proj", "to_workspace": "dst", "confirm": true, "allow_lossy_merge": true }),
    )
    .await;
    // Flaky under heavy parallel load / on Windows (observed in the 2.0
    // full-matrix run); keep the body in the failure so the next
    // occurrence is diagnosable instead of a bare status assert.
    let r2_status = r2.status();
    if r2_status != StatusCode::OK {
        let body = axum::body::to_bytes(r2.into_body(), usize::MAX)
            .await
            .unwrap();
        panic!(
            "second move-project failed: {r2_status} {}",
            String::from_utf8_lossy(&body)
        );
    }

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
            scope_by_id: false,
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

/// A preview must report the same counts a confirmed delete right after it
/// then produces — every numeric field plus `workstream_ids`, not a
/// hand-picked subset — and must not touch the database row, the wiki
/// directory, or a managed workstream's raw segment directory in the
/// meantime (a regression that made preview call
/// `remove_workstream_segment_storage` would fail the raw-dir assertions
/// below). Mirrors
/// `purge_project_dry_run_reports_the_same_counts_the_confirmed_purge_will`
/// one level up, at workspace granularity.
#[tokio::test]
async fn delete_workspace_dry_run_reports_the_same_counts_the_confirmed_delete_will() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("victim")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "proj", None)
        .await
        .unwrap();
    let page_id = state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/a.md".to_string()).unwrap(),
            frontmatter: json!({"title": "notes/a.md"}),
            body: "body".to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("notes/a.md".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
            evidence: Vec::new(),
        })
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
            actor_user: None,

            occurred_at: None,
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
                title: "obs".into(),
                body: "obs body".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: Some(sid),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .store_embeddings(vec![ai_memory_store::EmbeddingWrite {
            page_id,
            vector_bytes: vec![0u8; 4],
            provider: "test".into(),
            model: "model".into(),
            dim: 1,
        }])
        .await
        .unwrap();

    let prepared = store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            scope_by_id: false,
            workspace_id: ws,
            project_id: proj,
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

    let ws_dir = state.wiki.root().join(ws.to_string());
    assert!(
        ws_dir.exists(),
        "the workspace directory must exist before either call"
    );
    assert!(
        raw_dir.exists(),
        "the raw workstream segment directory must exist before either call"
    );

    let preview = post(
        state.clone(),
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "force": true, "dry_run": true }),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK, "a preview must succeed");
    let preview_body = body_json(preview).await;
    assert_eq!(preview_body["dry_run"], true);
    assert_eq!(preview_body["projects_deleted"], 1);
    assert_eq!(preview_body["pages_deleted"], 1);
    assert_eq!(preview_body["sessions_deleted"], 1);
    assert_eq!(preview_body["observations_deleted"], 1);
    assert_eq!(preview_body["handoffs_deleted"], 1);
    assert_eq!(preview_body["embeddings_deleted"], 1);
    assert_eq!(preview_body["collateral_observations_deleted"], 0);
    assert_eq!(preview_body["collateral_handoffs_denulled"], 0);
    assert_eq!(preview_body["workstreams_deleted"], 1);
    assert_eq!(preview_body["managed_runs_deleted"], 1);
    assert_eq!(
        preview_body["workstream_ids"],
        json!([prepared.workstream_id.to_string()])
    );
    assert_eq!(preview_body["files_deleted"], json!([]));
    assert_eq!(preview_body["files_failed"], json!([]));
    assert_eq!(preview_body["compacted"], false);
    assert!(
        preview_body.get("pre_checkpoint").is_none(),
        "a preview must not checkpoint the wiki tree"
    );
    assert!(
        preview_body.get("checkpoint").is_none(),
        "a preview must not checkpoint the wiki tree"
    );

    // Nothing was touched: the workspace directory, the raw workstream
    // segment directory, and the workspace row are all still there.
    assert!(
        ws_dir.exists(),
        "a preview must not remove the workspace directory"
    );
    assert!(
        raw_dir.exists(),
        "a preview must not remove the raw workstream segment directory"
    );
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_some(),
        "a preview must not delete the workspace row"
    );

    // The confirmed delete right after it must succeed with the same counts.
    let confirmed = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "force": true }),
    )
    .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let confirmed_body = body_json(confirmed).await;
    assert_eq!(
        confirmed_body.get("dry_run"),
        None,
        "a confirmed delete report has no dry_run key"
    );
    for field in [
        "projects_deleted",
        "pages_deleted",
        "sessions_deleted",
        "observations_deleted",
        "handoffs_deleted",
        "embeddings_deleted",
        "collateral_observations_deleted",
        "collateral_handoffs_denulled",
        "workstreams_deleted",
        "managed_runs_deleted",
        "workstream_ids",
    ] {
        assert_eq!(
            confirmed_body[field], preview_body[field],
            "the confirmed delete's {field} must match what the preview reported"
        );
    }
    assert!(
        !ws_dir.exists(),
        "the confirmed delete must remove the workspace directory"
    );
    assert!(
        !raw_dir.exists(),
        "the confirmed delete must remove the raw workstream segment directory"
    );
}

/// `dry_run` must always win over `force`. A caller must never be able to
/// combine `dry_run: true` with `force: true` and get a real delete — mirrors
/// `purge_project_confirm_true_and_dry_run_true_still_only_previews`.
#[tokio::test]
async fn delete_workspace_force_true_and_dry_run_true_still_only_previews() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "victim", "proj", "notes/a.md", "body").await;
    let ws = store
        .reader
        .find_workspace("victim".into())
        .await
        .unwrap()
        .expect("workspace exists");
    let ws_dir = state.wiki.root().join(ws.to_string());

    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "force": true, "dry_run": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["dry_run"], true,
        "force: true must not defeat dry_run: true"
    );
    assert_eq!(body["projects_deleted"], 1);

    // The only proof that matters: nothing was actually deleted.
    assert!(
        ws_dir.exists(),
        "{{force: true, dry_run: true}} must not delete the workspace directory"
    );
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_some(),
        "{{force: true, dry_run: true}} must not delete the workspace row"
    );
}

/// A non-empty workspace previewed without `force` must still 409, exactly
/// like the confirmed delete would — the preview promises to describe what a
/// confirmed call would do, and a confirmed call would refuse here too.
#[tokio::test]
async fn delete_workspace_dry_run_refuses_non_empty_without_force() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "victim", "proj", "notes/a.md", "body").await;

    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "dry_run": true }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "a non-empty workspace preview without force must still 409"
    );
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_some(),
        "a refused preview must not remove the workspace"
    );
}

/// A non-existent workspace must still 404 on a preview, exactly like the
/// confirmed delete does.
#[tokio::test]
async fn delete_workspace_dry_run_nonexistent_workspace_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;
    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "ghost", "dry_run": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A dry run must not dispatch the admission webhook: nothing was decided
/// yet, so there is nothing for a mirror to act on. Mirrors
/// `purge_project_dry_run_does_not_dispatch_admission_webhook`.
#[tokio::test]
async fn delete_workspace_dry_run_does_not_dispatch_admission_webhook() {
    let (url, rx) = spawn_capture_hook().await;

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

    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "victim", "force": true, "dry_run": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let outcome = tokio::time::timeout(std::time::Duration::from_millis(300), rx).await;
    assert!(
        outcome.is_err(),
        "a dry run must not dispatch the delete-workspace admission webhook"
    );
}

/// The mirror of the incident, exercised through the HTTP route: deleting a
/// workspace also collaterally deletes an observation stamped into a
/// *different* workspace (because `observations.session_id` cascades
/// regardless of the observation's own `workspace_id`), and orphans (nulls
/// the session reference of, without deleting) a handoff that lives in that
/// other workspace too. Mirrors
/// `purge_project_dry_run_and_confirmed_purge_both_report_collateral_damage_in_another_project`
/// one level up, at workspace granularity.
#[tokio::test]
async fn delete_workspace_dry_run_and_confirmed_delete_both_report_collateral_damage_in_another_workspace()
 {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let doomed_ws = store
        .writer
        .get_or_create_workspace("doomed")
        .await
        .unwrap();
    let doomed_proj = store
        .writer
        .get_or_create_project(doomed_ws, "proj", None)
        .await
        .unwrap();
    let other_ws = store.writer.get_or_create_workspace("other").await.unwrap();
    let other_proj = store
        .writer
        .get_or_create_project(other_ws, "proj", None)
        .await
        .unwrap();

    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: doomed_ws,
            project_id: doomed_proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,

            occurred_at: None,
        })
        .await
        .unwrap();
    // Collateral observation: session lives in `doomed`, observation is
    // stamped into `other` (a different workspace).
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid,
                workspace_id: other_ws,
                project_id: other_proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "collateral".into(),
                body: "collateral body".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    // Collateral handoff: lives in `other`, authored by the `doomed` session.
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: other_ws,
            project_id: other_proj,
            from_session_id: Some(sid),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "collateral handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();

    let preview = post(
        state.clone(),
        "/admin/delete-workspace",
        json!({ "workspace": "doomed", "force": true, "dry_run": true }),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview_body = body_json(preview).await;
    assert_eq!(preview_body["collateral_observations_deleted"], 1);
    assert_eq!(preview_body["collateral_handoffs_denulled"], 1);

    let confirmed = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "doomed", "force": true }),
    )
    .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let confirmed_body = body_json(confirmed).await;
    assert_eq!(confirmed_body["collateral_observations_deleted"], 1);
    assert_eq!(confirmed_body["collateral_handoffs_denulled"], 1);

    // `other` survives as a workspace; only the collateral rows are affected.
    assert!(
        store
            .reader
            .find_workspace("other".into())
            .await
            .unwrap()
            .is_some(),
        "the other workspace must survive"
    );
    assert_eq!(
        store.reader.status_counts().await.unwrap().observations,
        0,
        "the collateral observation in `other` must actually be gone"
    );
}

/// Adversarial control: previewing workspace A must never count rows that
/// live in workspace B, even though both hold a like-named project and B
/// has its own full set of sessions/observations/handoffs. Also pins the
/// reverse direction: a session that lives in B but stamps an observation
/// directly into A must be counted in A's own `observations_deleted` (its
/// row's own `workspace_id` is A), never in `collateral_observations_deleted`
/// (which only counts rows whose own `workspace_id` is NOT A) — the two
/// counts are mutually exclusive by construction (`workspace_id = ?1` vs
/// `workspace_id != ?1`), so this proves there is no double count either.
#[tokio::test]
async fn delete_workspace_dry_run_never_counts_a_different_workspaces_rows() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    // Workspace A: the one being previewed.
    let ws_a = store
        .writer
        .get_or_create_workspace("workspace-a")
        .await
        .unwrap();
    let proj_a = store
        .writer
        .get_or_create_project(ws_a, "proj", None)
        .await
        .unwrap();
    seed_page(
        &store,
        &state.wiki,
        "workspace-a",
        "proj",
        "notes/a.md",
        "a",
    )
    .await;
    let sid_a = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid_a,
            workspace_id: ws_a,
            project_id: proj_a,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,

            occurred_at: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid_a,
                workspace_id: ws_a,
                project_id: proj_a,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "a-own".into(),
                body: "a's own observation".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws_a,
            project_id: proj_a,
            from_session_id: Some(sid_a),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "a's own handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();

    // Workspace B: two projects, its own full set of rows. None of this may
    // ever be counted for a preview of A.
    let ws_b = store
        .writer
        .get_or_create_workspace("workspace-b")
        .await
        .unwrap();
    let proj_b1 = store
        .writer
        .get_or_create_project(ws_b, "proj", None)
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws_b, "proj2", None)
        .await
        .unwrap();
    seed_page(
        &store,
        &state.wiki,
        "workspace-b",
        "proj",
        "notes/b.md",
        "b",
    )
    .await;
    seed_page(
        &store,
        &state.wiki,
        "workspace-b",
        "proj2",
        "notes/c.md",
        "c",
    )
    .await;
    let sid_b = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid_b,
            workspace_id: ws_b,
            project_id: proj_b1,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,

            occurred_at: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid_b,
                workspace_id: ws_b,
                project_id: proj_b1,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "b-own".into(),
                body: "b's own observation".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws_b,
            project_id: proj_b1,
            from_session_id: Some(sid_b),
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "b's own handoff".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
            owner_user: None,
        })
        .await
        .unwrap();

    // Reverse-direction case: B's session stamps an observation directly
    // into A (own `workspace_id` = A). Must count as A's own, not collateral.
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid_b,
                workspace_id: ws_a,
                project_id: proj_a,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "cross".into(),
                body: "stamped into a by b's session".into(),
                importance: 5,

                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/delete-workspace",
        json!({ "workspace": "workspace-a", "force": true, "dry_run": true }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["projects_deleted"], 1,
        "must count only workspace-a's own project, not workspace-b's two"
    );
    assert_eq!(body["pages_deleted"], 1);
    assert_eq!(
        body["sessions_deleted"], 1,
        "workspace-b's session must not be counted"
    );
    assert_eq!(
        body["observations_deleted"], 2,
        "a's own observation plus the one stamped into it by b's session"
    );
    assert_eq!(
        body["handoffs_deleted"], 1,
        "workspace-b's handoff must not be counted"
    );
    assert_eq!(
        body["collateral_observations_deleted"], 0,
        "the cross-stamped observation belongs to A itself and is not collateral"
    );
    assert_eq!(body["collateral_handoffs_denulled"], 0);

    // Control: workspace-b's rows are untouched by the previewed workspace.
    assert!(
        store
            .reader
            .find_workspace("workspace-b".into())
            .await
            .unwrap()
            .is_some()
    );
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
async fn delete_workspace_refuses_invalid_namespace_before_db_or_notification() {
    let (url, rx) = spawn_capture_hook().await;

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

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("not a directory"),
        "{body}"
    );
    assert!(
        store
            .reader
            .find_workspace("victim".into())
            .await
            .unwrap()
            .is_some(),
        "namespace refusal must leave DB rows intact"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), rx)
            .await
            .is_err(),
        "a refused operation must not notify observers"
    );
}

// ---------------------------------------------------------------------------
// #1208: `project_id` as an alternative to `project`
// ---------------------------------------------------------------------------

/// `move-project` accepts `project_id` in place of `project`, targeting the
/// exact row rather than whichever row a name lookup happens to resolve to.
#[tokio::test]
async fn move_project_accepts_project_id_instead_of_name() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "movable", None)
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "default",
            "project_id": proj.to_string(),
            "to_workspace": "elsewhere",
            "confirm": true, "allow_lossy_merge": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "move by project_id must resolve the same target as move by name"
    );

    let dst_ws = store
        .writer
        .get_or_create_workspace("elsewhere")
        .await
        .unwrap();
    assert!(
        store
            .reader
            .find_project(dst_ws, "movable".to_string())
            .await
            .unwrap()
            .is_some(),
        "the row named by project_id must land in the destination"
    );
}

/// A malformed `project_id` is a 400, before any lookup or filesystem work.
#[tokio::test]
async fn move_project_rejects_a_malformed_project_id_with_400() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "default",
            "project_id": "not-a-uuid",
            "to_workspace": "elsewhere",
            "confirm": true, "allow_lossy_merge": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// An unknown (or foreign-scope) `project_id` is a 404, so a stray id from
/// another workspace can never drive a destructive move.
#[tokio::test]
async fn move_project_unknown_project_id_is_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "default",
            "project_id": "00000000-0000-0000-0000-0000000000ff",
            "to_workspace": "elsewhere",
            "confirm": true, "allow_lossy_merge": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// An id that really exists, but in ANOTHER workspace, is a 404 and moves
/// nothing. Control: the same id moves when the request names its own
/// workspace as the source. (#1208)
#[tokio::test]
async fn move_project_refuses_a_real_id_from_another_workspace() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let other_ws = store
        .writer
        .get_or_create_workspace("elsewhere")
        .await
        .unwrap();
    let foreign = store
        .writer
        .get_or_create_project(other_ws, "secret", None)
        .await
        .unwrap();

    let resp = post(
        state.clone(),
        "/admin/move-project",
        json!({
            "from_workspace": "default",
            "project_id": foreign.to_string(),
            "to_workspace": "target",
            "confirm": true, "allow_lossy_merge": true,
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a real id from another workspace must not resolve here"
    );
    assert!(
        store
            .reader
            .project_name_by_id(other_ws, foreign)
            .await
            .unwrap()
            .is_some(),
        "the foreign project stays where it was"
    );

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "elsewhere",
            "project_id": foreign.to_string(),
            "to_workspace": "target",
            "confirm": true, "allow_lossy_merge": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "control: its own workspace");
}

/// `project` and `project_id` together are ambiguous and refused with 400.
#[tokio::test]
async fn move_project_refuses_both_project_and_project_id() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "movable", None)
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "default",
            "project": "movable",
            "project_id": proj.to_string(),
            "to_workspace": "elsewhere",
            "confirm": true, "allow_lossy_merge": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    assert!(
        store
            .reader
            .find_project(ws, "movable".to_string())
            .await
            .unwrap()
            .is_some(),
        "an ambiguous request must not move anything"
    );
}

/// The existing name path still works when `project_id` is absent, so the
/// wire format stays backward compatible with older CLI builds.
#[tokio::test]
async fn move_project_still_accepts_a_bare_project_name() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "movable", None)
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/move-project",
        json!({
            "from_workspace": "default",
            "project": "movable",
            "to_workspace": "elsewhere",
            "confirm": true, "allow_lossy_merge": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn merge_project_dry_run_then_preserves_history_and_sessions() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(
        &store,
        &state.wiki,
        "default",
        "source",
        "notes/shared.md",
        "source body",
    )
    .await;
    seed_page(
        &store,
        &state.wiki,
        "default",
        "source",
        "notes/source-only.md",
        "source only",
    )
    .await;
    seed_page(
        &store,
        &state.wiki,
        "default",
        "target",
        "notes/shared.md",
        "target body",
    )
    .await;
    let ws = store
        .reader
        .find_workspace("default".into())
        .await
        .unwrap()
        .unwrap();
    let source = store
        .reader
        .find_project(ws, "source".into())
        .await
        .unwrap()
        .unwrap();
    let target = store
        .reader
        .find_project(ws, "target".into())
        .await
        .unwrap()
        .unwrap();
    let source_shared_id = store
        .reader
        .latest_page_id_by_ids(ws, source, "notes/shared.md".into())
        .await
        .unwrap()
        .expect("source page exists");
    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: source,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    store.writer.end_session(session, None).await.unwrap();

    let preview = post(
        state.clone(),
        "/admin/merge-project",
        json!({"workspace": "default", "from": "source", "into": "target"}),
    )
    .await;
    assert_eq!(preview.status(), StatusCode::OK);
    let preview = body_json(preview).await;
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["page_versions"], 2);
    assert_eq!(preview["sessions"], 1);
    assert!(
        store
            .reader
            .find_project(ws, "source".into())
            .await
            .unwrap()
            .is_some(),
        "dry run must preserve the source"
    );

    let merged = post(
        state.clone(),
        "/admin/merge-project",
        json!({
            "workspace": "default",
            "from": "source",
            "into": "target",
            "confirm": true
        }),
    )
    .await;
    assert_eq!(
        merged.status(),
        StatusCode::OK,
        "{}",
        body_json(merged).await
    );
    assert!(
        store
            .reader
            .find_project(ws, "source".into())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.reader.session_project_ids(session).await.unwrap(),
        Some((ws, target))
    );
    let shared = state
        .wiki
        .read_page(ws, target, &PagePath::new("notes/shared.md").unwrap())
        .unwrap();
    assert!(shared.body.contains("target body"));
    assert!(shared.body.contains("source body"));
    assert!(shared.body.contains("ai-memory:merged-from default/source"));
    let source_only = state
        .wiki
        .read_page(ws, target, &PagePath::new("notes/source-only.md").unwrap())
        .unwrap();
    assert_eq!(source_only.body, "source only");
    assert_eq!(
        store
            .reader
            .page_versions(ws, target, "notes/shared.md".into(), 10, 0)
            .await
            .unwrap()
            .total,
        3,
        "both old heads and the merged head remain reachable"
    );

    let restored = post(
        state.clone(),
        "/admin/restore-page",
        json!({
            "workspace": "default",
            "project": "target",
            "path": "notes/shared.md",
            "page_id": source_shared_id.to_string()
        }),
    )
    .await;
    assert_eq!(
        restored.status(),
        StatusCode::OK,
        "{}",
        body_json(restored).await
    );
    let shared = state
        .wiki
        .read_page(ws, target, &PagePath::new("notes/shared.md").unwrap())
        .unwrap();
    assert_eq!(shared.body, "source body");
    assert_eq!(
        store
            .reader
            .page_versions(ws, target, "notes/shared.md".into(), 10, 0)
            .await
            .unwrap()
            .total,
        4,
        "restoring a retained source version creates a new latest version"
    );
}

#[tokio::test]
async fn merge_project_refuses_open_sessions_and_foreign_ids() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let source = store
        .writer
        .get_or_create_project(ws, "source", None)
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "target", None)
        .await
        .unwrap();
    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: source,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    let open = post(
        state.clone(),
        "/admin/merge-project",
        json!({
            "workspace": "default",
            "from": "source",
            "into": "target",
            "confirm": true
        }),
    )
    .await;
    assert_eq!(open.status(), StatusCode::CONFLICT);

    let foreign_ws = store
        .writer
        .get_or_create_workspace("foreign")
        .await
        .unwrap();
    let foreign = store
        .writer
        .get_or_create_project(foreign_ws, "other", None)
        .await
        .unwrap();
    let foreign_response = post(
        state,
        "/admin/merge-project",
        json!({
            "workspace": "default",
            "from_project_id": source.to_string(),
            "into_project_id": foreign.to_string(),
            "confirm": true
        }),
    )
    .await;
    assert_eq!(foreign_response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn concurrent_project_merges_have_one_winner_without_losing_the_page() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(
        &store,
        &state.wiki,
        "default",
        "source",
        "notes/only.md",
        "keep me",
    )
    .await;
    let ws = store
        .reader
        .find_workspace("default".into())
        .await
        .unwrap()
        .unwrap();
    let target = store
        .writer
        .get_or_create_project(ws, "target", None)
        .await
        .unwrap();
    let request = json!({
        "workspace": "default",
        "from": "source",
        "into": "target",
        "confirm": true
    });
    let (first, second) = tokio::join!(
        post(state.clone(), "/admin/merge-project", request.clone()),
        post(state.clone(), "/admin/merge-project", request),
    );
    let statuses = [first.status(), second.status()];
    let bodies = [body_json(first).await, body_json(second).await];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1,
        "exactly one concurrent merge may commit: {statuses:?} {bodies:?}"
    );
    assert!(
        statuses
            .iter()
            .any(|status| matches!(*status, StatusCode::NOT_FOUND | StatusCode::CONFLICT)),
        "the losing merge must fail as missing or stale: {statuses:?} {bodies:?}"
    );
    assert_eq!(
        state
            .wiki
            .read_page(ws, target, &PagePath::new("notes/only.md").unwrap())
            .unwrap()
            .body,
        "keep me"
    );
    assert_eq!(
        store
            .reader
            .page_versions(ws, target, "notes/only.md".into(), 10, 0)
            .await
            .unwrap()
            .total,
        1
    );
}
