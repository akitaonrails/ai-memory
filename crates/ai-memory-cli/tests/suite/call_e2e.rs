//! `ai-memory call` through the shipped binary, against an in-process upstream
//! MCP server that records what reaches it.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, ServerHandler};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

type Seen = Arc<Mutex<Vec<(String, Option<JsonObject>)>>>;

/// A project-scoped tool, an unscoped one, and two that fail.
#[derive(Clone)]
struct Recorder {
    seen: Seen,
}

impl ServerHandler for Recorder {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let schema = |properties: Value| {
            let schema = json!({"type": "object", "properties": properties});
            Arc::new(schema.as_object().unwrap().clone())
        };
        Ok(ListToolsResult {
            tools: vec![
                Tool::new(
                    "memory_query",
                    "Search",
                    schema(json!({"query": {}, "workspace": {}, "project": {}})),
                ),
                Tool::new("memory_status", "Health", schema(json!({}))),
                Tool::new("tool_error", "Fails", schema(json!({}))),
                Tool::new("rpc_error", "Errors", schema(json!({}))),
            ],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.seen
            .lock()
            .unwrap()
            .push((request.name.to_string(), request.arguments));
        match request.name.as_ref() {
            "tool_error" => {
                Ok(CallToolResult::error(vec![ContentBlock::text("the tool failed")]).into())
            }
            "rpc_error" => Err(McpError::invalid_params(
                "bad arguments from the server",
                None,
            )),
            _ => Ok(CallToolResult::success(vec![ContentBlock::text("found it")]).into()),
        }
    }
}

struct Fixture {
    seen: Seen,
    url: String,
    home: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let seen = Seen::default();
        let recorder = Recorder { seen: seen.clone() };
        let service: StreamableHttpService<Recorder, LocalSessionManager> =
            StreamableHttpService::new(
                move || Ok(recorder.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default().with_json_response(true),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().nest_service("/mcp", service))
                .await
                .unwrap();
        });
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".ai-memory.toml"),
            "workspace = \"team\"\nproject = \"notes\"\n",
        )
        .unwrap();
        Self {
            seen,
            url,
            home,
            server,
        }
    }

    /// Run `ai-memory call <args>` from the marked checkout.
    async fn call(&self, args: &[&str]) -> std::process::Output {
        let home: &Path = self.home.path();
        let mut cmd = crate::e2e_support::hermetic(BIN);
        cmd.arg("call")
            .args(args)
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("AI_MEMORY_HOME", home)
            .env("AI_MEMORY_DATA_DIR", home.join("data"))
            .env("AI_MEMORY_SERVER_URL", &self.url)
            .env("RUST_LOG", "off")
            .current_dir(home.join("repo"));
        tokio::process::Command::from(cmd)
            .kill_on_drop(true)
            .output()
            .await
            .expect("run ai-memory call")
    }

    fn seen(&self) -> Vec<(String, Option<JsonObject>)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn arguments(call: &(String, Option<JsonObject>)) -> Value {
    Value::Object(call.1.clone().unwrap_or_default())
}

/// The arguments reach the server as a `tools/call`, the project of the launch
/// directory is filled in when none is named, and the result text is printed.
#[tokio::test]
async fn forwards_the_arguments_and_fills_the_launch_scope() {
    let fixture = Fixture::start().await;
    let output = fixture
        .call(&["memory_query", r#"{"query":"wal mode"}"#])
        .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim_end(), "found it");
    let seen = fixture.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].0, "memory_query");
    assert_eq!(
        arguments(&seen[0]),
        json!({"query": "wal mode", "workspace": "team", "project": "notes"})
    );
}

/// A call that names a scope keeps it, and a tool that takes no scope gets none.
#[tokio::test]
async fn leaves_a_named_scope_and_unscoped_tools_alone() {
    let fixture = Fixture::start().await;
    for (tool, given) in [
        ("memory_query", json!({"query": "x", "project": "other"})),
        ("memory_query", json!({"query": "x", "global": true})),
        ("memory_status", json!({})),
    ] {
        let output = fixture.call(&[tool, &given.to_string()]).await;
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(arguments(fixture.seen().last().unwrap()), given);
    }
    // No arguments at all: still a call to a tool that takes none.
    let output = fixture.call(&["memory_status"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(arguments(fixture.seen().last().unwrap()), json!({}));
}

/// A tool-level error and a JSON-RPC error both end non-zero with the message
/// on stderr and nothing on stdout.
#[tokio::test]
async fn a_tool_error_and_an_rpc_error_exit_non_zero() {
    let fixture = Fixture::start().await;
    let tool_error = fixture.call(&["tool_error"]).await;
    assert_eq!(tool_error.status.code(), Some(1));
    assert!(stderr(&tool_error).contains("the tool failed"));
    assert!(stdout(&tool_error).is_empty(), "{}", stdout(&tool_error));

    let rpc_error = fixture.call(&["rpc_error"]).await;
    assert!(!rpc_error.status.success());
    assert!(
        stderr(&rpc_error).contains("bad arguments from the server"),
        "{}",
        stderr(&rpc_error)
    );
    assert!(stdout(&rpc_error).is_empty(), "{}", stdout(&rpc_error));
}

/// With no tool name the server's tools are listed with their input schemas.
#[tokio::test]
async fn without_a_tool_it_lists_the_tools_and_their_schemas() {
    let fixture = Fixture::start().await;
    let output = fixture.call(&[]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let tools: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let names: Vec<_> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["memory_query", "memory_status", "tool_error", "rpc_error"]
    );
    assert!(tools[0]["inputSchema"]["properties"]["query"].is_object());
    assert!(fixture.seen().is_empty());
}

/// Arguments that are not one JSON object are a usage error, and nothing is
/// sent.
#[tokio::test]
async fn arguments_that_are_not_a_json_object_are_a_usage_error() {
    let fixture = Fixture::start().await;
    for bad in ["not json", "[1,2]", "\"text\""] {
        let output = fixture.call(&["memory_query", bad]).await;
        assert_eq!(output.status.code(), Some(2), "{bad}: {}", stderr(&output));
        assert!(stdout(&output).is_empty());
    }
    assert!(fixture.seen().is_empty());
}
