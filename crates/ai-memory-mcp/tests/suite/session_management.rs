//! Adversarial management tests exercise real cookie/CSRF and writer isolation.
use ai_memory_core::{
    ActorContext, AgentKind, NewSession, NewUser, OwnerFilter, ProjectId, SessionId, UserId,
    UserRole, WorkspaceId,
};
use ai_memory_mcp::{
    HumanAuthRuntime,
    auth::AuthState,
    human_auth::LoginLimiter,
    session_management::{SessionManagementState as ManagementState, router},
};
use ai_memory_store::{Store, hash_session_secret};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

struct Browser {
    cookie: String,
    csrf: String,
    user: UserId,
}
struct Harness {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    router: Router,
    alice: Browser,
    bob: Browser,
    root: Browser,
    ws: WorkspaceId,
    project: ProjectId,
}

async fn browser(store: &Store, name: &str, role: UserRole, must_change: bool) -> Browser {
    let phc = ai_memory_store::password::hash_password("password-for-test".into())
        .await
        .unwrap();
    let user = store
        .writer
        .create_human_user(
            NewUser {
                username: name.into(),
                name: None,
                email: None,
            },
            role,
            Some(phc.clone()),
            must_change,
        )
        .await
        .unwrap();
    let secret = ai_memory_store::web_sessions::generate_session_secret().unwrap();
    let csrf = ai_memory_store::web_sessions::generate_csrf_secret().unwrap();
    store
        .writer
        .issue_web_session(
            user,
            phc,
            role,
            must_change,
            hash_session_secret(&secret),
            hash_session_secret(&csrf),
        )
        .await
        .unwrap();
    Browser {
        user,
        cookie: format!("ai_memory_session={secret}; ai_memory_csrf={csrf}"),
        csrf,
    }
}
async fn harness() -> Harness {
    harness_with_chain(None).await
}
async fn harness_with_chain(chain: Option<ai_memory_wiki::AdmissionChain>) -> Harness {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let mut wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    if let Some(chain) = chain {
        wiki = wiki.with_admission_chain(chain);
    }
    let alice = browser(&store, "alice", UserRole::User, false).await;
    let bob = browser(&store, "bob", UserRole::User, false).await;
    let root = browser(&store, "root", UserRole::Root, false).await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "app", None)
        .await
        .unwrap();
    let auth = Arc::new(
        AuthState::new(Some("root-bearer".into())).with_human(HumanAuthRuntime {
            reader: store.reader.clone(),
            writer: store.writer.clone(),
            recovery_token_hash: None,
            root_username: "root".into(),
            root_name: None,
            root_email: None,
            reserved_passwords: vec![],
            trusted_proxy_cidrs: vec![],
            limiter: Arc::new(LoginLimiter::default()),
        }),
    );
    let router = router(
        ManagementState {
            reader: store.reader.clone(),
            writer: store.writer.clone(),
            wiki: wiki.clone(),
        },
        auth,
    );
    Harness {
        _tmp: tmp,
        store,
        wiki,
        router,
        alice,
        bob,
        root,
        ws,
        project,
    }
}
async fn session(h: &Harness, owner: Option<&str>) -> SessionId {
    let id = SessionId::new();
    h.store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id,
            workspace_id: h.ws,
            project_id: h.project,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: owner.map(|name| format!("user:{name}")),
        })
        .await
        .unwrap();
    id
}
async fn call(
    h: &Harness,
    browser: Option<&Browser>,
    method: &str,
    uri: &str,
    body: Value,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(browser) = browser {
        request = request.header("cookie", &browser.cookie);
        if csrf {
            request = request.header("x-csrf-token", &browser.csrf);
        }
    }
    let response = h
        .router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn management_refuses_foreign_ids_shared_deletion_and_missing_csrf_with_admin_control() {
    let h = harness().await;
    let alice = session(&h, Some("alice")).await;
    let bob = session(&h, Some("bob")).await;
    let shared = session(&h, None).await;
    let uri = "/management/v1/sessions?workspace=default&project=app";
    assert_eq!(
        call(&h, None, "GET", uri, Value::Null, false).await.0,
        StatusCode::UNAUTHORIZED
    );
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("authorization", "Bearer root-bearer")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "machine bearer cannot authenticate browser management"
    );
    let (status, listing) = call(&h, Some(&h.alice), "GET", uri, Value::Null, false).await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<_> = listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["session_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&alice.to_string().as_str()));
    assert!(ids.contains(&shared.to_string().as_str()));
    assert!(!ids.contains(&bob.to_string().as_str()));
    assert_eq!(
        call(&h, Some(&h.root), "GET", uri, Value::Null, false)
            .await
            .1["sessions"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let observations =
        format!("/management/v1/sessions/{bob}/observations?workspace=default&project=app");
    assert_eq!(
        call(&h, Some(&h.alice), "GET", &observations, Value::Null, false)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&h, Some(&h.bob), "GET", &observations, Value::Null, false)
            .await
            .0,
        StatusCode::OK
    );
    for sid in [bob, shared] {
        for dry_run in [false, true] {
            let uri = format!("/management/v1/sessions/{sid}/purge");
            assert_eq!(call(&h, Some(&h.alice), "POST", &uri, json!({"workspace":"default", "project":"app", "confirm":true, "dry_run":dry_run}), true).await.0, StatusCode::NOT_FOUND);
        }
        assert!(
            h.store
                .reader
                .session_summary_scoped(h.ws, h.project, sid, OwnerFilter::Any)
                .await
                .unwrap()
                .is_some()
        );
    }
    let uri = format!("/management/v1/sessions/{alice}/purge");
    let body = json!({"workspace":"default", "project":"app", "confirm":true});
    assert_eq!(
        call(&h, Some(&h.alice), "POST", &uri, body.clone(), false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &h,
            Some(&h.alice),
            "POST",
            &uri,
            json!({"workspace":"default", "project":"app", "dry_run":true, "confirm":true}),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        h.store
            .reader
            .session_summary_scoped(h.ws, h.project, alice, OwnerFilter::Any)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        call(
            &h,
            Some(&h.alice),
            "POST",
            &uri,
            json!({"workspace":"default", "project":"app"}),
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // A legitimate purge removes its summary but preserves maintained shared knowledge.
    for (path, frontmatter) in [
        (
            format!("sessions/{alice}.md"),
            json!({"session_id":alice.to_string()}),
        ),
        ("concepts/team.md".into(), json!({})),
    ] {
        h.wiki
            .write_page(WritePageRequest {
                workspace_id: h.ws,
                project_id: h.project,
                path: ai_memory_core::PagePath::new(path).unwrap(),
                frontmatter,
                body: "# Shared knowledge".into(),
                tier: ai_memory_core::Tier::Semantic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: Some(h.alice.user),
                actor: ActorContext::default(),
                evidence: vec![],
            })
            .await
            .unwrap();
    }
    assert_eq!(
        call(&h, Some(&h.alice), "POST", &uri, body.clone(), true)
            .await
            .0,
        StatusCode::OK
    );
    assert!(
        h.store
            .reader
            .session_summary_scoped(h.ws, h.project, alice, OwnerFilter::Any)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !h.wiki
            .abs_path(
                h.ws,
                h.project,
                &ai_memory_core::PagePath::new(format!("sessions/{alice}.md")).unwrap()
            )
            .exists()
    );
    assert!(
        h.wiki
            .abs_path(
                h.ws,
                h.project,
                &ai_memory_core::PagePath::new("concepts/team.md").unwrap()
            )
            .exists()
    );
    assert_eq!(
        call(
            &h,
            Some(&h.root),
            "POST",
            &format!("/management/v1/sessions/{bob}/purge"),
            body,
            true
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn management_raw_session_ids_fail_closed_on_wrong_and_partial_scope() {
    let h = harness().await;
    let sid = session(&h, Some("alice")).await;
    let other_workspace = h
        .store
        .writer
        .get_or_create_workspace("other")
        .await
        .unwrap();
    h.store
        .writer
        .get_or_create_project(other_workspace, "app", None)
        .await
        .unwrap();
    h.store
        .writer
        .get_or_create_project(h.ws, "other", None)
        .await
        .unwrap();
    for browser in [&h.alice, &h.root] {
        for (workspace, project) in [
            ("other", "app"),
            ("default", "other"),
            ("default", "missing"),
        ] {
            let uri = format!(
                "/management/v1/sessions/{sid}/observations?workspace={workspace}&project={project}"
            );
            assert_eq!(
                call(&h, Some(browser), "GET", &uri, Value::Null, false)
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
            let uri = format!("/management/v1/sessions/{sid}/purge");
            for dry_run in [true, false] {
                assert_eq!(call(&h, Some(browser), "POST", &uri, json!({"workspace":workspace,"project":project,"dry_run":dry_run,"confirm":true}), true).await.0, StatusCode::NOT_FOUND);
            }
        }
        for query in ["", "workspace=default", "project=app"] {
            let uri = format!("/management/v1/sessions/{sid}/observations?{query}");
            assert_eq!(
                call(&h, Some(browser), "GET", &uri, Value::Null, false)
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        for body in [json!({"workspace":"default"}), json!({"project":"app"})] {
            assert_eq!(
                call(
                    &h,
                    Some(browser),
                    "POST",
                    &format!("/management/v1/sessions/{sid}/purge"),
                    body,
                    true
                )
                .await
                .0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(
            call(
                &h,
                Some(browser),
                "GET",
                &format!(
                    "/management/v1/sessions/{sid}/observations?workspace=default&project=app"
                ),
                Value::Null,
                false
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert!(
        h.store
            .reader
            .find_project(h.ws, "missing".into())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.store
            .reader
            .find_session_scope(sid)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn management_purge_admission_rejection_preserves_rows_and_files() {
    use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig};
    use std::sync::atomic::{AtomicBool, Ordering};
    let allowed = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let hook = Router::new().route(
        "/guard",
        axum::routing::post({
            let allowed = allowed.clone();
            let calls = calls.clone();
            move |axum::Json(payload): axum::Json<Value>| {
                calls.lock().unwrap().push(payload);
                let allowed = allowed.load(Ordering::SeqCst);
                async move {
                    if allowed {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::FORBIDDEN
                    }
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, hook).await.unwrap() });
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "management-guard".into(),
        url: format!("http://{address}/guard"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::PurgeSession],
        blocking: true,
        projects: Vec::new(),
        headers: Vec::new(),
    }])
    .unwrap();
    let h = harness_with_chain(Some(chain)).await;
    let sid = session(&h, Some("alice")).await;
    let path = ai_memory_core::PagePath::new(format!("sessions/{sid}.md")).unwrap();
    h.wiki
        .write_page(WritePageRequest {
            workspace_id: h.ws,
            project_id: h.project,
            path: path.clone(),
            frontmatter: json!({"session_id":sid.to_string()}),
            body: "# Retained until admitted".into(),
            tier: ai_memory_core::Tier::Episodic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: Some(h.alice.user),
            actor: ActorContext::default(),
            evidence: vec![],
        })
        .await
        .unwrap();
    let abs = h.wiki.abs_path(h.ws, h.project, &path);
    let before = std::fs::read(&abs).unwrap();
    let uri = format!("/management/v1/sessions/{sid}/purge");
    let body = json!({"workspace":"default","project":"app","confirm":true});
    assert_eq!(
        call(
            &h,
            Some(&h.alice),
            "POST",
            &uri,
            json!({"workspace":"default","project":"app","dry_run":true,"confirm":true}),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(
        calls.lock().unwrap().is_empty(),
        "preview must not run admission"
    );
    assert_eq!(std::fs::read(&abs).unwrap(), before);
    assert_eq!(
        call(&h, Some(&h.alice), "POST", &uri, body.clone(), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert!(
        h.store
            .reader
            .find_session_scope(sid)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(std::fs::read(&abs).unwrap(), before);
    assert!(
        h.wiki
            .purged_sessions(h.ws, h.project)
            .await
            .unwrap()
            .is_empty()
    );
    allowed.store(true, Ordering::SeqCst);
    assert_eq!(
        call(&h, Some(&h.alice), "POST", &uri, body, true).await.0,
        StatusCode::OK
    );
    assert!(!abs.exists());
    assert!(
        h.store
            .reader
            .find_session_scope(sid)
            .await
            .unwrap()
            .is_none()
    );
    let recorded = calls.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    for payload in recorded.iter() {
        assert_eq!(payload["ctx"]["workspace"], "default");
        assert_eq!(payload["ctx"]["project"], "app");
        assert_eq!(payload["ctx"]["actor"]["user"], "alice");
        assert_eq!(payload["ctx"]["op"], "purge_session");
    }
    server.abort();
}

#[tokio::test]
async fn management_pagination_reports_its_limit_and_keeps_pages_disjoint() {
    use ai_memory_core::{NewObservation, ObservationKind, Sanitized, Sanitizer};
    let h = harness().await;
    let uri = "/management/v1/sessions?workspace=default&project=app";
    let limit = call(&h, Some(&h.alice), "GET", uri, Value::Null, false)
        .await
        .1["page_size"]
        .as_u64()
        .unwrap() as usize;
    let mut ids = Vec::new();
    for _ in 0..limit + 1 {
        ids.push(session(&h, Some("alice")).await);
    }
    let first = call(&h, Some(&h.alice), "GET", uri, Value::Null, false)
        .await
        .1;
    let next = call(
        &h,
        Some(&h.alice),
        "GET",
        &format!("{uri}&offset={limit}"),
        Value::Null,
        false,
    )
    .await
    .1;
    assert_eq!(first["sessions"].as_array().unwrap().len(), limit);
    assert_eq!(next["sessions"].as_array().unwrap().len(), 1);
    let next_id = &next["sessions"][0]["session_id"];
    assert!(
        first["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| &row["session_id"] != next_id)
    );
    for index in 0..limit + 1 {
        h.store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id: ids[0],
                    workspace_id: h.ws,
                    project_id: h.project,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: format!("event {index}"),
                    body: format!("event body {index}"),
                    importance: 5,
                    occurred_at: None,
                    file_path: None,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();
    }
    let uri = format!(
        "/management/v1/sessions/{}/observations?workspace=default&project=app",
        ids[0]
    );
    let first = call(&h, Some(&h.alice), "GET", &uri, Value::Null, false)
        .await
        .1;
    let next = call(
        &h,
        Some(&h.alice),
        "GET",
        &format!("{uri}&offset={limit}"),
        Value::Null,
        false,
    )
    .await
    .1;
    assert_eq!(first["page_size"], limit);
    assert_eq!(first["total"], limit + 1);
    assert_eq!(first["records"].as_array().unwrap().len(), limit);
    assert_eq!(next["records"].as_array().unwrap().len(), 1);
    let next_id = &next["records"][0]["id"];
    assert!(
        first["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| &row["id"] != next_id)
    );
}
