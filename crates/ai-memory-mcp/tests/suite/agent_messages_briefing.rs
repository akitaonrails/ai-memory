//! `memory_briefing`'s `pending_message_count` over the real `tools/call` path.
//!
//! The count is the on-demand twin of the non-consuming SessionStart inbox
//! notice (`docs/agent-messaging.md`, "On-start hot context and the inbox
//! notice"): a resuming agent asks `memory_briefing` how much cross-project mail
//! is waiting without popping any of it. What is pinned here is that the number
//! `memory_briefing` reports for a scope equals that scope's pending inbox depth
//! — it rises with each send addressed to the project, falls when the recipient
//! actually pops one, and is 0 for a project nobody has written to. This mirrors
//! `agent_messages_tools.rs`: three sibling projects in one workspace, messages
//! sent through `memory_message_send`, driven over the same Streamable-HTTP MCP
//! transport a static client uses (explicit scope on every call).

use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const A: &str = "project-a";
const B: &str = "project-b";
const C: &str = "project-c";

struct Harness {
    router: Router,
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

/// Three sibling projects in one workspace: `project-a` (the baked "current
/// project"), `project-b`, and `project-c`. All must exist because sends and
/// reads resolve scope through the no-create lookup.
async fn harness() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let baked = store
        .writer
        .get_or_create_project(ws, A.to_string(), None)
        .await
        .expect("project-a");
    for name in [B, C] {
        store
            .writer
            .get_or_create_project(ws, name.to_string(), None)
            .await
            .expect("sibling project");
    }
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked).with_wiki(wiki);
    Harness {
        router: mount(server),
        _tmp: tmp,
    }
}

/// Drive one real `tools/call` and return the parsed tool JSON payload.
async fn call(router: &Router, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .expect("mcp req");
    let resp = router.clone().oneshot(req).await.expect("oneshot");
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

/// `memory_message_send` from `project-a` to `project-b`.
async fn send_to_b(router: &Router, subject: &str, body: &str) {
    let sent = call(
        router,
        "memory_message_send",
        json!({
            "from_workspace": WS,
            "from_project": A,
            "to_workspace": WS,
            "to_project": B,
            "subject": subject,
            "body": body,
        }),
    )
    .await;
    assert!(
        sent.get("message_id").and_then(Value::as_str).is_some(),
        "send returned no message_id: {sent}"
    );
}

/// `memory_briefing`'s `pending_message_count` for `workspace/project`.
async fn pending_count(router: &Router, project: &str) -> u64 {
    let brief = call(
        router,
        "memory_briefing",
        json!({ "workspace": WS, "project": project }),
    )
    .await;
    brief
        .get("pending_message_count")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("briefing has no pending_message_count: {brief}"))
}

/// The count `memory_briefing` reports tracks the recipient scope's pending
/// inbox depth: it is exactly N after N sends, drops by one when the recipient
/// pops a message, and a scope with no mail reports 0.
#[tokio::test]
async fn briefing_pending_message_count_tracks_the_recipient_inbox() {
    let h = harness().await;

    // Baseline: an inbox nobody has written to reports 0.
    assert_eq!(
        pending_count(&h.router, B).await,
        0,
        "an empty inbox must brief a pending_message_count of 0",
    );

    // Three messages addressed to B ⇒ B's briefing reports exactly 3.
    for i in 0..3 {
        send_to_b(
            &h.router,
            &format!("subject {i}"),
            &format!("please do task {i}"),
        )
        .await;
    }
    assert_eq!(
        pending_count(&h.router, B).await,
        3,
        "N pending messages for the recipient must brief as pending_message_count == N",
    );

    // The count is per-recipient-scope: mail addressed to B never shows up in a
    // sibling project's briefing.
    assert_eq!(
        pending_count(&h.router, C).await,
        0,
        "a project with no inbox mail must report 0 even while a sibling has mail",
    );
    assert_eq!(
        pending_count(&h.router, A).await,
        0,
        "the sender's own inbox count must not include what it sent to B",
    );

    // After B deliberately pops one, the reported count drops by one — the
    // briefing reflects the real remaining inbox depth, not the send tally.
    let popped = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(
        popped["message"]["state"].as_str(),
        Some("claimed"),
        "pop must claim a pending message: {popped}",
    );
    assert_eq!(
        pending_count(&h.router, B).await,
        2,
        "popping one message must drop the briefed pending_message_count to 2",
    );
}

/// Regression for the field-reported dead-end (cross-project mail on the
/// homeserver): the on-start notice / `memory_briefing` counts one project's
/// inbox, but a *no-scope* `memory_message_pop` resolves the shared
/// active-project slot to a DIFFERENT project whose inbox is empty, and used to
/// return a bare `{"message": null}` — "you have mail" followed by an empty
/// fetch, with no way to tell they were looking at the wrong inbox.
///
/// The mail must stay put (the mis-scoped pop consumes nothing), the empty
/// result must NAME the inbox it actually resolved and how it was inferred, and
/// an explicit-scope pop must still deliver.
#[tokio::test]
async fn no_scope_pop_that_misses_the_mail_is_diagnosed_not_a_silent_null() {
    let h = harness().await;

    // Mail lands in project-b's inbox; the notice/briefing for B reports it.
    send_to_b(&h.router, "export", "please add the /v1/export endpoint").await;
    assert_eq!(pending_count(&h.router, B).await, 1);

    // A no-scope pop resolves the baked "current project" (project-a), whose
    // inbox is empty. It must not be a bare null: it names the resolved scope
    // and flags that the scope was inferred, not stated.
    let missed = call(&h.router, "memory_message_pop", json!({})).await;
    assert!(
        missed["message"].is_null(),
        "the wrong (inferred) inbox has no mail: {missed}",
    );
    assert_eq!(
        missed["resolved_scope"]["project"].as_str(),
        Some(A),
        "an empty inferred-scope pop must name the inbox it resolved: {missed}",
    );
    let src = missed["scope_source"].as_str().unwrap_or_default();
    assert!(
        !src.is_empty() && src != "explicit" && src != "session",
        "the miss must report an inferred (non-explicit, non-session) scope source, got {src:?}: {missed}",
    );
    assert!(
        missed["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("explicit"),
        "the hint must steer the caller to re-run with explicit scope: {missed}",
    );

    // The mis-scoped pop consumed nothing: B still has its message.
    assert_eq!(
        pending_count(&h.router, B).await,
        1,
        "a pop that resolved the wrong inbox must not consume B's mail",
    );

    // Popping with explicit scope delivers it.
    let got = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(
        got["message"]["body"], "please add the /v1/export endpoint",
        "an explicit-scope pop of B delivers the message: {got}",
    );
    assert!(
        got.get("hint").is_none(),
        "a successful pop carries no scope hint: {got}",
    );
    assert_eq!(pending_count(&h.router, B).await, 0);

    // An EXPLICIT pop of an empty inbox is unambiguous — no hint.
    let empty_explicit = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": C }),
    )
    .await;
    assert!(empty_explicit["message"].is_null());
    assert!(
        empty_explicit.get("hint").is_none(),
        "an explicitly-scoped empty pop is not ambiguous and must not add a hint: {empty_explicit}",
    );
}

/// The same divergence via `memory_message_list`: a no-scope inbox listing that
/// resolves the wrong (empty) project names the inferred scope instead of
/// silently returning `{"messages": []}`, while an explicit listing of B shows
/// the mail with no hint.
#[tokio::test]
async fn no_scope_inbox_list_that_misses_the_mail_names_the_inferred_scope() {
    let h = harness().await;
    send_to_b(&h.router, "export", "please add the /v1/export endpoint").await;

    // No-scope inbox list resolves project-a (empty) -> hint, not a silent [].
    let missed = call(&h.router, "memory_message_list", json!({ "box": "inbox" })).await;
    assert_eq!(
        missed["messages"].as_array().map(Vec::len),
        Some(0),
        "the inferred (wrong) inbox is empty: {missed}",
    );
    assert_eq!(
        missed["resolved_scope"]["project"].as_str(),
        Some(A),
        "an empty inferred-scope list must name the inbox it resolved: {missed}",
    );
    assert!(
        missed["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("explicit"),
        "the list hint must steer the caller to explicit scope: {missed}",
    );

    // Explicit inbox list of B shows the mail and carries no hint.
    let seen = call(
        &h.router,
        "memory_message_list",
        json!({ "box": "inbox", "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(
        seen["messages"].as_array().map(Vec::len),
        Some(1),
        "explicit list of B shows its mail: {seen}",
    );
    assert!(
        seen.get("hint").is_none(),
        "a non-empty explicit list carries no scope hint: {seen}",
    );
}
