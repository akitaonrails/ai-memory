//! `memory_message_send` and the observer webhook a bot wakes on.
//!
//! The observer is told which message arrived (`message_id`), for which
//! harness and from which project, so the bot can pop exactly that message
//! instead of draining a mailbox it shares with other consumers. The message
//! text never rides along: the body still reaches an agent only through
//! `memory_message_pop`.

use ai_memory_core::{AgentKind, NewSession, SessionId};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

type Seen = Arc<Mutex<Vec<Value>>>;

async fn recorder() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let app = Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(payload): axum::Json<Value>| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(payload);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/hook"), seen)
}

struct Harness {
    router: Router,
    seen: Seen,
    _tmp: TempDir,
}

/// Workspace `fuad` with a sender (`next`) and a bot project (`grok-bot`) that
/// has a registered Codex session; the observer watches only the bot.
async fn harness() -> Harness {
    let (url, seen) = recorder().await;
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store.writer.get_or_create_workspace("fuad").await.unwrap();
    let next = store
        .writer
        .get_or_create_project(ws, "next", None)
        .await
        .unwrap();
    let bot = store
        .writer
        .get_or_create_project(ws, "grok-bot", None)
        .await
        .unwrap();
    store
        .writer
        .begin_session(NewSession {
            id: SessionId::new(),
            workspace_id: ws,
            project_id: bot,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
            occurred_at: None,
            origin: None,
        })
        .await
        .unwrap();
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "bot-wake".into(),
        url,
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::MessageSend],
        blocking: false,
        projects: vec!["fuad/grok-bot".into()],
        headers: Vec::new(),
    }])
    .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone())
        .with_admission_chain(chain);
    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, next).with_wiki(wiki);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Harness {
        router: Router::new().nest_service("/mcp", service),
        seen,
        _tmp: tmp,
    }
}

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
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}");
    }
    let text = v
        .pointer("/result/content")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing result.content: {v}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("tool text not JSON: {text}: {e}"))
}

async fn send(router: &Router, subject: &str, body: &str, to_agent: Option<&str>) -> String {
    let mut args = json!({
        "from_workspace": "fuad",
        "from_project": "next",
        "to_workspace": "fuad",
        "to_project": "grok-bot",
        "subject": subject,
        "body": body,
    });
    if let Some(agent) = to_agent {
        args["to_agent"] = json!(agent);
    }
    let sent = call(router, "memory_message_send", args).await;
    sent["message_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no message_id: {sent}"))
        .to_string()
}

async fn wait_for(seen: &Seen, n: usize) {
    for _ in 0..200 {
        if seen.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn an_mcp_send_tells_the_observer_which_message_arrived() {
    let h = harness().await;

    let id = send(
        &h.router,
        "deploy request",
        "please deploy the hotfix",
        Some("codex"),
    )
    .await;
    wait_for(&h.seen, 1).await;

    let rec = h.seen.lock().unwrap();
    assert_eq!(rec.len(), 1);
    let ctx = &rec[0]["ctx"];
    assert_eq!(ctx["message_id"], id);
    assert_eq!(ctx["to_agent"], "codex");
    assert_eq!(ctx["from_workspace"], "fuad");
    assert_eq!(ctx["from_project"], "next");
    let text = rec[0].to_string();
    assert!(!text.contains("deploy the hotfix"), "no body: {text}");
    assert!(!text.contains("deploy request"), "no subject: {text}");
}

#[tokio::test]
async fn the_bot_pops_exactly_the_message_that_woke_it() {
    let h = harness().await;

    let first = send(&h.router, "first", "first body", None).await;
    let second = send(&h.router, "second", "second body", None).await;
    wait_for(&h.seen, 2).await;
    let woke_by = h
        .seen
        .lock()
        .unwrap()
        .iter()
        .find(|p| p["ctx"]["message_id"] == second.as_str())
        .map(|p| p["ctx"]["message_id"].as_str().unwrap().to_string())
        .expect("the second send must reach the observer with its id");

    let popped = call(
        &h.router,
        "memory_message_pop",
        json!({"workspace": "fuad", "project": "grok-bot", "message_id": woke_by}),
    )
    .await;
    assert_eq!(popped["message"]["id"], second);

    let inbox = call(
        &h.router,
        "memory_message_list",
        json!({"workspace": "fuad", "project": "grok-bot"}),
    )
    .await;
    let pending: Vec<&str> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(pending, vec![first.as_str()], "the first stays pending");
}
