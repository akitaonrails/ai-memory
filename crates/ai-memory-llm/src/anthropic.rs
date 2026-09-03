//! Anthropic Messages API client.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited};
use crate::types::{ChatRequest, ChatResponse, ExtraHeaders, ReasoningEffort, Usage};

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
}

impl AnthropicProvider {
    /// Construct a provider given an API key and model id.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the underlying HTTP client cannot
    /// be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::ApiKey(api_key),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
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
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            auth: AnthropicAuth::OAuth(token),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
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
        let body = self.build_request(&request, None);
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
        let body = self.build_request(&request, Some(schema));
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
    /// Build the `/v1/messages` body shared by `complete` and
    /// `complete_structured_raw`. Passing a schema turns the call into the
    /// forced-tool structured-output shape. Both paths go through here so the
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
        let (tools, tool_choice) = match schema {
            Some(input_schema) => (
                Some(vec![AnthropicTool {
                    name: "result".into(),
                    description: "Emit the structured result.".into(),
                    input_schema,
                }]),
                Some(AnthropicToolChoice::Tool {
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

    async fn post<B: Serialize, R: DeserializeOwned>(&self, body: &B) -> LlmResult<R> {
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
        let resp = self.extra_headers.apply(builder).json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
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
            Self::Haiku => None,
        }
    }

    fn adaptive_since(self) -> Option<(u32, u32)> {
        match self {
            Self::Opus | Self::Sonnet => Some((4, 6)),
            Self::Fable | Self::Mythos => Some((5, 0)),
            Self::Haiku => None,
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

    fn supports_effort(self) -> bool {
        self.meets(self.family().effort_since())
    }

    fn uses_adaptive_thinking(self) -> bool {
        self.meets(self.family().adaptive_since())
    }

    /// Fable 5, Mythos 5, and Mythos Preview reject `thinking.type=disabled`.
    fn thinking_always_on(self) -> bool {
        match self {
            Self::MythosPreview => true,
            Self::Released {
                family: ClaudeFamily::Fable | ClaudeFamily::Mythos,
                major,
                ..
            } => major >= 5,
            _ => false,
        }
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
    #[case::opus_45_effort_only("claude-opus-4-5", ReasoningEffort::High, Some("high"), None)]
    #[case::mythos_preview_none_omits_disabled(
        "claude-mythos-preview",
        ReasoningEffort::None,
        None,
        None
    )]
    #[case::fable_5_none_omits_disabled("claude-fable-5", ReasoningEffort::None, None, None)]
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
