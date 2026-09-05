//! The MCP handoff tools and the admission chain.
//!
//! Handoff ops carry no page and no body, so the chain has nothing to mutate:
//! the only reason to run a webhook BEFORE the operation is that it can refuse
//! it. Everything else is an observer, and an observer is owed the truth — it
//! must hear about a handoff that was actually created or claimed, and about
//! nothing else. `memory_handoff_accept` is the sharp case: its routine answer
//! is `{"handoff": null}` (the SessionStart hook has usually already consumed
//! the baton), so announcing the accept up front tells a mirror about accepts
//! that never happen, on most calls.
//!
//! These tests drive the tools through the production JSON-RPC transport, the
//! same way `autoscope_multiuser.rs` does, and point the chain at a real
//! webhook host that records every request it receives.

use ai_memory_mcp::AiMemoryServer;
use ai_memory_mcp::auth::{AuthState, require_bearer};
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

/// One recorded webhook request: which hook was called, for which op.
type Calls = Arc<Mutex<Vec<(String, String)>>>;

/// A webhook host with one route per hook, answering `204` and recording the
/// `X-Memory-Op` header it was called with.
async fn recording_webhook_host() -> (std::net::SocketAddr, Calls) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let mut app = axum::Router::new();
    for hook in ["guard", "mirror", "observer"] {
        let seen = calls.clone();
        app = app.route(
            &format!("/{hook}"),
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let seen = seen.clone();
                async move {
                    let op = headers
                        .get("x-memory-op")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen.lock().unwrap().push((hook.to_string(), op));
                    StatusCode::NO_CONTENT
                }
            }),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, calls)
}

fn hook(
    name: &str,
    addr: std::net::SocketAddr,
    blocking: bool,
    events: Vec<ai_memory_wiki::AdmissionOp>,
) -> ai_memory_wiki::WebhookConfig {
    ai_memory_wiki::WebhookConfig {
        name: name.to_string(),
        url: format!("http://{addr}/{name}"),
        timeout_ms: 2_000,
        failure_policy: ai_memory_wiki::FailurePolicy::Ignore,
        events,
        blocking,
    }
}

/// The one shape that decides: `blocking` + `reject`, the hook the engine
/// awaits BEFORE the operation. It is the only one an unauthorised call can
/// reach, since the observers are dispatched afterwards.
fn deciding_hook(
    name: &str,
    addr: std::net::SocketAddr,
    events: Vec<ai_memory_wiki::AdmissionOp>,
) -> ai_memory_wiki::WebhookConfig {
    ai_memory_wiki::WebhookConfig {
        failure_policy: ai_memory_wiki::FailurePolicy::Reject,
        ..hook(name, addr, true, events)
    }
}

fn handoff_ops() -> Vec<ai_memory_wiki::AdmissionOp> {
    vec![
        ai_memory_wiki::AdmissionOp::HandoffBegin,
        ai_memory_wiki::AdmissionOp::HandoffAccept,
        ai_memory_wiki::AdmissionOp::HandoffCancel,
    ]
}

/// A blocking observer (`ignore` policy, so it decides nothing) plus a
/// non-blocking one — the two shapes an operator writes when they mean
/// "tell me, don't ask me".
fn observer_chain(addr: std::net::SocketAddr) -> ai_memory_wiki::AdmissionChain {
    let events = handoff_ops();
    ai_memory_wiki::AdmissionChain::new(vec![
        hook("mirror", addr, true, events.clone()),
        hook("observer", addr, false, events),
    ])
    .unwrap()
}

/// The observers plus a decider, for the calls that must reach neither.
fn guarded_chain(addr: std::net::SocketAddr) -> ai_memory_wiki::AdmissionChain {
    let events = handoff_ops();
    ai_memory_wiki::AdmissionChain::new(vec![
        deciding_hook("guard", addr, events.clone()),
        hook("mirror", addr, true, events.clone()),
        hook("observer", addr, false, events),
    ])
    .unwrap()
}

const ROOT_TOKEN: &str = "the-root-token";
const PROXY_TOKEN: &str = "the-proxy-token";

struct Harness {
    router: Router,
    store: Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    _tmp: TempDir,
}

async fn harness(chain: ai_memory_wiki::AdmissionChain) -> Harness {
    build_harness(chain, false).await
}

/// The same server behind the production `require_bearer` layer with
/// `[auth].actor_proxy_bearer_token` configured: the only configuration in
/// which a caller can be BOTH named and non-admin, and therefore the only one
/// where the `any_owner` gate has anybody to refuse.
async fn proxied_harness(chain: ai_memory_wiki::AdmissionChain) -> Harness {
    build_harness(chain, true).await
}

async fn build_harness(chain: ai_memory_wiki::AdmissionChain, proxy_auth: bool) -> Harness {
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_admission_chain(chain);

    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki.clone())
        .with_trusted_proxy_identity(proxy_auth);
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    let mut router = Router::new().nest_service("/mcp", mcp_service);
    if proxy_auth {
        let auth_state = AuthState::new(Some(ROOT_TOKEN.to_string()))
            .with_root_actor(ai_memory_core::ActorContext {
                user: Some("dj".to_string()),
                ..ai_memory_core::ActorContext::default()
            })
            .with_trusted_proxy_bearer(PROXY_TOKEN);
        router = router.layer(axum::middleware::from_fn_with_state(
            Arc::new(auth_state),
            require_bearer,
        ));
    }
    Harness {
        router,
        store,
        ws,
        proj,
        _tmp: tmp,
    }
}

/// Headers a trusted proxy sends for a named, non-root end user.
fn proxied_as(user: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("authorization", "Bearer the-proxy-token"),
        ("x-memory-actor-user", user),
    ]
}

/// The raw JSON-RPC envelope, so a test can assert on a rejection.
async fn call_tool_raw(
    router: &Router,
    name: &str,
    arguments: serde_json::Value,
    headers: &[(&str, &str)],
) -> serde_json::Value {
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
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}\nerr: {e}"))
}

async fn call_tool(router: &Router, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    let v = call_tool_raw(router, name, arguments, &[]).await;
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}\nfull: {v}");
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {v}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&joined).unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}"))
}

/// Observer dispatch is fire-and-forget, so give a wrongly-spawned call time to
/// land before asserting that nothing was called.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

async fn wait_for(calls: &Calls, want: (&str, &str)) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let seen = calls.lock().unwrap().clone();
        if seen.iter().any(|(hook, op)| hook == want.0 && op == want.1) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "webhook {want:?} was never called; saw {seen:?}",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The routine `memory_handoff_accept` call finds nothing to accept. No webhook
/// may be told that an accept happened, because none did.
#[tokio::test]
async fn accept_with_nothing_pending_tells_no_webhook() {
    let (addr, calls) = recording_webhook_host().await;
    let h = harness(observer_chain(addr)).await;

    let result = call_tool(
        &h.router,
        "memory_handoff_accept",
        json!({ "workspace": "default", "project": "scratch" }),
    )
    .await;
    assert_eq!(
        result,
        json!({ "handoff": null }),
        "nothing was pending, so nothing is accepted",
    );

    settle().await;
    assert!(
        calls.lock().unwrap().is_empty(),
        "an accept that found no handoff must reach no webhook: {:?}",
        calls.lock().unwrap(),
    );
}

/// The other half, and the consistency rule: for an op that DID happen, both
/// observer shapes are notified — the blocking-but-undeciding one and the
/// non-blocking one an operator writes to say "off the critical path".
#[tokio::test]
async fn begin_and_accept_notify_both_observer_shapes_once_they_land() {
    let (addr, calls) = recording_webhook_host().await;
    let h = harness(observer_chain(addr)).await;

    let begun = call_tool(
        &h.router,
        "memory_handoff_begin",
        json!({
            "workspace": "default",
            "project": "scratch",
            "summary": "where the session left off",
            "shared": true,
        }),
    )
    .await;
    assert!(
        begun.get("handoff_id").is_some(),
        "handoff created: {begun}"
    );
    wait_for(&calls, ("mirror", "handoff_begin")).await;
    wait_for(&calls, ("observer", "handoff_begin")).await;

    let accepted = call_tool(
        &h.router,
        "memory_handoff_accept",
        json!({ "workspace": "default", "project": "scratch" }),
    )
    .await;
    assert!(
        accepted
            .pointer("/handoff/summary")
            .and_then(|s| s.as_str())
            .is_some_and(|s| s.contains("where the session left off")),
        "the pending baton is delivered: {accepted}",
    );
    wait_for(&calls, ("mirror", "handoff_accept")).await;
    wait_for(&calls, ("observer", "handoff_accept")).await;

    settle().await;
    let seen = calls.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        4,
        "each landed op is announced exactly once per subscriber: {seen:?}",
    );
}

/// A cross-owner `memory_handoff_cancel` from a caller without the capability
/// for it is not an operation, so the chain must not hear about it — not even
/// the decider, which sits in front. `memory_handoff_accept` already authorises
/// its `any_owner` before asking; the cancel path is the sibling.
#[tokio::test]
async fn unauthorised_any_owner_cancel_reaches_no_webhook() {
    let (addr, calls) = recording_webhook_host().await;
    let h = proxied_harness(guarded_chain(addr)).await;

    // Inserted directly: creating it through the tool would put a legitimate
    // `handoff_begin` in the recording, and what is under test is that the
    // refused cancel adds nothing to it.
    let id = h
        .store
        .writer
        .insert_handoff(ai_memory_core::NewHandoff {
            workspace_id: h.ws,
            project_id: h.proj,
            from_session_id: None,
            from_agent: ai_memory_core::AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "bobs-baton".to_string(),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            // Stamped through the contract, as the tool would: owners are
            // qualified storage keys, never raw names.
            owner_user: ai_memory_core::owner_stamp(
                Some(&ai_memory_core::IdentityKey::User("bob".into())),
                true,
            ),
        })
        .await
        .expect("insert handoff");

    let response = call_tool_raw(
        &h.router,
        "memory_handoff_cancel",
        json!({
            "workspace": "default",
            "project": "scratch",
            "handoff_id": id.to_string(),
            "any_owner": true,
        }),
        &proxied_as("alice"),
    )
    .await;
    // The message is asserted, not merely the presence of an error: a rejection
    // from the auth layer itself would also produce one, and would make this
    // test pass without the capability gate ever running.
    let message = response
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .unwrap_or_else(|| panic!("a non-admin caller must be refused any_owner: {response}"));
    assert!(
        message.contains("admin"),
        "expected the admin capability gate to refuse this, got: {message}",
    );

    settle().await;
    assert!(
        calls.lock().unwrap().is_empty(),
        "an operation the caller was never permitted must reach no webhook: {:?}",
        calls.lock().unwrap(),
    );
    assert_eq!(
        h.store
            .reader
            .handoff_by_id(id)
            .await
            .expect("read back")
            .expect("row still there")
            .lifecycle
            .state,
        ai_memory_core::HandoffState::Open,
        "the refused cancel must not have discarded the baton",
    );
}
