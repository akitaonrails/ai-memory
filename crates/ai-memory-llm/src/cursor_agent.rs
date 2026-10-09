//! Cursor subscription provider.
//!
//! Consolidation calls the logged-in Cursor Agent CLI (`agent --print`)
//! instead of a platform API key. The CLI already holds the Cursor
//! subscription. Invocations stay in `--mode ask`: captured session text is
//! untrusted, and `--yolo` / `--force` would let that text run tools.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::time::timeout;
use uuid::Uuid;

use crate::auth::CursorAuth;
use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, Role};

/// Model used when `AI_MEMORY_LLM_MODEL` is unset.
///
/// `agent --list-models` labels this id "Grok 4.6". Override with any other id
/// from that list.
pub const CURSOR_DEFAULT_MODEL: &str = "cursor-grok-4.6-high";

const MAX_STDOUT_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Chat provider that shells out to the Cursor `agent` CLI.
pub struct CursorAgentProvider {
    model: String,
    auth: CursorAuth,
    timeout: Duration,
}

impl CursorAgentProvider {
    /// Construct the provider.
    ///
    /// The executable is not probed here: a missing `agent` binary fails on
    /// the first completion with a path-specific error.
    #[must_use]
    pub fn new(auth: CursorAuth, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            auth,
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
        }
    }

    /// Override the CLI wait ceiling.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    async fn complete_text(
        &self,
        request: &ChatRequest,
        schema: Option<&serde_json::Value>,
    ) -> LlmResult<String> {
        let workspace = TempWorkspace::create()?;
        let body = render_request(request, schema);
        std::fs::write(workspace.path.join("request.md"), body)
            .map_err(|err| LlmError::UnexpectedShape(format!("writing cursor prompt: {err}")))?;

        let mut command = Command::new(&self.auth.executable);
        command
            .arg("--print")
            .arg("--mode")
            .arg("ask")
            .arg("--output-format")
            .arg("text")
            .arg("--trust")
            .arg("--workspace")
            .arg(&workspace.path);
        if self.model != "default" {
            command.arg("--model").arg(&self.model);
        }
        command
            .arg(
                "Read request.md in this workspace and answer it. \
                 Reply with only the requested answer. Do not use tools.",
            )
            .current_dir(&workspace.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|err| {
            LlmError::NotConfigured(format!(
                "cursor agent CLI ({}) failed to start: {err}. Install the Cursor agent \
                 or set AI_MEMORY_CURSOR_AGENT",
                self.auth.executable.display()
            ))
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LlmError::UnexpectedShape("cursor agent stdout missing".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LlmError::UnexpectedShape("cursor agent stderr missing".into()))?;

        let collected = timeout(self.timeout, async {
            // Both pipes drain at once: reading stdout to EOF first would
            // leave a chatty child blocked on a full stderr pipe until the
            // timeout. `try_join!` also stops at the first capped stream
            // instead of waiting on the other one (the child is killed on
            // drop).
            let (stdout, stderr) = tokio::try_join!(
                read_capped(stdout, MAX_STDOUT_BYTES),
                read_capped(stderr, MAX_STDERR_BYTES)
            )?;
            let status = child.wait().await.map_err(|err| {
                LlmError::UnexpectedShape(format!("waiting for cursor agent: {err}"))
            })?;
            Ok::<_, LlmError>((status, stdout, stderr))
        })
        .await
        .map_err(|_| LlmError::Provider {
            status: 504,
            body: "cursor agent timed out".into(),
        })??;

        let (status, stdout, stderr) = collected;
        if !status.success() {
            let code = status.code().unwrap_or(1);
            let body = String::from_utf8_lossy(&stderr);
            return Err(LlmError::Provider {
                status: u16::try_from(code).unwrap_or(1),
                body: truncate_chars(&body, 500),
            });
        }
        let text = String::from_utf8_lossy(&stdout).trim().to_string();
        if text.is_empty() {
            return Err(LlmError::UnexpectedShape(
                "cursor agent returned an empty answer".into(),
            ));
        }
        Ok(text)
    }
}

#[async_trait]
impl LlmProvider for CursorAgentProvider {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let text = self.complete_text(&request, None).await?;
        Ok(ChatResponse {
            text,
            usage: None,
            model: self.model.clone(),
        })
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let text = self.complete_text(&request, Some(&schema)).await?;
        parse_json_answer(&text)
    }
}

struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn create() -> LlmResult<Self> {
        let path = std::env::temp_dir().join(format!("ai-memory-cursor-{}", Uuid::now_v7()));
        // The prompt is captured session text: keep the workspace private
        // to the server's user on a shared temp directory.
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt as _;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        builder.create(&path).map_err(|err| {
            LlmError::UnexpectedShape(format!("creating cursor workspace: {err}"))
        })?;
        Ok(Self { path })
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn render_request(request: &ChatRequest, schema: Option<&serde_json::Value>) -> String {
    let mut body = String::new();
    if let Some(system) = &request.system {
        body.push_str("# System\n");
        body.push_str(system);
        body.push_str("\n\n");
    }
    for message in &request.messages {
        let heading = match message.role {
            Role::User => "# User\n",
            Role::Assistant => "# Assistant\n",
        };
        body.push_str(heading);
        body.push_str(&message.content);
        body.push_str("\n\n");
    }
    if let Some(schema) = schema {
        body.push_str("# Output\nReturn one JSON value matching this schema and nothing else.\n");
        body.push_str(&schema.to_string());
        body.push('\n');
    }
    body
}

fn parse_json_answer(text: &str) -> LlmResult<serde_json::Value> {
    let stripped = strip_fence(text.trim());
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stripped) {
        return Ok(value);
    }
    let start = stripped.find(['{', '[']);
    let end = stripped.rfind(['}', ']']);
    if let (Some(start), Some(end)) = (start, end)
        && end > start
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&stripped[start..=end])
    {
        return Ok(value);
    }
    Err(LlmError::UnexpectedShape(format!(
        "cursor agent did not return JSON: {}",
        truncate_chars(stripped, 200)
    )))
}

fn strip_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '-');
    let rest = rest.trim_start_matches(['\r', '\n']);
    rest.trim_end().trim_end_matches('`').trim_end()
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if out.chars().count() >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

async fn read_capped(mut reader: impl AsyncRead + Unpin, cap: usize) -> LlmResult<Vec<u8>> {
    let mut buf = [0_u8; 8192];
    let mut out = Vec::new();
    loop {
        let n = reader
            .read(&mut buf)
            .await
            .map_err(|err| LlmError::UnexpectedShape(format!("reading cursor agent: {err}")))?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > cap {
            return Err(LlmError::UnexpectedShape(
                "cursor agent output exceeded the size cap".into(),
            ));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Executable the provider will spawn. `AI_MEMORY_CURSOR_AGENT` wins; otherwise
/// the name `agent` is resolved from `PATH` at spawn time.
#[must_use]
pub fn cursor_executable(override_path: Option<&Path>) -> PathBuf {
    override_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("agent"))
}
