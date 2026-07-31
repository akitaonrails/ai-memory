//! Integration tests for `POST /admin/rename-project`.
//!
//! Follows the same pattern as `admin_purge.rs`: build a real
//! [`AdminState`] over a tmpdir-backed store + wiki, drive the router
//! with `tower::ServiceExt::oneshot`.

use ai_memory_core::{PagePath, Tier};
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

/// Seed `default/old-name` with one page. Returns the page path.
async fn seed_page(store: &Store, wiki: &Wiki, project: &str) -> String {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project, None)
        .await
        .unwrap();
    let path = format!("notes/{project}.md");
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path.clone()).unwrap(),
        frontmatter: serde_json::json!({"title": project}),
        body: format!("Content for {project}."),
        tier: Tier::Semantic,
        pinned: false,
        title: Some(project.into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    })
    .await
    .unwrap();
    path
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path: seed `default/old-name`, rename to `new-name`, assert 200
/// and that `pages=1`.
#[tokio::test]
async fn rename_project_happy_path() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "old-name").await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "old-name", "to": "new-name" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "rename must succeed");

    let body = body_json(resp).await;
    assert_eq!(
        body["from"].as_str().unwrap_or(""),
        "old-name",
        "from field: {body}"
    );
    assert_eq!(
        body["to"].as_str().unwrap_or(""),
        "new-name",
        "to field: {body}"
    );
    assert_eq!(
        body["pages"].as_u64().unwrap_or(0),
        1,
        "one page under new name: {body}"
    );

    // Verify the project row was actually renamed in the DB.
    let ws = store
        .reader
        .find_workspace("default".to_string())
        .await
        .unwrap()
        .expect("workspace must exist");
    let old_id = store
        .reader
        .find_project(ws, "old-name".to_string())
        .await
        .unwrap();
    assert!(
        old_id.is_none(),
        "old project name must no longer be findable"
    );
    let new_id = store
        .reader
        .find_project(ws, "new-name".to_string())
        .await
        .unwrap();
    assert!(new_id.is_some(), "new project name must be findable");
}

/// Conflict: renaming `default/keep` to `default/doomed` when `doomed`
/// already exists must return 422.
#[tokio::test]
async fn rename_project_conflict_returns_422() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "keep").await;
    seed_page(&store, &state.wiki, "doomed").await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "keep", "to": "doomed" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "name conflict must be 422"
    );

    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or("").contains("doomed"),
        "error must mention the taken name: {body}"
    );
}

/// Source project missing: must return 404.
#[tokio::test]
async fn rename_project_source_missing_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "nonexistent", "to": "anything" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "missing source project must be 404"
    );

    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or("").contains("not found"),
        "error must say 'not found': {body}"
    );
}

/// Workspace missing: must return 404.
#[tokio::test]
async fn rename_project_workspace_missing_returns_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "ghost", "from": "any", "to": "other" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "missing workspace must be 404"
    );
}

/// Invalid destination name: empty string must return 422.
#[tokio::test]
async fn rename_project_empty_to_returns_422() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "src").await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "src", "to": "" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty name must be 422"
    );
}

/// Invalid destination name: name containing a slash must return 422.
#[tokio::test]
async fn rename_project_slash_in_name_returns_422() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "src").await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "src", "to": "has/slash" }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "slash in name must be 422"
    );
}

/// Invalid destination name: all-whitespace must return 422.
#[tokio::test]
async fn rename_project_whitespace_name_returns_422() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "src").await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "src", "to": "   " }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "whitespace-only name must be 422"
    );
}

/// Data integrity: page seeded before rename is still found via search
/// after rename. The `project_id` foreign key is unchanged; only the
/// name column on the project row changes.
#[tokio::test]
async fn rename_project_pages_still_searchable() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "before-rename").await;

    // Build a second AdminState that shares the same writer + reader so
    // the single-writer invariant is preserved. We need two states because
    // axum's `oneshot` consumes the router.
    let state2 = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: state.wiki.clone(),
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

    let rename_resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "before-rename", "to": "after-rename" }),
    )
    .await;
    assert_eq!(rename_resp.status(), StatusCode::OK);

    // Search for the page's distinctive content via the shared-state router.
    let router = admin_router(state2);
    // Use percent-encoded double-quotes so FTS5 treats the hyphenated
    // string as a phrase rather than a subtraction expression.
    let search_req = Request::builder()
        .method("GET")
        .uri("/admin/search?q=%22before-rename%22")
        .body(Body::empty())
        .unwrap();
    let search_resp = router.oneshot(search_req).await.unwrap();
    assert_eq!(search_resp.status(), StatusCode::OK);

    let hits = body_json(search_resp).await;
    assert!(
        hits.as_array().is_some_and(|a| !a.is_empty()),
        "page must still be searchable after rename: {hits}"
    );
}

/// Regression for the rename-vs-purge race surfaced by live exploration.
///
/// The race shape: admin handler resolves `(ws_id, proj_id)` via the
/// reader pool, then dispatches the rename to the writer actor. A
/// `POST /admin/purge-project` that arrives between those two steps
/// deletes the row. The writer's `UPDATE projects` then affects ZERO
/// rows but `conn.execute` returns `Ok(0)` — the pre-fix code accepted
/// any `Ok` as success and the handler responded `200 OK` for an
/// operation that touched nothing.
///
/// We don't need a concurrent purge to reproduce the symptom: any
/// state where the project row is gone but the admin handler still
/// holds its id triggers the same code path. The test takes the
/// shortcut of purging serially first, then asserting that a rename
/// of the now-vanished project returns `404 Not Found` instead of a
/// false `200 OK`.
#[tokio::test]
async fn rename_project_after_purge_returns_404_not_silent_200() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    // Seed default/race-victim so the workspace + project rows exist.
    seed_page(&store, &state.wiki, "race-victim").await;

    // Purge the row out from under any in-flight rename.
    let purge_state = AdminState {
        writer: state.writer.clone(),
        reader: state.reader.clone(),
        wiki: state.wiki.clone(),
        llm: state.llm.clone(),
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: state.embedder.clone(),
        provider_health: state.provider_health.clone(),
        decay_params: state.decay_params,
        data_dir: state.data_dir.clone(),
        db_path: state.db_path.clone(),
        bind: state.bind.clone(),
        home_dir: None,
        bootstrap_lock: state.bootstrap_lock.clone(),
        token_pepper: None,
        active_project: state.active_project.clone(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };
    let purge_resp = post(
        purge_state,
        "/admin/purge-project",
        json!({ "workspace": "default", "project": "race-victim", "confirm": true }),
    )
    .await;
    assert_eq!(
        purge_resp.status(),
        StatusCode::OK,
        "purge step must succeed"
    );

    // Now rename the now-deleted project. Without the fix this returns
    // `200 OK` with `pages: 0` — visibly indistinguishable from a happy
    // path on an empty project. With the fix the writer surfaces
    // `StoreError::NotFound` and the admin handler maps to 404.
    let rename_resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "race-victim", "to": "race-survivor" }),
    )
    .await;

    // The pre-lookup also runs against the missing project; depending
    // on which step trips the absence (handler-level `lookup_ws_proj_no_create`
    // vs writer-side `UPDATE projects ... = 0 rows`), either path must
    // produce a 404. Accept either — the load-bearing invariant is
    // "no false 200 OK".
    assert_eq!(
        rename_resp.status(),
        StatusCode::NOT_FOUND,
        "rename of a vanished project must NOT silently return 200"
    );
}
