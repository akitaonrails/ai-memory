//! Integration tests for `POST /admin/rename-project`.
//!
//! Follows the same pattern as `admin_purge.rs`: build a real
//! [`AdminState`] over a tmpdir-backed store + wiki, drive the router
//! with `tower::ServiceExt::oneshot`.

use super::common::post;
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
        llm_providers: ai_memory_llm::LlmProviders {
            default: None,
            overrides: Default::default(),
        },
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
        evidence: Vec::new(),
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
    assert!(body["manifest_warning"].is_null(), "{body}");
    assert!(
        body["checkpoint"].as_str().is_some(),
        "manifest refresh must be checkpointed: {body}"
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
    let manifest = std::fs::read_to_string(
        tmp.path()
            .join("wiki")
            .join(ws.to_string())
            .join(new_id.unwrap().to_string())
            .join("_meta.md"),
    )
    .unwrap();
    assert!(manifest.contains("project: new-name"), "{manifest}");
}

/// Conflict: renaming `default/keep` to `default/doomed` when `doomed`
/// already exists must return 422.
#[tokio::test]
async fn rename_project_manifest_failure_is_disclosed_after_commit() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(&store, &state.wiki, "old-name").await;
    let workspace = store
        .reader
        .find_workspace("default".into())
        .await
        .unwrap()
        .unwrap();
    let project = store
        .reader
        .find_project(workspace, "old-name".into())
        .await
        .unwrap()
        .unwrap();
    let manifest = tmp
        .path()
        .join("wiki")
        .join(workspace.to_string())
        .join(project.to_string())
        .join("_meta.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "old-name", "to": "new-name" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(
        body["manifest_warning"]
            .as_str()
            .is_some_and(|warning| !warning.is_empty()),
        "{body}"
    );
    assert!(
        store
            .reader
            .find_project(workspace, "new-name".into())
            .await
            .unwrap()
            .is_some(),
        "the SQL rename stays committed"
    );
}

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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: state.wiki.clone(),
        llm_providers: ai_memory_llm::LlmProviders {
            default: None,
            overrides: Default::default(),
        },
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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: state.writer.clone(),
        reader: state.reader.clone(),
        wiki: state.wiki.clone(),
        llm_providers: ai_memory_llm::LlmProviders {
            default: state.llm_providers.default.clone(),
            overrides: Default::default(),
        },
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: state.embedder.clone(),
        provider_health: state.provider_health.clone(),
        decay_params: state.decay_params,
        contradiction_band_min: state.contradiction_band_min,
        contradiction_band_max: state.contradiction_band_max,
        data_dir: state.data_dir.clone(),
        db_path: state.db_path.clone(),
        bind: state.bind.clone(),
        home_dir: None,
        bootstrap_gate: state.bootstrap_gate.clone(),
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

// ---------------------------------------------------------------------------
// #1208: `project_id` as an alternative to `from`
// ---------------------------------------------------------------------------

/// `rename-project` accepts `project_id` in place of `from`, so an operator
/// can rename a legacy twin row whose name is shadowed by another project's
/// `legacy_name`.
#[tokio::test]
async fn rename_project_accepts_project_id_instead_of_from() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "old-name").await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .reader
        .find_project(ws, "old-name".to_string())
        .await
        .unwrap()
        .expect("seeded project exists");

    let resp = post(
        state,
        "/admin/rename-project",
        json!({
            "workspace": "default",
            "project_id": proj.to_string(),
            "to": "new-name",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(
        store
            .reader
            .find_project(ws, "new-name".to_string())
            .await
            .unwrap()
            .is_some(),
        "the row named by project_id must be renamed"
    );
}

/// A malformed `project_id` is a 400, before any lookup.
#[tokio::test]
async fn rename_project_rejects_a_malformed_project_id_with_400() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({
            "workspace": "default",
            "project_id": "not-a-uuid",
            "to": "new-name",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// An unknown (or foreign-scope) `project_id` is a 404.
#[tokio::test]
async fn rename_project_unknown_project_id_is_404() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let resp = post(
        state,
        "/admin/rename-project",
        json!({
            "workspace": "default",
            "project_id": "00000000-0000-0000-0000-0000000000ff",
            "to": "new-name",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// An id that really exists, but in ANOTHER workspace, is a 404 and leaves
/// that project's name alone. Control: the same id renames when the request
/// names its own workspace. (#1208)
#[tokio::test]
async fn rename_project_refuses_a_real_id_from_another_workspace() {
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
        "/admin/rename-project",
        json!({
            "workspace": "default",
            "project_id": foreign.to_string(),
            "to": "hijacked",
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "a real id from another workspace must not resolve here"
    );
    assert_eq!(
        store
            .reader
            .project_name_by_id(other_ws, foreign)
            .await
            .unwrap()
            .as_deref(),
        Some("secret"),
        "the foreign project keeps its name"
    );

    let resp = post(
        state,
        "/admin/rename-project",
        json!({
            "workspace": "elsewhere",
            "project_id": foreign.to_string(),
            "to": "renamed",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "control: its own workspace");
}

/// `from` and `project_id` together are ambiguous and refused with 400.
#[tokio::test]
async fn rename_project_refuses_both_from_and_project_id() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;

    seed_page(&store, &state.wiki, "old-name").await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .reader
        .find_project(ws, "old-name".to_string())
        .await
        .unwrap()
        .expect("seeded project exists");

    let resp = post(
        state,
        "/admin/rename-project",
        json!({
            "workspace": "default",
            "from": "old-name",
            "project_id": proj.to_string(),
            "to": "new-name",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    assert!(
        store
            .reader
            .find_project(ws, "old-name".to_string())
            .await
            .unwrap()
            .is_some(),
        "an ambiguous request must not rename anything"
    );
}

/// `POST /admin/adopt-project-identity` joins a split checkout (#1993): the
/// session-holding project takes the repository identity and the
/// remote-derived name, the empty one is archived, a refusal is a 409 that
/// changes nothing, and a repeat is a no-op 200.
#[tokio::test]
async fn adopt_project_identity_joins_a_split_and_refuses_with_409() {
    use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};

    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    seed_page(&store, &state.wiki, "api").await;
    let folder = store
        .writer
        .get_or_create_project(ws, "api", None)
        .await
        .unwrap();
    let identity = || RepositoryIdentity {
        identity: "github.com/acme/api".into(),
        source: IdentitySource::GitRemote,
    };
    let (remote, _) = store
        .writer
        .resolve_project_by_identity(
            ws,
            identity(),
            IdentityStyle::Path,
            "acme-api",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(folder, remote);
    let adopt = |from: &str, into: &str| {
        json!({
            "workspace": "default",
            "project_id": into,
            "from_project_id": from,
            "archive_name": "acme-api-split-20261105",
        })
    };

    // Reversed: the session-holding project is not empty, so nothing moves.
    let refused = post(
        state.clone(),
        "/admin/adopt-project-identity",
        adopt(&folder.to_string(), &remote.to_string()),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let body = body_json(refused).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("holds its own sessions or pages"),
        "{body}"
    );

    let joined = post(
        state.clone(),
        "/admin/adopt-project-identity",
        adopt(&remote.to_string(), &folder.to_string()),
    )
    .await;
    assert_eq!(joined.status(), StatusCode::OK);
    let body = body_json(joined).await;
    assert_eq!(body["project"], "acme-api", "{body}");
    assert_eq!(body["archived"], "acme-api-split-20261105", "{body}");
    assert_eq!(body["identity"], "github.com/acme/api", "{body}");
    let (resolved, _) = store
        .writer
        .resolve_project_by_identity(
            ws,
            identity(),
            IdentityStyle::Path,
            "acme-api",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        resolved, folder,
        "identity resolution now reaches the session project"
    );

    let again = post(
        state,
        "/admin/adopt-project-identity",
        adopt(&remote.to_string(), &folder.to_string()),
    )
    .await;
    assert_eq!(again.status(), StatusCode::OK, "a repeat is a no-op");
}

/// `POST /admin/adopt-project-identity` drops the hook router's project-cache
/// entries that still resolve the moved identity to the project it came from
/// (#1993). Those slots are keyed on the repository identity, so without the
/// eviction the next hook keeps writing the checkout's work into the emptied
/// project. Entries for an unrelated project must survive: the eviction is
/// targeted, not a cache wipe.
#[tokio::test]
async fn adopt_project_identity_evicts_the_identity_keyed_scope_cache() {
    use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
    use ai_memory_mcp::ScopeInvalidation;
    use std::collections::HashMap;
    use std::sync::Arc;

    // Mirrors `ai_memory_hooks::ProjectCacheKey`, which the hooks crate keeps
    // private to its router module.
    type ProjectCacheKey = (String, String, String, String, String);

    let tmp = TempDir::new().unwrap();
    let (mut state, store) = make_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    seed_page(&store, &state.wiki, "api").await;
    let folder = store
        .writer
        .get_or_create_project(ws, "api", None)
        .await
        .unwrap();
    let unrelated = store
        .writer
        .get_or_create_project(ws, "docs", None)
        .await
        .unwrap();
    let (remote, _) = store
        .writer
        .resolve_project_by_identity(
            ws,
            RepositoryIdentity {
                identity: "github.com/acme/api".into(),
                source: IdentitySource::GitRemote,
            },
            IdentityStyle::Path,
            "acme-api",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(folder, remote);

    // Two slots shaped like `cache_key_for`'s output: the fifth element is
    // what carries the repository identity into the key.
    let identity_slot: ProjectCacheKey = (
        "/src/api".to_string(),
        String::new(),
        String::new(),
        "identity".to_string(),
        "git_remote:github.com/acme/api:path::identity:write".to_string(),
    );
    let unrelated_slot: ProjectCacheKey = (
        "/src/docs".to_string(),
        String::new(),
        "docs".to_string(),
        "folder".to_string(),
        ":::folder:write".to_string(),
    );
    let cache = Arc::new(tokio::sync::Mutex::new(HashMap::from([
        (identity_slot.clone(), (ws, remote)),
        (unrelated_slot.clone(), (ws, unrelated)),
    ])));
    state.scope_invalidator = Some({
        let cache = cache.clone();
        Arc::new(
            move |target: ScopeInvalidation| -> std::pin::Pin<
                Box<dyn std::future::Future<Output = ()> + Send + 'static>,
            > {
                let cache = cache.clone();
                // The predicate the serve command installs over the real
                // `ProjectCacheStore`.
                Box::pin(async move {
                    cache.lock().await.retain(|_, v| match target {
                        ScopeInvalidation::Project(proj) => v.1 != proj,
                        ScopeInvalidation::Workspace(workspace) => v.0 != workspace,
                    });
                })
            },
        )
    });

    let joined = post(
        state,
        "/admin/adopt-project-identity",
        json!({
            "workspace": "default",
            "project_id": folder.to_string(),
            "from_project_id": remote.to_string(),
            "archive_name": "acme-api-split-20261105",
        }),
    )
    .await;
    assert_eq!(joined.status(), StatusCode::OK);

    let cache = cache.lock().await;
    assert!(
        !cache.contains_key(&identity_slot),
        "the slot keyed on the moved identity still resolves to the emptied project"
    );
    assert!(
        cache.contains_key(&unrelated_slot),
        "an unrelated project's slot must survive the adoption"
    );
}

/// A rename must evict the hook router's cached scope slots for the renamed
/// project, and must leave every other project's slot alone.
///
/// The declared-`project` rung routes by NAME, never by repository identity
/// (`repository_identity`'s module docs; the router drops the identity for a
/// marker-sourced override that carries no aliases). So a repo whose
/// `.ai-memory.toml` pins `project = "pinned-api"` keeps hitting its cached
/// slot after the rename and keeps writing into the renamed project, while a
/// cold server re-resolves the same marker through `get_or_create_project` and
/// mints a fresh project under the old name.
///
/// `ProjectCacheStore::insert` is private to `ai-memory-hooks`, so the cache
/// here is a plain map driven by the SAME `retain` predicate the real
/// invalidator in `serve.rs` installs.
#[tokio::test]
async fn rename_project_evicts_only_the_renamed_projects_cache_slots() {
    use ai_memory_core::{ProjectId, WorkspaceId};
    use ai_memory_mcp::ScopeInvalidation;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    let tmp = TempDir::new().unwrap();
    let (mut state, store) = make_state(&tmp).await;

    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let renamed = store
        .writer
        .get_or_create_project(ws, "pinned-api", None)
        .await
        .unwrap();
    let untouched = store
        .writer
        .get_or_create_project(ws, "unrelated", None)
        .await
        .unwrap();
    assert_ne!(renamed, untouched);

    // Two warm slots, as two repos with declared markers would leave behind.
    let cache: Arc<Mutex<HashMap<&'static str, (WorkspaceId, ProjectId)>>> = Arc::new(Mutex::new(
        [
            ("marker:pinned-api", (ws, renamed)),
            ("marker:unrelated", (ws, untouched)),
        ]
        .into_iter()
        .collect(),
    ));

    state.scope_invalidator = Some({
        let cache = cache.clone();
        Arc::new(move |target: ScopeInvalidation| {
            let cache = cache.clone();
            Box::pin(async move {
                // Identical predicate to the one `serve.rs` installs.
                cache
                    .lock()
                    .unwrap()
                    .retain(|_, v: &mut (WorkspaceId, ProjectId)| match target {
                        ScopeInvalidation::Project(proj) => v.1 != proj,
                        ScopeInvalidation::Workspace(w) => v.0 != w,
                    });
            })
        })
    });

    let resp = post(
        state,
        "/admin/rename-project",
        json!({ "workspace": "default", "from": "pinned-api", "to": "legacy-api" }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "rename must succeed");

    let cache = cache.lock().unwrap();
    assert!(
        !cache.contains_key("marker:pinned-api"),
        "the renamed project's cached slot must be evicted, got {cache:?}"
    );
    assert!(
        cache.contains_key("marker:unrelated"),
        "an unrelated project's cached slot must survive, got {cache:?}"
    );
}
