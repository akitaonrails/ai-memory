//! Session-aware Claude Code stdio bridge for a remote ai-memory MCP server.

use std::collections::HashMap;

use anyhow::{Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams, ServerInfo,
};
use rmcp::service::{RequestContext, RoleClient, RoleServer, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, stdio};
use rmcp::{ErrorData as McpError, Peer, ServerHandler, ServiceExt};

use crate::cli::McpBridgeArgs;
use crate::commands::install_mcp::mcp_server_url_from_base;
use crate::config::Config;

const ACTOR_SESSION_HEADER: HeaderName = HeaderName::from_static("x-memory-actor-session-id");

#[derive(Clone)]
struct SessionAwareBridge {
    upstream: Peer<RoleClient>,
    server_info: ServerInfo,
}

impl ServerHandler for SessionAwareBridge {
    fn get_info(&self) -> ServerInfo {
        self.server_info.clone()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.upstream
            .list_tools(request)
            .await
            .map_err(upstream_error)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.upstream
            .call_tool(request)
            .await
            .map_err(upstream_error)
    }
}

fn upstream_error(error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(error) => error,
        other => McpError::internal_error(format!("upstream MCP request failed: {other}"), None),
    }
}

fn upstream_config(
    server_url: &str,
    session_id: &str,
    auth_token: Option<&str>,
) -> Result<StreamableHttpClientTransportConfig> {
    let mut headers = HashMap::new();
    headers.insert(
        ACTOR_SESSION_HEADER,
        HeaderValue::from_str(session_id).context(
            "CLAUDE_CODE_SESSION_ID contains characters that are invalid in an HTTP header",
        )?,
    );

    let mut config =
        StreamableHttpClientTransportConfig::with_uri(server_url).custom_headers(headers);
    if let Some(token) = auth_token {
        config = config.auth_header(token);
    }
    Ok(config)
}

/// Run the Claude Code session-aware stdio bridge.
///
/// # Errors
/// Returns an error when Claude did not provide a lifecycle session id, the
/// upstream HTTP MCP server cannot initialize, or either transport fails.
pub async fn run(config: &Config, args: McpBridgeArgs) -> Result<()> {
    let session_id = config.runtime_env.claude_code_session_id().context(
        "CLAUDE_CODE_SESSION_ID is missing; this bridge must be launched by Claude Code as an stdio MCP server",
    )?;
    let server_url = args
        .server_url
        .as_deref()
        .map(mcp_server_url_from_base)
        .unwrap_or_else(|| mcp_server_url_from_base(&config.server_url));
    let transport = StreamableHttpClientTransport::from_config(upstream_config(
        &server_url,
        session_id,
        config.auth.bearer_token.as_deref(),
    )?);

    let mut upstream = ()
        .serve(transport)
        .await
        .with_context(|| format!("connecting session-aware bridge to {server_url}"))?;
    let server_info = upstream
        .peer_info()
        .map(|info| (*info).clone())
        .context("upstream MCP server completed initialization without server info")?;
    let bridge = SessionAwareBridge {
        upstream: upstream.peer().clone(),
        server_info,
    };
    let downstream = bridge
        .serve(stdio())
        .await
        .context("starting session-aware stdio MCP bridge")?;

    downstream
        .waiting()
        .await
        .context("waiting for Claude Code stdio MCP transport")?;
    upstream
        .close()
        .await
        .context("closing upstream HTTP MCP transport")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use rmcp::model::{Content, ServerCapabilities, Tool};
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    #[derive(Clone)]
    struct EchoServer {
        seen_session: Arc<Mutex<Option<String>>>,
    }

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, McpError> {
            Ok(ListToolsResult {
                tools: vec![Tool::new(
                    "echo",
                    "Echo through the bridge",
                    Arc::new(Default::default()),
                )],
                ..Default::default()
            })
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, McpError> {
            let session = context
                .extensions
                .get::<axum::http::request::Parts>()
                .and_then(|parts| parts.headers.get(&ACTOR_SESSION_HEADER))
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            *self.seen_session.lock().unwrap() = session;
            Ok(CallToolResult::success(vec![Content::text(format!(
                "echo:{}",
                request.name
            ))]))
        }
    }

    #[test]
    fn upstream_config_injects_session_id_and_raw_bearer_token() {
        let config = upstream_config(
            "https://memory.example/mcp",
            "550e8400-e29b-41d4-a716-446655440000",
            Some("secret-token"),
        )
        .unwrap();

        assert_eq!(config.auth_header.as_deref(), Some("secret-token"));
        assert_eq!(
            config
                .custom_headers
                .get(&ACTOR_SESSION_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert!(config.allow_stateless);
        assert!(config.reinit_on_expired_session);
    }

    #[test]
    fn upstream_config_rejects_an_invalid_header_value() {
        let error =
            upstream_config("https://memory.example/mcp", "session\ninjected", None).unwrap_err();
        assert!(
            error.to_string().contains("invalid in an HTTP header"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn bridge_fails_closed_without_a_claude_session_id() {
        let error = run(
            &Config::default(),
            McpBridgeArgs {
                server_url: Some("https://memory.example/mcp".into()),
            },
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("CLAUDE_CODE_SESSION_ID is missing"),
            "{error:#}"
        );
    }

    async fn assert_bridge_round_trip(stateful: bool) {
        let seen_session = Arc::new(Mutex::new(None));
        let echo = EchoServer {
            seen_session: seen_session.clone(),
        };
        let service: StreamableHttpService<EchoServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(echo.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default()
                    .with_stateful_mode(stateful)
                    .with_json_response(!stateful),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let http_server = tokio::spawn(async move {
            axum::serve(listener, Router::new().nest_service("/mcp", service))
                .await
                .unwrap();
        });

        let upstream_transport = StreamableHttpClientTransport::from_config(
            upstream_config(&format!("http://{address}/mcp"), "claude-session-244", None).unwrap(),
        );
        let upstream = ().serve(upstream_transport).await.unwrap();
        let bridge = SessionAwareBridge {
            upstream: upstream.peer().clone(),
            server_info: upstream.peer_info().map(|info| (*info).clone()).unwrap(),
        };
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let downstream_server = tokio::spawn(async move { bridge.serve(server_io).await.unwrap() });
        let downstream_client = ().serve(client_io).await.unwrap();
        let downstream_server = downstream_server.await.unwrap();

        let tools = downstream_client.list_all_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = downstream_client
            .call_tool(CallToolRequestParams::new("echo"))
            .await
            .unwrap();
        assert_eq!(
            result
                .content
                .first()
                .and_then(|content| content.as_text())
                .map(|text| text.text.as_str()),
            Some("echo:echo")
        );
        assert_eq!(
            seen_session.lock().unwrap().as_deref(),
            Some("claude-session-244")
        );

        downstream_client.cancel().await.unwrap();
        downstream_server.cancel().await.unwrap();
        upstream.cancel().await.unwrap();
        http_server.abort();
    }

    #[tokio::test]
    async fn stdio_bridge_forwards_tools_and_session_header_in_both_http_modes() {
        assert_bridge_round_trip(false).await;
        assert_bridge_round_trip(true).await;
    }
}
