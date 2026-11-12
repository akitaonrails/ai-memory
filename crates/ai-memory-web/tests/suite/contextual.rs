//! Concrete installation states and source permissions at the consulted page.
use ai_memory_core::{
    ActorContext, AgentKind, AuthLevel, AuthorizedViewer, NewSession, NewUser, OwnerFilter,
    PageEvidence, PageEvidenceKind, PagePath, SessionId, Tier, UserRole,
};
use ai_memory_store::{GrantLevel, Store};
use ai_memory_web::{Recall, WebMountSpec, features::RuntimeFeatures, split_web_routers};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

fn mounted(store: &Store, wiki: Wiki, runtime: Option<RuntimeFeatures>) -> axum::Router {
    split_web_routers(
        true,
        store.reader.clone(),
        wiki,
        WebMountSpec {
            enable_api: true,
            web_ui_dir: None,
            cors_origins: &[],
            web_slug: "/web",
            base_href: "/web/",
            base_path: "",
            trusted_proxy_identity: false,
            recall: Recall::default(),
            runtime,
            health: None,
        },
    )
    .unwrap()
    .protected
}
async fn get(
    app: &axum::Router,
    uri: &str,
    level: AuthLevel,
    viewer: Option<ai_memory_core::UserId>,
) -> (StatusCode, String) {
    let mut request = Request::builder().uri(uri).extension(level);
    if let Some(v) = viewer {
        request = request.extension(AuthorizedViewer(v));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}
#[tokio::test]
async fn contextual_states_report_effective_configuration_and_mask_admin_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let user = store
        .writer
        .create_human_user(
            NewUser {
                username: "reader".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let app = mounted(
        &store,
        wiki.clone(),
        Some(RuntimeFeatures {
            learning_scheduled: true,
            retention_scheduled: true,
            maintenance_enabled: false,
            ..Default::default()
        }),
    );
    let (status, root) = get(&app, "/api/v1/docs/features/status", AuthLevel::Root, None).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&root).unwrap();
    let state = |id: &str| {
        json["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .unwrap()["state"]
            .as_str()
            .unwrap()
    };
    assert_eq!(state("learning"), "Unavailable without LLM");
    assert_eq!(state("retention"), "Disabled");
    assert_eq!(state("embeddings"), "Not configured");
    assert_eq!(state("external-clients"), "Client setup unknown");
    for level in [AuthLevel::User, AuthLevel::Anonymous] {
        let (status, body) = get(&app, "/api/v1/docs/features/status", level, Some(user)).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let learning = json["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "learning")
            .unwrap();
        assert_eq!(learning["state"], "Restricted");
        assert!(
            !learning.to_string().contains("without LLM"),
            "Mask detail as well as the badge"
        );
    }
    let (_, html) = get(&app, "/web/docs/features", AuthLevel::Root, None).await;
    assert!(html.contains("Effective installation state"));
    assert!(html.contains("Unavailable without LLM"));
    assert!(html.contains("Next step:"));
    let app = mounted(&store, wiki.clone(), None);
    let (_, body) = get(&app, "/api/v1/docs/features/status", AuthLevel::Root, None).await;
    assert!(body.contains("Not reported"));
    assert!(
        !body.contains("Unavailable without LLM"),
        "An absent snapshot is not a disabled provider"
    );
    let app = mounted(
        &store,
        wiki,
        Some(RuntimeFeatures {
            llm_configured: true,
            learning_scheduled: true,
            consolidate_on_session_end: true,
            maintenance_enabled: true,
            retention_scheduled: true,
            ..Default::default()
        }),
    );
    let (_, body) = get(&app, "/api/v1/docs/features/status", AuthLevel::Root, None).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "learning")
            .unwrap()["state"],
        "Scheduled"
    );
}

#[tokio::test]
async fn contextual_page_sources_preserve_shared_pages_and_refuse_foreign_or_private_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    store
        .writer
        .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let foreign = store
        .writer
        .get_or_create_project(ws, "private", None)
        .await
        .unwrap();
    let user = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    store
        .writer
        .grant_memory(user, project, GrantLevel::Read, None)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (scope, owner) in [
        (project, Some("alice")),
        (project, Some("bob")),
        (foreign, Some("alice")),
        (project, None),
    ] {
        let id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id,
                workspace_id: ws,
                project_id: scope,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: owner
                    .map(|name| ai_memory_core::IdentityKey::User(name.into()).storage_key()),
                occurred_at: None,
                origin: None,
            })
            .await
            .unwrap();
        ids.push(id);
    }
    let request = |scope, path: &str, body: &str, evidence: Vec<PageEvidence>| WritePageRequest {
        workspace_id: ws,
        project_id: scope,
        path: PagePath::new(path).unwrap(),
        frontmatter: serde_json::json!({"kind":"fact"}),
        body: body.into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::anonymous(),
        evidence,
    };
    wiki.write_page(request(foreign, "secret.md", "# FOREIGN_SECRET", vec![]))
        .await
        .unwrap();
    wiki.write_page(request(
        project,
        "target.md",
        "# Evidence target\n\nRotation evidence searchable.",
        ids.iter()
            .map(|id| PageEvidence {
                kind: PageEvidenceKind::Session,
                source_id: id.to_string(),
            })
            .chain([PageEvidence {
                kind: PageEvidenceKind::Feedback,
                source_id: "OPAQUE_PRIVATE_IDENTIFIER".into(),
            }])
            .collect(),
    ))
    .await
    .unwrap();
    let p = store
        .reader
        .page_provenance(
            ws,
            project,
            "target.md".into(),
            Some(user),
            OwnerFilter::User(ai_memory_core::IdentityKey::User("alice".into()).storage_key()),
            0,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.source_count, 3);
    assert!(p.sources.iter().any(|s| s.session_id == Some(ids[0])));
    assert!(p.sources.iter().any(|s| s.session_id == Some(ids[3])));
    assert!(
        !p.sources
            .iter()
            .any(|s| s.session_id == Some(ids[1]) || s.session_id == Some(ids[2]))
    );
    // Seed typed contradiction edges directly as adversarial index fixtures.
    let local = wiki
        .write_page(request(
            project,
            "other.md",
            "# LOCAL_CONTRADICTION",
            vec![],
        ))
        .await
        .unwrap();
    let private = store
        .reader
        .page_provenance(ws, foreign, "secret.md".into(), None, OwnerFilter::Any, 0)
        .await
        .unwrap()
        .unwrap()
        .page_id;
    let conn = rusqlite::Connection::open(tmp.path().join("db/memory.sqlite")).unwrap();
    for (far, far_path) in [(local, "other.md"), (private, "secret.md")] {
        conn.execute(
            "INSERT INTO links(from_page_id,to_page_id,to_path,link_type) VALUES (?1,?2,?3,'contradicts')",
            rusqlite::params![p.page_id.as_bytes(), far.as_bytes(),far_path],
        )
        .unwrap();
    }
    let p = store
        .reader
        .page_provenance(
            ws,
            project,
            "target.md".into(),
            Some(user),
            OwnerFilter::User(ai_memory_core::IdentityKey::User("alice".into()).storage_key()),
            0,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.disagreement_count, 1);
    assert_eq!(p.disagreements[0].page.path, "other.md");
    let root = store
        .reader
        .page_provenance(ws, project, "target.md".into(), None, OwnerFilter::Any, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        root.source_count, 4,
        "Root sees both source owners, but foreign-project citations stay invalid"
    );
    assert_eq!(
        root.disagreement_count, 2,
        "Root control must see the private far end"
    );
    let app = mounted(&store, wiki, None);
    let owned = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/web/w/team/app/p/target.md")
                .extension(AuthLevel::User)
                .extension(AuthorizedViewer(user))
                .extension(ActorContext {
                    user: Some("alice".into()),
                    ..ActorContext::anonymous()
                })
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(owned.status(), StatusCode::OK);
    let owned_html = String::from_utf8(
        axum::body::to_bytes(owned.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        owned_html.contains(&ids[0].to_string()),
        "The route must forward the legitimate owner's context"
    );
    assert!(
        !owned_html.contains(&ids[1].to_string()),
        "Another owner's source must stay hidden"
    );
    let (_, html) = get(
        &app,
        "/web/w/team/app/p/target.md",
        AuthLevel::User,
        Some(user),
    )
    .await;
    assert!(html.contains("Knowledge evidence and validity"));
    assert!(html.contains("LOCAL_CONTRADICTION"));
    assert!(!html.contains("FOREIGN_SECRET"));
    assert!(!html.contains("OPAQUE_PRIVATE_IDENTIFIER"));
    assert!(html.contains(&ids[3].to_string()));
    assert!(!html.contains(&ids[1].to_string()));
    assert!(!html.contains(&ids[2].to_string()));
    let (status, _) = get(
        &app,
        "/web/w/team/private/p/secret.md?recall_q=secret&recall_explain=1",
        AuthLevel::User,
        Some(user),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    let (status, html) = get(
        &app,
        "/web/w/team/app/p/target.md?recall_q=Rotation&recall_limit=20&recall_explain=1",
        AuthLevel::User,
        Some(user),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("data-page-recall-trace"), "{html}");
    assert!(html.contains("Recomputed now"));
    assert!(!html.contains("FOREIGN_SECRET"));
    let (_, search) = get(
        &app,
        "/web/search?workspace=team&project=app&q=Rotation&kind=fact&limit=7",
        AuthLevel::User,
        Some(user),
    )
    .await;
    assert!(search.contains("recall_q=Rotation"));
    assert!(search.contains("recall_kind=fact"));
    assert!(search.contains("recall_limit=7"));
}

#[tokio::test]
async fn contextual_evidence_paginates_versions_and_hides_expired_disagreements() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let ws = store.writer.get_or_create_workspace("team").await.unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let request = |path: &str, frontmatter, evidence| WritePageRequest {
        workspace_id: ws,
        project_id: project,
        path: PagePath::new(path).unwrap(),
        frontmatter,
        body: "# Version evidence\n\nSource fixture".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::anonymous(),
        evidence,
    };
    let id = wiki
        .write_page(request(
            "page.md",
            serde_json::json!({}),
            (0..25)
                .map(|n| PageEvidence {
                    kind: PageEvidenceKind::Reconsolidation,
                    source_id: format!("private-opaque-{n}"),
                })
                .collect(),
        ))
        .await
        .unwrap();
    let expired = wiki
        .write_page(request(
            "expired.md",
            serde_json::json!({"expires_at":"2020-01-01T00:00:00Z"}),
            vec![],
        ))
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(tmp.path().join("db/memory.sqlite")).unwrap();
    conn.execute("INSERT INTO links(from_page_id,to_page_id,to_path,link_type) VALUES(?1,?2,'expired.md','contradicts')",rusqlite::params![id.as_bytes(),expired.as_bytes()]).unwrap();
    let read = |offset| {
        store.reader.page_provenance(
            ws,
            project,
            "page.md".into(),
            None,
            OwnerFilter::Any,
            offset,
        )
    };
    let first = read(0).await.unwrap().unwrap();
    let last = read(20).await.unwrap().unwrap();
    assert_eq!(
        (first.source_count, first.sources.len(), last.sources.len()),
        (25, 20, 5)
    );
    assert_eq!(
        first.disagreement_count, 0,
        "Expired targets must not look like live disagreements"
    );
    let app = mounted(&store, wiki.clone(), None);
    let (_, html) = get(
        &app,
        "/web/w/team/app/p/page.md?evidence_offset=20",
        AuthLevel::Root,
        None,
    )
    .await;
    assert!(html.contains("Newer sources"));
    assert!(!html.contains("Older sources →"));
    assert!(!html.contains("private-opaque-"));
    let (_, html) = get(&app, "/web/w/team/app/p/expired.md", AuthLevel::Root, None).await;
    assert!(html.contains("Expired: excluded from normal retrieval"));
    let same = wiki
        .write_page(request("page.md", serde_json::json!({}), vec![]))
        .await
        .unwrap();
    assert_eq!(
        same, id,
        "Identical writes keep the version and its citations"
    );
    let mut rewritten = request("page.md", serde_json::json!({}), vec![]);
    rewritten.body = "# Changed version\n\nThis rewrite has different evidence.".into();
    wiki.write_page(rewritten).await.unwrap();
    let now = read(0).await.unwrap().unwrap();
    assert_ne!(now.page_id, id);
    assert_eq!(
        now.source_count, 0,
        "Old version citations must not be transferred to a rewrite"
    );
}
