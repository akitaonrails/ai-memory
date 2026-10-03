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
use ai_memory_wiki::Wiki;
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

mod native_session_source {
    // Native source inspection through the production JSON-RPC MCP handler.
    use ai_memory_core::{
        ActorContext, ApiCredentialId, IdentityKey, NewUser, SessionId, UserRole,
    };
    use ai_memory_mcp::AiMemoryServer;
    use ai_memory_mcp::auth::{AuthState, require_bearer};
    use ai_memory_store::{Store, TokenPepper, generate_api_key, hash_token};
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const ROOT_TOKEN: &str = "native-source-test-root";
    const NOW: i64 = 1_700_000_000_000_000;

    struct Harness {
        store: Store,
        ws: ai_memory_core::WorkspaceId,
        pj: ai_memory_core::ProjectId,
        alice_token: String,
        bob_token: String,
        sid: SessionId,
        pepper: TokenPepper,
        _tmp: TempDir,
    }

    impl Harness {
        async fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let store = Store::open(tmp.path()).unwrap();
            let ws = store
                .writer
                .get_or_create_workspace("fixture-ws")
                .await
                .unwrap();
            let pj = store
                .writer
                .get_or_create_project(ws, "fixture-project", None)
                .await
                .unwrap();
            let foreign = store
                .writer
                .get_or_create_project(ws, "foreign-project", None)
                .await
                .unwrap();
            let foreign_ws = store
                .writer
                .get_or_create_workspace("foreign-ws")
                .await
                .unwrap();
            let foreign_pj = store
                .writer
                .get_or_create_project(foreign_ws, "fixture-project", None)
                .await
                .unwrap();
            let pepper = TokenPepper::new("native-source-test-pepper");
            let mut users = Vec::new();
            for name in ["alice", "bob"] {
                let uid = store
                    .writer
                    .create_human_user(
                        NewUser {
                            username: name.into(),
                            name: None,
                            email: None,
                        },
                        UserRole::User,
                        None,
                        false,
                    )
                    .await
                    .unwrap();
                let token = generate_api_key().unwrap();
                store
                    .writer
                    .create_api_credential(
                        ApiCredentialId::new(),
                        uid,
                        name.into(),
                        hash_token(&token, &pepper),
                        None,
                    )
                    .await
                    .unwrap();
                users.push((uid, token));
            }
            let (alice, alice_token) = users.remove(0);
            let (_, bob_token) = users.remove(0);
            let conn = Connection::open(store.db_path()).unwrap();
            for (w, p) in [(ws, pj), (ws, foreign), (foreign_ws, foreign_pj)] {
                conn.execute(
                    "UPDATE projects SET access_mode='restricted' WHERE id=?1",
                    params![p.as_bytes()],
                )
                .unwrap();
                conn.execute("INSERT INTO project_grants(workspace_id,project_id,user_id,level,granted_at) VALUES(?1,?2,?3,'read',?4)", params![w.as_bytes(), p.as_bytes(), alice.as_bytes(), NOW]).unwrap();
            }
            let sid = SessionId::new();
            conn.execute("INSERT INTO sessions(id,workspace_id,project_id,agent_kind,cwd,started_at,actor_user) VALUES(?1,?2,?3,'codex','/fixture',?4,?5)", params![sid.as_bytes(), ws.as_bytes(), pj.as_bytes(), NOW, IdentityKey::User("alice".into()).storage_key()]).unwrap();
            for (title, created) in [("first", NOW + 2), ("second", NOW + 1)] {
                conn.execute("INSERT INTO observations(id,session_id,workspace_id,project_id,kind,title,body,importance,created_at) VALUES(?1,?2,?3,?4,'user-prompt',?5,'historical body',5,?6)", params![SessionId::new().as_bytes(), sid.as_bytes(), ws.as_bytes(), pj.as_bytes(), title, created]).unwrap();
            }
            // A legitimate legacy session crossed projects while its native anchor stayed put.
            conn.execute("INSERT INTO observations(id,session_id,workspace_id,project_id,kind,title,body,importance,created_at) VALUES(?1,?2,?3,?4,'user-prompt','foreign observation','foreign body',5,?5)", params![SessionId::new().as_bytes(), sid.as_bytes(), ws.as_bytes(), foreign.as_bytes(), NOW + 3]).unwrap();
            Self {
                store,
                ws,
                pj,
                alice_token,
                bob_token,
                sid,
                pepper,
                _tmp: tmp,
            }
        }

        fn args(&self) -> Value {
            json!({"workspace":"fixture-ws", "project":"fixture-project", "native_source":{"kind":"session", "id":self.sid.to_string()}, "limit":2})
        }

        fn share_source(&self) {
            Connection::open(self.store.db_path())
                .unwrap()
                .execute(
                    "UPDATE sessions SET actor_user=NULL WHERE id=?1",
                    params![self.sid.as_bytes()],
                )
                .unwrap();
        }

        fn router(
            &self,
            authenticated: bool,
            probe: Option<Arc<dyn Fn() + Send + Sync>>,
        ) -> Router {
            self.router_with_root_actor(authenticated, probe, ActorContext::default())
        }

        fn router_with_root_actor(
            &self,
            authenticated: bool,
            probe: Option<Arc<dyn Fn() + Send + Sync>>,
            root_actor: ActorContext,
        ) -> Router {
            let auth = if authenticated {
                AuthState::new(Some(ROOT_TOKEN.into()))
                    .with_root_actor(root_actor)
                    .with_multiuser(
                        self.pepper.clone(),
                        self.store.reader.clone(),
                        self.store.writer.clone(),
                    )
            } else {
                AuthState::default()
            };
            self.service(probe)
                .layer(axum::middleware::from_fn_with_state(
                    Arc::new(auth),
                    require_bearer,
                ))
        }

        fn service(&self, probe: Option<Arc<dyn Fn() + Send + Sync>>) -> Router {
            let mut server = AiMemoryServer::new(
                self.store.reader.clone(),
                self.store.writer.clone(),
                self.ws,
                self.pj,
            );
            server = server.with_wiki(
                ai_memory_wiki::Wiki::new(self._tmp.path(), self.store.writer.clone()).unwrap(),
            );
            server.native_capture_probe = probe;
            let service = StreamableHttpService::new(
                move || Ok(server.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default()
                    .with_stateful_mode(false)
                    .with_json_response(true),
            );
            Router::new().nest_service("/mcp", service)
        }

        fn probe(&self, sql: &str) -> Arc<dyn Fn() + Send + Sync> {
            let path = self.store.db_path().to_path_buf();
            let sql = sql.to_string();
            Arc::new(move || {
                Connection::open(&path)
                    .unwrap()
                    .execute_batch(&sql)
                    .unwrap()
            })
        }

        fn content_state(&self) -> Value {
            let conn = Connection::open(self.store.db_path()).unwrap();
            let mut state = serde_json::Map::new();
            for table in [
                "pages",
                "page_evidence",
                "sessions",
                "observations",
                "workspaces",
                "projects",
                "project_grants",
            ] {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
                    .unwrap();
                let columns = stmt.column_count();
                let rows = stmt
                    .query_map([], |row| {
                        (0..columns)
                            .map(|i| row.get_ref(i).map(|v| format!("{v:?}")))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                state.insert(table.into(), json!(rows));
            }
            Value::Object(state)
        }
    }

    async fn wire(router: &Router, token: Option<&str>, arguments: Value) -> (Value, usize) {
        rpc(
            router,
            token,
            "tools/call",
            json!({"name":"memory_read_session_observations", "arguments":arguments}),
        )
        .await
    }

    async fn rpc(
        router: &Router,
        token: Option<&str>,
        method: &str,
        params: Value,
    ) -> (Value, usize) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            // These untrusted headers must never grant authority or replace the real caller.
            .header("x-memory-actor-user", "alice")
            .header("x-memory-auth-level", "root");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .clone()
            .oneshot(
                request
                    .body(Body::from(
                        json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
            .await
            .unwrap();
        (serde_json::from_slice(&bytes).unwrap(), bytes.len())
    }

    fn payload(v: &Value) -> Value {
        assert!(v.get("error").is_none(), "legitimate control failed: {v}");
        assert_ne!(
            v.pointer("/result/isError"),
            Some(&json!(true)),
            "tool error: {v}"
        );
        serde_json::from_str(
            v.pointer("/result/content/0/text")
                .unwrap()
                .as_str()
                .unwrap(),
        )
        .unwrap()
    }

    fn refused(v: &Value) {
        assert!(
            v.get("error").is_some() || v.pointer("/result/isError") == Some(&json!(true)),
            "ATTACK unexpectedly admitted: {v}"
        );
        assert!(
            !v.to_string().contains("historical body"),
            "private data leaked: {v}"
        );
    }

    fn unavailable(v: &Value) {
        refused(v);
        assert!(
            v.to_string().contains("native source unavailable"),
            "denial must be generic: {v}"
        );
        assert!(!v.to_string().contains("foreign body"));
    }

    async fn authority_change_after_capture(sql: &str) {
        let h = Harness::new().await;
        payload(
            &wire(&h.router(true, None), Some(&h.alice_token), h.args())
                .await
                .0,
        );
        let (v, _) = wire(
            &h.router(true, Some(h.probe(sql))),
            Some(&h.alice_token),
            h.args(),
        )
        .await;
        unavailable(&v);
        for private in [
            h.sid.to_string(),
            "missing_credentials".into(),
            "digest".into(),
        ] {
            assert!(!v.to_string().contains(&private), "private diagnostic: {v}");
        }
    }

    #[tokio::test]
    async fn native_session_source_credential_rechecks_after_capture() {
        for sql in [
            "UPDATE api_credentials SET revoked_at=1 WHERE user_id=(SELECT id FROM users WHERE username='alice')",
            "UPDATE api_credentials SET expires_at=1 WHERE user_id=(SELECT id FROM users WHERE username='alice')",
            "DELETE FROM users WHERE username='alice'",
            "ALTER TABLE api_credentials RENAME TO missing_credentials",
        ] {
            authority_change_after_capture(sql).await;
        }
    }

    #[tokio::test]
    async fn native_session_source_rotation_keeps_id_but_refuses_old_hash() {
        authority_change_after_capture("UPDATE api_credentials SET token_hash=randomblob(32) WHERE user_id=(SELECT id FROM users WHERE username='alice')").await;
    }

    #[tokio::test]
    async fn native_session_source_changed_user_id_is_refused() {
        authority_change_after_capture("UPDATE users SET username='former-alice' WHERE username='alice'; UPDATE users SET username='alice' WHERE username='bob'; UPDATE api_credentials SET user_id=(SELECT id FROM users WHERE username='alice') WHERE user_id=(SELECT id FROM users WHERE username='former-alice')").await;
    }

    #[tokio::test]
    async fn native_session_source_changed_credential_id_is_refused() {
        authority_change_after_capture("UPDATE api_credentials SET id=randomblob(16) WHERE user_id=(SELECT id FROM users WHERE username='alice')").await;
    }

    #[tokio::test]
    async fn native_session_source_changed_canonical_owner_is_refused() {
        authority_change_after_capture(
            "UPDATE users SET username='renamed' WHERE username='alice'",
        )
        .await;
    }

    #[tokio::test]
    async fn native_session_source_requires_genuine_origin_before_capture() {
        use ai_memory_core::AuthLevel;
        use std::sync::atomic::{AtomicBool, Ordering};
        let h = Harness::new().await;
        h.share_source(); // A private owner would mask a missing HTTP-origin guard.
        let captured = Arc::new(AtomicBool::new(false));
        let seen = captured.clone();
        let router = h
            .service(Some(Arc::new(move || {
                seen.store(true, Ordering::SeqCst);
            })))
            .layer(axum::middleware::from_fn(
                |mut req: Request<Body>, next: axum::middleware::Next| async move {
                    req.extensions_mut().insert(AuthLevel::Root);
                    req.extensions_mut().insert(ActorContext {
                        user: Some("alice".into()),
                        ..Default::default()
                    });
                    next.run(req).await
                },
            ));
        payload(
            &wire(&h.router(true, None), Some(ROOT_TOKEN), h.args())
                .await
                .0,
        );
        let (v, _) = wire(&router, None, h.args()).await;
        assert!(
            !captured.load(Ordering::SeqCst),
            "capture reached without authenticating origin"
        );
        unavailable(&v);
        assert!(!v.to_string().contains(&h.sid.to_string()));
    }

    #[tokio::test]
    async fn native_session_source_rechecks_credential_before_capture() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let h = Harness::new().await;
        let captured = Arc::new(AtomicBool::new(false));
        let seen = captured.clone();
        let revoke = h.probe("UPDATE api_credentials SET revoked_at=1 WHERE user_id=(SELECT id FROM users WHERE username='alice')");
        let router = h
            .service(Some(Arc::new(move || {
                seen.store(true, Ordering::SeqCst);
            })))
            .layer(axum::middleware::from_fn(
                move |req: Request<Body>, next: axum::middleware::Next| {
                    let revoke = revoke.clone();
                    async move {
                        revoke();
                        next.run(req).await
                    }
                },
            ))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(AuthState::new(Some(ROOT_TOKEN.into())).with_multiuser(
                    h.pepper.clone(),
                    h.store.reader.clone(),
                    h.store.writer.clone(),
                )),
                require_bearer,
            ));
        payload(
            &wire(&h.router(true, None), Some(&h.alice_token), h.args())
                .await
                .0,
        );
        let (v, _) = wire(&router, Some(&h.alice_token), h.args()).await;
        assert!(
            !captured.load(Ordering::SeqCst),
            "capture reached with inactive credential"
        );
        unavailable(&v);
        assert!(!v.to_string().contains(&h.sid.to_string()));
    }

    #[tokio::test]
    async fn native_session_source_legacy_expiry_and_human_disabled_api_control() {
        let h = Harness::new().await;
        let legacy_token = generate_api_key().unwrap();
        let legacy = h
            .store
            .writer
            .create_user(
                NewUser {
                    username: "legacy".into(),
                    name: None,
                    email: None,
                },
                hash_token(&legacy_token, &h.pepper),
            )
            .await
            .unwrap();
        Connection::open(h.store.db_path()).unwrap().execute_batch(
            "UPDATE projects SET access_mode='open'; UPDATE sessions SET actor_user='user:legacy'",
        ).unwrap();
        payload(
            &wire(&h.router(true, None), Some(&legacy_token), h.args())
                .await
                .0,
        );
        let (v, _) = wire(
            &h.router(
                true,
                Some(h.probe("UPDATE users SET token_expired_at=1 WHERE username='legacy'")),
            ),
            Some(&legacy_token),
            h.args(),
        )
        .await;
        unavailable(&v);
        assert!(!v.to_string().contains(&legacy.to_string()));
        let h = Harness::new().await;
        payload(
            &wire(
                &h.router(
                    true,
                    Some(h.probe("UPDATE users SET disabled_at=1 WHERE username='alice'")),
                ),
                Some(&h.alice_token),
                h.args(),
            )
            .await
            .0,
        );
        payload(
            &wire(&h.router(true, None), Some(&h.alice_token), h.args())
                .await
                .0,
        );
    }

    #[tokio::test]
    async fn native_session_source_trusted_proxy_and_metadata_controls() {
        let h = Harness::new().await;
        h.share_source();
        Connection::open(h.store.db_path())
            .unwrap()
            .execute_batch("UPDATE projects SET access_mode='open'")
            .unwrap();
        let router = h.service(None).layer(axum::middleware::from_fn_with_state(
            Arc::new(
                AuthState::new(Some(ROOT_TOKEN.into()))
                    .with_trusted_proxy_bearer("proxy-test")
                    .with_root_actor(ActorContext {
                        issuer: Some("https://fixture-issuer".into()),
                        sub: Some("root-subject".into()),
                        ..Default::default()
                    }),
            ),
            require_bearer,
        ));
        // This proxy authenticates Alice but supplies no DB UserId for native access.
        unavailable(&wire(&router, Some("proxy-test"), h.args()).await.0);
        for owned in [false, true] {
            if owned {
                Connection::open(h.store.db_path())
                    .unwrap()
                    .execute_batch("UPDATE sessions SET actor_user='user:alice'")
                    .unwrap();
            }
            let request = Request::builder().method("POST").uri("/mcp")
                .header("host", "localhost").header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", "Bearer proxy-test")
                .header("x-memory-actor-user", "alice")
                .header("x-memory-actor-issuer", "https://fixture-issuer")
                .header("x-memory-actor-sub", "root-subject")
                .body(Body::from(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"memory_read_session_observations", "arguments":h.args()}}).to_string())).unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
                .await
                .unwrap();
            let value = serde_json::from_slice::<Value>(&bytes).unwrap();
            if owned {
                unavailable(&value);
            } else {
                payload(&value);
            }
        }
        let h = Harness::new().await;
        Connection::open(h.store.db_path())
            .unwrap()
            .execute_batch("UPDATE projects SET access_mode='open'")
            .unwrap();
        let (v, _) = rpc(&h.router(true, None), Some(&h.bob_token), "tools/call", json!({"name":"memory_read_session_observations", "arguments":h.args(), "_meta":{"ai.opencode/sessionID":h.sid.to_string(), "authLevel":"root", "user":"alice", "nativeAuthOrigin":"ConfiguredRoot"}})).await;
        unavailable(&v);
        unavailable(&wire(&h.router(false, None), None, h.args()).await.0);
    }

    #[tokio::test]
    async fn native_session_source_root_is_unattributed_and_db_root_remains_owned() {
        let h = Harness::new().await;
        let conn = Connection::open(h.store.db_path()).unwrap();
        let actors = [
            ActorContext::default(),
            ActorContext {
                user: Some("alice".into()),
                ..ActorContext::default()
            },
        ];
        payload(
            &wire(&h.router(true, None), Some(&h.alice_token), h.args())
                .await
                .0,
        );
        h.share_source();
        for actor in &actors {
            payload(
                &wire(
                    &h.router_with_root_actor(true, None, actor.clone()),
                    Some(ROOT_TOKEN),
                    h.args(),
                )
                .await
                .0,
            );
        }
        conn.execute("UPDATE sessions SET actor_user='user:alice'", [])
            .unwrap();
        for actor in &actors {
            let (v, _) = wire(
                &h.router_with_root_actor(true, None, actor.clone()),
                Some(ROOT_TOKEN),
                h.args(),
            )
            .await;
            assert!(
                v.get("error").is_some() || v.pointer("/result/isError") == Some(&json!(true)),
                "ROOT PRIVATE OWNER REFUSAL: root must not read Alice: {v}"
            );
            unavailable(&v);
            assert!(!v.to_string().contains(&h.sid.to_string()));
        }
        conn.execute("UPDATE users SET role='root' WHERE username='alice'", [])
            .unwrap();
        let router = h.router(true, None);
        payload(&wire(&router, Some(&h.alice_token), h.args()).await.0);
        conn.execute("UPDATE sessions SET actor_user='user:bob'", [])
            .unwrap();
        unavailable(&wire(&router, Some(&h.alice_token), h.args()).await.0);
        conn.execute("UPDATE sessions SET actor_user='user:alice'", [])
            .unwrap();
        conn.execute("DELETE FROM project_grants", []).unwrap();
        refused(&wire(&router, Some(&h.alice_token), h.args()).await.0);
    }

    #[tokio::test]
    async fn native_session_source_real_caller_controls_and_legacy_origin() {
        let h = Harness::new().await;
        let router = h.router(true, None);
        let before = h.content_state();
        let (v, _) = wire(&router, Some(&h.alice_token), h.args()).await;
        let p = payload(&v);
        assert_eq!(p["observations"][0]["title"], "first");
        assert_eq!(p["observations"][1]["title"], "second");
        assert_eq!(p["observations"].as_array().unwrap().len(), 2);
        for hidden in [
            "elided_other_scope",
            "digest",
            "receipt",
            "verified",
            "auth",
            "session",
        ] {
            assert!(p.get(hidden).is_none());
        }
        unavailable(&wire(&router, Some(ROOT_TOKEN), h.args()).await.0);
        let (legacy, _) = wire(&router, Some(&h.alice_token), json!({"workspace":"fixture-ws", "project":"foreign-project", "session_id":h.sid.to_string()})).await;
        let legacy = payload(&legacy);
        assert_eq!(legacy["observations"][0]["body"], "foreign body");
        assert_eq!(legacy["elided_other_scope"], 2);
        for (ws, pj) in [
            ("fixture-ws", "foreign-project"),
            ("foreign-ws", "fixture-project"),
        ] {
            let mut args = h.args();
            args["workspace"] = json!(ws);
            args["project"] = json!(pj);
            let (v, _) = wire(&router, Some(&h.alice_token), args).await;
            unavailable(&v); // Alice has grants on both scopes; origin still must match exactly.
            assert!(!v.to_string().contains(&h.sid.to_string()));
        }
        assert_eq!(
            before,
            h.content_state(),
            "inspection must not write pages, projections or source rows"
        );
        for (ws, pj) in [
            ("missing-ws", "fixture-project"),
            ("fixture-ws", "missing-project"),
        ] {
            let mut args = h.args();
            args["workspace"] = json!(ws);
            args["project"] = json!(pj);
            let (v, _) = wire(&router, Some(ROOT_TOKEN), args).await;
            refused(&v);
        }
        assert_eq!(
            before,
            h.content_state(),
            "scope lookup must not create missing scopes"
        );
    }

    #[tokio::test]
    async fn native_session_source_private_second_operator_and_forged_authority() {
        let h = Harness::new().await;
        let conn = Connection::open(h.store.db_path()).unwrap();
        conn.execute("UPDATE projects SET access_mode='open'", [])
            .unwrap();
        let (v, _) = wire(&h.router(true, None), Some(&h.bob_token), h.args()).await;
        unavailable(&v); // An open project does not make Alice's private source Bob's.
        let (v, _) = wire(&h.router(false, None), None, h.args()).await;
        unavailable(&v); // Raw actor/root headers do not authenticate an anonymous client.
        let (control, _) = wire(&h.router(true, None), Some(&h.alice_token), h.args()).await;
        assert_eq!(
            payload(&control)["observations"].as_array().unwrap().len(),
            2
        );
        conn.execute("UPDATE sessions SET actor_user=NULL", [])
            .unwrap();
        let (control, _) = wire(&h.router(true, None), Some(&h.bob_token), h.args()).await;
        payload(&control); // Unattributed sources are shared with authenticated callers.
        let (v, _) = wire(&h.router(false, None), None, h.args()).await;
        unavailable(&v); // Even a shared source requires the real native-mode auth context.
    }

    #[tokio::test]
    async fn native_session_source_shape_is_rejected_before_private_lookup() {
        let h = Harness::new().await;
        let router = h.router(true, None);
        // Any accidental private read now produces a SQL error instead of the expected parameter error.
        Connection::open(h.store.db_path())
            .unwrap()
            .execute_batch("ALTER TABLE sessions RENAME TO unavailable_sessions")
            .unwrap();
        let mut attacks = Vec::new();
        let mut unscoped = h.args();
        unscoped.as_object_mut().unwrap().remove("workspace");
        unscoped.as_object_mut().unwrap().remove("project");
        attacks.push(unscoped);
        for field in ["workspace", "project"] {
            let mut args = h.args();
            args.as_object_mut().unwrap().remove(field);
            attacks.push(args);
            let mut args = h.args();
            args[field] = json!("   ");
            attacks.push(args);
        }
        for (field, value) in [
            ("session_id", json!(h.sid.to_string())),
            ("body_max_chars", json!(200)),
            ("kinds", json!([])),
            ("query", json!("")),
            ("offset", json!(1)),
            ("order", json!("desc")),
        ] {
            let mut args = h.args();
            args[field] = value;
            attacks.push(args);
        }
        for source in [
            json!({"kind":"page","id":h.sid.to_string()}),
            json!({"kind":"session","id":h.sid.to_string(),"root":true}),
            json!({"kind":"session","id":"bad"}),
            json!({"kind":"session","id":"00000000-0000-0000-0000-000000000000"}),
            json!({"kind":"session","id":"11111111-1111-1111-1111-11111111111A"}),
            json!({"kind":"session","id":"x".repeat(100_000)}),
        ] {
            let mut args = h.args();
            args["native_source"] = source;
            attacks.push(args);
        }
        for args in attacks {
            let (v, _) = wire(&router, Some(ROOT_TOKEN), args).await;
            refused(&v);
            assert!(
                v.pointer("/error/code") == Some(&json!(-32602))
                    || v.to_string().contains("failed to deserialize parameters"),
                "cheap validation must precede lookup: {v}"
            );
        }
    }

    #[tokio::test]
    async fn native_session_source_rechecks_prefix_owner_existence_and_grants() {
        for sql in [
            "UPDATE observations SET body='revised' WHERE title='first'",
            "DELETE FROM observations WHERE title='first'",
            "DELETE FROM observations; DELETE FROM sessions",
            "UPDATE sessions SET actor_user='user:bob'",
            "UPDATE sessions SET project_id=(SELECT id FROM projects WHERE name='foreign-project')",
            "DELETE FROM project_grants",
        ] {
            let h = Harness::new().await;
            let (control, _) = wire(&h.router(true, None), Some(&h.alice_token), h.args()).await;
            payload(&control);
            let (v, _) = wire(
                &h.router(true, Some(h.probe(sql))),
                Some(&h.alice_token),
                h.args(),
            )
            .await;
            unavailable(&v);
        }
        // Root captures only a shared source; becoming owned invalidates that capture.
        let h = Harness::new().await;
        h.share_source();
        payload(
            &wire(&h.router(true, None), Some(ROOT_TOKEN), h.args())
                .await
                .0,
        );
        let (v, _) = wire(
            &h.router(
                true,
                Some(h.probe("UPDATE sessions SET actor_user='changed'")),
            ),
            Some(ROOT_TOKEN),
            h.args(),
        )
        .await;
        unavailable(&v);
    }

    #[tokio::test]
    async fn native_session_source_append_end_and_summary_retention_are_independent() {
        let h = Harness::new().await;
        let conn = Connection::open(h.store.db_path()).unwrap();
        conn.execute("INSERT INTO pages(id,workspace_id,project_id,path,title,body,body_sha256,tier,frontmatter_json,pinned,created_at,updated_at,expires_at) VALUES(?1,?2,?3,?4,'summary','expired summary',zeroblob(32),'episodic','{}',1,?5,?5,1)", params![SessionId::new().as_bytes(), h.ws.as_bytes(), h.pj.as_bytes(), format!("sessions/{}.md",h.sid), NOW]).unwrap();
        let (v, _) = wire(&h.router(true, None), Some(&h.alice_token), h.args()).await;
        assert_eq!(payload(&v)["observations"].as_array().unwrap().len(), 2);
        let append = "INSERT INTO observations(id,session_id,workspace_id,project_id,kind,title,body,importance,created_at) SELECT randomblob(16),id,workspace_id,project_id,'session-end','append','new body',5,1700000000000004 FROM sessions; UPDATE sessions SET ended_at=1700000000000004; DELETE FROM pages";
        let (v, _) = wire(
            &h.router(true, Some(h.probe(append))),
            Some(&h.alice_token),
            h.args(),
        )
        .await;
        let p = payload(&v);
        assert_eq!(p["observations"].as_array().unwrap().len(), 2);
        assert_eq!(p["observations"][1]["title"], "second");
        let (v, _) = wire(&h.router(true, None), Some(&h.alice_token), h.args()).await;
        payload(&v);
        conn.execute_batch("DELETE FROM observations; DELETE FROM sessions")
            .unwrap();
        let (v, _) = wire(&h.router(true, None), Some(&h.alice_token), h.args()).await;
        unavailable(&v);
    }

    #[tokio::test]
    async fn native_session_source_sql_policy_errors_never_fall_back() {
        let h = Harness::new().await;
        h.share_source();
        let (control, _) = wire(&h.router(true, None), Some(ROOT_TOKEN), h.args()).await;
        payload(&control);
        let (v, _) = wire(
            &h.router(
                true,
                Some(h.probe("ALTER TABLE project_grants RENAME TO missing_grants")),
            ),
            Some(ROOT_TOKEN),
            h.args(),
        )
        .await;
        refused(&v);
        unavailable(&v);
    }

    #[tokio::test]
    async fn native_session_source_raw_bytes_headers_and_wire_are_bounded() {
        for sql in [
            "UPDATE observations SET body=CAST(zeroblob(4097) AS TEXT) WHERE title='first'",
            "UPDATE observations SET title=CAST(zeroblob(4097) AS TEXT) WHERE title='first'",
            "UPDATE observations SET extension=CAST(zeroblob(4097) AS TEXT) WHERE title='first'",
            "UPDATE observations SET source_event=CAST(zeroblob(4097) AS TEXT) WHERE title='first'",
            "UPDATE sessions SET cwd=CAST(zeroblob(262145) AS TEXT)",
        ] {
            let h = Harness::new().await;
            h.share_source();
            let router = h.router(true, None);
            payload(&wire(&router, Some(ROOT_TOKEN), h.args()).await.0);
            Connection::open(h.store.db_path())
                .unwrap()
                .execute_batch(sql)
                .unwrap();
            unavailable(&wire(&router, Some(ROOT_TOKEN), h.args()).await.0);
        }
        let h = Harness::new().await;
        h.share_source();
        let conn = Connection::open(h.store.db_path()).unwrap();
        conn.execute(
            "UPDATE observations SET body=?1 WHERE title='first'",
            params!["界".repeat(1400)],
        )
        .unwrap();
        unavailable(
            &wire(&h.router(true, None), Some(ROOT_TOKEN), h.args())
                .await
                .0,
        );
        conn.execute(
            "UPDATE observations SET body=?1 WHERE title='first'",
            params!["界".repeat(1000)],
        )
        .unwrap();
        let p = payload(
            &wire(&h.router(true, None), Some(ROOT_TOKEN), h.args())
                .await
                .0,
        );
        assert_eq!(p["observations"][0]["body"], "界".repeat(1000));
        conn.execute_batch("DELETE FROM observations").unwrap();
        for _ in 0..201 {
            conn.execute("INSERT INTO observations(id,session_id,workspace_id,project_id,kind,title,body,importance,created_at) VALUES(randomblob(16),?1,?2,?3,'other','t','b',5,?4)", params![h.sid.as_bytes(), h.ws.as_bytes(), h.pj.as_bytes(), NOW]).unwrap();
        }
        let mut args = h.args();
        args["limit"] = json!(999999);
        let (v, size) = wire(&h.router(true, None), Some(ROOT_TOKEN), args.clone()).await;
        assert_eq!(payload(&v)["observations"].as_array().unwrap().len(), 200);
        assert!(size < 100_000);
        conn.execute(
            "UPDATE observations SET body=?1",
            params!["\u{1}".repeat(180)],
        )
        .unwrap();
        unavailable(
            &wire(&h.router(true, None), Some(ROOT_TOKEN), args.clone())
                .await
                .0,
        );
        conn.execute(
            "UPDATE observations SET body=?1",
            params!["\u{1}".repeat(60)],
        )
        .unwrap();
        let (v, size) = wire(&h.router(true, None), Some(ROOT_TOKEN), args).await;
        assert_eq!(payload(&v)["observations"].as_array().unwrap().len(), 200);
        assert!(
            size < 500_000,
            "bounded double-encoded MCP envelope: {size}"
        );
    }

    #[tokio::test]
    async fn native_session_source_schema_and_instructions_keep_legacy_contract() {
        let h = Harness::new().await;
        let router = h.router(true, None);
        let (v, _) = rpc(&router, Some(ROOT_TOKEN), "tools/list", json!({})).await;
        let tools = v.pointer("/result/tools").unwrap().as_array().unwrap();
        let names: std::collections::BTreeSet<_> =
            tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        let expected: std::collections::BTreeSet<_> = [
            "memory_query",
            "memory_recent",
            "memory_read_page",
            "memory_read_session_observations",
            "memory_write_page",
            "memory_delete_page",
            "memory_status",
            "memory_briefing",
            "memory_explore",
            "memory_consolidate",
            "memory_lint",
            "memory_auto_improve",
            "memory_forget_sweep",
            "memory_feedback",
            "memory_handoff_begin",
            "memory_handoff_list",
            "memory_handoff_accept",
            "memory_handoff_cancel",
            "memory_message_send",
            "memory_message_list",
            "memory_message_pop",
            "memory_message_cancel",
            "memory_install_self_routing",
        ]
        .into_iter()
        .collect();
        assert_eq!(tools.len(), 23);
        assert_eq!(names, expected);

        let tool = tools
            .iter()
            .find(|t| t["name"] == "memory_read_session_observations")
            .unwrap();
        assert!(
            tool["inputSchema"]["properties"]
                .get("native_source")
                .is_some()
        );
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .contains("plain stdio")
        );
        let native = &tool["inputSchema"]["$defs"]["NativeSessionEvidence"];
        assert!(
            native
                .to_string()
                .contains("\"additionalProperties\":false"),
            "closed source schema: {native}"
        );
        assert!(native.to_string().contains("\"minLength\":36"));
        assert!(native.to_string().contains("\"maxLength\":36"));
        let (init, _) = rpc(&router, Some(ROOT_TOKEN), "initialize", json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"native-test","version":"1"}})).await;
        let first = init["result"]["instructions"]
            .as_str()
            .unwrap()
            .chars()
            .take(2048)
            .collect::<String>();
        assert!(first.contains("with `native_source` requires explicit"));
        assert!(first.contains("including session-aware clients"));
        assert!(first.contains("another handoff"));
        // The canonical instruction exception; this cut does not disturb its handshake.
        let instructions = ai_memory_mcp::MEMORY_INSTRUCTIONS;
        assert!(
            instructions
                .find("--- Detailed tool routing follows. ---")
                .unwrap()
                <= 2048
        );
        let (v, _) = wire(&router, Some(&h.alice_token), json!({"workspace":"fixture-ws", "project":"fixture-project", "session_id":h.sid.to_string(), "query":"historical", "order":"desc", "offset":1, "limit":1})).await;
        assert_eq!(payload(&v)["observations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn native_session_scope_exception_is_installed_and_in_handshake_core() {
        let core = ai_memory_mcp::MEMORY_INSTRUCTIONS
            .split("--- Detailed tool routing follows. ---")
            .next()
            .unwrap();
        let required = "`workspace` and `project` together for every client, including";
        for prompt in [
            core,
            ai_memory_core::SNIPPET_BODY,
            ai_memory_core::COMPACT_SNIPPET_BODY,
        ] {
            assert!(
                prompt
                    .contains("Exception: `memory_read_session_observations` with `native_source`")
            );
            assert!(prompt.contains(required));
        }
        for skill in ai_memory_core::routing_skills::MANAGED_SKILLS
            .iter()
            .filter(|skill| skill.name == "ai-memory-retrieval")
        {
            assert!(skill.content.contains("with `native_source` requires"));
            assert!(skill.content.contains(required), "{}", skill.name);
        }
        assert!(
            core.len() <= 2048,
            "native scope exception must fit handshake core"
        );
    }
    #[tokio::test]
    async fn native_session_source_stdio_metadata_cannot_authorize() {
        use rmcp::ServiceExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let h = Harness::new().await;
        let server =
            AiMemoryServer::new(h.store.reader.clone(), h.store.writer.clone(), h.ws, h.pj);
        let (client, server_io) = tokio::io::duplex(8192);
        let task = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write.write_all(concat!("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"stdio-native\",\"version\":\"1\"}}}","\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(
            serde_json::from_str::<Value>(&line)
                .unwrap()
                .get("result")
                .is_some()
        );
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let request = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_read_session_observations","arguments":h.args(),"_meta":{"authLevel":"root","user":"alice","session_id":h.sid.to_string(),"authorization":ROOT_TOKEN}}});
        write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(std::time::Duration::from_secs(5), read.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        unavailable(&v);
        assert!(!line.contains(&h.sid.to_string()));
        drop(write);
        drop(read);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn native_session_source_purge_recreated_identical_prefix_is_refused() {
        for tombstone in [
            "INSERT INTO purged_sessions SELECT id,workspace_id,project_id,1700000000000010 FROM sessions",
            "INSERT INTO purged_scopes SELECT workspace_id,project_id,1700000000000010 FROM sessions",
            "INSERT INTO purged_scopes SELECT workspace_id,zeroblob(16),1700000000000010 FROM sessions",
        ] {
            let h = Harness::new().await;
            h.share_source();
            payload(
                &wire(&h.router(true, None), Some(ROOT_TOKEN), h.args())
                    .await
                    .0,
            );
            let conn = Connection::open(h.store.db_path()).unwrap();
            let columns = |table| {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .unwrap();
                stmt.query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
                    .join(",")
            };
            let sql = format!(
                "CREATE TEMP TABLE saved_s AS SELECT rowid,* FROM sessions; CREATE TEMP TABLE saved_o AS SELECT rowid,* FROM observations; {tombstone}; DELETE FROM observations; DELETE FROM sessions; INSERT INTO sessions(rowid,{}) SELECT * FROM saved_s; INSERT INTO observations(rowid,{}) SELECT * FROM saved_o;",
                columns("sessions"),
                columns("observations")
            );
            let (v, _) = wire(
                &h.router(true, Some(h.probe(&sql))),
                Some(ROOT_TOKEN),
                h.args(),
            )
            .await;
            unavailable(&v);
            assert!(!v.to_string().contains(&h.sid.to_string()));
        }
    }

    #[tokio::test]
    async fn native_session_source_shared_pages_and_legacy_rows_have_no_author_filter() {
        let h = Harness::new().await;
        let conn = Connection::open(h.store.db_path()).unwrap();
        conn.execute("INSERT INTO project_grants(workspace_id,project_id,user_id,level,granted_at) SELECT ?1,?2,id,'read',?3 FROM users WHERE username='bob'", params![h.ws.as_bytes(),h.pj.as_bytes(),NOW]).unwrap();
        for name in ["alice", "bob"] {
            conn.execute("INSERT INTO pages(id,workspace_id,project_id,path,title,body,body_sha256,tier,frontmatter_json,pinned,created_at,updated_at,author_id) SELECT randomblob(16),?1,?2,?3,?4,'shared page',zeroblob(32),'semantic','{}',0,?5,?5,id FROM users WHERE username=?4", params![h.ws.as_bytes(),h.pj.as_bytes(),format!("{name}.md"),name,NOW]).unwrap();
        }
        let router = h.router(true, None);
        for token in [&h.alice_token, &h.bob_token] {
            for name in ["alice", "bob"] {
                let (v, _) = rpc(&router, Some(token), "tools/call", json!({"name":"memory_read_page","arguments":{"workspace":"fixture-ws","project":"fixture-project","path":format!("{name}.md")}})).await;
                assert!(payload(&v).to_string().contains("shared page"));
            }
        }
        unavailable(&wire(&router, Some(&h.bob_token), h.args()).await.0);
        conn.execute("UPDATE sessions SET actor_user=NULL", [])
            .unwrap();
        for token in [&h.alice_token, &h.bob_token] {
            let legacy = payload(&wire(&router, Some(token), json!({"workspace":"fixture-ws","project":"fixture-project","session_id":h.sid.to_string()})).await.0);
            assert_eq!(legacy["observations"].as_array().unwrap().len(), 2);
            payload(&wire(&router, Some(token), h.args()).await.0);
        }
    }
    #[tokio::test]
    async fn native_session_source_initial_grant_and_explicit_pair_are_required() {
        let h = Harness::new().await;
        h.share_source();
        let router = h.router(true, None);
        payload(&wire(&router, Some(ROOT_TOKEN), h.args()).await.0);
        for remove in [
            vec!["workspace"],
            vec!["project"],
            vec!["workspace", "project"],
        ] {
            let mut args = h.args();
            for field in remove {
                args.as_object_mut().unwrap().remove(field);
            }
            refused(&wire(&router, Some(ROOT_TOKEN), args).await.0);
        }
        let conn = Connection::open(h.store.db_path()).unwrap();
        conn.execute("UPDATE sessions SET actor_user='user:bob'", [])
            .unwrap();
        refused(&wire(&router, Some(&h.bob_token), h.args()).await.0);
        conn.execute("INSERT INTO project_grants(workspace_id,project_id,user_id,level,granted_at) SELECT ?1,?2,id,'read',?3 FROM users WHERE username='bob'",params![h.ws.as_bytes(),h.pj.as_bytes(),NOW]).unwrap();
        payload(&wire(&router, Some(&h.bob_token), h.args()).await.0);
    }
}
