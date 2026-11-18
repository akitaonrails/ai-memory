//! `ai-memory call` — invoke any MCP tool of the configured server from the
//! shell, for agents that cannot or should not load MCP.
//!
//! Thin client over the server's `/mcp` endpoint, reached the way the other
//! thin-client commands reach the server (`ServerEndpoint::for_checkout`) and
//! through the same rmcp transport the stdio bridge uses, so a stateful
//! server's SSE replies decode too. The launch scope is filled by the
//! session-less bridge's own rule (#1979): a call that names no scope gets the
//! current project's `workspace` + `project` when its tool takes both.

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;

use super::mcp_bridge::{
    SERVER_WAIT, SERVER_WAIT_POLL, launch_dir, launch_project, launch_scope, upstream_transport,
    wait_for_server,
};
use super::show::terminal_body;
use crate::cli::CallArgs;
use crate::commands::install_mcp::mcp_server_url_from_base;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, display_url};

/// Run the `call` subcommand and return its exit code: 0, or 1 when the tool
/// reported an error. A JSON-RPC error or an unreachable server is an `Err`.
///
/// # Errors
/// Returns an error when the server cannot be reached or refuses the
/// connection, or answers the request with a JSON-RPC error.
pub async fn run(config: &Config, args: CallArgs) -> Result<i32> {
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let server_url = mcp_server_url_from_base(&format!("{}{}", endpoint.url, endpoint.base_path));
    wait_for_server(&server_url, SERVER_WAIT, SERVER_WAIT_POLL).await?;
    let transport = upstream_transport(&server_url, None, endpoint.auth_token.as_deref(), None)?;
    let client = ().serve(transport).await.with_context(|| {
        format!(
            "connecting to the MCP endpoint at {}",
            display_url(&server_url)
        )
    })?;

    let outcome = call(config, &client, args).await;
    // The reply is already in hand; a failed goodbye changes nothing.
    let _ = client.cancel().await;
    outcome
}

async fn call(
    config: &Config,
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    args: CallArgs,
) -> Result<i32> {
    let Some(tool) = args.tool else {
        let tools = client.peer().list_all_tools().await?;
        println!("{}", serde_json::to_string_pretty(&tools)?);
        return Ok(0);
    };
    let mut request = CallToolRequestParams::new(tool);
    request.arguments = args.arguments;
    if let Some(project) = launch_dir(config).and_then(|dir| launch_project(config, &dir))
        && let Some(scope) = launch_scope(client.peer(), project).await
    {
        request = scope.apply(request);
    }
    let result = client.peer().call_tool(request).await?;

    let mut text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty()
        && let Some(structured) = &result.structured_content
    {
        text = structured.to_string();
    }
    let text = terminal_body(&text);
    if result.is_error == Some(true) {
        eprintln!("{text}");
        return Ok(1);
    }
    println!("{text}");
    Ok(0)
}
