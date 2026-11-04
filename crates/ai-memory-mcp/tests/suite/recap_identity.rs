//! `GET /recap` through the production auth stack.
//!
//! The recap's unit tests inject an `ActorContext` straight into the handler.
//! These drive the real `require_bearer` middleware with real `aim_` DB-user
//! tokens, so the identity the recap filters on is the one the middleware
//! derives from a credential, the same way `/hook` stamps a session's owner.
//! Every refusal must read exactly like an unknown session id: a 403, or a
//! body naming the project, would let a prober map session ids to projects.

use ai_memory_core::{ApiCredentialId, NewUser, SessionId, UserId, UserRole};
use ai_memory_hooks::{HookState, ProjectCacheStore, SubagentSessionSet, hook_router};
use ai_memory_mcp::auth::{AuthState, require_bearer};
use ai_memory_store::{AccessMode, GrantLevel, Store, TokenPepper, generate_api_key, hash_token};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

const ROOT_TOKEN: &str = "the-root-token";
const PROJECT: &str = "recap-shared";
const NO_SESSION: &str = "no session is recorded under this id for you";

struct Harness {
    router: Router,
    store: Store,
    ingest_semaphore: Arc<tokio::sync::Semaphore>,
    _tmp: TempDir,
}

struct DbUser {
    id: UserId,
    token: String,
}

async fn harness() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let baked = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .expect("baked");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    // One permit makes "the hook finished" observable: the handler takes it
    // before answering 202 and the spawned task releases it when done.
    let ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let hooks = hook_router(HookState {
        ingest_metrics: Arc::new(ai_memory_core::IngestMetrics::default()),
        workspace_id: ws,
        project_id: baked,
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        consolidator: None,
        sanitizer: ai_memory_core::Sanitizer::default(),
        project_cache: Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default())),
        active_project: ai_memory_core::ActiveProject::new(),
        ingest_semaphore: ingest_semaphore.clone(),
        ingest_gates: ai_memory_hooks::IngestGates::default(),
        cwd_probes: ai_memory_hooks::CwdProbes::default(),
        ingest_drop_list: ai_memory_hooks::IngestDropList::default(),
        record_tool_names: false,
        skip_wiki_checkpoints: false,
        consolidate_on_session_end: false,
        session_consolidation_notify: None,
        profile_notify: None,
        capture_assistant_enabled: false,
        claim_handoff_on_session_start: false,
        create_handoff_on_session_end: false,
        per_user_slots: false,
        mid_session_routing: ai_memory_core::MidSessionRouting::default(),
        profile: ai_memory_core::profile::ProfileSettings::default(),
        subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
        ingest_rate: Arc::new(tokio::sync::Mutex::new(
            ai_memory_hooks::IngestRateLimiter::disabled(),
        )),
        home_dir: None,
        trusted_proxy_identity: false,
        index_file_paths: false,
    });
    let auth = AuthState::new(Some(ROOT_TOKEN.to_string())).with_multiuser(
        TokenPepper::new("test-pepper-recap-identity-1234567890ab"),
        store.reader.clone(),
        store.writer.clone(),
    );
    let router = hooks.layer(axum::middleware::from_fn_with_state(
        Arc::new(auth),
        require_bearer,
    ));
    Harness {
        router,
        store,
        ingest_semaphore,
        _tmp: tmp,
    }
}

async fn db_user(h: &Harness, username: &str) -> DbUser {
    let pepper = TokenPepper::new("test-pepper-recap-identity-1234567890ab");
    let token = generate_api_key().expect("token");
    let id = h
        .store
        .writer
        .create_human_user(
            NewUser {
                username: username.into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .expect("user");
    h.store
        .writer
        .create_api_credential(
            ApiCredentialId::new(),
            id,
            "test".into(),
            hash_token(&token, &pepper),
            None,
        )
        .await
        .expect("credential");
    DbUser { id, token }
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

/// POST one hook event as `token` and wait until the server has stored it.
async fn hook(h: &Harness, token: &str, session: &str, event: &str, prompt: Option<&str>) {
    let mut body = json!({ "session_id": session, "cwd": format!("/tmp/aim-test/{PROJECT}") });
    if let Some(prompt) = prompt {
        body["prompt"] = json!(prompt);
    }
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/hook?event={event}&agent=claude-code&project={PROJECT}"
        ))
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("hook req");
    let resp = h.router.clone().oneshot(req).await.expect("hook");
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "{event} not accepted");
    let _ = body_text(resp).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.ingest_semaphore.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("hook processing finished");
}

/// A session with one compaction window holding `WINDOW-PROMPT-<tag>`.
async fn compacted_session(h: &Harness, token: &str, session: &str, tag: &str) {
    hook(
        h,
        token,
        session,
        "user-prompt-submit",
        Some("an older prompt"),
    )
    .await;
    hook(h, token, session, "pre-compact", None).await;
    let window = format!("WINDOW-PROMPT-{tag}");
    hook(h, token, session, "user-prompt-submit", Some(&window)).await;
    hook(h, token, session, "pre-compact", None).await;
}

async fn recap(h: &Harness, token: &str, session: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .uri(format!("/recap?session_id={session}&agent=claude-code"))
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("recap req");
    let resp = h.router.clone().oneshot(req).await.expect("recap");
    let status = resp.status();
    (status, body_text(resp).await)
}

fn assert_reads_as_unknown(status: StatusCode, body: &str) {
    assert_eq!(
        status,
        StatusCode::OK,
        "a refusal must not be a distinct status: {body}"
    );
    assert!(
        body.contains(NO_SESSION),
        "expected the unknown-id notice: {body}"
    );
    assert!(
        !body.contains("WINDOW-PROMPT"),
        "prompt text leaked: {body}"
    );
    assert!(!body.contains(PROJECT), "project name leaked: {body}");
}

#[tokio::test]
async fn a_db_user_cannot_read_another_users_session_through_the_middleware() {
    let h = harness().await;
    let alice = db_user(&h, "alice").await;
    let bob = db_user(&h, "bob").await;
    let session = "11111111-1111-4111-8111-111111111111";
    compacted_session(&h, &alice.token, session, "ALICE").await;

    let (status, body) = recap(&h, &bob.token, session).await;
    assert_reads_as_unknown(status, &body);

    // Control: the owner's own token reads the window.
    let (status, body) = recap(&h, &alice.token, session).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("WINDOW-PROMPT-ALICE"), "{body}");
}

#[tokio::test]
async fn an_unattributed_caller_cannot_read_an_owned_session() {
    let h = harness().await;
    let alice = db_user(&h, "alice").await;
    let owned = "22222222-2222-4222-8222-222222222222";
    compacted_session(&h, &alice.token, owned, "ALICE").await;

    // The root token without `root_username` carries no actor: the same
    // unattributed identity `/handoff` filters on.
    let (status, body) = recap(&h, ROOT_TOKEN, owned).await;
    assert_reads_as_unknown(status, &body);

    // Control: an unowned session written by that caller stays readable to it.
    let unowned = "33333333-3333-4333-8333-333333333333";
    compacted_session(&h, ROOT_TOKEN, unowned, "SHARED").await;
    let (status, body) = recap(&h, ROOT_TOKEN, unowned).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("WINDOW-PROMPT-SHARED"), "{body}");
}

#[tokio::test]
async fn a_db_user_without_a_grant_reads_their_own_restricted_session_as_unknown() {
    let h = harness().await;
    let alice = db_user(&h, "alice").await;
    let bob = db_user(&h, "bob").await;
    // Alice's first hook creates the project, so she is its creator and Bob,
    // who writes his own session into it while it is open, is not.
    let first = "55555555-5555-4555-8555-555555555555";
    hook(
        &h,
        &alice.token,
        first,
        "user-prompt-submit",
        Some("alice was here"),
    )
    .await;
    let session = "44444444-4444-4444-8444-444444444444";
    compacted_session(&h, &bob.token, session, "BOB").await;
    let (_, project) = h
        .store
        .reader
        .session_project_ids(SessionId::from_native(session))
        .await
        .expect("lookup")
        .expect("session stored");
    h.store
        .writer
        .set_access_mode(project, AccessMode::Restricted)
        .await
        .expect("restrict");

    // The owner filter would pass; only the grant check refuses.
    let (status, body) = recap(&h, &bob.token, session).await;
    assert_reads_as_unknown(status, &body);

    // Control: a read grant restores it.
    h.store
        .writer
        .grant_memory(bob.id, project, GrantLevel::Read, None)
        .await
        .expect("grant");
    let (status, body) = recap(&h, &bob.token, session).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("WINDOW-PROMPT-BOB"), "{body}");
}

async fn anchor(h: &Harness, token: &str, session: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .uri(format!(
            "/recap?session_id={session}&agent=claude-code&mode=anchor"
        ))
        .header("host", "localhost")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("anchor req");
    let resp = h.router.clone().oneshot(req).await.expect("anchor");
    let status = resp.status();
    (status, body_text(resp).await)
}

/// The post-compaction anchor reads the whole session, not one window, so it
/// gets the same identity gates as the recap: another DB user and an
/// unattributed root token both read an owned session as unknown.
#[tokio::test]
async fn the_anchor_refuses_another_users_session_through_the_middleware() {
    let h = harness().await;
    let alice = db_user(&h, "alice").await;
    let bob = db_user(&h, "bob").await;
    let session = "66666666-6666-4666-8666-666666666666";
    compacted_session(&h, &alice.token, session, "ALICE").await;

    for token in [bob.token.as_str(), ROOT_TOKEN] {
        let (status, body) = anchor(&h, token, session).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains(NO_SESSION), "{body}");
        assert!(
            !body.contains("WINDOW-PROMPT"),
            "prompt text leaked: {body}"
        );
        assert!(
            !body.contains("an older prompt"),
            "prompt text leaked: {body}"
        );
        assert!(!body.contains(PROJECT), "project name leaked: {body}");
    }
    let (_, unknown) = anchor(&h, &bob.token, "77777777-7777-4777-8777-777777777777").await;
    let (_, refused) = anchor(&h, &bob.token, session).await;
    assert_eq!(unknown, refused, "a refusal must read like an unknown id");

    // Control: the owner's anchor holds the first prompt and the latest one.
    let (status, body) = anchor(&h, &alice.token, session).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("post-compaction anchor for this session"),
        "{body}"
    );
    let first = body.find("an older prompt").expect("first prompt");
    let latest = body.find("WINDOW-PROMPT-ALICE").expect("latest prompt");
    assert!(first < latest, "{body}");
}
