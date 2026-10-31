//! The write seam for the agentmemory routes: the public `memory_write_page`
//! and `memory_read_page` MCP tools, called with explicit `workspace` and
//! `project`, the way `ai-memory-wikisync` calls them.
//!
//! Every write is conditional (`create_only` for a page's first version,
//! `expected_page_id` for each later one), so a page that appeared or changed
//! underneath the import is refused by the server instead of overwritten.
//! That closes the check-then-write race the `/admin/write-page` routes carry.

use std::fmt;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::ImportClient;

/// Bound on server text echoed to the terminal or a manifest.
const ERROR_PREVIEW: usize = 300;
/// JSON-RPC `invalid_request`, the code a failed precondition carries.
const INVALID_REQUEST: i64 = -32600;
const MCP_SESSION_ID: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
/// The server release that added conditional page writes.
pub(crate) const MIN_SERVER: &str = "3.0";
pub(crate) const MIN_BATCH_SERVER: &str = "3.4";
/// The most characters `memory_read_page` returns of one body
/// (`READ_PAGE_MAX_MAX_CHARS` in ai-memory-mcp `server.rs`).
pub(crate) const READ_PAGE_MAX_CHARS: usize = 64_000;

/// What a conditional write requires of the destination page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Precondition<'a> {
    /// No page may exist at the path.
    Absent,
    /// The page's latest version must still be this id.
    Latest(&'a str),
}

/// The server refused a conditional write; nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreconditionFailed {
    pub(crate) current_page_id: Option<String>,
    pub(crate) index: Option<usize>,
}

impl fmt::Display for PreconditionFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the destination page changed (current version {})",
            self.current_page_id
                .as_deref()
                .map_or_else(|| "none".to_owned(), preview)
        )
    }
}

impl std::error::Error for PreconditionFailed {}

/// One page write: the fields the MCP tool round-trips. The tool clears what
/// a call omits, so every field is always sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpWrite<'a> {
    pub(crate) workspace: &'a str,
    pub(crate) project: &'a str,
    pub(crate) project_id: Option<&'a str>,
    pub(crate) path: &'a str,
    pub(crate) body: &'a str,
    pub(crate) title: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) tier: &'a str,
    pub(crate) tags: &'a [String],
    pub(crate) expires_at: Option<&'a str>,
}

/// A page as `memory_read_page` returns it.
#[derive(Debug, Clone)]
pub(crate) struct ReadPage {
    pub(crate) page_id: Option<String>,
    pub(crate) body: String,
    /// The server cut `body` short, so it is not the stored page.
    pub(crate) truncated: bool,
    /// The stored body's full length in characters, when the server says.
    pub(crate) total_chars: Option<u64>,
}

impl ReadPage {
    /// Why a rerun cannot compare this page with a planned body, if the
    /// server cut it. A page longer than [`READ_PAGE_MAX_CHARS`] never comes
    /// back whole, so it can be told apart neither from the planned body nor
    /// from someone else's edit.
    pub(crate) fn unverifiable(&self, path: &str) -> Option<anyhow::Error> {
        self.truncated.then(|| {
            let length = self.total_chars.map_or_else(
                || "longer".to_owned(),
                |total| format!("{total} characters long, longer"),
            );
            anyhow!(
                "{path} is {length} than memory_read_page returns, so the import cannot \
                 compare it with the planned body; nothing was written to it"
            )
        })
    }
}

impl ImportClient {
    /// Validate one exact existing write destination without creating it.
    pub(crate) async fn mcp_resolve_write_target(
        &self,
        workspace: &str,
        project: &str,
        project_id: Option<&str>,
    ) -> Result<()> {
        let mut arguments = json!({"workspace": workspace});
        if let Some(project_id) = project_id {
            arguments["project_id"] = json!(project_id);
        } else {
            arguments["project"] = json!(project);
        }
        let result = self
            .call_tool("memory_resolve_write_target", arguments)
            .await?;
        let resolved = tool_json(&result)
            .ok_or_else(|| anyhow!("memory_resolve_write_target answered without a target"))?;
        if resolved.get("project_id").and_then(Value::as_str).is_none() {
            bail!("memory_resolve_write_target answered without project_id");
        }
        Ok(())
    }

    /// Refuse a server that predates conditional writes. It would accept
    /// `create_only` as an unknown argument and ignore it, turning every
    /// guarded write into an unconditional one.
    pub(crate) async fn ensure_conditional_writes(&self) -> Result<()> {
        let result = self.mcp_request("tools/list", json!({})).await?;
        let supported = result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("memory_write_page"))
            .and_then(|tool| tool.pointer("/inputSchema/properties/create_only"))
            .is_some();
        if !supported {
            bail!(
                "server too old for --apply; needs ai-memory >= {MIN_SERVER} (conditional page writes)"
            );
        }
        Ok(())
    }

    /// Refuse a server without the bounded atomic batch tool used by corpus
    /// imports. Falling back to one checkpoint per page can turn a large
    /// import into hours of git work and loses all-or-nothing batches.
    pub(crate) async fn ensure_batch_writes(&self) -> Result<()> {
        let result = self.mcp_request("tools/list", json!({})).await?;
        let supported = result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|tool| {
                tool.get("name").and_then(Value::as_str) == Some("memory_write_pages_batch")
            });
        if !supported {
            bail!(
                "server too old for this corpus import; needs ai-memory >= {MIN_BATCH_SERVER} (atomic page batches)"
            );
        }
        Ok(())
    }

    /// Write one page version only if `precondition` holds. Returns the new
    /// version id; a refused precondition comes back as [`PreconditionFailed`].
    pub(crate) async fn mcp_write_page(
        &self,
        write: &McpWrite<'_>,
        precondition: Precondition<'_>,
    ) -> Result<String> {
        let mut arguments = json!({
            "workspace": write.workspace,
            "path": write.path,
            "body": write.body,
            "kind": write.kind,
            "tier": write.tier,
            "tags": write.tags,
            "pinned": false,
        });
        if let Some(project_id) = write.project_id {
            arguments["project_id"] = json!(project_id);
        } else {
            arguments["project"] = json!(write.project);
        }
        if let Some(title) = write.title {
            arguments["title"] = json!(title);
        }
        if let Some(expires_at) = write.expires_at {
            arguments["expires_at"] = json!(expires_at);
        }
        match precondition {
            Precondition::Absent => arguments["create_only"] = json!(true),
            Precondition::Latest(id) => arguments["expected_page_id"] = json!(id),
        }
        let result = self.call_tool("memory_write_page", arguments).await?;
        tool_json(&result)
            .and_then(|written| written.get("page_id")?.as_str().map(str::to_owned))
            .ok_or_else(|| anyhow!("memory_write_page answered without a page_id"))
    }

    /// Write one atomic batch into a single destination. The response ids
    /// preserve request order. A refused precondition changes no page.
    pub(crate) async fn mcp_write_pages_batch(
        &self,
        writes: &[(&McpWrite<'_>, Precondition<'_>)],
    ) -> Result<Vec<String>> {
        let Some((first, _)) = writes.first() else {
            bail!("cannot write an empty page batch");
        };
        let mut pages = Vec::with_capacity(writes.len());
        for (write, precondition) in writes {
            if write.workspace != first.workspace
                || write.project != first.project
                || write.project_id != first.project_id
            {
                bail!("one page batch cannot span destinations");
            }
            let mut page = json!({
                "path": write.path,
                "body": write.body,
                "kind": write.kind,
                "tier": write.tier,
                "tags": write.tags,
                "pinned": false,
            });
            if let Some(title) = write.title {
                page["title"] = json!(title);
            }
            if let Some(expires_at) = write.expires_at {
                page["expires_at"] = json!(expires_at);
            }
            match precondition {
                Precondition::Absent => page["create_only"] = json!(true),
                Precondition::Latest(id) => page["expected_page_id"] = json!(id),
            }
            pages.push(page);
        }
        let mut arguments = json!({"workspace": first.workspace, "pages": pages});
        if let Some(project_id) = first.project_id {
            arguments["project_id"] = json!(project_id);
        } else {
            arguments["project"] = json!(first.project);
        }
        let result = self
            .call_tool("memory_write_pages_batch", arguments)
            .await?;
        let written = tool_json(&result)
            .and_then(|value| value.get("pages").cloned())
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| anyhow!("memory_write_pages_batch answered without pages"))?;
        if written.len() != writes.len() {
            bail!(
                "memory_write_pages_batch returned {} page ids for {} writes",
                written.len(),
                writes.len()
            );
        }
        written
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if value.get("index").and_then(Value::as_u64) != Some(index as u64) {
                    bail!("memory_write_pages_batch returned pages out of order");
                }
                value
                    .get("page_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("memory_write_pages_batch page {index} has no page_id"))
            })
            .collect()
    }

    /// Read one page by exact path.
    ///
    /// Since 3.3.0 the server cuts a body at 12,000 characters unless the
    /// caller asks for more (#1827), and a rerun compares the whole stored
    /// body with the planned one, so ask for the server's ceiling. Older
    /// servers ignore the argument and return the whole body.
    pub(crate) async fn mcp_read_page(
        &self,
        workspace: &str,
        project: &str,
        project_id: Option<&str>,
        path: &str,
    ) -> Result<ReadPage> {
        let mut arguments = json!({
            "workspace": workspace,
            "path": path,
            "max_chars": READ_PAGE_MAX_CHARS,
        });
        if let Some(project_id) = project_id {
            arguments["project_id"] = json!(project_id);
        } else {
            arguments["project"] = json!(project);
        }
        let result = self.call_tool("memory_read_page", arguments).await?;
        let page = tool_json(&result)
            .ok_or_else(|| anyhow!("memory_read_page answered without a page"))?;
        Ok(ReadPage {
            page_id: page
                .get("page_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            body: page
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            truncated: page
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            total_chars: page.get("total_chars").and_then(Value::as_u64),
        })
    }

    /// Ask for one page of the destination, the read every live run makes
    /// before writing: reads never create a scope, so an error means the
    /// destination does not exist (or cannot be read).
    pub(crate) async fn mcp_scope_exists(&self, workspace: &str, project: &str) -> bool {
        self.call_tool(
            "memory_recent",
            json!({"workspace": workspace, "project": project, "limit": 1}),
        )
        .await
        .is_ok()
    }

    /// Call one tool; a tool-level error becomes an `Err` carrying a bounded
    /// preview of the server's text.
    pub(crate) async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let result = self
            .mcp_request("tools/call", json!({"name": name, "arguments": arguments}))
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
    async fn mcp_request(&self, method: &str, params: Value) -> Result<Value> {
        let session = self.mcp_session().await?;
        let id = self.mcp_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut builder = self
            .request(reqwest::Method::POST, "/mcp")
            // The streamable HTTP transport refuses a request that does not
            // accept both; it answers plain JSON in its default stateless
            // mode and SSE frames in stateful mode.
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&request);
        if let Some(session) = session.as_deref() {
            builder = builder.header(MCP_SESSION_ID, session);
        }
        let response = builder
            .send()
            .await
            .map_err(|e| anyhow!("MCP {method} request failed: {e}"))?;
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

    async fn mcp_session(&self) -> Result<Option<String>> {
        if let Some(session) = self
            .mcp_session
            .lock()
            .map_err(|_| anyhow!("MCP session state is unavailable"))?
            .clone()
        {
            return Ok(session);
        }

        let id = self.mcp_request_id.fetch_add(1, Ordering::Relaxed) + 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ai-memory-importer", "version": env!("CARGO_PKG_VERSION")}
            }
        });
        let response = self
            .request(reqwest::Method::POST, "/mcp")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&request)
            .send()
            .await
            .context("MCP initialize request failed")?;
        let status = response.status();
        let session = response
            .headers()
            .get(MCP_SESSION_ID)
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .filter(|value| {
                        !value.is_empty()
                            && value.len() <= 256
                            && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                    })
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("MCP initialize returned an invalid session id"))
            })
            .transpose()?;
        let text = response
            .text()
            .await
            .context("reading the MCP initialize response failed")?;
        if !status.is_success() {
            bail!("MCP initialize returned HTTP {status}: {}", preview(&text));
        }
        let message = json_rpc_message(&text)
            .ok_or_else(|| anyhow!("MCP initialize answered with no JSON-RPC message"))?;
        if let Some(error) = message.get("error") {
            bail!("MCP initialize failed: {}", preview(&error.to_string()));
        }
        if message
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .is_none()
        {
            bail!("MCP initialize answered without a protocol version");
        }

        if let Some(session_id) = session.as_deref() {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            });
            let response = self
                .request(reqwest::Method::POST, "/mcp")
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                )
                .header(MCP_SESSION_ID, session_id)
                .json(&notification)
                .send()
                .await
                .context("MCP initialized notification failed")?;
            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!(
                    "MCP initialized notification returned HTTP {status}: {}",
                    preview(&text)
                );
            }
        }

        *self
            .mcp_session
            .lock()
            .map_err(|_| anyhow!("MCP session state is unavailable"))? = Some(session.clone());
        Ok(session)
    }
}

/// The typed precondition failure in a JSON-RPC error, if that is what it is.
fn precondition_failure(error: &Value) -> Option<PreconditionFailed> {
    let data = error.get("data")?;
    if error.get("code").and_then(Value::as_i64) != Some(INVALID_REQUEST)
        || data.get("reason").and_then(Value::as_str) != Some("precondition_failed")
    {
        return None;
    }
    Some(PreconditionFailed {
        index: data
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok()),
        current_page_id: data
            .get("current_page_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// The JSON a tool returned as its first text content, if any.
pub(crate) fn tool_json(result: &Value) -> Option<Value> {
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
pub(crate) fn preview(text: &str) -> String {
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
    fn only_a_typed_precondition_error_is_a_precondition_failure() {
        let error = json!({
            "code": -32600,
            "data": {"reason": "precondition_failed", "current_page_id": "v2"},
        });
        assert_eq!(
            precondition_failure(&error),
            Some(PreconditionFailed {
                current_page_id: Some("v2".into()),
                index: None,
            })
        );
        assert_eq!(
            precondition_failure(&json!({"code": -32600, "data": {"reason": "nope"}})),
            None
        );
        assert_eq!(
            precondition_failure(
                &json!({"code": -32602, "data": {"reason": "precondition_failed"}})
            ),
            None
        );
    }

    #[test]
    fn previews_are_bounded_and_inert() {
        let shown = preview(&format!("\u{1b}[31m{}", "x".repeat(1_000)));
        assert!(!shown.contains('\u{1b}'));
        assert_eq!(shown.chars().count(), ERROR_PREVIEW);
    }
}
