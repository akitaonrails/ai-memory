//! Who a request names, and what that means for a handoff's owner.
//!
//! Two independent decisions have to agree or a baton lands somewhere its
//! author cannot reach:
//!
//! 1. **Which field names the human.** The auth middleware resolves a proxy
//!    that asserts a complete OIDC issuer/subject pair to `AuthLevel::User` —
//!    it named somebody. If ownership keys on `actor.user` alone, that request is
//!    "nobody" when a row is stamped, so every operator behind such a proxy
//!    writes into the one shared bucket and reads each other's prompt trails.
//! 2. **Whether the deployment tells operators apart at all.** A server with
//!    `[auth].bearer_token` + `[auth].root_username`, no `users` rows and no
//!    proxy has exactly one operator, but two transports: HTTP requests carry
//!    that one name, while the local stdio / in-process transport carries no
//!    actor. Stamping the name splits one person in half — what they write
//!    over HTTP becomes invisible to their own local CLI on the same data
//!    directory.
//!
//! These drive the real tools through the production JSON-RPC transport with
//! the production `require_bearer` middleware in front, so the rungs under test
//! are the ones an operator actually configures.

use ai_memory_core::ActorContext;
use ai_memory_mcp::AiMemoryServer;
use ai_memory_mcp::auth::{AuthState, require_bearer};
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const ROOT_TOKEN: &str = "the-root-token";
const PROXY_TOKEN: &str = "the-proxy-token";

struct Harness {
    /// The HTTP transport: production `require_bearer` in front of `/mcp`.
    http: Router,
    /// The stdio / in-process transport, modelled as the SAME server mounted
    /// with no auth layer at all — a call therefore carries neither an
    /// `ActorContext` nor an `AuthLevel`, which is exactly what a local
    /// `ai-memory` process or an editor's stdio MCP client produces.
    local: Router,
    store: Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    _tmp: TempDir,
}

fn mount(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

/// `root_username` is `[auth].root_username`; `proxy` is
/// `[auth].actor_proxy_bearer_token`, which is also what makes the deployment
/// distinguish operators without ever writing a `users` row.
async fn harness(root_username: Option<&str>, proxy: bool) -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");

    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_trusted_proxy_identity(proxy);

    let mut auth_state = AuthState::new(Some(ROOT_TOKEN.to_string()));
    if let Some(user) = root_username {
        auth_state = auth_state.with_root_actor(ActorContext {
            user: Some(user.to_string()),
            ..ActorContext::default()
        });
    }
    if proxy {
        auth_state = auth_state.with_trusted_proxy_bearer(PROXY_TOKEN);
    }
    let http = mount(server.clone()).layer(axum::middleware::from_fn_with_state(
        Arc::new(auth_state),
        require_bearer,
    ));

    Harness {
        http,
        local: mount(server),
        store,
        ws,
        proj,
        _tmp: tmp,
    }
}

async fn call(router: &Router, name: &str, arguments: Value, headers: &[(&str, &str)]) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).expect("mcp req"))
        .await
        .expect("oneshot");
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"));
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}\nfull: {text}");
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {text}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&joined).unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}"))
}

fn root_headers() -> Vec<(&'static str, &'static str)> {
    vec![("authorization", "Bearer the-root-token")]
}

fn proxied(header: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
    let mut headers = vec![("authorization", "Bearer the-proxy-token"), (header, value)];
    if header == "x-memory-actor-sub" {
        headers.push(("x-memory-actor-issuer", "https://idp.example"));
    }
    headers
}

async fn begin(router: &Router, summary: &str, headers: &[(&str, &str)]) {
    let begun = call(
        router,
        "memory_handoff_begin",
        json!({ "workspace": "default", "project": "scratch", "summary": summary }),
        headers,
    )
    .await;
    assert!(
        begun.get("handoff_id").is_some(),
        "handoff not created: {begun}"
    );
}

/// The accepted handoff's summary, or `None` when the caller was shown nothing.
async fn accept(router: &Router, headers: &[(&str, &str)]) -> Option<String> {
    let got = call(
        router,
        "memory_handoff_accept",
        json!({ "workspace": "default", "project": "scratch" }),
        headers,
    )
    .await;
    got.pointer("/handoff/summary")
        .and_then(|s| s.as_str())
        .map(str::to_owned)
}

/// A single-operator server that happens to name its operator: one person, one
/// data directory, two transports. What the HTTP side writes must stay visible
/// to the local one, because there is nobody else it could belong to.
#[tokio::test]
async fn single_operator_handoff_crosses_between_http_and_local_transports() {
    let h = harness(Some("dj"), false).await;

    begin(&h.http, "the-http-baton", &root_headers()).await;

    assert_eq!(
        accept(&h.local, &[]).await.as_deref(),
        Some("the-http-baton"),
        "the operator's own local transport cannot see what they wrote over HTTP",
    );
}

/// …and the row itself carries no owner, so it is shared exactly as it was
/// before ownership existed. Pins the stamp, not just the read that follows it.
#[tokio::test]
async fn single_operator_handoff_is_stamped_shared() {
    let h = harness(Some("dj"), false).await;

    begin(&h.http, "the-http-baton", &root_headers()).await;

    let rows = h
        .store
        .reader
        .list_handoffs(h.ws, h.proj, None, ai_memory_core::OwnerFilter::Any, 10)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].origin.owner_user, None,
        "a deployment that separates nobody must stamp nobody",
    );
}

/// An ingress that terminates OIDC and forwards the stable issuer/subject pair
/// names a human. Ownership has to retain both parts or proxied operators can
/// share a bucket and read batons synthesised from a colleague's prompts.
#[tokio::test]
async fn oidc_proxy_operators_do_not_share_a_bucket() {
    let h = harness(Some("dj"), true).await;

    begin(
        &h.http,
        "alices-baton",
        &proxied("x-memory-actor-sub", "oidc-subject-alice"),
    )
    .await;

    assert_eq!(
        accept(&h.http, &proxied("x-memory-actor-sub", "oidc-subject-bob")).await,
        None,
        "a second proxied operator consumed a baton asserted for somebody else",
    );
    assert_eq!(
        accept(
            &h.http,
            &proxied("x-memory-actor-sub", "oidc-subject-alice")
        )
        .await
        .as_deref(),
        Some("alices-baton"),
        "the operator who created the baton must still be able to claim it",
    );
}

/// The named-user rung of the same rule, unchanged: two operators a deployment
/// really does tell apart stay isolated from each other.
#[tokio::test]
async fn named_proxy_operators_stay_isolated() {
    let h = harness(Some("dj"), true).await;

    begin(
        &h.http,
        "alices-baton",
        &proxied("x-memory-actor-user", "alice"),
    )
    .await;

    assert_eq!(
        accept(&h.http, &proxied("x-memory-actor-user", "bob")).await,
        None,
        "bob claimed alice's baton",
    );
    assert_eq!(
        accept(&h.http, &proxied("x-memory-actor-user", "alice"))
            .await
            .as_deref(),
        Some("alices-baton"),
    );
}

/// Absent = shared. A handoff with no owner — every row written before
/// ownership existed, and everything an unattributed caller writes — stays
/// visible to every reader in every mode, named or not.
#[tokio::test]
async fn legacy_unowned_handoffs_stay_visible_to_everyone() {
    let h = harness(Some("dj"), true).await;
    for i in 0..3 {
        h.store
            .writer
            .insert_handoff(ai_memory_core::NewHandoff {
                workspace_id: h.ws,
                project_id: h.proj,
                from_session_id: None,
                from_agent: ai_memory_core::AgentKind::ClaudeCode,
                to_agent: None,
                cwd: None,
                summary: format!("legacy-baton-{i}"),
                open_questions: Vec::new(),
                next_steps: Vec::new(),
                files_touched: Vec::new(),
                owner_user: None,
            })
            .await
            .expect("insert legacy handoff");
    }

    for (label, headers) in [
        ("named proxy user", proxied("x-memory-actor-user", "alice")),
        (
            "OIDC proxy user",
            proxied("x-memory-actor-sub", "oidc-subject-bob"),
        ),
        ("root with no assertion", root_headers()),
    ] {
        assert!(
            accept(&h.http, &headers)
                .await
                .is_some_and(|s| s.starts_with("legacy-baton-")),
            "{label} was refused a handoff that belongs to nobody",
        );
    }
}

/// `memory_handoff_accept`'s `status`, checked against the body: a handoff
/// comes back exactly when the status says this call claimed one.
async fn accept_status(router: &Router, headers: &[(&str, &str)]) -> String {
    let got = call(
        router,
        "memory_handoff_accept",
        json!({ "workspace": "default", "project": "scratch" }),
        headers,
    )
    .await;
    let status = got
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no status in {got}"))
        .to_owned();
    assert_eq!(
        got.get("handoff").is_some_and(|h| !h.is_null()),
        status == "claimed",
        "the body and the status disagree: {got}",
    );
    status
}

fn with_session(
    mut headers: Vec<(&'static str, &'static str)>,
    session: &'static str,
) -> Vec<(&'static str, &'static str)> {
    headers.push(("x-memory-actor-session-id", session));
    headers
}

/// `consumed_by_hook` tells an agent the baton is already in its context, so it
/// is said only to the session whose SessionStart claimed it. The session id on
/// the request is routing data a client can forge: a colleague forwarding that
/// id, the same operator from another session, and a request with no session
/// at all must each hear `none_pending`, not a pointer at someone else's context.
#[tokio::test]
async fn consumed_by_hook_is_reported_only_to_the_session_that_claimed_it() {
    use ai_memory_core::{AgentKind, HandoffAcceptance, NewSession, OwnerFilter, SessionId};

    let h = harness(Some("dj"), true).await;
    let alice = proxied("x-memory-actor-user", "alice");
    assert_eq!(accept_status(&h.http, &alice).await, "none_pending");

    begin(&h.http, "alices-baton", &alice).await;
    let rows = h
        .store
        .reader
        .list_handoffs(h.ws, h.proj, None, OwnerFilter::Any, 10)
        .await
        .expect("list");
    let owner = rows[0]
        .origin
        .owner_user
        .clone()
        .expect("a proxied operator owns the baton");

    // What the SessionStart hook does: claim the baton for Alice's session under
    // the native id her session-aware client then forwards on every MCP call.
    let session = SessionId::from_native("alice-claude-session");
    h.store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: h.ws,
            project_id: h.proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: Some(owner.clone()),
        })
        .await
        .expect("receiving session");
    let claimed = h
        .store
        .writer
        .accept_handoff(HandoffAcceptance {
            handoff_id: rows[0].scope.id,
            workspace_id: h.ws,
            project_id: h.proj,
            accepting_agent: AgentKind::ClaudeCode,
            accepting_session: Some(session),
            accepting_user: Some(owner.clone()),
            owner_filter: OwnerFilter::User(owner),
            receiving_cwd: None,
        })
        .await
        .expect("session-start claim");
    assert!(claimed);

    assert_eq!(
        accept_status(
            &h.http,
            &with_session(alice.clone(), "alice-claude-session")
        )
        .await,
        "consumed_by_hook",
        "the session that received the baton must be told it is in its context",
    );
    for (label, headers) in [
        (
            "bob forwarding alice's session id",
            with_session(
                proxied("x-memory-actor-user", "bob"),
                "alice-claude-session",
            ),
        ),
        (
            "alice from another session",
            with_session(alice.clone(), "alice-other-session"),
        ),
        ("alice with no session id", alice.clone()),
    ] {
        assert_eq!(
            accept_status(&h.http, &headers).await,
            "none_pending",
            "{label} was pointed at a handoff that is not in its context",
        );
    }

    // A baton left after the session started is still claimed normally.
    begin(&h.http, "a-later-baton", &alice).await;
    assert_eq!(
        accept_status(&h.http, &with_session(alice, "alice-claude-session")).await,
        "claimed",
    );
}

#[tokio::test]
async fn targeted_handoff_filters_mismatched_agent_and_accept_by_id_bypasses() {
    use ai_memory_core::{AgentKind, HandoffId};
    use std::str::FromStr;

    let h = harness(None, true).await;

    let claude_headers = [
        ("authorization", "Bearer the-proxy-token"),
        ("x-memory-actor-user", "alice"),
        ("x-memory-actor-agent", "claude-code"),
    ];
    let codex_headers = [
        ("authorization", "Bearer the-proxy-token"),
        ("x-memory-actor-user", "alice"),
        ("x-memory-actor-agent", "codex"),
    ];

    // Alice as claude-code creates a handoff targeted to codex.
    let targeted_res = call(
        &h.http,
        "memory_handoff_begin",
        json!({
            "workspace": "default",
            "project": "scratch",
            "summary": "baton targeted to codex",
            "to_agent": "codex",
        }),
        &claude_headers,
    )
    .await;
    let targeted_id_str = targeted_res["handoff_id"]
        .as_str()
        .expect("handoff_id")
        .to_string();
    let targeted_id = HandoffId::from_str(&targeted_id_str).unwrap();

    // Verify metadata stored in db: from_agent is claude-code, to_agent is codex.
    let stored = h
        .store
        .reader
        .handoff_by_id(targeted_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.origin.from_agent, AgentKind::ClaudeCode);
    assert_eq!(stored.origin.to_agent, Some(AgentKind::Codex));

    // Claude Code calling implicit accept: filtered out, none pending.
    let claude_accept = call(
        &h.http,
        "memory_handoff_accept",
        json!({ "workspace": "default", "project": "scratch" }),
        &claude_headers,
    )
    .await;
    assert!(
        claude_accept["handoff"].is_null(),
        "claude-code must not claim a handoff targeted to codex"
    );
    assert_eq!(claude_accept["status"].as_str(), Some("none_pending"));

    // Now create a second, untargeted handoff.
    let untargeted_res = call(
        &h.http,
        "memory_handoff_begin",
        json!({
            "workspace": "default",
            "project": "scratch",
            "summary": "general baton for anyone",
        }),
        &claude_headers,
    )
    .await;
    let untargeted_id_str = untargeted_res["handoff_id"]
        .as_str()
        .expect("handoff_id")
        .to_string();

    // Claude Code calling implicit accept now: skips the targeted handoff and claims the untargeted one!
    let claude_accept_general = call(
        &h.http,
        "memory_handoff_accept",
        json!({ "workspace": "default", "project": "scratch" }),
        &claude_headers,
    )
    .await;
    assert_eq!(
        claude_accept_general["status"].as_str(),
        Some("claimed"),
        "claude-code must fall through to the untargeted handoff"
    );
    assert_eq!(
        claude_accept_general["handoff"]["id"].as_str(),
        Some(untargeted_id_str.as_str())
    );

    // Explicit claim by exact handoff_id: Claude Code claims the targeted handoff by id.
    // Exact-id claims bypass targeting per design.
    let claude_accept_targeted_by_id = call(
        &h.http,
        "memory_handoff_accept",
        json!({
            "workspace": "default",
            "project": "scratch",
            "handoff_id": targeted_id_str,
        }),
        &claude_headers,
    )
    .await;
    assert_eq!(
        claude_accept_targeted_by_id["status"].as_str(),
        Some("claimed"),
        "exact-id claim must bypass targeting"
    );
    assert_eq!(
        claude_accept_targeted_by_id["handoff"]["id"].as_str(),
        Some(targeted_id_str.as_str())
    );

    // Verify targeted claim by matching agent:
    // Create another handoff targeted to codex.
    let targeted2_res = call(
        &h.http,
        "memory_handoff_begin",
        json!({
            "workspace": "default",
            "project": "scratch",
            "summary": "second baton for codex",
            "to_agent": "codex",
        }),
        &claude_headers,
    )
    .await;
    let targeted2_id_str = targeted2_res["handoff_id"]
        .as_str()
        .expect("handoff_id")
        .to_string();

    // Codex calling implicit accept: claims the targeted handoff directly!
    let codex_accept = call(
        &h.http,
        "memory_handoff_accept",
        json!({ "workspace": "default", "project": "scratch" }),
        &codex_headers,
    )
    .await;
    assert_eq!(
        codex_accept["status"].as_str(),
        Some("claimed"),
        "matching target agent must claim the targeted handoff"
    );
    assert_eq!(
        codex_accept["handoff"]["id"].as_str(),
        Some(targeted2_id_str.as_str())
    );
}
