//! Integration tests for the Streamable HTTP transport's stateless vs.
//! stateful behaviour (issue #3).
//!
//! Stateless clients (OpenCode `type: "remote"`, curl) send `initialize`
//! and `tools/call` as independent requests without echoing an
//! `Mcp-Session-Id`. In rmcp's default *stateful* mode the server demands
//! that header and rejects the second request with 422 "Unexpected
//! message, expect initialize request". `ai-memory serve --transport http`
//! now defaults to *stateless* mode (`legacy_session_mode=false` +
//! `json_response=true`), so those clients work with no `mcp-remote` shim.
//! `--http-stateful` restores the initialize-era session behaviour.
//!
//! These tests drive the exact `StreamableHttpService` wiring from
//! `serve.rs` through an axum router, so they catch a regression in either
//! direction.

use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tempfile::TempDir;
use tower::ServiceExt;

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
const DISCOVER: &str = r#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"dual-era-test","version":"1.0"}}}}"#;
const TOOLS_CALL_STATUS: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_status","arguments":{}}}"#;
const TOOLS_LIST: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}"#;
/// Exact JSON the generated Pi `mcpNotify` posts (#1136).
const INITIALIZED_NOTIFICATION: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
/// The previous Pi `mcpRpc` shape: a notification sent as a request (#1136).
const INITIALIZED_AS_REQUEST: &str =
    r#"{"jsonrpc":"2.0","id":63,"method":"notifications/initialized","params":{}}"#;

/// Build a `/mcp` router exactly like `serve.rs` does, toggling stateful
/// mode. Returns the `Store` too so the writer actor stays alive for the
/// duration of the test.
async fn make_router(tmp: &TempDir, stateful: bool) -> (Router, Store) {
    make_router_with_strip(tmp, stateful, false).await
}

/// [`make_router`] with the `strip_root_combinators` server toggle exposed.
async fn make_router_with_strip(
    tmp: &TempDir,
    stateful: bool,
    strip_root_combinators: bool,
) -> (Router, Store) {
    make_router_with_dialect(tmp, stateful, strip_root_combinators, false).await
}

/// [`make_router`] with the `gemini_safe_schemas` server toggle on.
async fn make_router_gemini_safe(tmp: &TempDir, stateful: bool) -> (Router, Store) {
    make_router_with_dialect(tmp, stateful, false, true).await
}

/// [`make_router`] with both schema-dialect toggles exposed.
async fn make_router_with_dialect(
    tmp: &TempDir,
    stateful: bool,
    strip_root_combinators: bool,
    gemini_safe_schemas: bool,
) -> (Router, Store) {
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
        .with_wiki(Wiki::new(tmp.path(), store.writer.clone()).unwrap())
        .with_strip_root_combinators(strip_root_combinators)
        .with_gemini_safe_schemas(gemini_safe_schemas);
    let svc = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(stateful)
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

/// [`post`] for a 2026-07-28 request: that revision carries the negotiated
/// version both in `_meta` and the `MCP-Protocol-Version` header.
fn post_discover() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "server/discover")
        .body(Body::from(DISCOVER))
        .unwrap()
}

/// [`post`] against an explicit URI (tests carrying `?flavor=moonshot`).
fn post_to(uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        // rmcp's DNS-rebinding guard rejects a missing/disallowed Host with
        // 400; `localhost` is in the default allowlist. Real HTTP clients
        // always send Host — oneshot does not, so set it explicitly.
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body.into())
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

/// Older clients keep negotiating their own revision after the rmcp 3 bump
/// (#1924): an `initialize` naming 2024-11-05, 2025-03-26 or 2025-06-18 gets
/// that version back with the ai-memory server identity and instructions, in
/// both the default stateless mode and `--http-stateful`.
#[tokio::test]
async fn legacy_initialize_negotiates_each_older_protocol_version() {
    for stateful in [false, true] {
        for version in ["2024-11-05", "2025-03-26", "2025-06-18"] {
            let tmp = TempDir::new().unwrap();
            let (router, _store) = make_router(&tmp, stateful).await;
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"legacy","version":"1.0"}}}}}}"#
            );
            let resp = router.oneshot(post_to("/mcp", request)).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{version} stateful={stateful}"
            );
            let body = body_string(resp).await;
            // Stateful mode answers over SSE; take the JSON-RPC payload line.
            let payload = body
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim))
                .find(|data| !data.is_empty())
                .unwrap_or(body.as_str());
            let json: serde_json::Value = serde_json::from_str(payload)
                .unwrap_or_else(|_| panic!("initialize returns JSON: {body}"));
            assert_eq!(
                json["result"]["protocolVersion"],
                serde_json::json!(version),
                "stateful={stateful}: {body}"
            );
            assert_eq!(
                json["result"]["serverInfo"]["name"],
                serde_json::json!("ai-memory"),
                "{body}"
            );
            assert!(
                json["result"]["instructions"]
                    .as_str()
                    .is_some_and(|text| text.contains("memory_query")),
                "{body}"
            );
        }
    }
}

/// A 2026-07-28 client skips `initialize` entirely: a `tools/call` carrying
/// the per-request version metadata is served on its own, and reaches the
/// same tool router (#1924).
#[tokio::test]
async fn modern_tools_call_is_served_without_a_handshake() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", "memory_status")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"memory_status","arguments":{},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"modern","version":"1.0"}}}}"#,
        ))
        .unwrap();
    let resp = router.oneshot(request).await.unwrap();
    let status = resp.status();
    let body = body_string(resp).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("tools/call returns JSON: {body}"));
    assert!(json.get("error").is_none(), "{body}");
    assert!(json["result"]["content"].is_array(), "{body}");
}

/// rmcp 3 serves the MCP 2026-07-28 lifecycle probe directly: a stateless
/// `server/discover` names the versions and tool capability without an
/// initialize handshake, while the test above proves that handshake still
/// works for a legacy client (#1924).
#[tokio::test]
async fn stateless_server_discover_names_both_eras() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.clone().oneshot(post_discover()).await.unwrap();
    let status = resp.status();
    let body = body_string(resp).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("discover returns JSON: {body}"));
    let result = json["result"]
        .as_object()
        .unwrap_or_else(|| panic!("missing discover result: {body}"));
    assert_eq!(
        result.get("resultType").and_then(|value| value.as_str()),
        Some("complete"),
        "{body}"
    );
    let versions = result
        .get("supportedVersions")
        .and_then(|value| value.as_array())
        .unwrap_or_else(|| panic!("missing supportedVersions: {body}"));
    assert!(
        versions.contains(&serde_json::json!("2026-07-28")),
        "{body}"
    );
    assert!(
        versions.contains(&serde_json::json!("2024-11-05")),
        "the legacy initialize era stays advertised: {body}"
    );
    assert!(
        result
            .get("capabilities")
            .and_then(|value| value.get("tools"))
            .is_some(),
        "{body}"
    );
}

/// #1136: JSON-RPC 2.0 notifications have no `id`. Streamable HTTP in the
/// default stateless JSON mode must accept `notifications/initialized` with
/// 202 and an empty body, which is what the generated Pi `mcpNotify` now
/// posts and treats as success without calling `response.json()`.
#[tokio::test]
async fn stateless_initialized_notification_is_accepted_empty() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let initialized = router.clone().oneshot(post(INITIALIZE)).await.unwrap();
    assert_eq!(initialized.status(), StatusCode::OK);

    let resp = router
        .clone()
        .oneshot(post(INITIALIZED_NOTIFICATION))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "a JSON-RPC notification must be 202 Accepted"
    );
    let body = body_string(resp).await;
    assert!(
        body.is_empty(),
        "notification response must be empty so clients do not parse JSON, got: {body:?}"
    );

    let listed = router.oneshot(post(TOOLS_LIST)).await.unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = body_string(listed).await;
    let json: serde_json::Value =
        serde_json::from_str(&listed_body).expect("tools/list returns JSON");
    assert!(
        json.get("error").is_none(),
        "tools/list after the notification must succeed: {listed_body}"
    );
    assert!(
        listed_body.contains("memory_status"),
        "tools/list should name a real tool: {listed_body}"
    );
}

/// #1136 adversarial control: the previous Pi generator assigned an `id`,
/// so rmcp treated `notifications/initialized` as a request method and
/// returned JSON-RPC -32601. This is the warning operators saw on every
/// Pi launch. Restoring that payload must still fail this way; a blanket
/// accept of the method name would hide a client regression.
#[tokio::test]
async fn stateless_initialized_sent_as_a_request_is_method_not_found() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.oneshot(post(INITIALIZED_AS_REQUEST)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a JSON-RPC *request* still gets an HTTP 200 envelope, got {}",
        resp.status()
    );
    let body = body_string(resp).await;
    let json: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("expected JSON-RPC error body: {body}\n{e}"));
    assert_eq!(
        json["error"]["code"].as_i64(),
        Some(-32601),
        "request-shaped initialized must be method-not-found, got: {body}"
    );
}

async fn rpc(router: &Router, token: &str, body: serde_json::Value) -> serde_json::Value {
    let mut request = post_to("/mcp", body.to_string());
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_str(&body_string(response).await).unwrap()
}

async fn generic_tool(
    router: &Router,
    token: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let reply = rpc(
        router,
        token,
        serde_json::json!({
            "jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":name,"arguments":arguments},
        }),
    )
    .await;
    assert!(reply.get("error").is_none(), "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let text = reply["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&text).unwrap()
}

/// The public guide's direct-memory sequence works for a static machine
/// client with a user key, without native hooks or a transport session.
#[tokio::test]
async fn generic_machine_client_writes_queries_and_claims_a_handoff() {
    use ai_memory_core::{ApiCredentialId, NewUser, UserRole};
    use ai_memory_mcp::auth::{AuthState, require_bearer};
    use ai_memory_store::{TokenPepper, api_key_preview, generate_api_key, hash_token};
    use serde_json::json;
    use std::sync::Arc;

    let tmp = TempDir::new().unwrap();
    let (router, store) = make_router(&tmp, false).await;
    let user_id = store
        .writer
        .create_human_user(
            NewUser {
                username: "example-client".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let pepper = TokenPepper::new("generic-client-test-pepper");
    let token = generate_api_key().unwrap();
    store
        .writer
        .create_api_credential(
            ApiCredentialId::new(),
            user_id,
            "generic-client".into(),
            hash_token(&token, &pepper),
            Some(api_key_preview(&token)),
        )
        .await
        .unwrap();
    let auth = AuthState::new(Some("generic-client-root-control".into())).with_multiuser(
        pepper,
        store.reader.clone(),
        store.writer.clone(),
    );
    let router = router.layer(axum::middleware::from_fn_with_state(
        Arc::new(auth),
        require_bearer,
    ));
    let initialized = rpc(
        &router,
        &token,
        json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"protocolVersion":"2024-11-05","capabilities":{},
              "clientInfo":{"name":"example-client","version":"1.0"}},
        }),
    )
    .await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "ai-memory");
    let tools = rpc(
        &router,
        &token,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/list","params":{},
        }),
    )
    .await;
    for name in [
        "memory_write_page",
        "memory_query",
        "memory_handoff_begin",
        "memory_handoff_accept",
    ] {
        assert!(
            tools["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name)
        );
    }

    let written = generic_tool(
        &router,
        &token,
        "memory_write_page",
        json!({
            "workspace":"demo","project":"app","path":"notes/retries.md",
            "body":"# Retry policy\nKeep the same event ID when retrying delivery.",
        }),
    )
    .await;
    assert_eq!(written["path"], "notes/retries.md");
    assert!(written["page_id"].is_string());
    let queried = generic_tool(
        &router,
        &token,
        "memory_query",
        json!({
            "workspace":"demo","project":"app","query":"retry policy",
        }),
    )
    .await;
    assert_eq!(queried["hits"].as_array().unwrap().len(), 1);
    assert_eq!(queried["hits"][0]["path"], "notes/retries.md");

    // A same-named project in another workspace must never become the target.
    generic_tool(
        &router,
        &token,
        "memory_write_page",
        json!({
            "workspace":"other-team","project":"app","path":"notes/foreign.md",
            "body":"# Foreign project\nForeign workspace control.",
        }),
    )
    .await;
    let foreign = generic_tool(
        &router,
        &token,
        "memory_query",
        json!({
            "workspace":"other-team","project":"app","query":"retry policy",
        }),
    )
    .await;
    assert_eq!(foreign["hits"], json!([]));
    let partial = rpc(
        &router,
        &token,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"memory_write_page","arguments":{
                "workspace":"demo","path":"notes/partial.md","body":"Must be refused",
            }},
        }),
    )
    .await;
    assert!(
        partial.get("error").is_some() || partial["result"]["isError"] == true,
        "{partial}"
    );

    let pending = generic_tool(
        &router,
        &token,
        "memory_handoff_begin",
        json!({
            "workspace":"demo","project":"app",
            "summary":"The retry policy was saved. Add the delivery test next.",
            "next_steps":["Add a retry regression test."],
        }),
    )
    .await;
    let handoff_id = pending["handoff_id"].as_str().unwrap();
    let foreign_claim = generic_tool(
        &router,
        &token,
        "memory_handoff_accept",
        json!({
            "workspace":"other-team","project":"app","handoff_id":handoff_id,
        }),
    )
    .await;
    assert_eq!(foreign_claim["handoff"], serde_json::Value::Null);
    let claimed = generic_tool(
        &router,
        &token,
        "memory_handoff_accept",
        json!({
            "workspace":"demo","project":"app","handoff_id":handoff_id,
        }),
    )
    .await;
    assert_eq!(claimed["status"], "claimed");
    assert_eq!(
        claimed["handoff"]["summary"],
        "The retry policy was saved. Add the delivery test next."
    );
    let repeated = generic_tool(
        &router,
        &token,
        "memory_handoff_accept",
        json!({
            "workspace":"demo","project":"app","handoff_id":handoff_id,
        }),
    )
    .await;
    assert_eq!(repeated["status"], "none_pending");
    assert_eq!(repeated["handoff"], serde_json::Value::Null);
}

/// Contrast / guard: with `--http-stateful` (legacy session mode), the same
/// session-less `tools/call` is rejected with 400 "Session ID is required".
/// This proves the default stateless mode is what resolves issue #3, and
/// pins the opt-in behaviour so a future change to the default can't
/// silently regress it.
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
        StatusCode::BAD_REQUEST,
        "stateful mode must reject a session-less tools/call"
    );
    let body = body_string(resp).await;
    assert!(
        body.contains("Session ID is required"),
        "stateful rejection should name the missing session: {body}"
    );
}

/// Pull `memory_read_page`'s inputSchema from a tools/list response body.
fn read_page_input_schema(body: &str) -> serde_json::Value {
    input_schema_of(body, "memory_read_page")
}

/// Pull any named tool's inputSchema from a tools/list response body.
fn input_schema_of(body: &str, name: &str) -> serde_json::Value {
    let json: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("tools/list response must be JSON, got: {body}\nerr: {e}"));
    let tools = json["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("missing result.tools: {body}"));
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from tools/list: {body}"))["inputSchema"]
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

/// Generic clients (OpenCode, Cursor) never send the `?flavor=` marker, yet
/// forward tool schemas verbatim to strict upstreams. The
/// `strip_root_combinators` toggle must serve the restricted dialect to them
/// anyway (issue #412) — same transport wiring, no flavor parameter.
#[tokio::test]
async fn stateless_config_strip_strips_root_any_of_without_flavor() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router_with_strip(&tmp, false, true).await;

    let resp = router.oneshot(post_to("/mcp", TOOLS_LIST)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "config strip must remove root `{key}` without a flavor marker: {schema}"
        );
    }
    assert!(
        schema.get("properties").is_some(),
        "the flat schema must keep describing the args: {schema}"
    );
}

/// #577 inverted the default: the source schema no longer carries a
/// root `anyOf` at all, so even with the strip toggle OFF and no flavor
/// marker, every Messages-API-routed client gets a session-safe schema.
/// (#155's early refusal moved to descriptions + runtime validation.)
#[tokio::test]
async fn stateless_config_without_strip_is_already_root_combinator_free() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.oneshot(post_to("/mcp", TOOLS_LIST)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "the default schema must be root-combinator-free (#577): {schema}"
        );
    }
    assert!(
        schema.get("properties").is_some(),
        "the flat schema must keep describing the args: {schema}"
    );
}

/// A pass-through client on a Gemini/Vertex model 400s on the union types
/// `schemars` emits for optional args ("specified other fields alongside
/// any_of"). `?flavor=gemini` must collapse them to Google's single-`type` plus
/// `nullable` form, and strip the root combinators the older dialects strip.
#[tokio::test]
async fn stateless_gemini_flavor_collapses_nullable_unions() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .oneshot(post_to("/mcp?flavor=gemini", TOOLS_LIST))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "gemini flavor must strip root `{key}`: {schema}"
        );
    }
    assert_eq!(
        schema["properties"]["query"]["type"],
        serde_json::json!("string"),
        "the nullable union must collapse to a single type: {schema}"
    );
    assert_eq!(
        schema["properties"]["query"]["nullable"],
        serde_json::json!(true),
        "optionality must survive as `nullable`: {schema}"
    );
}

/// OpenCode and friends cannot carry a `?flavor=` marker, so the config toggle
/// has to serve the same dialect without one — the issue #412 rationale, now
/// for Vertex.
#[tokio::test]
async fn stateless_config_gemini_safe_collapses_unions_without_flavor() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router_gemini_safe(&tmp, false).await;

    let resp = router.oneshot(post_to("/mcp", TOOLS_LIST)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    assert!(
        schema.get("anyOf").is_none(),
        "gemini_safe_schemas implies stripping the root anyOf: {schema}"
    );
    assert_eq!(
        schema["properties"]["query"]["type"],
        serde_json::json!("string"),
        "config toggle must collapse unions without a marker: {schema}"
    );
}

/// The Moonshot/Bedrock dialect must not start collapsing unions: it is a
/// narrower patch, and changing it would alter shipped behavior for Kimi/Kiro.
#[tokio::test]
async fn stateless_moonshot_flavor_keeps_nullable_unions() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .oneshot(post_to("/mcp?flavor=moonshot", TOOLS_LIST))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    assert_eq!(
        schema["properties"]["query"]["type"],
        serde_json::json!(["string", "null"]),
        "the root-combinator dialect must leave union types alone: {schema}"
    );
}

/// Codex forwards MCP input schemas into Responses `tools.function.parameters`
/// verbatim, and Moonshot's validator never resolves `$ref` — every reference
/// 400s the call with "detected infinite recursion without termination
/// condition". `?flavor=moonshot` must therefore inline every `#/$defs/*`
/// reference (nested combinators themselves pass, so the nullable union on
/// `reasoning` stays).
#[tokio::test]
async fn stateless_moonshot_flavor_inlines_defs() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .oneshot(post_to("/mcp?flavor=moonshot", TOOLS_LIST))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_string(resp).await;
    for tool_name in ["memory_query", "memory_explore", "memory_feedback"] {
        let schema = input_schema_of(&body, tool_name);
        assert!(
            schema.get("$defs").is_none(),
            "moonshot flavor must drop the $defs table from {tool_name}: {schema}"
        );
        assert!(
            !serde_json::to_string(&schema)
                .expect("schema serializes")
                .contains("$ref"),
            "moonshot flavor must not leave a single $ref in {tool_name}: {schema}"
        );
    }
    let query = input_schema_of(&body, "memory_query");
    assert!(
        query["properties"]["reasoning"]["anyOf"].is_array(),
        "the nested nullable union must survive inlining: {query}"
    );
}

/// Kiro's Bedrock dialect is deliberately narrower than Moonshot's: it strips
/// root combinators but keeps `$defs`/`$ref` pairs, which Bedrock resolves.
#[tokio::test]
async fn stateless_bedrock_flavor_keeps_defs() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .oneshot(post_to("/mcp?flavor=bedrock", TOOLS_LIST))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = input_schema_of(&body_string(resp).await, "memory_feedback");
    assert!(
        schema.get("$defs").is_some(),
        "bedrock flavor must keep the $defs table: {schema}"
    );
    assert!(
        serde_json::to_string(&schema)
            .expect("schema serializes")
            .contains("$ref"),
        "bedrock flavor must keep $ref pairs: {schema}"
    );
}

/// Vertex accepts `$defs`/`$ref` and Gemini CLI ships them untouched, so the
/// Gemini dialect must keep flattening references out of scope.
#[tokio::test]
async fn stateless_gemini_flavor_keeps_defs() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router
        .oneshot(post_to("/mcp?flavor=gemini", TOOLS_LIST))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = input_schema_of(&body_string(resp).await, "memory_feedback");
    assert!(
        schema.get("$defs").is_some(),
        "gemini flavor must keep the $defs table: {schema}"
    );
    assert!(
        serde_json::to_string(&schema)
            .expect("schema serializes")
            .contains("$ref"),
        "gemini flavor must keep $ref pairs: {schema}"
    );
}

/// Unflavored tools/list is root-combinator-free too (#577): the safe
/// shape is the default, not a per-flavor patch.
#[tokio::test]
async fn stateless_tools_list_without_flavor_is_root_combinator_free() {
    let tmp = TempDir::new().unwrap();
    let (router, _store) = make_router(&tmp, false).await;

    let resp = router.oneshot(post(TOOLS_LIST)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema = read_page_input_schema(&body_string(resp).await);
    for key in ["anyOf", "oneOf", "allOf"] {
        assert!(
            schema.get(key).is_none(),
            "unflavored tools/list must be root-combinator-free (#577): {schema}"
        );
    }
}
