//! `/messages*` behind the real bearer middleware.
//!
//! These routes exist so a non-root operator can use `ai-memory message`, which
//! `/admin/messages*` refuses on a multi-user server. They are therefore a
//! non-root path to other projects' mail, and every test here attempts a
//! crossing the per-project grants or admission chain must refuse, with root
//! or a granted user as the control.

use std::sync::Arc;

use ai_memory_core::{
    AgentKind, ApiCredentialId, MessageBox, MessageNature, NewAgentMessage, NewUser, ProjectId,
    SKIP_ADMISSION_CHAIN_HEADER, UserId, UserRole, WorkspaceId,
};
use ai_memory_mcp::auth::{AuthState, require_bearer};
use ai_memory_mcp::messages::{MessageState, router};
use ai_memory_store::{AccessMode, GrantLevel, Store, TokenPepper, generate_api_key, hash_token};
use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const ROOT: &str = "root-token";

struct Fixture {
    app: Router,
    store: Store,
    ws: WorkspaceId,
    open_a: ProjectId,
    open_b: ProjectId,
    secret: ProjectId,
    read_only: ProjectId,
    alice: String,
    _tmp: TempDir,
}

async fn rejecting_webhook() -> String {
    let app = Router::new().route(
        "/hook",
        axum::routing::post(|| async { StatusCode::FORBIDDEN }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn fixture(reject_admission: bool) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("w").await.unwrap();
    let mut projects = Vec::new();
    for name in ["open-a", "open-b", "secret", "read-only"] {
        projects.push(
            store
                .writer
                .get_or_create_project(ws, name, None)
                .await
                .unwrap(),
        );
    }
    let [open_a, open_b, secret, read_only] = projects[..] else {
        panic!("four projects");
    };
    for project in [secret, read_only] {
        store
            .writer
            .set_access_mode(project, AccessMode::Restricted)
            .await
            .unwrap();
    }
    let pepper = TokenPepper::new("message-routes");
    let user: UserId = store
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
        .grant_memory(user, read_only, GrantLevel::Read, None)
        .await
        .unwrap();
    let alice = generate_api_key().unwrap();
    store
        .writer
        .create_api_credential(
            ApiCredentialId::new(),
            user,
            "test".into(),
            hash_token(&alice, &pepper),
            None,
        )
        .await
        .unwrap();

    let mut wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    if reject_admission {
        wiki = wiki.with_admission_chain(
            AdmissionChain::new(vec![WebhookConfig {
                name: "mail-gate".into(),
                url: rejecting_webhook().await,
                timeout_ms: 2_000,
                failure_policy: FailurePolicy::Reject,
                events: vec![AdmissionOp::MessagePop, AdmissionOp::MessageCancel],
                blocking: true,
                headers: Default::default(),
                projects: Default::default(),
            }])
            .unwrap(),
        );
    }
    let auth = Arc::new(AuthState::new(Some(ROOT.into())).with_multiuser(
        pepper,
        store.reader.clone(),
        store.writer.clone(),
    ));
    let app = router(MessageState {
        reader: store.reader.clone(),
        writer: store.writer.clone(),
        wiki,
        trusted_proxy_identity: false,
    })
    .layer(axum::middleware::from_fn_with_state(auth, require_bearer));
    Fixture {
        app,
        store,
        ws,
        open_a,
        open_b,
        secret,
        read_only,
        alice,
        _tmp: tmp,
    }
}

impl Fixture {
    async fn seed(&self, from: ProjectId, to: ProjectId) -> String {
        self.store
            .writer
            .insert_message(NewAgentMessage {
                from_workspace_id: self.ws,
                from_project_id: from,
                from_agent: AgentKind::Other,
                from_session_id: None,
                from_owner_user: None,
                to_workspace_id: self.ws,
                to_project_id: to,
                to_agent: None,
                subject: None,
                body: "mail-body-sentinel".into(),
                nature: MessageNature::Unknown,
                evidence: None,
            })
            .await
            .unwrap()
            .to_string()
    }

    async fn pending(&self, project: ProjectId, mailbox: MessageBox) -> usize {
        self.store
            .reader
            .list_messages(self.ws, project, mailbox, 10)
            .await
            .unwrap()
            .len()
    }

    async fn post(&self, token: Option<&str>, uri: &str, body: Value) -> (StatusCode, Value) {
        self.post_with(token, uri, body, None).await
    }

    async fn post_with(
        &self,
        token: Option<&str>,
        uri: &str,
        body: Value,
        skip: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(token) = token {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        if let Some(skip) = skip {
            req = req.header(SKIP_ADMISSION_CHAIN_HEADER, skip);
        }
        reply(
            self.app.clone(),
            req.body(Body::from(body.to_string())).unwrap(),
        )
        .await
    }

    async fn list(&self, token: &str, project: &str, mailbox: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .uri(format!(
                "/messages?workspace=w&project={project}&box={mailbox}"
            ))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        reply(self.app.clone(), req).await
    }
}

async fn reply(app: Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn send_body(from: &str, to: &str) -> Value {
    json!({
        "from_workspace": "w", "from_project": from,
        "to_workspace": "w", "to_project": to,
        "body": "hello"
    })
}

fn scope(project: &str) -> Value {
    json!({ "workspace": "w", "project": project })
}

/// The reported defect: a normal API key can run every message operation.
#[tokio::test]
async fn a_database_user_round_trips_mail_between_open_projects() {
    let f = fixture(false).await;
    let token = f.alice.as_str();

    let (status, sent) = f
        .post(Some(token), "/messages/send", send_body("open-a", "open-b"))
        .await;
    assert_eq!(status, StatusCode::OK, "{sent}");

    let (status, inbox) = f.list(token, "open-b", "inbox").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["messages"][0]["from_owner_user"], "user:alice");

    let (status, popped) = f.post(Some(token), "/messages/pop", scope("open-b")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(popped["message"]["body"], "hello");
    assert!(popped["security_notice"].is_string());
    let (_, again) = f.post(Some(token), "/messages/pop", scope("open-b")).await;
    assert!(again["message"].is_null(), "a message pops exactly once");

    f.post(Some(token), "/messages/send", send_body("open-a", "open-b"))
        .await;
    let (status, cancelled) = f
        .post(Some(token), "/messages/cancel", scope("open-a"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["cancelled"], 1);
}

#[tokio::test]
async fn an_ungranted_user_cannot_touch_a_restricted_projects_mail() {
    let f = fixture(false).await;
    let token = Some(f.alice.as_str());
    let to_secret = f.seed(f.open_a, f.secret).await;
    f.seed(f.secret, f.open_a).await;

    let (status, body) = f.list(&f.alice, "secret", "inbox").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!body.to_string().contains("mail-body-sentinel"), "{body}");

    let (status, body) = f.post(token, "/messages/pop", scope("secret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!body.to_string().contains("mail-body-sentinel"), "{body}");
    let (status, _) = f
        .post(
            token,
            "/messages/pop",
            json!({ "workspace": "w", "project": "secret", "message_id": to_secret }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a known id is no way in");

    let (status, _) = f.post(token, "/messages/cancel", scope("secret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f
        .post(token, "/messages/send", send_body("open-a", "secret"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "delivery is a write");
    let (status, _) = f
        .post(token, "/messages/send", send_body("secret", "open-b"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cannot send as the project");

    assert_eq!(f.pending(f.secret, MessageBox::Inbox).await, 1);
    assert_eq!(f.pending(f.secret, MessageBox::Outbox).await, 1);
    assert_eq!(f.pending(f.open_b, MessageBox::Inbox).await, 0);

    // Control: root is not subject to project grants.
    let (status, popped) = f.post(Some(ROOT), "/messages/pop", scope("secret")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(popped["message"]["body"], "mail-body-sentinel");
}

#[tokio::test]
async fn a_read_grant_lists_but_cannot_change_a_mailbox() {
    let f = fixture(false).await;
    let token = Some(f.alice.as_str());
    f.seed(f.open_a, f.read_only).await;

    let (status, inbox) = f.list(&f.alice, "read-only", "inbox").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);

    let (status, _) = f.post(token, "/messages/pop", scope("read-only")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f.post(token, "/messages/cancel", scope("read-only")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f
        .post(token, "/messages/send", send_body("open-a", "read-only"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(f.pending(f.read_only, MessageBox::Inbox).await, 1);

    // Control: a read grant is enough to name the project as the sender.
    let (status, _) = f
        .post(token, "/messages/send", send_body("read-only", "open-b"))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_missing_or_unknown_bearer_is_refused() {
    let f = fixture(false).await;
    f.seed(f.open_a, f.open_b).await;
    for token in [None, Some("unknown-token")] {
        let (status, _) = f.post(token, "/messages/pop", scope("open-b")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?}");
    }
    assert_eq!(f.pending(f.open_b, MessageBox::Inbox).await, 1);
}

#[tokio::test]
async fn a_missing_project_is_a_json_404_and_is_never_created() {
    let f = fixture(false).await;
    let (status, body) = f.post(Some(ROOT), "/messages/pop", scope("typo")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The CLI tells this apart from an old server by the JSON `error`.
    assert!(body["error"].is_string(), "{body}");
}

#[tokio::test]
async fn an_unknown_receiving_agent_is_rejected_without_claiming() {
    let f = fixture(false).await;
    f.seed(f.open_a, f.open_b).await;
    let (status, _) = f
        .post(
            Some(f.alice.as_str()),
            "/messages/pop",
            json!({ "workspace": "w", "project": "open-b", "agent": "not-a-harness" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(f.pending(f.open_b, MessageBox::Inbox).await, 1);
}

#[tokio::test]
async fn admission_refuses_pop_and_cancel_and_only_root_can_skip_it() {
    let f = fixture(true).await;
    let alice = Some(f.alice.as_str());
    f.seed(f.open_a, f.open_b).await;

    let (status, _) = f.post(alice, "/messages/pop", scope("open-b")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = f.post(alice, "/messages/cancel", scope("open-a")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The loop-prevention header is root-only on this non-admin route.
    for uri_body in [
        ("/messages/pop", scope("open-b")),
        ("/messages/cancel", scope("open-a")),
    ] {
        let (status, _) = f
            .post_with(alice, uri_body.0, uri_body.1, Some("mail-gate"))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", uri_body.0);
    }
    assert_eq!(f.pending(f.open_b, MessageBox::Inbox).await, 1);

    let (status, popped) = f
        .post_with(
            Some(ROOT),
            "/messages/pop",
            scope("open-b"),
            Some("mail-gate"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(popped["message"]["body"], "mail-body-sentinel");
}
