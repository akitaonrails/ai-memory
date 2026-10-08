//! The only write seam: the public `memory_write_page` MCP tool.
//!
//! Imports never touch the wiki directory or SQLite. They call the same tool
//! an agent calls, so sanitization, admission, attribution and scope
//! resolution all apply. The request carries `workspace` and `project`
//! explicitly on every call; this companion has no session to route by.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use url::Url;

use crate::page_file::PageMeta;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on a tool error echoed to the terminal.
const ERROR_PREVIEW: usize = 300;

pub struct McpClient {
    http: reqwest::Client,
    url: Url,
    token: Option<String>,
}

impl McpClient {
    /// `server` is the origin `ApiClient::new` already validated.
    pub fn new(server: &str, token: Option<String>) -> Result<Self> {
        let mut url =
            Url::parse(server).map_err(|e| anyhow!("invalid --server {server:?}: {e}"))?;
        url.set_path("/mcp");
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| anyhow!("cannot build HTTP client: {e}"))?;
        Ok(Self { http, url, token })
    }

    /// Replace (or create) one page with exactly `meta` and `body`. The
    /// tool clears whatever the call omits, which is why every field the
    /// file carries is sent, defaults included.
    pub async fn write_page(
        &self,
        workspace: &str,
        project: &str,
        path: &str,
        meta: &PageMeta,
        body: &str,
    ) -> Result<()> {
        let mut arguments = json!({
            "workspace": workspace,
            "project": project,
            "path": path,
            "body": body,
            "tags": meta.tags,
            "pinned": meta.pinned,
        });
        if let Some(title) = &meta.title {
            arguments["title"] = json!(title);
        }
        if let Some(tier) = &meta.tier {
            arguments["tier"] = json!(tier);
        }
        self.call_tool("memory_write_page", arguments).await
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<()> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let mut builder = self
            .http
            .post(self.url.clone())
            // The streamable HTTP transport refuses a request that does not
            // accept both; it answers plain JSON in its default stateless
            // mode and SSE frames in stateful mode.
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&request);
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| anyhow!("MCP {name} request to {} failed: {e}", self.url))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| anyhow!("reading the MCP {name} response failed: {e}"))?;
        if !status.is_success() {
            bail!("MCP {name} returned HTTP {status}: {}", preview(&text));
        }
        let message = json_rpc_message(&text)
            .ok_or_else(|| anyhow!("MCP {name} answered with no JSON-RPC message"))?;
        if let Some(error) = message.get("error") {
            bail!("MCP {name} failed: {}", preview(&error.to_string()));
        }
        let result = message
            .get("result")
            .ok_or_else(|| anyhow!("MCP {name} answered with no result"))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let detail = result
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("no detail");
            bail!("MCP {name} refused the write: {}", preview(detail));
        }
        Ok(())
    }
}

/// The JSON-RPC message in a response body: the body itself, or the last
/// `data:` frame of an SSE stream.
fn json_rpc_message(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Some(value);
    }
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .next_back()
}

/// Server text is untrusted: bound it and neutralise control characters so
/// it cannot drive the terminal.
fn preview(text: &str) -> String {
    text.chars()
        .take(ERROR_PREVIEW)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_json_and_sse_frames() {
        let plain = r#"{"jsonrpc":"2.0","id":1,"result":{"isError":false}}"#;
        assert_eq!(json_rpc_message(plain).unwrap()["id"], 1);
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        assert!(json_rpc_message(sse).unwrap().get("result").is_some());
        assert!(json_rpc_message("event: ping\n\n").is_none());
    }

    #[test]
    fn previews_are_bounded_and_inert() {
        let hostile = format!("\u{1b}[31m{}", "x".repeat(1_000));
        let shown = preview(&hostile);
        assert!(!shown.contains('\u{1b}'));
        assert_eq!(shown.chars().count(), ERROR_PREVIEW);
    }
}
