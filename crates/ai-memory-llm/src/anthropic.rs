//! Anthropic Messages API client.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::openai_compat::{first_json_object, strip_reasoning_blocks};
use crate::provider::LlmProvider;
use crate::response::{provider_error, response_json_limited};
use crate::types::{
    ChatRequest, ChatResponse, ExtraHeaders, LlmOperationId, ReasoningEffort, Usage,
};

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
    timeout: Duration,
    reasoning_effort: Option<ReasoningEffort>,
    extra_headers: ExtraHeaders,
    client_headers: Option<ClientHeaders>,
}

#[derive(Clone, Copy)]
pub(crate) struct ClientHeaders {
    user_agent: &'static str,
    operation_id: &'static str,
}

impl AnthropicProvider {
    /// Construct a provider given an API key and model id.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the underlying HTTP client cannot
    /// be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(crate::redirect::same_origin_redirects())
            .build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::ApiKey(api_key),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
            client_headers: None,
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
            .redirect(crate::redirect::same_origin_redirects())
            .build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::OAuth(token),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
            client_headers: None,
        })
    }

    /// Override the API base URL (mostly for tests against wiremock).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the per-request timeout (default
    /// [`crate::DEFAULT_REQUEST_TIMEOUT_SECS`]).
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    /// Set Claude effort / thinking. `None` omits both fields so the model
    /// default applies. On models that accept `output_config.effort`, a
    /// configured value is mapped onto that field; `none` disables thinking.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// Attach operator-configured headers to every chat request. The factory
    /// calls this with `ProviderConfig::extra_headers`.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        self.extra_headers = headers;
        self
    }

    #[must_use]
    pub(crate) fn with_client_headers(
        mut self,
        user_agent: &'static str,
        operation_id: &'static str,
    ) -> Self {
        self.client_headers = Some(ClientHeaders {
            user_agent,
            operation_id,
        });
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
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
}

#[derive(Debug, Serialize)]
struct AnthropicOutputConfig {
    effort: ReasoningEffort,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicThinking {
    Adaptive,
    Disabled,
    /// Sonnet 5.5's lowest setting, no extended thinking: the model's way to
    /// turn thinking off, since it answers `disabled` with a 400.
    BetweenTools,
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
    Text {
        text: String,
    },
    ToolUse {
        input: serde_json::Value,
    },
    /// A block this provider does not read, such as `thinking` or
    /// `redacted_thinking`. Models that think by default (Opus 5 and later,
    /// Sonnet 5 and later, Haiku 5.5, Fable, Mythos), and every model sent
    /// adaptive thinking for a configured effort, can start a response with
    /// such blocks. Without this variant one of them fails the whole parse.
    #[serde(other)]
    Other,
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
        self.complete_with_operation_id(request, LlmOperationId::new())
            .await
    }

    async fn complete_with_operation_id(
        &self,
        request: ChatRequest,
        operation_id: LlmOperationId,
    ) -> LlmResult<ChatResponse> {
        let body = self.build_request(&request, None);
        let response: AnthropicResponse = self.post(&body, operation_id).await?;
        let text = response
            .content
            .iter()
            .filter_map(|c| match c {
                AnthropicContent::Text { text } => Some(text.as_str()),
                AnthropicContent::ToolUse { .. } | AnthropicContent::Other => None,
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
        self.complete_structured_raw_with_operation_id(request, schema, LlmOperationId::new())
            .await
    }

    async fn complete_structured_raw_with_operation_id(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<serde_json::Value> {
        let body = self.build_request(&request, Some(schema));
        let response: AnthropicResponse = self.post(&body, operation_id).await?;
        let forced = self.forces_result_tool();
        let mut text = Vec::new();
        for c in response.content {
            match c {
                AnthropicContent::ToolUse { input, .. } => return Ok(input),
                // Under `auto` the model can reply with the JSON itself
                // instead of calling the tool.
                AnthropicContent::Text { text: part } if !forced => text.push(part),
                _ => {}
            }
        }
        if let Some(value) = json_object_in(&text.join("\n")) {
            return Ok(value);
        }
        Err(LlmError::UnexpectedShape(
            "anthropic response had no tool_use block".into(),
        ))
    }
}

/// The JSON object in a text reply, bare, fenced, or set in prose.
fn json_object_in(text: &str) -> Option<serde_json::Value> {
    let cleaned = strip_reasoning_blocks(text);
    serde_json::from_str::<serde_json::Value>(&cleaned)
        .ok()
        .filter(serde_json::Value::is_object)
        .or_else(|| serde_json::from_str(first_json_object(&cleaned)?).ok())
}

impl AnthropicProvider {
    /// Build the `/v1/messages` body shared by `complete` and
    /// `complete_structured_raw`. Passing a schema turns the call into the
    /// `result`-tool structured-output shape. Both paths go through here so the
    /// temperature rule below can't apply to one and silently miss the other.
    fn build_request<'a>(
        &'a self,
        request: &'a ChatRequest,
        schema: Option<serde_json::Value>,
    ) -> AnthropicRequest<'a> {
        let messages: Vec<AnthropicMsg<'a>> = request
            .messages
            .iter()
            .map(|m| AnthropicMsg {
                role: m.role.as_str(),
                content: &m.content,
            })
            .collect();
        let forced = self.forces_result_tool();
        // Without a forced `tool_choice`, the description is what asks for
        // the call.
        let description = if forced {
            "Emit the structured result."
        } else {
            "Emit the structured result. Call this tool once instead of replying in text."
        };
        let (tools, tool_choice) = match schema {
            Some(input_schema) => (
                Some(vec![AnthropicTool {
                    name: "result".into(),
                    description: description.into(),
                    input_schema,
                }]),
                forced.then(|| AnthropicToolChoice::Tool {
                    name: "result".into(),
                }),
            ),
            None => (None, None),
        };
        let claude = ClaudeId::parse(&self.model);
        let temperature = request
            .temperature
            .filter(|_| !claude.is_some_and(ClaudeId::rejects_temperature));
        let (output_config, thinking) = self
            .reasoning_effort
            .zip(claude)
            .map(|(effort, claude)| claude.reasoning_fields(effort))
            .unwrap_or((None, None));
        AnthropicRequest {
            model: &self.model,
            max_tokens: request.max_tokens,
            system: request.system.as_deref(),
            messages,
            temperature,
            tools,
            tool_choice,
            output_config,
            thinking,
        }
    }

    /// Whether the structured path pins `tool_choice` to the `result` tool.
    /// The models that answer a forced `tool_choice` with a 400 keep the
    /// default `auto`.
    fn forces_result_tool(&self) -> bool {
        let claude = ClaudeId::parse(&self.model);
        !claude.is_some_and(ClaudeId::rejects_forced_tool_choice)
    }

    async fn post<B: Serialize, R: DeserializeOwned>(
        &self,
        body: &B,
        operation_id: LlmOperationId,
    ) -> LlmResult<R> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        debug!(url, "POST anthropic");
        let mut builder = self
            .client
            .post(&url)
            .timeout(self.timeout)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        // Apply the auth headers through the same helper the tests assert on,
        // so a change to one can't silently diverge from the other.
        for (name, value) in self.auth_headers() {
            builder = builder.header(name, value);
        }
        let mut headers = self.extra_headers.clone();
        if let Some(client_headers) = self.client_headers {
            headers.set_default(
                HeaderName::from_static("user-agent"),
                HeaderValue::from_static(client_headers.user_agent),
            );
            headers.set_default(
                HeaderName::from_static(client_headers.operation_id),
                HeaderValue::from_str(&operation_id.to_string()).map_err(|error| {
                    LlmError::UnexpectedShape(format!("invalid operation ID header: {error}"))
                })?,
            );
        }
        let resp = headers.apply(builder).json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(provider_error(resp).await);
        }
        response_json_limited::<R>(resp).await
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

/// Modern `claude-<family>-<major>[-<minor>]` id, or dateless Mythos Preview.
///
/// Legacy family-last ids (`claude-3-5-sonnet-…`) and unversioned aliases
/// (`claude-opus-latest`) stay `None` so we leave those requests alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeId {
    Released {
        family: ClaudeFamily,
        major: u32,
        minor: u32,
    },
    MythosPreview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeFamily {
    Opus,
    Sonnet,
    Haiku,
    Fable,
    Mythos,
}

impl ClaudeFamily {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Sonnet => "sonnet",
            Self::Haiku => "haiku",
            Self::Fable => "fable",
            Self::Mythos => "mythos",
        }
    }

    fn effort_since(self) -> Option<(u32, u32)> {
        match self {
            Self::Opus => Some((4, 5)),
            Self::Sonnet => Some((4, 6)),
            Self::Fable | Self::Mythos => Some((5, 0)),
            // Haiku 4.5 400s on `output_config.effort`; Haiku 5.5 accepts it.
            Self::Haiku => Some((5, 5)),
        }
    }

    fn adaptive_since(self) -> Option<(u32, u32)> {
        match self {
            Self::Opus | Self::Sonnet => Some((4, 6)),
            Self::Fable | Self::Mythos => Some((5, 0)),
            Self::Haiku => Some((5, 5)),
        }
    }
}

impl std::str::FromStr for ClaudeFamily {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for family in [
            Self::Opus,
            Self::Sonnet,
            Self::Haiku,
            Self::Fable,
            Self::Mythos,
        ] {
            if s == family.as_str() {
                return Ok(family);
            }
        }
        Err(())
    }
}

impl ClaudeId {
    fn parse(model: &str) -> Option<Self> {
        let lower = model.to_ascii_lowercase();
        // Bedrock/Vertex wrap the same `claude-<family>-…` core in a prefix
        // and snapshot suffix.
        let rest = lower.split_once("claude-")?.1;
        let mut parts = rest.split(|c: char| !c.is_ascii_alphanumeric());
        let family = parts.next()?.parse().ok()?;
        match parts.next()? {
            "preview" if family == ClaudeFamily::Mythos => Some(Self::MythosPreview),
            major => Some(Self::Released {
                family,
                major: version_segment(major)?,
                minor: parts.next().and_then(version_segment).unwrap_or(0),
            }),
        }
    }

    fn family(self) -> ClaudeFamily {
        match self {
            Self::MythosPreview => ClaudeFamily::Mythos,
            Self::Released { family, .. } => family,
        }
    }

    fn meets(self, floor: Option<(u32, u32)>) -> bool {
        match self {
            Self::MythosPreview => true,
            Self::Released { major, minor, .. } => floor.is_some_and(|min| (major, minor) >= min),
        }
    }

    /// Claude 4.7+ and Mythos Preview 400 if we send a non-default
    /// `temperature`. Unparsed ids keep the caller's value.
    fn rejects_temperature(self) -> bool {
        self.meets(Some((4, 7)))
    }

    /// Opus 5.5, Sonnet 5.5, Fable 5.1, and Mythos 5.1 400 on a forced
    /// `tool_choice` (`any` or a named tool). Haiku 5.5, Opus 5, Sonnet 5,
    /// and Fable 5 still accept one.
    fn rejects_forced_tool_choice(self) -> bool {
        match self {
            Self::Released {
                family: ClaudeFamily::Opus | ClaudeFamily::Sonnet,
                major,
                minor,
            } => (major, minor) >= (5, 5),
            Self::Released {
                family: ClaudeFamily::Fable | ClaudeFamily::Mythos,
                major,
                minor,
            } => (major, minor) >= (5, 1),
            _ => false,
        }
    }

    fn supports_effort(self) -> bool {
        self.meets(self.family().effort_since())
    }

    fn uses_adaptive_thinking(self) -> bool {
        self.meets(self.family().adaptive_since())
    }

    /// Fable 5, Mythos 5, Mythos Preview, and Opus 5.5 reject
    /// `thinking.type=disabled` at every effort.
    fn thinking_always_on(self) -> bool {
        match self {
            Self::MythosPreview => true,
            Self::Released {
                family: ClaudeFamily::Fable | ClaudeFamily::Mythos,
                major,
                ..
            } => major >= 5,
            Self::Released {
                family: ClaudeFamily::Opus,
                major,
                minor,
            } => (major, minor) >= (5, 5),
            _ => false,
        }
    }

    /// Sonnet 5.5 rejects `thinking.type=disabled` too, but turns thinking
    /// off with `between_tools`.
    fn turns_thinking_off_between_tools(self) -> bool {
        matches!(
            self,
            Self::Released {
                family: ClaudeFamily::Sonnet,
                major,
                minor,
            } if (major, minor) >= (5, 5)
        )
    }

    fn accepts_thinking_disabled(self) -> bool {
        self.supports_effort() && !self.thinking_always_on()
    }

    /// Official effort availability: Opus 4.5 is `high` and below; Opus/Sonnet
    /// 4.6 and Mythos Preview accept `max` but not `xhigh`; later models take
    /// the full set.
    fn clamp_output_effort(self, effort: ReasoningEffort) -> Option<ReasoningEffort> {
        let mapped = effort.anthropic_output_effort()?;
        Some(match (self, mapped) {
            (
                Self::Released {
                    family: ClaudeFamily::Opus,
                    major: 4,
                    minor: 5,
                },
                ReasoningEffort::XHigh | ReasoningEffort::Max,
            ) => ReasoningEffort::High,
            (
                Self::MythosPreview
                | Self::Released {
                    family: ClaudeFamily::Opus | ClaudeFamily::Sonnet,
                    major: 4,
                    minor: 6,
                },
                ReasoningEffort::XHigh,
            ) => ReasoningEffort::Max,
            (_, mapped) => mapped,
        })
    }

    fn reasoning_fields(
        self,
        effort: ReasoningEffort,
    ) -> (Option<AnthropicOutputConfig>, Option<AnthropicThinking>) {
        match effort {
            ReasoningEffort::None if self.turns_thinking_off_between_tools() => {
                (None, Some(AnthropicThinking::BetweenTools))
            }
            ReasoningEffort::None => (
                None,
                self.accepts_thinking_disabled()
                    .then_some(AnthropicThinking::Disabled),
            ),
            effort if self.supports_effort() => (
                self.clamp_output_effort(effort)
                    .map(|effort| AnthropicOutputConfig { effort }),
                self.uses_adaptive_thinking()
                    .then_some(AnthropicThinking::Adaptive),
            ),
            _ => (None, None),
        }
    }
}

/// One or two digits. Longer runs are date snapshots (`…-4-20250514`).
fn version_segment(s: &str) -> Option<u32> {
    s.parse().ok().filter(|_| (1..=2).contains(&s.len()))
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::types::{ChatMessage, ReasoningEffort};
    use rstest::rstest;

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

    fn chat_request() -> ChatRequest {
        ChatRequest {
            system: None,
            messages: vec![ChatMessage::user("x")],
            max_tokens: 256,
            temperature: Some(0.2),
        }
    }

    /// Serialized `/v1/messages` body for `model`, with the caller's
    /// temperature set to 0.2 — what bootstrap / consolidation actually send.
    fn body_for(model: &str, schema: Option<serde_json::Value>) -> serde_json::Value {
        let provider = AnthropicProvider::new(SecretString::from("sk-ant-test"), model).unwrap();
        let request = chat_request();
        serde_json::to_value(provider.build_request(&request, schema)).unwrap()
    }

    #[test]
    fn build_request_omits_temperature_for_models_that_deprecated_it() {
        // The structured path is the one that actually broke in the field:
        // bootstrap sends temperature 0.2 and Anthropic answers 400
        // "`temperature` is deprecated for this model".
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
            "anthropic.claude-mythos-preview-v1:0",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "anthropic.claude-opus-5-v1:0",
        ] {
            let body = body_for(model, Some(json!({})));
            assert!(
                body.get("temperature").is_none(),
                "temperature must be omitted for {model}"
            );
        }
    }

    #[test]
    fn build_request_keeps_temperature_for_models_that_still_accept_it() {
        // Determinism matters for consolidation output, so the models that
        // still honour sampling params must keep the caller's 0.2.
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-haiku-4-5-20251001",
            "claude-opus-4-20250514",
            "claude-3-5-sonnet-20241022",
        ] {
            let body = body_for(model, None);
            let temp = body["temperature"]
                .as_f64()
                .unwrap_or_else(|| panic!("temperature must be forwarded for {model}, got {body}"));
            assert!((temp - 0.2).abs() < 1e-6, "{model}: got {temp}");
        }
    }

    #[test]
    fn build_request_keeps_the_structured_tool_shape() {
        // The temperature rule must not disturb the forced-tool wiring the
        // structured path depends on.
        let body = body_for("claude-opus-5", Some(json!({"type": "object"})));
        assert_eq!(body["tools"][0]["name"], json!("result"));
        assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "result"})
        );

        let plain = body_for("claude-opus-5", None);
        assert!(plain.get("tools").is_none());
        assert!(plain.get("tool_choice").is_none());
        assert!(plain.get("output_config").is_none());
        assert!(plain.get("thinking").is_none());
    }

    /// A model that thinks opens its response with `thinking` blocks, empty
    /// under the default `display: "omitted"`, and may return
    /// `redacted_thinking`. Both completion paths must read past them.
    #[tokio::test]
    async fn thinking_blocks_do_not_fail_either_completion_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "claude-opus-5",
                "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig"},
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "text", "text": "OK"},
                    {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "result",
                        "input": {"answer": "OK"}
                    }
                ],
                "usage": {"input_tokens": 3, "output_tokens": 5}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let provider = AnthropicProvider::new(SecretString::from("sk-ant-test"), "claude-opus-5")
            .unwrap()
            .with_base_url(server.uri());

        let response = provider.complete(chat_request()).await.unwrap();
        assert_eq!(response.text, "OK");

        let value = provider
            .complete_structured_raw(chat_request(), json!({"type": "object"}))
            .await
            .unwrap();
        assert_eq!(value, json!({"answer": "OK"}));
    }

    fn body_for_effort(model: &str, effort: ReasoningEffort) -> serde_json::Value {
        let provider = AnthropicProvider::new(SecretString::from("sk-ant-test"), model)
            .unwrap()
            .with_reasoning_effort(Some(effort));
        serde_json::to_value(provider.build_request(&chat_request(), None)).unwrap()
    }

    #[rstest]
    #[case::sonnet_46_adaptive(
        "claude-sonnet-4-6",
        ReasoningEffort::Low,
        Some("low"),
        Some("adaptive")
    )]
    #[case::opus_5_disabled("claude-opus-5", ReasoningEffort::None, None, Some("disabled"))]
    #[case::haiku_45_omits("claude-haiku-4-5", ReasoningEffort::Low, None, None)]
    #[case::haiku_45_none_omits("claude-haiku-4-5", ReasoningEffort::None, None, None)]
    #[case::haiku_55_adaptive(
        "claude-haiku-5-5",
        ReasoningEffort::Low,
        Some("low"),
        Some("adaptive")
    )]
    #[case::haiku_55_disabled("claude-haiku-5-5", ReasoningEffort::None, None, Some("disabled"))]
    #[case::haiku_55_xhigh_kept(
        "claude-haiku-5-5",
        ReasoningEffort::XHigh,
        Some("xhigh"),
        Some("adaptive")
    )]
    #[case::opus_45_effort_only("claude-opus-4-5", ReasoningEffort::High, Some("high"), None)]
    #[case::mythos_preview_none_omits_disabled(
        "claude-mythos-preview",
        ReasoningEffort::None,
        None,
        None
    )]
    #[case::fable_5_none_omits_disabled("claude-fable-5", ReasoningEffort::None, None, None)]
    #[case::opus_55_none_omits_disabled("claude-opus-5-5", ReasoningEffort::None, None, None)]
    #[case::bedrock_opus_55_none_omits_disabled(
        "anthropic.claude-opus-5-5",
        ReasoningEffort::None,
        None,
        None
    )]
    #[case::sonnet_5_disabled("claude-sonnet-5", ReasoningEffort::None, None, Some("disabled"))]
    #[case::sonnet_55_none_between_tools(
        "claude-sonnet-5-5",
        ReasoningEffort::None,
        None,
        Some("between_tools")
    )]
    #[case::sonnet_55_low_adaptive(
        "claude-sonnet-5-5",
        ReasoningEffort::Low,
        Some("low"),
        Some("adaptive")
    )]
    #[case::sonnet_46_xhigh_clamps_max(
        "claude-sonnet-4-6",
        ReasoningEffort::XHigh,
        Some("max"),
        Some("adaptive")
    )]
    #[case::opus_45_max_clamps_high("claude-opus-4-5", ReasoningEffort::Max, Some("high"), None)]
    fn maps_effort_to_native_thinking_fields(
        #[case] model: &str,
        #[case] effort: ReasoningEffort,
        #[case] output_effort: Option<&str>,
        #[case] thinking: Option<&str>,
    ) {
        let body = body_for_effort(model, effort);
        match output_effort {
            Some(expected) => assert_eq!(body["output_config"]["effort"], json!(expected)),
            None => assert!(body.get("output_config").is_none()),
        }
        match thinking {
            Some(expected) => assert_eq!(body["thinking"]["type"], json!(expected)),
            None => assert!(body.get("thinking").is_none()),
        }
    }

    #[test]
    fn claude_family_parses_lowercase_labels() {
        for family in [
            ClaudeFamily::Opus,
            ClaudeFamily::Sonnet,
            ClaudeFamily::Haiku,
            ClaudeFamily::Fable,
            ClaudeFamily::Mythos,
        ] {
            assert_eq!(family.as_str().parse::<ClaudeFamily>().unwrap(), family);
        }
        assert!("unknown".parse::<ClaudeFamily>().is_err());
    }

    #[test]
    fn with_base_url_is_preserved_after_oauth_construction() {
        let provider = AnthropicProvider::new_oauth(SecretString::from("tok"), "claude-sonnet-4-6")
            .unwrap()
            .with_base_url("http://localhost:9999");
        assert_eq!(provider.base_url, "http://localhost:9999");
    }
}
