//! Adversarial management tests exercise real cookie/CSRF and writer isolation.
use ai_memory_core::{
    ActorContext, AgentKind, NewSession, NewUser, OwnerFilter, ProjectId, SessionId, UserId,
    UserRole, WorkspaceId,
};
use ai_memory_mcp::{
    HumanAuthRuntime,
    auth::AuthState,
    human_auth::LoginLimiter,
    self_service_keys::{SelfServiceKeysState, router as key_router},
    session_management::{SessionManagementState, router as session_router},
};
use ai_memory_store::{Store, TokenPepper, hash_session_secret};
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
    let router = session_router(
        SessionManagementState {
            reader: store.reader.clone(),
            writer: store.writer.clone(),
            wiki: wiki.clone(),
        },
        auth.clone(),
    )
    .merge(key_router(
        SelfServiceKeysState {
            reader: store.reader.clone(),
            writer: store.writer.clone(),
            pepper: Some(TokenPepper::new("test-pepper")),
        },
        auth.clone(),
    ));
    let backend = router
        .clone()
        .merge(ai_memory_mcp::public_auth_router(auth.clone()))
        .merge(ai_memory_mcp::session_auth_router(auth));
    let router = router.merge(ai_memory_web::management::router("/memory", backend));
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
async fn management_project_and_credential_isolation_and_password_change_gate() {
    let h = harness().await;
    let bob = session(&h, Some("bob")).await;
    h.store
        .writer
        .set_access_mode(h.project, ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let projects = call(
        &h,
        Some(&h.alice),
        "GET",
        "/management/v1/projects",
        Value::Null,
        false,
    )
    .await
    .1;
    assert!(projects["projects"].as_array().unwrap().is_empty());
    let uri = format!("/management/v1/sessions/{bob}/observations?workspace=default&project=app");
    assert_eq!(
        call(&h, Some(&h.alice), "GET", &uri, Value::Null, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&h, Some(&h.root), "GET", &uri, Value::Null, false)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &h,
            Some(&h.alice),
            "GET",
            "/management/v1/sessions?workspace=default&project=missing",
            Value::Null,
            false
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let key = call(
        &h,
        Some(&h.bob),
        "POST",
        "/management/v1/credentials",
        json!({"label":"bob-only"}),
        true,
    )
    .await;
    assert_eq!(key.0, StatusCode::OK);
    assert!(key.1["token"].as_str().unwrap().starts_with("aim_"));
    let uri = format!(
        "/management/v1/credentials/{}/revoke",
        key.1["id"].as_str().unwrap()
    );
    assert_eq!(
        call(&h, Some(&h.alice), "POST", &uri, json!({}), true)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert!(
        call(
            &h,
            Some(&h.alice),
            "GET",
            "/management/v1/credentials",
            Value::Null,
            false
        )
        .await
        .1["credentials"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        call(&h, Some(&h.bob), "POST", &uri, json!({}), true)
            .await
            .0,
        StatusCode::OK
    );
    let pending = browser(&h.store, "pending", UserRole::User, true).await;
    assert_eq!(
        call(
            &h,
            Some(&pending),
            "GET",
            "/management/v1/projects",
            Value::Null,
            false
        )
        .await
        .0,
        StatusCode::FORBIDDEN
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

async fn html_request(
    h: &Harness,
    browser: Option<&Browser>,
    method: &str,
    uri: &str,
    body: &str,
    origin: &str,
    bearer: Option<&str>,
) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "memory.example")
        .header("origin", origin)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(browser) = browser {
        request = request.header("cookie", &browser.cookie);
    }
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = h
        .router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "private, no-store");
    assert_eq!(response.headers()["x-frame-options"], "DENY");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn management_html_plain_forms_preserve_csrf_owner_and_machine_boundaries() {
    let h = harness().await;
    let alice = session(&h, Some("alice")).await;
    let bob = session(&h, Some("bob")).await;
    let origin = "https://memory.example";
    let machine_only = html_request(
        &h,
        None,
        "GET",
        "/manage/users",
        "",
        origin,
        Some("root-bearer"),
    )
    .await;
    assert_eq!(machine_only.0, StatusCode::SEE_OTHER);
    assert!(machine_only.1.is_empty());
    let own_events = html_request(
        &h,
        Some(&h.alice),
        "GET",
        &format!("/manage/session?workspace=default&project=app&session={alice}"),
        "",
        origin,
        None,
    )
    .await;
    assert_eq!(own_events.0, StatusCode::OK);
    assert!(own_events.1.contains("Session events"));
    assert_eq!(
        html_request(
            &h,
            Some(&h.alice),
            "GET",
            &format!("/manage/session?workspace=default&project=app&session={bob}"),
            "",
            origin,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let form = format!(
        "action=preview-purge&workspace=default&project=app&session={bob}&csrf={}",
        h.alice.csrf
    );
    assert_eq!(
        html_request(
            &h,
            Some(&h.alice),
            "POST",
            "/manage/action",
            &form,
            origin,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let own_form = form.replace(&bob.to_string(), &alice.to_string());
    assert_eq!(
        html_request(
            &h,
            Some(&h.alice),
            "POST",
            "/manage/action",
            &own_form.replace(&h.alice.csrf, "forged"),
            origin,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        html_request(
            &h,
            Some(&h.alice),
            "POST",
            "/manage/action",
            &own_form,
            "https://evil.example",
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        html_request(
            &h,
            Some(&h.root),
            "POST",
            "/manage/action",
            &own_form,
            origin,
            Some("root-bearer")
        )
        .await
        .0,
        StatusCode::SEE_OTHER
    );
    let (status, preview) = html_request(
        &h,
        Some(&h.alice),
        "POST",
        "/manage/action",
        &own_form,
        origin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(preview.contains("Handoffs unlinked in other projects"));
    assert!(
        h.store
            .reader
            .find_session_scope(alice)
            .await
            .unwrap()
            .is_some()
    );
    let purging = own_form.replace("preview-purge", "purge");
    assert_eq!(
        html_request(
            &h,
            Some(&h.alice),
            "POST",
            "/manage/action",
            &purging,
            origin,
            None
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        html_request(
            &h,
            Some(&h.alice),
            "POST",
            "/manage/action",
            &(purging + "&confirm=true"),
            origin,
            None
        )
        .await
        .0,
        StatusCode::SEE_OTHER
    );
    assert!(
        h.store
            .reader
            .find_session_scope(alice)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.store
            .reader
            .find_session_scope(bob)
            .await
            .unwrap()
            .is_some()
    );
    let pending = browser(&h.store, "html-pending", UserRole::User, true).await;
    assert_eq!(
        html_request(
            &h,
            Some(&pending),
            "POST",
            "/manage/action",
            &format!("action=create-key&label=refused&csrf={}", pending.csrf),
            origin,
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let key_form = format!(
        "action=create-key&label=%3Cscript%3Eattack%3C%2Fscript%3E&csrf={}",
        h.alice.csrf
    );
    let (status, secret) = html_request(
        &h,
        Some(&h.alice),
        "POST",
        "/manage/action",
        &key_form,
        origin,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(secret.contains("issued-secret"));
    let keys = html_request(&h, Some(&h.alice), "GET", "/manage/keys", "", origin, None).await;
    assert_eq!(keys.0, StatusCode::OK);
    assert!(keys.1.contains("&lt;script&gt;attack&lt;/script&gt;"));
    assert!(!keys.1.contains("<script>attack</script>"));
    let key = h
        .store
        .reader
        .list_api_credentials_for_user(h.alice.user)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        html_request(
            &h,
            Some(&h.bob),
            "POST",
            "/manage/action",
            &format!(
                "action=revoke-key&credential={}&csrf={}",
                key.id, h.bob.csrf
            ),
            origin,
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        html_request(
            &h,
            Some(&h.alice),
            "POST",
            "/manage/action",
            &format!(
                "action=revoke-key&credential={}&csrf={}",
                key.id, h.alice.csrf
            ),
            origin,
            None
        )
        .await
        .0,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn management_html_login_rejects_cross_origin_forms_and_preserves_session_cookies() {
    let h = harness().await;
    let empty_sessions = html_request(
        &h,
        Some(&h.alice),
        "GET",
        "/manage/sessions?workspace=default&project=app",
        "",
        "https://memory.example",
        None,
    )
    .await;
    assert_eq!(empty_sessions.0, StatusCode::OK);
    assert!(empty_sessions.1.contains("No sessions in this project"));
    assert!(!empty_sessions.1.contains("<table"));
    let body = "action=login&username=alice&password=password-for-test";
    for origin in ["https://evil.example", "null", ""] {
        assert_eq!(
            html_request(&h, None, "POST", "/manage/action", body, origin, None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    for site in ["same-site", "cross-site", "invalid"] {
        let response = h
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/manage/action")
                    .header("host", "memory.example")
                    .header("origin", "https://memory.example")
                    .header("sec-fetch-site", site)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{site}");
    }
    for site in ["same-origin", "none"] {
        let response = h
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/manage/action")
                    .header("host", "memory.example")
                    .header("origin", "https://memory.example")
                    .header("sec-fetch-site", site)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{site}");
    }
    let response = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/manage/action")
                .header("host", "memory.example")
                .header("origin", "https://memory.example")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/memory/manage/sessions");
    assert!(
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .any(|cookie| cookie.to_str().unwrap().starts_with("ai_memory_session="))
    );
    let page = html_request(
        &h,
        None,
        "GET",
        "/manage/login",
        "",
        "https://memory.example",
        None,
    )
    .await;
    assert_eq!(page.0, StatusCode::OK);
    assert!(page.1.contains(r#"method="post""#));
    assert!(!page.1.contains("fetch("));
}
