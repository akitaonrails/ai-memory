//! Adversarial tests for browser-session decisions on pending writes.
//!
//! The builtin `/web/pending` triage page posts approve and reject from the
//! browser, with the password session cookie and the CSRF header, to the
//! existing `/admin/pending-writes/{id}/approve|reject` handlers. These tests
//! drive that exact path through the production `require_dual_auth` and the
//! root gate on the admin router: every refused attempt must leave the
//! proposal pending and the target file absent.

use std::sync::Arc;

use ai_memory_core::{
    ActorContext, AutoImproveProposalId, NewUser, PagePath, ProjectId, UserId, UserRole,
    WorkspaceId,
};
use ai_memory_mcp::auth::AuthState;
use ai_memory_mcp::human_auth::{CSRF_COOKIE, CSRF_HEADER, LoginLimiter, SESSION_COOKIE};
use ai_memory_mcp::{
    AdminState, HumanAuthRuntime, admin_router, public_auth_router, require_dual_auth,
};
use ai_memory_store::{
    AutoImproveProposalOperation, AutoImproveProposalStatus, DecayParams, NewAutoImproveProposal,
    StageAutoImproveRun, Store,
};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tempfile::TempDir;
use tower::ServiceExt;

const ROOT_PASSWORD: &str = "root-password-12";
const USER_PASSWORD: &str = "alice-password-12";
const TARGET: &str = "notes/pending.md";

struct Session {
    session: String,
    csrf: String,
}

struct Harness {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    router: Router,
    root_id: UserId,
    root: Session,
    user: Session,
    ws: WorkspaceId,
    proj: ProjectId,
    proposal: AutoImproveProposalId,
}

async fn harness() -> Harness {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());

    let root_id = create_user(&store, "root", UserRole::Root, ROOT_PASSWORD).await;
    create_user(&store, "alice", UserRole::User, USER_PASSWORD).await;
    let (ws, proj, proposal) = stage(&store).await;

    let auth = Arc::new(
        AuthState::new(Some("root-bearer".into()))
            .with_root_actor(ActorContext {
                user: Some("root".into()),
                ..ActorContext::default()
            })
            .with_human(HumanAuthRuntime {
                reader: store.reader.clone(),
                writer: store.writer.clone(),
                recovery_token_hash: None,
                root_username: "root".into(),
                root_name: None,
                root_email: None,
                reserved_passwords: vec!["root-bearer".into()],
                trusted_proxy_cidrs: Vec::new(),
                limiter: Arc::new(LoginLimiter::default()),
            }),
    );
    let root = login(auth.clone(), "root", ROOT_PASSWORD).await;
    let user = login(auth.clone(), "alice", USER_PASSWORD).await;
    let router = admin_router(admin_state(&tmp, &store, wiki.clone())).layer(
        axum::middleware::from_fn_with_state(auth, require_dual_auth),
    );

    Harness {
        _tmp: tmp,
        store,
        wiki,
        router,
        root_id,
        root,
        user,
        ws,
        proj,
        proposal,
    }
}

async fn create_user(store: &Store, username: &str, role: UserRole, password: &str) -> UserId {
    let phc = ai_memory_store::password::hash_password(password.to_owned())
        .await
        .unwrap();
    let new_user = NewUser {
        username: username.into(),
        name: None,
        email: None,
    };
    store
        .writer
        .create_human_user(new_user, role, Some(phc), false)
        .await
        .unwrap()
}

async fn stage(store: &Store) -> (WorkspaceId, ProjectId, AutoImproveProposalId) {
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let staged = store
        .writer
        .stage_auto_improve_run(StageAutoImproveRun {
            workspace_id: ws,
            project_id: proj,
            session_id: None,
            provider: Some("test".into()),
            model: Some("model".into()),
            summary: Some("summary".into()),
            warnings_json: serde_json::json!([]),
            rejected_candidates_json: serde_json::json!([]),
            config_json: serde_json::json!({"mode": "stage"}),
            proposal_actor: ActorContext {
                agent: Some("auto_improve".into()),
                ..ActorContext::default()
            },
            proposals: vec![NewAutoImproveProposal {
                operation: AutoImproveProposalOperation::Create,
                target_path: PagePath::new(TARGET).unwrap(),
                kind: "note".into(),
                title: "Pending title".into(),
                confidence: 0.9,
                rationale: "rationale".into(),
                evidence_json: serde_json::json!([{"source": "test"}]),
                body_markdown: "# Pending\n\nproposed body".into(),
                artifact_sha256: None,
                edit_mode: None,
                patch_json: None,
                expected_base_body_sha256: None,
            }],
        })
        .await
        .unwrap();
    (ws, proj, staged.proposal_ids[0])
}

fn admin_state(tmp: &TempDir, store: &Store, wiki: Wiki) -> AdminState {
    AdminState {
        ingest_metrics: Arc::new(ai_memory_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: true,
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
        bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    }
}

fn set_cookie(headers: &axum::http::HeaderMap, name: &str) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix(&format!("{name}=")))
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("{name} cookie missing"))
        .to_owned()
}

async fn login(auth: Arc<AuthState>, username: &str, password: &str) -> Session {
    let resp = public_auth_router(auth)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "username": username,
                        "password": password,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login {username}");
    Session {
        session: set_cookie(resp.headers(), SESSION_COOKIE),
        csrf: set_cookie(resp.headers(), CSRF_COOKIE),
    }
}

/// What the browser sends for a decision.
enum Csrf<'a> {
    None,
    Header(&'a str),
}

impl Harness {
    async fn decide(&self, action: &str, session: Option<&Session>, csrf: Csrf<'_>) -> StatusCode {
        let body = if action == "reject" {
            serde_json::json!({"reason": "not useful"})
        } else {
            serde_json::json!({})
        };
        let mut req = Request::builder()
            .method("POST")
            .uri(format!(
                "/admin/pending-writes/{}/{action}?workspace=default&project=scratch",
                self.proposal
            ))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(s) = session {
            req = req.header(
                header::COOKIE,
                format!("{SESSION_COOKIE}={}; {CSRF_COOKIE}={}", s.session, s.csrf),
            );
        }
        if let Csrf::Header(value) = csrf {
            req = req.header(CSRF_HEADER, value);
        }
        self.router
            .clone()
            .oneshot(
                req.body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    async fn detail(&self) -> ai_memory_store::AutoImproveProposalDetail {
        self.store
            .reader
            .auto_improve_proposal_detail(self.ws, self.proj, self.proposal)
            .await
            .unwrap()
            .unwrap()
    }

    fn target_file(&self) -> std::path::PathBuf {
        self.wiki
            .abs_path(self.ws, self.proj, &PagePath::new(TARGET).unwrap())
    }

    async fn assert_untouched(&self, context: &str) {
        assert_eq!(
            self.detail().await.summary.status,
            AutoImproveProposalStatus::Pending,
            "{context}: proposal left the queue"
        );
        assert!(
            !self.target_file().exists(),
            "{context}: target file was written"
        );
        assert!(
            self.store
                .reader
                .page_body_by_ids(self.ws, self.proj, TARGET)
                .await
                .unwrap()
                .is_none(),
            "{context}: target page was indexed"
        );
    }
}

#[tokio::test]
async fn anonymous_browser_cannot_decide_a_pending_write() {
    let h = harness().await;
    for action in ["approve", "reject"] {
        let status = h.decide(action, None, Csrf::None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous {action}");
        h.assert_untouched(&format!("anonymous {action}")).await;
    }
}

#[tokio::test]
async fn non_root_session_cannot_decide_a_pending_write() {
    let h = harness().await;
    for action in ["approve", "reject"] {
        let status = h
            .decide(action, Some(&h.user), Csrf::Header(&h.user.csrf))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "db user {action}");
        h.assert_untouched(&format!("db user {action}")).await;
    }
}

#[tokio::test]
async fn root_session_without_a_matching_csrf_header_is_refused() {
    let h = harness().await;
    for action in ["approve", "reject"] {
        let missing = h.decide(action, Some(&h.root), Csrf::None).await;
        assert_eq!(missing, StatusCode::FORBIDDEN, "missing csrf {action}");
        h.assert_untouched(&format!("missing csrf {action}")).await;

        // Another session's token does not stand in for this one's.
        let foreign = h
            .decide(action, Some(&h.root), Csrf::Header(&h.user.csrf))
            .await;
        assert_eq!(foreign, StatusCode::FORBIDDEN, "foreign csrf {action}");
        h.assert_untouched(&format!("foreign csrf {action}")).await;
    }
}

#[tokio::test]
async fn root_session_approval_keeps_auto_improve_provenance_and_the_approver() {
    let h = harness().await;
    let status = h
        .decide("approve", Some(&h.root), Csrf::Header(&h.root.csrf))
        .await;
    assert_eq!(status, StatusCode::OK);

    let detail = h.detail().await;
    assert_eq!(detail.summary.status, AutoImproveProposalStatus::Approved);
    assert_eq!(detail.decided_by_author_id, Some(h.root_id));
    assert_eq!(
        detail
            .decided_by_actor_json
            .as_ref()
            .and_then(|a| a["user"].as_str()),
        Some("root")
    );
    // The stage event still names the automated proposer.
    let staged = detail
        .events
        .iter()
        .find(|e| e.event == "staged")
        .expect("staged event");
    assert_eq!(staged.actor_json["agent"], "auto_improve");

    let file = std::fs::read_to_string(h.target_file()).expect("approval writes the page");
    assert!(file.contains("proposed body"), "{file}");
    assert!(
        file.contains(&format!("auto_improve_proposal_id: {}", h.proposal)),
        "{file}"
    );
    assert!(
        file.contains(&format!("auto_improve_run_id: {}", detail.summary.run_id)),
        "{file}"
    );
    assert!(file.contains("last_modified_by"), "{file}");
    assert!(file.contains("username: root"), "{file}");
}

#[tokio::test]
async fn root_session_rejection_records_the_decision_and_writes_no_file() {
    let h = harness().await;
    let status = h
        .decide("reject", Some(&h.root), Csrf::Header(&h.root.csrf))
        .await;
    assert_eq!(status, StatusCode::OK);

    let detail = h.detail().await;
    assert_eq!(detail.summary.status, AutoImproveProposalStatus::Rejected);
    assert_eq!(detail.decision_reason.as_deref(), Some("not useful"));
    assert_eq!(detail.decided_by_author_id, Some(h.root_id));
    assert!(!h.target_file().exists(), "reject wrote the target file");
    assert!(
        h.store
            .reader
            .page_body_by_ids(h.ws, h.proj, TARGET)
            .await
            .unwrap()
            .is_none(),
        "reject indexed the target page"
    );
}
