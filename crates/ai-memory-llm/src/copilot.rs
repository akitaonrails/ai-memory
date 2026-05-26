//! GitHub Copilot LLM provider.
//!
//! The GitHub OAuth token (`gho_…`) obtained via device flow is used directly
//! as the Bearer token to `api.githubcopilot.com`. The token is persisted to
//! `oauth_token.json` and loaded on each provider construction.
//!
//! Wire format is OpenAI-compatible chat completions. Copilot routes all
//! model families (OpenAI, Anthropic, Google, reasoning) through the same
//! endpoint — no per-family client needed.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::openai::{
    enforce_strict_object_schemas, max_output_tokens_for, model_requires_default_temperature,
    model_requires_max_completion_tokens,
};
use crate::provider::LlmProvider;
use crate::text::truncate_with_ellipsis;
use crate::types::{ChatRequest, ChatResponse, Role, Usage};

/// GitHub OAuth app client ID registered for ai-memory.
pub const COPILOT_CLIENT_ID: &str = "Ov23liMZsAig9Z7ob61M";

/// GitHub device authorization endpoint (step 1: request device + user code).
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// GitHub OAuth token endpoint (step 2: poll until authorized).
pub const DEVICE_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// GitHub Copilot Chat Completions API base URL.
pub const COPILOT_API_BASE: &str = "https://api.githubcopilot.com";

/// OAuth scope needed to use GitHub Copilot via the API.
pub const COPILOT_SCOPE: &str = "read:user";

/// Persisted GitHub Copilot token.
///
/// The GitHub OAuth token (`gho_…`) does not expire and is the only thing
/// written to disk. The short-lived Copilot bearer token is derived from it
/// at runtime and never persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotToken {
    /// GitHub OAuth access token (`gho_…`).
    pub github_token: String,
}

impl CopilotToken {
    /// Load from the unified token store. Returns `None` when no Copilot
    /// token is saved.
    ///
    /// # Errors
    /// `LlmError::AuthExpired` if the file exists but cannot be parsed.
    pub fn load(path: &std::path::Path) -> LlmResult<Option<Self>> {
        Ok(crate::token_store::TokenFile::load(path)?
            .copilot
            .map(|e| Self { github_token: e.access }))
    }

    /// Persist into the unified token store (reads-then-writes to preserve
    /// tokens for other providers stored in the same file).
    ///
    /// # Errors
    /// Propagates IO errors as `LlmError::AuthExpired`.
    pub fn save(&self, path: &std::path::Path) -> LlmResult<()> {
        use crate::token_store::OAuthEntry;
        let mut file = crate::token_store::TokenFile::load(path)?;
        // GitHub tokens do not expire; refresh == access, expires == 0.
        file.copilot = Some(OAuthEntry {
            kind: "oauth".into(),
            access: self.github_token.clone(),
            refresh: self.github_token.clone(),
            expires: 0,
            account_id: None,
        });
        file.save(path)
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// GitHub Copilot LLM provider backed by `api.githubcopilot.com`.
///
/// Supports all model families Copilot exposes:
/// - OpenAI:    `gpt-4o`, `gpt-4.1`, `gpt-4o-mini`, …
/// - Anthropic: `claude-3.5-sonnet`, `claude-3.7-sonnet`, `claude-sonnet-4`, …
/// - Google:    `gemini-2.0-flash-001`, `gemini-2.5-pro`, …
/// - Reasoning: `o1`, `o3-mini`, `o4-mini`, …
pub struct GitHubCopilotProvider {
    client: reqwest::Client,
    model: String,
    #[allow(dead_code)]
    token_path: PathBuf,
    github_token: String,
}

impl GitHubCopilotProvider {
    /// Load the saved Copilot token and construct the provider.
    ///
    /// # Errors
    /// - `LlmError::AuthExpired` when the token file is missing — run
    ///   `ai-memory auth login copilot` first.
    /// - Propagates IO / parse errors from [`CopilotToken::load`].
    pub fn new(token_path: PathBuf, model: impl Into<String>) -> LlmResult<Self> {
        let token = CopilotToken::load(&token_path)?.ok_or_else(|| {
            LlmError::AuthExpired(
                "no Copilot token found — run `ai-memory auth login copilot` to authenticate"
                    .into(),
            )
        })?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(LlmError::from)?;
        Ok(Self {
            client,
            model: model.into(),
            token_path,
            github_token: token.github_token,
        })
    }

    fn build_request<'a>(
        &'a self,
        request: &'a ChatRequest,
        response_format: Option<CopilotResponseFormat>,
    ) -> CopilotRequest<'a> {
        let mut messages = Vec::new();
        if let Some(sys) = request.system.as_deref() {
            messages.push(CopilotMsg { role: "system", content: sys });
        }
        for m in &request.messages {
            messages.push(CopilotMsg {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            });
        }
        let capped = request.max_tokens.min(max_output_tokens_for(&self.model));
        let (max_tokens, max_completion_tokens) =
            if model_requires_max_completion_tokens(&self.model) {
                (None, Some(request.max_tokens))
            } else {
                (Some(capped), None)
            };
        let temperature = if model_requires_default_temperature(&self.model) {
            None
        } else {
            request.temperature
        };
        CopilotRequest {
            model: &self.model,
            messages,
            max_tokens,
            max_completion_tokens,
            temperature,
            response_format,
        }
    }

    async fn post<B: Serialize>(&self, body: &B) -> LlmResult<CopilotResponse> {
        let url = format!("{COPILOT_API_BASE}/chat/completions");
        debug!(url, "POST github-copilot");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.github_token)
            .header("content-type", "application/json")
            .header("Editor-Version", concat!("ai-memory/", env!("CARGO_PKG_VERSION")))
            .header(
                "Editor-Plugin-Version",
                concat!("ai-memory/", env!("CARGO_PKG_VERSION")),
            )
            .header("Copilot-Integration-Id", "vscode-chat")
            .json(body)
            .send()
            .await
            .map_err(LlmError::from)?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body: truncate_with_ellipsis(&body_text, 1024),
            });
        }
        resp.json::<CopilotResponse>().await.map_err(LlmError::from)
    }
}

#[async_trait]
impl LlmProvider for GitHubCopilotProvider {
    fn name(&self) -> &'static str {
        "github-copilot"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let response = self.post(&self.build_request(&request, None)).await?;
        Ok(into_chat_response(response))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        mut schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        enforce_strict_object_schemas(&mut schema);
        let response_format = CopilotResponseFormat::JsonSchema {
            json_schema: CopilotJsonSchema {
                name: "Result".into(),
                schema,
                strict: true,
            },
        };
        let response = self
            .post(&self.build_request(&request, Some(response_format)))
            .await?;
        let text = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .unwrap_or("");
        serde_json::from_str::<serde_json::Value>(text).map_err(LlmError::from)
    }
}

// ---------------------------------------------------------------------------
// Wire types — OpenAI-compatible request / response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CopilotRequest<'a> {
    model: &'a str,
    messages: Vec<CopilotMsg<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<CopilotResponseFormat>,
}

#[derive(Debug, Serialize)]
struct CopilotMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CopilotResponseFormat {
    JsonSchema { json_schema: CopilotJsonSchema },
}

#[derive(Debug, Serialize)]
struct CopilotJsonSchema {
    name: String,
    schema: serde_json::Value,
    strict: bool,
}

#[derive(Debug, Deserialize)]
struct CopilotResponse {
    choices: Vec<CopilotChoice>,
    model: String,
    #[serde(default)]
    usage: Option<CopilotUsage>,
}

#[derive(Debug, Deserialize)]
struct CopilotChoice {
    message: CopilotMessage,
}

#[derive(Debug, Deserialize)]
struct CopilotMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CopilotUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

fn into_chat_response(response: CopilotResponse) -> ChatResponse {
    let text = response
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    ChatResponse {
        text,
        usage: response.usage.map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        }),
        model: response.model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copilot_token.json");
        let token = CopilotToken {
            github_token: "gho_test_token_abc123".into(),
        };
        token.save(&path).unwrap();
        let loaded = CopilotToken::load(&path).unwrap().unwrap();
        assert_eq!(loaded.github_token, "gho_test_token_abc123");
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(CopilotToken::load(&path).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copilot_token.json");
        let token = CopilotToken { github_token: "gho_test".into() };
        token.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "copilot token file must be mode 0600");
    }
}
