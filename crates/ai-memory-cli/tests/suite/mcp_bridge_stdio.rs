//! `ai-memory mcp-bridge` over stdio, through the shipped binary, against an
//! in-process upstream MCP server.
//!
//! A dual-era client (Antigravity CLI 1.4, the Antigravity 2.24 app) probes
//! with `server/discover` before anything else and falls back to
//! `initialize` when a legacy server answers with any error or not at all:
//! <https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio#backward-compatibility>.
//! Since the rmcp 3 bump (#1924) the bridge answers the probe itself — both
//! lifecycles work — while still absorbing the probe-cancel notification a
//! client that already gave up on it sends next.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use rmcp::model::{
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

#[derive(Clone)]
struct OneTool;

impl ServerHandler for OneTool {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: vec![Tool::new(
                "probe_tool",
                "Reached through the bridge",
                Arc::new(Default::default()),
            )],
            ..Default::default()
        })
    }
}

/// The upstream the bridge connects to before it serves stdio, stateless like
/// `ai-memory serve`'s default.
async fn upstream() -> (String, tokio::task::JoinHandle<()>) {
    let service: StreamableHttpService<OneTool, LocalSessionManager> = StreamableHttpService::new(
        || Ok(OneTool),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().nest_service("/mcp", service))
            .await
            .unwrap();
    });
    (format!("http://{address}/mcp"), server)
}

async fn send(stdin: &mut ChildStdin, message: Value) {
    stdin
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
    stdin.flush().await.unwrap();
}

/// The next line the bridge writes, which must be the reply to `id`: nothing
/// else (a reply to a notification included) may come before it.
async fn reply(stdout: &mut Lines<BufReader<ChildStdout>>, id: Value) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(20), stdout.next_line())
        .await
        .unwrap_or_else(|_| panic!("no reply to {id} within 20s"))
        .unwrap()
        .unwrap_or_else(|| panic!("the bridge closed stdout instead of answering {id}"));
    let message: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(message["id"], id, "{message}");
    message
}

/// Spawn the shipped bridge against `url` with a throwaway home, plus `env`.
fn spawn_bridge(url: &str, home: &std::path::Path, env: &[(&str, &str)]) -> tokio::process::Child {
    let mut cmd = crate::e2e_support::hermetic(BIN);
    cmd.args(["mcp-bridge", "--no-session-id", "--server-url", url])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("AI_MEMORY_HOME", home)
        .env("AI_MEMORY_DATA_DIR", home.join("data"))
        .env("RUST_LOG", "off")
        .envs(env.iter().copied())
        .current_dir(home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // Shown with the test's output, so a bridge that exits says why.
        .stderr(std::process::Stdio::inherit());
    tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .expect("spawn mcp-bridge")
}

/// The modern and legacy lifecycles both reach the upstream tools. A
/// `server/discover` probe is answered, the cancellation a client sends after
/// giving up on it is absorbed, a request with complete 2026-07-28 `_meta` is
/// served without a handshake, and a separate legacy client still starts at
/// `initialize` and lists the same tool.
#[tokio::test]
async fn a_server_discover_probe_is_answered_and_initialize_still_works() {
    let (url, server) = upstream().await;
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_bridge(&url, home.path(), &[]);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let modern_meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "dual-era-e2e", "version": "0"}
    });

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover",
               "params": {"_meta": modern_meta}}),
    )
    .await;
    let discover = reply(&mut stdout, json!(1)).await;
    assert_eq!(
        discover["result"]["resultType"],
        json!("complete"),
        "{discover}"
    );
    assert!(
        discover["result"]["supportedVersions"]
            .as_array()
            .is_some_and(|versions| versions.contains(&json!("2026-07-28"))),
        "{discover}"
    );
    assert!(
        discover["result"]["capabilities"]["tools"].is_object(),
        "{discover}"
    );

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
               "params": {"requestId": 1, "reason": "probe timed out"}}),
    )
    .await;
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": "early-ping", "method": "ping"}),
    )
    .await;
    let ping = reply(&mut stdout, json!("early-ping")).await;
    assert_eq!(ping["result"], json!({}), "{ping}");

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list",
               "params": {"_meta": modern_meta}}),
    )
    .await;
    let early = reply(&mut stdout, json!(2)).await;
    assert_eq!(
        early["result"]["tools"][0]["name"],
        json!("probe_tool"),
        "a request with complete 2026-07-28 meta is served statelessly: {early}"
    );

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("the modern bridge exits when stdin closes")
        .unwrap();
    assert!(status.success(), "{status}");

    // A legacy client never probes: it starts at `initialize`, and rmcp 3
    // must still complete that handshake and serve the same upstream tools.
    let mut legacy_child = spawn_bridge(&url, home.path(), &[]);
    let mut legacy_stdin = legacy_child.stdin.take().unwrap();
    let mut legacy_stdout = BufReader::new(legacy_child.stdout.take().unwrap()).lines();
    send(
        &mut legacy_stdin,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                          "clientInfo": {"name": "dual-era-e2e", "version": "0"}}}),
    )
    .await;
    let init = reply(&mut legacy_stdout, json!(1)).await;
    assert_eq!(
        init["result"]["protocolVersion"],
        json!("2025-06-18"),
        "{init}"
    );
    send(
        &mut legacy_stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    send(
        &mut legacy_stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = reply(&mut legacy_stdout, json!(2)).await;
    assert_eq!(
        tools["result"]["tools"][0]["name"],
        json!("probe_tool"),
        "{tools}"
    );

    drop(legacy_stdin);
    let status = tokio::time::timeout(Duration::from_secs(20), legacy_child.wait())
        .await
        .expect("the legacy bridge exits when stdin closes")
        .unwrap();
    assert!(status.success(), "{status}");
    server.abort();
}

/// A proxy from the environment never sees the bridge's requests, as with the
/// hooks' client (`hook_capture::build_client`). A sandbox such as ai-jail
/// exports `HTTP_PROXY` with an empty `NO_PROXY`; a filtering proxy there
/// answered the bridge's loopback `initialize` with 405, and the bridge
/// exited before Claude Code could list a tool.
#[tokio::test]
async fn a_proxy_in_the_environment_is_not_used_for_the_server() {
    let (url, server) = upstream().await;
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", proxy_listener.local_addr().unwrap());
    // Answers every request the way ai-jail's filtering proxy did.
    let proxy = tokio::spawn(async move {
        while let Ok((mut socket, _)) = proxy_listener.accept().await {
            let mut request = [0_u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n")
                .await;
        }
    });
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_bridge(
        &url,
        home.path(),
        &[
            ("HTTP_PROXY", &proxy_url),
            ("http_proxy", &proxy_url),
            ("ALL_PROXY", &proxy_url),
            ("all_proxy", &proxy_url),
            ("NO_PROXY", ""),
            ("no_proxy", ""),
        ],
    );
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();

    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                          "clientInfo": {"name": "proxy-e2e", "version": "0"}}}),
    )
    .await;
    let init = reply(&mut stdout, json!(1)).await;
    assert!(init.get("result").is_some(), "{init}");
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = reply(&mut stdout, json!(2)).await;
    assert_eq!(
        tools["result"]["tools"][0]["name"],
        json!("probe_tool"),
        "{tools}"
    );

    drop(stdin);
    let _ = child.wait().await;
    proxy.abort();
    server.abort();
}

/// #1960: neither the `ai-memory starting` line every command logs nor a thin
/// client's unreachable-server error prints the `user:pass@` userinfo a
/// configured server URL may carry.
#[test]
fn startup_log_and_unreachable_error_strip_userinfo_from_the_server_url() {
    let tmp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(BIN)
        .arg("--data-dir")
        .arg(tmp.path())
        .arg("status")
        .env("AI_MEMORY_DATA_DIR", tmp.path())
        .env("AI_MEMORY_SERVER_URL", "http://alice:hunter2@127.0.0.1:9")
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .env("RUST_LOG", "info")
        .env_remove("AI_MEMORY_HOOK_URL")
        .env_remove("AI_MEMORY_AUTH_TOKEN")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ai-memory starting") && stderr.contains("http://127.0.0.1:9/"),
        "{stderr}"
    );
    assert!(!stderr.contains("hunter2"), "{stderr}");
    assert!(!stderr.contains("alice"), "{stderr}");
}
