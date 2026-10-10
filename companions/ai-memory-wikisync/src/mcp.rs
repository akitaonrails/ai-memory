//! The only write seam: the public `memory_write_page` and
//! `memory_delete_page` MCP tools.
//!
//! Imports and deletes never touch the wiki directory or SQLite. They call
//! the same tools an agent calls, so sanitization, admission, attribution and
//! scope resolution all apply. The request carries `workspace` and `project`
//! explicitly on every call; this companion has no session to route by.
//!
//! Every write and delete is conditional (`expected_page_id` or
//! `create_only`), so a page that changed after the sync classified it is
//! refused by the server instead of overwritten.

use std::fmt;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use url::Url;

use crate::page_file::PageMeta;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on a tool error echoed to the terminal.
const ERROR_PREVIEW: usize = 300;
/// JSON-RPC `invalid_request`, the code a failed precondition carries.
const INVALID_REQUEST: i64 = -32600;
/// The server release that added conditional writes and deletes.
const MIN_SERVER: &str = "2.7";

/// What a conditional write requires of the server's current page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precondition<'a> {
    /// The page's latest version must still be this id.
    Latest(&'a str),
    /// No page may exist at the path.
    Absent,
}

/// The server refused a conditional write or delete because the page is no
/// longer the version the sync classified; nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreconditionFailed {
    pub path: String,
    pub expected_page_id: Option<String>,
    pub current_page_id: Option<String>,
}

impl fmt::Display for PreconditionFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} changed on the server during this run (expected version {}, now {})",
            preview(&self.path),
            self.expected_page_id
                .as_deref()
                .map_or("none".into(), preview),
            self.current_page_id
                .as_deref()
                .map_or("none".into(), preview),
        )
    }
}

impl std::error::Error for PreconditionFailed {}

/// The typed precondition failure in a JSON-RPC error, if that is what it is.
fn precondition_failure(error: &Value) -> Option<PreconditionFailed> {
    let data = error.get("data")?;
    if error.get("code").and_then(Value::as_i64) != Some(INVALID_REQUEST)
        || data.get("reason").and_then(Value::as_str) != Some("precondition_failed")
    {
        return None;
    }
    let text = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(PreconditionFailed {
        path: text("path").unwrap_or_default(),
        expected_page_id: text("expected_page_id"),
        current_page_id: text("current_page_id"),
    })
}

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

    /// Refuse a server that predates conditional writes. It would accept
    /// `expected_page_id` as an unknown argument and ignore it, turning every
    /// guarded write into an unconditional one.
    pub async fn ensure_conditional_writes(&self) -> Result<()> {
        let result = self.request("tools/list", json!({})).await?;
        let supported = result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("memory_write_page"))
            .and_then(|tool| tool.pointer("/inputSchema/properties/expected_page_id"))
            .is_some();
        if !supported {
            bail!(server_too_old());
        }
        Ok(())
    }

    /// Replace (or create) one page with exactly `meta` and `body`, only if
    /// `precondition` still holds. The tool clears whatever the call omits,
    /// which is why every field the file carries is sent, defaults included.
    /// Returns the new version id.
    pub async fn write_page(
        &self,
        scope: &Scope<'_>,
        path: &str,
        meta: &PageMeta,
        body: &str,
        precondition: Precondition<'_>,
    ) -> Result<Option<String>> {
        let mut arguments = json!({
            "workspace": scope.workspace,
            "project": scope.project,
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
        match precondition {
            Precondition::Latest(id) => arguments["expected_page_id"] = json!(id),
            Precondition::Absent => arguments["create_only"] = json!(true),
        }
        let result = self.call_tool("memory_write_page", arguments).await?;
        Ok(tool_json(&result)
            .and_then(|written| written.get("page_id")?.as_str().map(str::to_owned)))
    }

    /// Delete one page, only if its latest version is still `expected_page_id`.
    pub async fn delete_page(
        &self,
        scope: &Scope<'_>,
        path: &str,
        expected_page_id: &str,
    ) -> Result<()> {
        let arguments = json!({
            "workspace": scope.workspace,
            "project": scope.project,
            "path": path,
            "expected_page_id": expected_page_id,
        });
        self.call_tool("memory_delete_page", arguments).await?;
        Ok(())
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
            .map_err(|error| match error.downcast::<PreconditionFailed>() {
                Ok(failed) => anyhow::Error::new(failed),
                Err(error) => error.context(format!("MCP {name} failed")),
            })?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let detail = result
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("no detail");
            bail!("MCP {name} refused the call: {}", preview(detail));
        }
        Ok(result)
    }

    /// One JSON-RPC request; its `result`, or the error it answered with.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
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
            .map_err(|e| anyhow!("MCP {method} request to {} failed: {e}", self.url))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| anyhow!("reading the MCP {method} response failed: {e}"))?;
        if !status.is_success() {
            bail!("MCP {method} returned HTTP {status}: {}", preview(&text));
        }
        let mut message = json_rpc_message(&text)
            .ok_or_else(|| anyhow!("MCP {method} answered with no JSON-RPC message"))?;
        if let Some(error) = message.get("error") {
            if let Some(failed) = precondition_failure(error) {
                return Err(anyhow::Error::new(failed));
            }
            bail!("MCP {method} failed: {}", preview(&error.to_string()));
        }
        message
            .get_mut("result")
            .map(Value::take)
            .ok_or_else(|| anyhow!("MCP {method} answered with no result"))
    }
}

/// The explicit scope every call carries.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub workspace: &'a str,
    pub project: &'a str,
}

/// The refusal for a server without conditional writes.
pub fn server_too_old() -> String {
    format!(
        "server too old for sync --apply; needs ai-memory >= {MIN_SERVER} (conditional \
         page writes)"
    )
}

/// The JSON a tool returned as its first text content, if any.
fn tool_json(result: &Value) -> Option<Value> {
    let text = result.pointer("/content/0/text")?.as_str()?;
    serde_json::from_str(text).ok()
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
    fn precondition_errors_are_typed() {
        let error = json!({
            "code": -32600,
            "message": "precondition failed",
            "data": {
                "reason": "precondition_failed",
                "path": "notes/a.md",
                "expected_page_id": "v1",
                "current_page_id": null,
            },
        });
        assert_eq!(
            precondition_failure(&error),
            Some(PreconditionFailed {
                path: "notes/a.md".into(),
                expected_page_id: Some("v1".into()),
                current_page_id: None,
            })
        );
        let other_reason = json!({"code": -32600, "data": {"reason": "nope"}});
        assert_eq!(precondition_failure(&other_reason), None);
        let other_code = json!({"code": -32602, "data": {"reason": "precondition_failed"}});
        assert_eq!(precondition_failure(&other_code), None);
        assert_eq!(precondition_failure(&json!({"code": -32600})), None);
    }

    #[test]
    fn previews_are_bounded_and_inert() {
        let hostile = format!("\u{1b}[31m{}", "x".repeat(1_000));
        let shown = preview(&hostile);
        assert!(!shown.contains('\u{1b}'));
        assert_eq!(shown.chars().count(), ERROR_PREVIEW);
    }
}
