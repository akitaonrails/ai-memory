//! Integration tests for `GET /admin/status` and `GET /admin/search`.
//!
//! Exercises the read-only admin surface end-to-end through the axum
//! router: build an `AdminState` over a real on-disk store + wiki,
//! seed a couple of pages, and hit each route.

use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
use ai_memory_core::{ActorContext, AuthLevel, NewPage, NewUser, PagePath, Tier};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{AccessMode, DecayParams, GrantLevel, Store};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

async fn make_admin_state(tmp: &TempDir) -> (AdminState, Store) {
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
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };
    (state, store)
}

async fn seed_page(store: &Store, title: &str, path: &str, body: &str) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch".to_string(), None)
        .await
        .unwrap();
    let page = NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.to_string(),
        body: body.to_string(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    };
    store.writer.upsert_page(page).await.unwrap();
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    use axum::body::to_bytes;
    to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap()
        .to_vec()
}

async fn coordinate(app: axum::Router, query: &str) -> serde_json::Value {
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/admin/project-coordinate?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

#[tokio::test]
async fn status_returns_counts_and_paths() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page(
        &store,
        "Karpathy LLM wiki",
        "concepts/karpathy.md",
        "Compile-not-retrieve pattern from Karpathy's notes.",
    )
    .await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    // Sanity-check the JSON shape.
    assert!(body["version"].is_string());
    assert_eq!(body["bind"], "127.0.0.1:49374");
    assert!(
        body["data_dir"]
            .as_str()
            .unwrap()
            .contains(tmp.path().to_str().unwrap())
    );
    assert!(body["db_path"].as_str().unwrap().ends_with(".sqlite"));
    assert_eq!(body["counts"]["pages_latest"].as_u64().unwrap(), 1);
    assert_eq!(body["counts"]["pages_all"].as_u64().unwrap(), 1);
    assert_eq!(body["derived"]["pages_rows"].as_u64().unwrap(), 1);
    assert_eq!(body["derived"]["pages_fts_rows"].as_u64().unwrap(), 1);
    assert_eq!(
        body["derived"]["latest_pages_missing_embeddings"]
            .as_u64()
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn list_projects_returns_workspace_project_pairs() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    // Seed pages in two distinct (workspace, project) scopes.
    seed_page(&store, "A", "notes/a.md", "body a").await; // default/scratch
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "infra".to_string(), None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/b.md").unwrap(),
            title: "B".into(),
            body: "body b".into(),
            tier: Tier::Semantic,
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
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/projects")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    let pairs: Vec<(String, String)> = body["projects"]
        .as_array()
        .expect("projects array")
        .iter()
        .map(|p| {
            (
                p["workspace_name"].as_str().unwrap().to_string(),
                p["project_name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        pairs.contains(&("default".to_string(), "scratch".to_string())),
        "{pairs:?}"
    );
    assert!(
        pairs.contains(&("acme".to_string(), "infra".to_string())),
        "{pairs:?}"
    );
}

#[tokio::test]
async fn project_coordinate_reports_all_statuses_and_never_mutates() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("coordinates")
        .await
        .unwrap();
    let repository = RepositoryIdentity {
        identity: "github.com/acme/api".into(),
        source: IdentitySource::GitRemote,
    };
    let project = store
        .writer
        .resolve_project_by_identity(
            ws,
            repository,
            IdentityStyle::HostPath,
            "checkout-blue",
            Some("/private/repository/path-sentinel".into()),
            None,
            None,
        )
        .await
        .unwrap()
        .0;
    let manifest = state.wiki.project_root(ws, project).join("_meta.md");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(&manifest, "sentinel manifest\n").unwrap();
    let manifest_before = std::fs::read(&manifest).unwrap();
    let snapshot = || {
        rusqlite::Connection::open(store.db_path())
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM projects), name, \
                 (SELECT COUNT(*) FROM audit_log) FROM projects WHERE id = ?1",
                [project.as_bytes().to_vec()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .unwrap()
    };
    let operator = store
        .writer
        .create_user(
            NewUser {
                username: "operator-sentinel".into(),
                name: Some("private-user-sentinel".into()),
                email: Some("private-email-sentinel@example.test".into()),
            },
            [6; ai_memory_store::TOKEN_HASH_LEN],
        )
        .await
        .unwrap();
    store
        .writer
        .set_access_mode(project, AccessMode::Restricted)
        .await
        .unwrap();
    store
        .writer
        .grant_memory(operator, project, GrantLevel::Read, None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: project,
            path: PagePath::new("private/repository-path-sentinel.md").unwrap(),
            title: "Private page sentinel".into(),
            body: "private-page-body-sentinel".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: Some(operator),
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let before = snapshot();
    let related_before: (i64, i64, i64) = rusqlite::Connection::open(store.db_path())
        .unwrap()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM users), (SELECT COUNT(*) FROM project_grants), \
             (SELECT COUNT(*) FROM pages)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let app = admin_router(state.clone()).layer(axum::middleware::from_fn(
        |mut request: Request<Body>, next: axum::middleware::Next| async move {
            request.extensions_mut().insert(AuthLevel::Root);
            request.extensions_mut().insert(ActorContext::anonymous());
            next.run(request).await
        },
    ));
    for (name, expected, rename_eligible) in [
        ("checkout-blue", "exact", false),
        ("acme-api", "canonical_compat", true),
        ("api", "legacy_compat", false),
        ("unknown", "missing", false),
    ] {
        let body = coordinate(
            app.clone(),
            &format!(
                "workspace=coordinates&project={name}&identity=github.com%2Facme%2Fapi&identity_source=git_remote&identity_style=path"
            ),
        )
        .await;
        assert_eq!(body["status"], expected, "{body}");
        assert_eq!(body["rename_eligible"], rename_eligible, "{body}");
        let rendered = body.to_string();
        for sentinel in [
            "github.com/acme/api",
            "/private/repository/path-sentinel",
            "private-user-sentinel",
            "private-email-sentinel@example.test",
            "private/repository-path-sentinel.md",
            "private-page-body-sentinel",
        ] {
            assert!(
                !rendered.contains(sentinel),
                "leaked {sentinel}: {rendered}"
            );
        }
    }
    assert_eq!(snapshot(), before, "diagnostic calls must not mutate SQL");
    assert_eq!(
        rusqlite::Connection::open(store.db_path())
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM users), (SELECT COUNT(*) FROM project_grants), \
                 (SELECT COUNT(*) FROM pages)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        related_before,
        "diagnostic calls must not mutate users, grants, or pages",
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), manifest_before);

    store
        .writer
        .resolve_project_by_identity(
            ws,
            RepositoryIdentity {
                identity: "gitlab.com/acme/api".into(),
                source: IdentitySource::GitRemote,
            },
            IdentityStyle::HostPath,
            "gitlab-api",
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let before_ambiguous = snapshot();
    let ambiguous = coordinate(
        app,
        "workspace=coordinates&project=acme-api&identity=github.com%2Facme%2Fapi&identity_source=git_remote&identity_style=path",
    )
    .await;
    assert_eq!(ambiguous["status"], "ambiguous");
    assert_eq!(ambiguous["collision_reason"], "cross_forge_collision");
    let ambiguous_rendered = ambiguous.to_string();
    for sentinel in [
        "github.com/acme/api",
        "/private/repository/path-sentinel",
        "private-user-sentinel",
        "private-email-sentinel@example.test",
        "private/repository-path-sentinel.md",
        "private-page-body-sentinel",
    ] {
        assert!(
            !ambiguous_rendered.contains(sentinel),
            "leaked {sentinel}: {ambiguous_rendered}"
        );
    }
    assert_eq!(
        snapshot(),
        before_ambiguous,
        "ambiguous diagnosis must not mutate SQL"
    );
    assert_eq!(std::fs::read(&manifest).unwrap(), manifest_before);
    assert_eq!(
        rusqlite::Connection::open(store.db_path())
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM users), (SELECT COUNT(*) FROM project_grants), \
                 (SELECT COUNT(*) FROM pages)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap(),
        related_before,
    );
    let after = snapshot();
    assert_eq!(after.0, before.0 + 1);
    assert_eq!(after.1, before.1);
    assert_eq!(after.2, before.2);
}

#[tokio::test]
async fn project_coordinate_single_user_admin_contract_remains_open() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let response = admin_router(state)
        .oneshot(
            Request::builder()
                .uri("/admin/project-coordinate?workspace=unknown&project=unknown")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(body["status"], "missing");
}

#[tokio::test]
async fn project_coordinate_is_root_only_after_multiuser_enablement() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    store
        .writer
        .create_user(
            NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            [7; ai_memory_store::TOKEN_HASH_LEN],
        )
        .await
        .unwrap();
    let app = admin_router(state).layer(axum::middleware::from_fn(
        |mut request: Request<Body>, next: axum::middleware::Next| async move {
            let level = match request
                .headers()
                .get("x-level")
                .and_then(|value| value.to_str().ok())
            {
                Some("root") => AuthLevel::Root,
                Some("user") => AuthLevel::User,
                _ => AuthLevel::Anonymous,
            };
            request.extensions_mut().insert(level);
            request.extensions_mut().insert(ActorContext::anonymous());
            next.run(request).await
        },
    ));
    for (level, expected) in [
        (Some("root"), StatusCode::OK),
        (Some("user"), StatusCode::FORBIDDEN),
        (None, StatusCode::UNAUTHORIZED),
    ] {
        let mut request =
            Request::builder().uri("/admin/project-coordinate?workspace=unknown&project=unknown");
        if let Some(level) = level {
            request = request.header("x-level", level);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "level {level:?}");
    }
}

#[tokio::test]
async fn search_returns_matching_hits() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page(
        &store,
        "Storage architecture",
        "concepts/storage.md",
        "We use SQLite in WAL mode with a single-writer actor for safe concurrency.",
    )
    .await;
    seed_page(
        &store,
        "Hook fire-and-forget",
        "concepts/hooks.md",
        "Lifecycle hooks POST with a 200ms hard timeout — fire and forget.",
    )
    .await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=sqlite&limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(hits.len(), 1, "only the storage page mentions sqlite");
    assert_eq!(hits[0]["path"].as_str().unwrap(), "concepts/storage.md");
    assert_eq!(hits[0]["title"].as_str().unwrap(), "Storage architecture");
}

#[tokio::test]
async fn search_with_empty_results_returns_empty_array() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=nonexistentterm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn search_rejects_partial_scope_instead_of_global_fallback() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=anything&workspace=default")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("workspace and project must be provided together"),
        "unexpected error body: {body}"
    );
}

#[tokio::test]
async fn search_limit_is_clamped_to_100() {
    // Server-side clamp prevents callers from requesting a million
    // hits; verify by passing a huge limit and ensuring we still get
    // a 200 (no panic, no OOM).
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=anything&limit=9999999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
