//! `memory_forget_sweep` and the admission chain.
//!
//! The sweep raises `forget_sweep` so a scope guard can decline a project, and
//! announces the same op to its observers afterwards. A `dry_run = true` call is
//! the sharp case: it scores candidates and returns the preview WITHOUT touching
//! a single page version, so a mirror told a sweep happened would go reconcile
//! nothing — the same lie as announcing an `accept` that found no pending
//! handoff. The deciders are still asked, because a guard may legitimately
//! refuse to have its project scored at all.
//!
//! Driven through the production JSON-RPC transport, against a real webhook host
//! that records every request it receives.

use ai_memory_mcp::AiMemoryServer;
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
    failure_policy: ai_memory_wiki::FailurePolicy,
) -> ai_memory_wiki::WebhookConfig {
    ai_memory_wiki::WebhookConfig {
        name: name.to_string(),
        url: format!("http://{addr}/{name}"),
        timeout_ms: 2_000,
        failure_policy,
        events: vec![ai_memory_wiki::AdmissionOp::ForgetSweep],
        blocking,
    }
}

/// The three shapes an operator writes, all subscribed to `forget_sweep`: a
/// decider (`blocking` + `reject`), a blocking-but-undeciding observer, and a
/// non-blocking one.
fn sweep_chain(addr: std::net::SocketAddr) -> ai_memory_wiki::AdmissionChain {
    use ai_memory_wiki::FailurePolicy;
    ai_memory_wiki::AdmissionChain::new(vec![
        hook("guard", addr, true, FailurePolicy::Reject),
        hook("mirror", addr, true, FailurePolicy::Ignore),
        hook("observer", addr, false, FailurePolicy::Ignore),
    ])
    .unwrap()
}

struct Harness {
    router: Router,
    _store: Store,
    _tmp: TempDir,
}

async fn harness(chain: ai_memory_wiki::AdmissionChain) -> Harness {
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

    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj).with_wiki(wiki);
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    Harness {
        router: Router::new().nest_service("/mcp", mcp_service),
        _store: store,
        _tmp: tmp,
    }
}

async fn call_tool(router: &Router, name: &str, arguments: serde_json::Value) -> serde_json::Value {
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
    let v: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}\nerr: {e}"));
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

/// A preview mutates nothing, so no observer may hear that a sweep happened —
/// while the decider is still consulted, since refusing the preview and
/// permitting the real sweep would be the wrong way round.
#[tokio::test]
async fn dry_run_sweep_asks_the_decider_and_tells_no_observer() {
    let (addr, calls) = recording_webhook_host().await;
    let h = harness(sweep_chain(addr)).await;

    let report = call_tool(
        &h.router,
        "memory_forget_sweep",
        json!({ "workspace": "default", "project": "scratch", "dry_run": true }),
    )
    .await;
    assert_eq!(
        report.get("dry_run"),
        Some(&json!(true)),
        "the sweep ran as a preview: {report}",
    );
    wait_for(&calls, ("guard", "forget_sweep")).await;

    settle().await;
    let seen = calls.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![("guard".to_string(), "forget_sweep".to_string())],
        "a preview evicted nothing, so only the decider may have been called",
    );
}

/// The other half: a real sweep is announced to both observer shapes, so the
/// assertion above is about `dry_run` and not about observers never firing.
#[tokio::test]
async fn real_sweep_notifies_both_observer_shapes() {
    let (addr, calls) = recording_webhook_host().await;
    let h = harness(sweep_chain(addr)).await;

    let report = call_tool(
        &h.router,
        "memory_forget_sweep",
        json!({ "workspace": "default", "project": "scratch", "dry_run": false }),
    )
    .await;
    assert_eq!(
        report.get("dry_run"),
        Some(&json!(false)),
        "the sweep ran for real: {report}",
    );
    wait_for(&calls, ("guard", "forget_sweep")).await;
    wait_for(&calls, ("mirror", "forget_sweep")).await;
    wait_for(&calls, ("observer", "forget_sweep")).await;

    settle().await;
    assert_eq!(
        calls.lock().unwrap().len(),
        3,
        "each subscriber hears about the sweep exactly once: {:?}",
        calls.lock().unwrap(),
    );
}
