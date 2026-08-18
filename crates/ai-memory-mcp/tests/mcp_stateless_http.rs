//! Integration tests for the Streamable HTTP transport's stateless vs.
//! stateful behaviour (issue #3).
//!
//! Stateless clients (OpenCode `type: "remote"`, curl) send `initialize`
//! and `tools/call` as independent requests without echoing an
//! `Mcp-Session-Id`. In rmcp's default *stateful* mode the server demands
//! that header and rejects the second request with 422 "Unexpected
//! message, expect initialize request". `ai-memory serve --transport http`
//! now defaults to *stateless* mode (`stateful_mode=false` +
//! `json_response=true`), so those clients work with no `mcp-remote` shim.
//! `--http-stateful` restores the session behaviour.
//!
//! These tests drive the exact `StreamableHttpService` wiring from
//! `serve.rs` through an axum router, so they catch a regression in either
//! direction.

use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tempfile::TempDir;
use tower::ServiceExt;

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
const TOOLS_CALL_STATUS: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_status","arguments":{}}}"#;
const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#;

/// Build a `/mcp` router exactly like `serve.rs` does, toggling stateful
/// mode. Returns the `Store` too so the writer actor stays alive for the
/// duration of the test.
async fn make_router(tmp: &TempDir, stateful: bool) -> (Router, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch".to_string(), None)
        .await
        .unwrap();
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj);
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(stateful)
            .with_json_response(!stateful),
    );
    let router = Router::new().nest_service("/mcp", svc);
    (router, store)
}

/// POST a JSON-RPC body to `/mcp` with the Accept header every compliant
/// Streamable HTTP client sends (both JSON and event-stream), and no
/// session id.
fn post(body: &'static str) -> Request<Body> {
    post_to("/mcp", body)
}

/// [`post`] against an explicit URI (tests carrying `?flavor=moonshot`).
fn post_to(uri: &str, body: &'static str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        // rmcp's DNS-rebinding guard rejects a missing/disallowed Host with
        // 400; `localhost` is in the default allowlist. Real HTTP clients
        // always send Host — oneshot does not, so set it explicitly.
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 2_000_000)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The fix: in the default stateless mode, a `tools/call` arriving with no
/// prior session and no `Mcp-Session-Id` header is serviced and returns a
/// JSON-RPC result — not a 422 / "Session not found".
#[tokio::test]
async fn stateless_tools_call_without_session_succeeds() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .clone()
        .oneshot(post(TOOLS_CALL_STATUS))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "stateless tools/call must succeed without a session id"
    );
    let body = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("stateless response must be JSON, got: {body}\nerr: {e}"));
    assert!(
        json.get("error").is_none(),
        "expected a JSON-RPC result, got an error: {body}"
    );
    assert!(json.get("result").is_some(), "missing result: {body}");
    // memory_status serialises StatusCounts, whose fields include
    // `pages_latest` — proves the tool actually ran, not just an empty ack.
    assert!(
        body.contains("pages_latest"),
        "result should carry status counts: {body}"
    );
}

/// `initialize` in stateless mode also returns a plain JSON-RPC result
/// (no session handshake required).
#[tokio::test]
async fn stateless_initialize_returns_json_result() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.clone().oneshot(post(INITIALIZE)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("initialize returns JSON");
    assert!(
        json.get("result").is_some(),
        "missing initialize result: {body}"
    );
    assert!(
        body.contains("serverInfo") || body.contains("protocolVersion"),
        "initialize result should carry server info: {body}"
    );
}

/// Contrast / guard: with `--http-stateful` (session mode), the same
/// session-less `tools/call` is rejected with 422 "Unexpected message,
/// expect initialize request" — the exact symptom from issue #3. This
/// proves the default flip is what resolves it, and pins the opt-in
/// behaviour so a future change to the default can't silently regress it.
#[tokio::test]
async fn stateful_tools_call_without_session_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, true).await;

    let resp = router
        .clone()
        .oneshot(post(TOOLS_CALL_STATUS))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "stateful mode must reject a session-less tools/call"
    );
    let body = body_string(resp).await;
    assert!(
        body.contains("initialize"),
        "stateful rejection should mention the missing initialize: {body}"
    );
}

/// Pull `memory_read_page`'s inputSchema from a tools/list response body.
fn read_page_input_schema(body: &str) -> serde_json::Value {
    let json: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("tools/list response must be JSON, got: {body}\nerr: {e}"));
    let tools = json["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("missing result.tools: {body}"));
    tools
        .iter()
        .find(|tool| tool["name"] == "memory_read_page")
        .unwrap_or_else(|| panic!("memory_read_page missing from tools/list: {body}"))[
        "inputSchema"
    ]
    .clone()
}

/// Kimi Code's real flow: independent stateless POSTs against
/// `/mcp?flavor=moonshot` must return `memory_read_page` without root
/// combinators, the rest of the schema intact.
#[tokio::test]
async fn stateless_moonshot_flavor_strips_root_any_of() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let init = router
        .clone()
        .oneshot(post_to("/mcp?flavor=moonshot", INITIALIZE))
        .await
        .unwrap();
    assert_eq!(init.status(), StatusCode::OK);

    let resp = router
        .oneshot(post_to("/mcp?flavor=moonshot", TOOLS_LIST))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "moonshot flavor must strip root `{key}`: {schema}"
        );
    }
    assert!(
        schema.get("properties").is_some(),
        "the flat schema must keep describing the args: {schema}"
    );
}

/// Kiro's Bedrock requests use the same restricted root-schema dialect while
/// retaining a provider-specific marker for diagnostics and compatibility.
#[tokio::test]
async fn stateless_bedrock_flavor_strips_root_any_of() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .oneshot(post_to("/mcp?flavor=bedrock", TOOLS_LIST))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "bedrock flavor must strip root `{key}`: {schema}"
        );
    }
    assert!(schema.get("properties").is_some());
}

/// Control: without the marker, tools/list keeps the upstream root `anyOf`.
#[tokio::test]
async fn stateless_tools_list_without_flavor_keeps_root_any_of() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.oneshot(post(TOOLS_LIST)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    assert!(
        schema.get("anyOf").is_some(),
        "unflavored tools/list must keep the upstream root anyOf: {schema}"
    );
}

/// `strict_schema=true` serves the restricted tool list to every client —
/// including generic ones that never send `?flavor=` (issue #412: OpenCode,
/// Cursor etc. forward schemas verbatim to strict validators).
#[tokio::test]
async fn stateless_strict_schema_strips_root_any_of_without_flavor() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch".to_string(), None)
        .await
        .unwrap();
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_strict_schema(true);
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    let router = Router::new().nest_service("/mcp", svc);

    let resp = router.clone().oneshot(post(TOOLS_LIST)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "strict schema must strip root `{key}` with no flavor marker: {schema}"
        );
    }
    assert!(
        schema.get("properties").is_some(),
        "the flat schema must keep describing the args: {schema}"
    );
}
