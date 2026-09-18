//! The user-facing MCP surface for cross-project agent messaging (V64).
//!
//! The store layer is pinned in `ai-memory-store`'s `agent_messages.rs`; what is
//! untested there is the four tools an agent actually calls —
//! `memory_message_send`, `memory_message_list`, `memory_message_pop`, and
//! `memory_message_cancel` — driven through the real `tools/call` JSON-RPC path
//! and asserted on the user-visible JSON they return. These are the surfaces
//! `docs/agent-messaging.md` documents, so the behaviours pinned here are the
//! ones a Kimi-in-A → Claude-in-B round trip depends on: a message crosses the
//! per-project isolation boundary on purpose, so it is visible only in the
//! recipient's inbox and the sender's outbox; it pops exactly once; and the
//! sender can retract it while the recipient cannot cancel someone else's send.
//!
//! The harness mounts the production `AiMemoryServer` over the same
//! Streamable-HTTP transport `handoff_identity.rs` uses, with no auth layer
//! (the local/stdio shape), and drives every scope explicitly the way a static
//! MCP client does — so `to_*`/`from_*`/`workspace`+`project` are the exact
//! arguments an agent sends.

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

/// A server over a temp store/wiki with three sibling projects in one
/// workspace: `project-a` (the baked "current project"), `project-b`, and
/// `project-c`. All three must already exist, because sends and reads resolve
/// scope through the no-create lookup.
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

/// `memory_message_send` from `project-a` to `project-b`, returning the new id.
async fn send(router: &Router, subject: &str, body: &str) -> String {
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
    sent.get("message_id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("send returned no message_id: {sent}"))
        .to_string()
}

/// The messages array from a `memory_message_list` for `workspace/project`.
async fn list(router: &Router, project: &str, mailbox: &str) -> Vec<Value> {
    let listed = call(
        router,
        "memory_message_list",
        json!({ "workspace": WS, "project": project, "box": mailbox }),
    )
    .await;
    listed
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("list returned no messages array: {listed}"))
        .clone()
}

/// The full end-to-end round trip through the tools: A sends to B, only B's
/// inbox (and A's outbox) shows it, B pops it exactly once, and a second pop
/// finds nothing.
#[tokio::test]
async fn send_list_pop_delivers_to_recipient_exactly_once() {
    let h = harness().await;
    let body = "Add a /v1/export endpoint that streams NDJSON.";
    let id = send(&h.router, "export endpoint", body).await;

    // B's inbox shows exactly the one message, with the sent content and id.
    let inbox = list(&h.router, B, "inbox").await;
    assert_eq!(
        inbox.len(),
        1,
        "B's inbox must show exactly the sent message"
    );
    assert_eq!(inbox[0]["id"].as_str(), Some(id.as_str()));
    assert_eq!(inbox[0]["body"].as_str(), Some(body));
    assert_eq!(inbox[0]["subject"].as_str(), Some("export endpoint"));
    assert_eq!(inbox[0]["state"].as_str(), Some("pending"));

    // Recipient-only visibility: neither the sender's inbox nor an unrelated
    // third project sees mail addressed to B.
    assert!(
        list(&h.router, A, "inbox").await.is_empty(),
        "the sender must not see its own outbound mail in its inbox"
    );
    assert!(
        list(&h.router, C, "inbox").await.is_empty(),
        "an unrelated project must not see mail addressed to B"
    );

    // The sender does see it in its OUTBOX (it is what A can still cancel).
    let outbox = list(&h.router, A, "outbox").await;
    assert_eq!(outbox.len(), 1, "A's outbox must show the message it sent");
    assert_eq!(outbox[0]["id"].as_str(), Some(id.as_str()));

    // B pops it exactly once: the body comes back fenced with a security notice.
    let popped = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(popped["message"]["id"].as_str(), Some(id.as_str()));
    assert_eq!(popped["message"]["body"].as_str(), Some(body));
    assert_eq!(
        popped["message"]["state"].as_str(),
        Some("claimed"),
        "a popped message is marked claimed: {popped}"
    );
    assert!(
        popped["security_notice"].as_str().is_some(),
        "pop must carry the untrusted-input security notice: {popped}"
    );

    // Claim-once: the inbox is empty and a second pop returns nothing.
    assert!(
        list(&h.router, B, "inbox").await.is_empty(),
        "the inbox must be empty once the message is claimed"
    );
    let again = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(
        again["message"],
        Value::Null,
        "a claimed message must not be poppable again: {again}"
    );
}

/// A non-recipient project cannot pop mail addressed to B: its own inbox pop
/// returns nothing and B's message stays pending and claimable.
#[tokio::test]
async fn a_non_recipient_cannot_pop_the_recipients_mail() {
    let h = harness().await;
    let id = send(&h.router, "for B only", "for B only").await;

    // C pops from its own (empty) inbox — it cannot reach B's mail.
    let c_pop = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": C }),
    )
    .await;
    assert_eq!(
        c_pop["message"],
        Value::Null,
        "C must not be able to pop mail addressed to B: {c_pop}"
    );

    // B's message is untouched and still deliverable to B.
    let b_pop = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(b_pop["message"]["id"].as_str(), Some(id.as_str()));
}

/// The sender retracts a pending message through `memory_message_cancel`; once
/// cancelled the recipient can no longer pop it.
#[tokio::test]
async fn sender_can_cancel_a_pending_message() {
    let h = harness().await;
    let id = send(&h.router, "never mind", "never mind soon").await;

    let cancelled = call(
        &h.router,
        "memory_message_cancel",
        json!({ "workspace": WS, "project": A, "message_id": id }),
    )
    .await;
    assert_eq!(
        cancelled["cancelled"].as_u64(),
        Some(1),
        "the sender must cancel exactly its one pending message: {cancelled}"
    );

    assert!(
        list(&h.router, B, "inbox").await.is_empty(),
        "a cancelled message must leave the recipient inbox"
    );
    let popped = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(
        popped["message"],
        Value::Null,
        "a cancelled message must not be deliverable: {popped}"
    );
}

/// Cancel is scoped to the SENDER coordinate: the recipient calling cancel from
/// its own project clears nothing and the message it received survives — the
/// tool-surface mirror of the store's `recipient_cannot_cancel_the_senders_outbox`.
#[tokio::test]
async fn recipient_cannot_cancel_the_senders_message() {
    let h = harness().await;
    let id = send(&h.router, "do the work", "do the work").await;

    // B cancels scoped to its own project: it has no outbox mail, so nothing
    // is cancelled and A's message to B survives.
    let cancelled = call(
        &h.router,
        "memory_message_cancel",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(
        cancelled["cancelled"].as_u64(),
        Some(0),
        "the recipient cancelling its own outbox must touch nothing: {cancelled}"
    );

    // The message is still there and still poppable by B.
    let inbox = list(&h.router, B, "inbox").await;
    assert_eq!(
        inbox.len(),
        1,
        "A's message must survive B's cancel attempt"
    );
    let popped = call(
        &h.router,
        "memory_message_pop",
        json!({ "workspace": WS, "project": B }),
    )
    .await;
    assert_eq!(popped["message"]["id"].as_str(), Some(id.as_str()));
}
