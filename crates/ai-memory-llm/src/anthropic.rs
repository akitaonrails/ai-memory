//! Anthropic Messages API client.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::text::truncate_with_ellipsis;
use crate::types::{ChatRequest, ChatResponse, Role, Usage};

/// Default Anthropic API base.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Pinned Anthropic API version header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `anthropic-beta` header sent on OAuth (subscription) requests. Mirrors
/// the Claude Code OAuth handshake: the `oauth-2025-04-20` feature is what
/// authorises a subscription bearer token against /v1/messages, and the
/// `claude-code-*` feature matches what the official CLI sends. Values
/// cross-checked against oh-my-pi's `claudeCodeBetaDefaults`.
///
/// NOTE: this header combination is derived from Claude Code's documented OAuth
/// handshake and should be smoke-tested with a real `claude setup-token` token
/// before use in production, as Anthropic may update the required beta values.
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20,claude-code-20250219";

/// Authentication mode for the Anthropic provider.
#[derive(Clone)]
enum AnthropicAuth {
    /// Static API key sent as `x-api-key`.
    ApiKey(SecretString),
    /// OAuth bearer token from a Claude Pro/Max subscription
    /// (obtained via `claude setup-token`).
    OAuth(SecretString),
}

/// Anthropic Messages-API-backed provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
    auth: AnthropicAuth,
    base_url: String,
    model: String,
}

impl AnthropicProvider {
    /// Construct a provider given an API key and model id.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the underlying HTTP client cannot
    /// be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        // 300s matches the OpenAI/openai-compat client — same reason:
        // first request after a model swap on a local inference server
        // (Ollama, llama-swap, vLLM) can take 30-90s of cold-load.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::ApiKey(api_key),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
        })
    }

    /// Construct a provider using an OAuth subscription token from
    /// `claude setup-token` (Claude Pro/Max subscription). Hits the same
    /// `/v1/messages` endpoint as `new`, but uses a Bearer token and the
    /// `anthropic-beta: oauth-2025-04-20,claude-code-20250219` header
    /// instead of `x-api-key`.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the underlying HTTP client cannot
    /// be built.
    pub fn new_oauth(token: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::OAuth(token),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
        })
    }

    /// Override the API base URL (mostly for tests against wiremock).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<AnthropicMsg<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Debug, Serialize)]
struct AnthropicMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicToolChoice {
    Tool { name: String },
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    model: String,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContent {
    Text { text: String },
    ToolUse { input: serde_json::Value },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let messages: Vec<AnthropicMsg<'_>> = request
            .messages
            .iter()
            .map(|m| AnthropicMsg {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            })
            .collect();
        let body = AnthropicRequest {
            model: &self.model,
            max_tokens: request.max_tokens,
            system: request.system.as_deref(),
            messages,
            temperature: request.temperature,
            tools: None,
            tool_choice: None,
        };
        let response: AnthropicResponse = self.post(&body).await?;
        let text = response
            .content
            .iter()
            .filter_map(|c| match c {
                AnthropicContent::Text { text } => Some(text.as_str()),
                AnthropicContent::ToolUse { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ChatResponse {
            text,
            usage: response.usage.map(|u| Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
            }),
            model: response.model,
        })
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let tool = AnthropicTool {
            name: "result".into(),
            description: "Emit the structured result.".into(),
            input_schema: schema,
        };
        let messages: Vec<AnthropicMsg<'_>> = request
            .messages
            .iter()
            .map(|m| AnthropicMsg {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                content: &m.content,
            })
            .collect();
        let body = AnthropicRequest {
            model: &self.model,
            max_tokens: request.max_tokens,
            system: request.system.as_deref(),
            messages,
            temperature: request.temperature,
            tools: Some(vec![tool]),
            tool_choice: Some(AnthropicToolChoice::Tool {
                name: "result".into(),
            }),
        };
        let response: AnthropicResponse = self.post(&body).await?;
        for c in response.content {
            if let AnthropicContent::ToolUse { input, .. } = c {
                return Ok(input);
            }
        }
        Err(LlmError::UnexpectedShape(
            "anthropic response had no tool_use block".into(),
        ))
    }
}

impl AnthropicProvider {
    async fn post<B: Serialize, R: DeserializeOwned>(&self, body: &B) -> LlmResult<R> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        debug!(url, "POST anthropic");
        let mut builder = self
            .client
            .post(&url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        // Apply the auth headers through the same helper the tests assert on,
        // so a change to one can't silently diverge from the other.
        for (name, value) in self.auth_headers() {
            builder = builder.header(name, value);
        }
        let resp = builder.json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body: truncate_with_ellipsis(&body, 1024),
            });
        }
        resp.json::<R>().await.map_err(LlmError::from)
    }

    /// The auth headers for this provider instance: `x-api-key` for a static
    /// key, or `Authorization: Bearer` + `anthropic-beta` for an OAuth
    /// subscription token. The two modes are mutually exclusive — OAuth must
    /// never send `x-api-key` or Anthropic rejects the request. `post` applies
    /// these, and the unit tests assert on them, so both stay in lockstep.
    fn auth_headers(&self) -> Vec<(&'static str, String)> {
        match &self.auth {
            AnthropicAuth::ApiKey(key) => vec![("x-api-key", key.expose_secret().to_string())],
            AnthropicAuth::OAuth(token) => vec![
                ("authorization", format!("Bearer {}", token.expose_secret())),
                ("anthropic-beta", ANTHROPIC_OAUTH_BETA.to_string()),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    #[test]
    fn api_key_provider_sends_x_api_key_no_authorization() {
        let provider =
            AnthropicProvider::new(SecretString::from("sk-ant-test"), "claude-sonnet-4-6").unwrap();
        let headers = provider.auth_headers();
        let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"x-api-key"), "expected x-api-key header");
        assert!(
            !names.contains(&"authorization"),
            "api-key mode must NOT send authorization header"
        );
        assert!(
            !names.contains(&"anthropic-beta"),
            "api-key mode must NOT send anthropic-beta header"
        );
        let key_val = headers
            .iter()
            .find(|(n, _)| *n == "x-api-key")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert_eq!(key_val, "sk-ant-test");
    }

    #[test]
    fn oauth_provider_sends_bearer_and_beta_no_x_api_key() {
        let provider =
            AnthropicProvider::new_oauth(SecretString::from("tok-oauth-test"), "claude-sonnet-4-6")
                .unwrap();
        let headers = provider.auth_headers();
        let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
        assert!(
            !names.contains(&"x-api-key"),
            "oauth mode must NOT send x-api-key header"
        );
        assert!(
            names.contains(&"authorization"),
            "expected authorization header"
        );
        assert!(
            names.contains(&"anthropic-beta"),
            "expected anthropic-beta header"
        );
        let auth_val = headers
            .iter()
            .find(|(n, _)| *n == "authorization")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert_eq!(auth_val, "Bearer tok-oauth-test");
        let beta_val = headers
            .iter()
            .find(|(n, _)| *n == "anthropic-beta")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        assert!(
            beta_val.contains("oauth-2025-04-20"),
            "anthropic-beta must contain oauth-2025-04-20"
        );
    }

    #[test]
    fn with_base_url_is_preserved_after_oauth_construction() {
        let provider = AnthropicProvider::new_oauth(SecretString::from("tok"), "claude-sonnet-4-6")
            .unwrap()
            .with_base_url("http://localhost:9999");
        assert_eq!(provider.base_url, "http://localhost:9999");
    }
}
