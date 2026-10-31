//! Integration tests for `GET /admin/status`, `GET /admin/search`, and
//! `GET /admin/recent`.
//!
//! Exercises the read-only admin surface end-to-end through the axum
//! router: build an `AdminState` over a real on-disk store + wiki,
//! seed a couple of pages, and hit each route.

use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
use ai_memory_core::{ActorContext, AuthLevel, NewPage, NewUser, PagePath, Tier};
use ai_memory_llm::SyntheticEmbedder;
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{AccessMode, DecayParams, GrantLevel, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
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
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };
    (state, store)
}

async fn seed_page(store: &Store, title: &str, path: &str, body: &str) {
    seed_page_in(store, "default", "scratch", title, path, body).await;
}

async fn seed_page_in(
    store: &Store,
    workspace: &str,
    project: &str,
    title: &str,
    path: &str,
    body: &str,
) -> ai_memory_core::ProjectId {
    let ws = store
        .writer
        .get_or_create_workspace(workspace.to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project.to_string(), None)
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
    proj
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

/// `GET /admin/status` carries the consolidation queue: exact counts plus the
/// sessions that gave up, newest first, with the worker's error scrubbed — so a
/// watchdog reads one JSON field instead of grepping the journal for a log
/// line (#1692). The queue's own `/health` view stays counts-only.
#[tokio::test]
async fn status_carries_the_consolidation_queue_and_its_failed_sessions() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let session = ai_memory_core::SessionId::new();
    store
        .writer
        .begin_session(ai_memory_core::NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: proj,
            agent_kind: ai_memory_core::AgentKind::Codex,
            cwd: Some("/repo".into()),
            actor_user: None,
        })
        .await
        .unwrap();
    // A provider body can quote the key it rejected; the status copy is
    // scrubbed like `/admin/llm-test`'s.
    let planted = format!("{} from provider", "sk-live-0123456789abcdef");
    rusqlite::Connection::open(store.db_path())
        .unwrap()
        .execute(
            "INSERT INTO session_consolidation_jobs \
             (session_id, workspace_id, project_id, generation, state, requested_at, \
              next_attempt_at, attempts, completed_at, last_error) \
             VALUES (?1, ?2, ?3, 1, 'failed', 0, 0, 5, 1700000000000000, ?4)",
            rusqlite::params![
                session.as_bytes().to_vec(),
                ws.as_bytes().to_vec(),
                proj.as_bytes().to_vec(),
                planted
            ],
        )
        .unwrap();
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

    assert_eq!(body["consolidation"]["failed"], 1);
    assert_eq!(body["consolidation"]["pending"], 0);
    assert_eq!(
        body["consolidation"]["failed_sessions"][0]["session_id"],
        serde_json::json!(session.to_string())
    );
    assert_eq!(body["consolidation"]["failed_sessions"][0]["attempts"], 5);
    let rendered = body.to_string();
    assert!(
        !rendered.contains("sk-live-0123456789abcdef"),
        "the provider body must not reach an operator: {rendered}"
    );
    assert!(
        rendered.contains("from provider"),
        "the error class stays useful: {rendered}"
    );
}

/// After an embedding-model switch, `/admin/status` counts a page whose
/// only vector was stored under the old triple as pending — status must
/// match the embedder's stored identity the way `handle_embed` writes it —
/// and reports it covered again once a vector for the configured triple
/// exists.
#[tokio::test]
async fn status_counts_missing_embeddings_against_configured_triple() {
    let tmp = TempDir::new().unwrap();
    let (mut state, store) = make_admin_state(&tmp).await;
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
    // One page, written while the OLD model (8-dim synthetic) was
    // configured: its only vector is stale once the model switches.
    let old_model_wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(std::sync::Arc::new(SyntheticEmbedder::new(8)));
    old_model_wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/old-model.md").unwrap(),
            frontmatter: serde_json::json!({ "title": "Old model page" }),
            body: "embedded under the old model".to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Old model page".into()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    // The server now runs the NEW model (16-dim synthetic): the same
    // provider/model identity, a different dimension.
    state.embedder = Some(std::sync::Arc::new(SyntheticEmbedder::new(16)));

    let missing_count = |state: AdminState| async move {
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
        body["derived"]["latest_pages_missing_embeddings"]
            .as_u64()
            .unwrap()
    };

    assert_eq!(
        missing_count(state.clone()).await,
        1,
        "a page whose only vector is from the old triple counts as pending"
    );

    // Legitimate control: re-embedded under the configured model, the
    // page is covered again.
    old_model_wiki
        .with_embedder(std::sync::Arc::new(SyntheticEmbedder::new(16)))
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/old-model.md").unwrap(),
            frontmatter: serde_json::json!({ "title": "Old model page" }),
            body: "re-embedded under the new model".to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Old model page".into()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        missing_count(state).await,
        0,
        "a vector under the configured triple covers the page"
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
        .create_human_user(
            NewUser {
                username: "operator-sentinel".into(),
                name: Some("private-user-sentinel".into()),
                email: Some("private-email-sentinel@example.test".into()),
            },
            ai_memory_core::UserRole::User,
            None,
            false,
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
        .create_human_user(
            NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
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

async fn get(app: axum::Router, uri: &str, level: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut request = Request::builder().uri(uri);
    if let Some(level) = level {
        request = request.header("x-level", level);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    (response.status(), body_bytes(response).await)
}

/// A search with no scope (`ai-memory search --global`, #1253) covers every
/// project and says where each hit lives; a scoped search is unchanged.
#[tokio::test]
async fn global_search_tags_each_hit_with_its_workspace_and_project() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page_in(
        &store,
        "default",
        "alpha",
        "Alpha notes",
        "notes/alpha.md",
        "zephyrine storage notes for alpha",
    )
    .await;
    seed_page_in(
        &store,
        "acme",
        "beta",
        "Beta notes",
        "notes/beta.md",
        "zephyrine storage notes for beta",
    )
    .await;
    let app = admin_router(state);

    let (status, body) = get(app.clone(), "/admin/search?q=zephyrine", None).await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let mut tagged: Vec<(String, String, String)> = hits
        .iter()
        .map(|hit| {
            assert!(hit["id"].is_string(), "global hits keep their id: {hit}");
            assert!(hit["title"].is_string() && hit["snippet"].is_string());
            assert!(hit["rank"].is_number());
            (
                hit["workspace"].as_str().unwrap().to_owned(),
                hit["project"].as_str().unwrap().to_owned(),
                hit["path"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    tagged.sort();
    assert_eq!(
        tagged,
        [
            ("acme".into(), "beta".into(), "notes/beta.md".into()),
            ("default".into(), "alpha".into(), "notes/alpha.md".into()),
        ]
    );

    let (status, body) = get(
        app,
        "/admin/search?q=zephyrine&workspace=acme&project=beta",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(hits.len(), 1, "a scoped search stays in its project");
    assert_eq!(hits[0]["path"], "notes/beta.md");
    assert!(hits[0].get("workspace").is_none() && hits[0].get("project").is_none());
}

/// The global search reads every project as the operator, so it must stay
/// behind the root-only admin gate once the deployment distinguishes
/// operators: a DB user or proxy-asserted user who could reach it would read a
/// restricted project they hold no grant on. Root (the control) still sees it.
#[tokio::test]
async fn global_search_never_reaches_a_non_root_caller() {
    for trusted_proxy in [false, true] {
        let tmp = TempDir::new().unwrap();
        let (mut state, store) = make_admin_state(&tmp).await;
        state.trusted_proxy_identity = trusted_proxy;
        seed_page_in(
            &store,
            "default",
            "open",
            "Open notes",
            "notes/open.md",
            "zephyrine open notes",
        )
        .await;
        let secret = seed_page_in(
            &store,
            "default",
            "secret",
            "Secret notes",
            "notes/secret-sentinel.md",
            "zephyrine restricted notes",
        )
        .await;
        store
            .writer
            .set_access_mode(secret, AccessMode::Restricted)
            .await
            .unwrap();
        if !trusted_proxy {
            // A DB user exists (no grant on `secret`): the store-level
            // multi-user switch. The proxy case has no `users` rows at all.
            store
                .writer
                .create_human_user(
                    NewUser {
                        username: "alice".into(),
                        name: None,
                        email: None,
                    },
                    ai_memory_core::UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap();
        }
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
            (Some("user"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let (status, body) = get(app.clone(), "/admin/search?q=zephyrine", level).await;
            assert_eq!(status, expected, "proxy={trusted_proxy} level={level:?}");
            let body = String::from_utf8_lossy(&body);
            assert!(
                !body.contains("secret-sentinel") && !body.contains("zephyrine"),
                "proxy={trusted_proxy} level={level:?} leaked a hit: {body}"
            );
        }

        let (status, body) = get(app, "/admin/search?q=zephyrine", Some("root")).await;
        assert_eq!(status, StatusCode::OK, "proxy={trusted_proxy}");
        let hits: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let mut projects: Vec<&str> = hits
            .iter()
            .map(|hit| hit["project"].as_str().unwrap())
            .collect();
        projects.sort_unstable();
        assert_eq!(projects, ["open", "secret"], "proxy={trusted_proxy}");
    }
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

/// A listing with no scope (`ai-memory recent --global`) covers every
/// project and says where each hit lives; a scoped listing is unchanged.
#[tokio::test]
async fn global_recent_tags_each_hit_with_its_workspace_and_project() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page_in(
        &store,
        "default",
        "alpha",
        "Alpha notes",
        "notes/alpha.md",
        "recent notes for alpha",
    )
    .await;
    seed_page_in(
        &store,
        "acme",
        "beta",
        "Beta notes",
        "notes/beta.md",
        "recent notes for beta",
    )
    .await;
    // Re-upsert alpha so it is strictly newer than beta.
    seed_page_in(
        &store,
        "default",
        "alpha",
        "Alpha notes",
        "notes/alpha.md",
        "recent notes for alpha, revised",
    )
    .await;
    let app = admin_router(state);

    let (status, body) = get(app.clone(), "/admin/recent", None).await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(hits.len(), 2, "both projects appear on a global listing");
    assert_eq!(hits[0]["path"], "notes/alpha.md");
    assert_eq!(hits[0]["workspace"], "default");
    assert_eq!(hits[0]["project"], "alpha");
    assert_eq!(hits[1]["path"], "notes/beta.md");
    assert_eq!(hits[1]["workspace"], "acme");
    assert_eq!(hits[1]["project"], "beta");
    for hit in &hits {
        assert!(hit["id"].is_string(), "recent hits keep their id: {hit}");
        assert!(hit["title"].is_string() && hit["snippet"].is_string());
        hit["updated_at"]
            .as_str()
            .expect("updated_at is a string")
            .parse::<jiff::Timestamp>()
            .expect("updated_at is RFC 3339");
        assert!(hit.get("rank").is_none(), "recent does not leak FTS rank");
    }

    let (status, body) = get(app, "/admin/recent?workspace=acme&project=beta", None).await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(hits.len(), 1, "a scoped listing stays in its project");
    assert_eq!(hits[0]["path"], "notes/beta.md");
    assert!(hits[0].get("workspace").is_none() && hits[0].get("project").is_none());
}

/// The global listing reads every project as the operator, so it must stay
/// behind the root-only admin gate once the deployment distinguishes
/// operators: a DB user or proxy-asserted user who could reach it would read a
/// restricted project they hold no grant on. Root (the control) still sees it.
#[tokio::test]
async fn global_recent_never_reaches_a_non_root_caller() {
    for trusted_proxy in [false, true] {
        let tmp = TempDir::new().unwrap();
        let (mut state, store) = make_admin_state(&tmp).await;
        state.trusted_proxy_identity = trusted_proxy;
        seed_page_in(
            &store,
            "default",
            "open",
            "Open notes",
            "notes/open.md",
            "recent open notes",
        )
        .await;
        let secret = seed_page_in(
            &store,
            "default",
            "secret",
            "Secret notes",
            "notes/secret-sentinel.md",
            "recent restricted notes",
        )
        .await;
        store
            .writer
            .set_access_mode(secret, AccessMode::Restricted)
            .await
            .unwrap();
        if !trusted_proxy {
            store
                .writer
                .create_human_user(
                    NewUser {
                        username: "alice".into(),
                        name: None,
                        email: None,
                    },
                    ai_memory_core::UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap();
        }
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
            (Some("user"), StatusCode::FORBIDDEN),
            (None, StatusCode::UNAUTHORIZED),
        ] {
            let (status, body) = get(app.clone(), "/admin/recent", level).await;
            assert_eq!(status, expected, "proxy={trusted_proxy} level={level:?}");
            let body = String::from_utf8_lossy(&body);
            assert!(
                !body.contains("secret-sentinel") && !body.contains("restricted notes"),
                "proxy={trusted_proxy} level={level:?} leaked a hit: {body}"
            );
        }

        let (status, body) = get(app, "/admin/recent", Some("root")).await;
        assert_eq!(status, StatusCode::OK, "proxy={trusted_proxy}");
        let hits: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let mut projects: Vec<&str> = hits
            .iter()
            .map(|hit| hit["project"].as_str().unwrap())
            .collect();
        projects.sort_unstable();
        assert_eq!(projects, ["open", "secret"], "proxy={trusted_proxy}");
    }
}

#[tokio::test]
async fn recent_with_empty_results_returns_empty_array() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let app = admin_router(state);

    let (status, body) = get(app, "/admin/recent", None).await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn recent_rejects_partial_scope_instead_of_global_fallback() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page(
        &store,
        "Secret notes",
        "notes/secret-sentinel.md",
        "must not leak through a partial scope",
    )
    .await;
    let app = admin_router(state);

    let (status, body) = get(app, "/admin/recent?workspace=default", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("workspace and project must be provided together"),
        "unexpected error body: {body}"
    );
    let rendered = body.to_string();
    assert!(
        !rendered.contains("secret-sentinel"),
        "partial scope must not leak a hit: {rendered}"
    );
}

#[tokio::test]
async fn recent_unknown_scope_is_404_and_does_not_leak_other_projects() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page(
        &store,
        "Secret notes",
        "notes/secret-sentinel.md",
        "must not leak through an unknown project",
    )
    .await;
    let app = admin_router(state);

    let (status, body) = get(app, "/admin/recent?workspace=default&project=missing", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let rendered = String::from_utf8_lossy(&body);
    assert!(
        !rendered.contains("secret-sentinel"),
        "unknown scope must not leak a hit: {rendered}"
    );
}

#[tokio::test]
async fn recent_limit_is_clamped_to_100() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let app = admin_router(state);

    let (status, _) = get(app, "/admin/recent?limit=9999999", None).await;
    assert_eq!(status, StatusCode::OK);
}
