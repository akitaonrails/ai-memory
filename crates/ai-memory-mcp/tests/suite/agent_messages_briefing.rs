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
