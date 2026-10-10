//! OpenCode Go/Zen provider.
//!
//! Selects the wire API published for each model: Responses, Anthropic Messages,
//! or OpenAI-compatible Chat Completions.
//! Defaults to Go's catalogue at `https://opencode.ai/zen/go/v1`; set
//! `AI_MEMORY_LLM_BASE_URL` (or call [`OpenCodeProvider::with_base_url`]) to
//! point at Zen's general catalogue instead. Accepts an `sk-...` API key from
//! `OPENCODE_API_KEY`. Identifies itself with [`crate::DEFAULT_USER_AGENT`]
//! and defaults the `x-opencode-session` correlation header, both of which an
//! `AI_MEMORY_LLM_HEADERS` entry can override.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::AnthropicProvider;
use crate::error::{LlmError, LlmResult};
use crate::openai::{
    STRUCTURED_OUTPUT_SCHEMA_NAME, enforce_strict_object_schemas, normalize_openai_base,
};
use crate::openai_compat::OpenAiCompatProvider;
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited};
use crate::types::{
    ChatRequest, ChatResponse, ExtraHeaders, LlmOperationId, ReasoningEffort, Usage,
};

/// Primary OpenCode Go OpenAI-compatible base URL.
pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Public OpenCode Zen/Go OpenAI-compatible base URL.
#[deprecated(
    since = "2.1.0",
    note = "names Zen but holds Go's endpoint; use OPENCODE_GO_BASE_URL"
)]
pub const OPENCODE_ZEN_BASE_URL: &str = OPENCODE_GO_BASE_URL;

/// Default model when `AI_MEMORY_LLM_MODEL` is not set.
pub const OPENCODE_DEFAULT_MODEL: &str = "mimo-v2.6-flash";

/// Session-correlation header OpenCode asks callers to send, on Zen and Go alike.
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// OpenCode Go LLM provider.
///
/// Routes through `https://opencode.ai/zen/go/v1` using the model's published
/// Responses, Anthropic Messages, or Chat Completions wire format. Authenticate
/// with the `sk-...` key obtained from <https://opencode.ai/auth>.
pub struct OpenCodeProvider {
    base_url: String,
    chat_completions: OpenAiCompatProvider,
    responses: OpenCodeResponsesProvider,
    anthropic_messages: AnthropicProvider,
}

impl OpenCodeProvider {
    /// Construct an OpenCode Zen/Go provider.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        Self::new_with_base_url(api_key, model, OPENCODE_GO_BASE_URL)
    }

    fn new_with_base_url(
        api_key: SecretString,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> LlmResult<Self> {
        let model = model.into();
        let base_url = base_url.into();
        Ok(Self {
            chat_completions: OpenAiCompatProvider::new(
                base_url.clone(),
                Some(api_key.clone()),
                model.clone(),
            )?
            .with_client_headers(crate::DEFAULT_USER_AGENT, OPENCODE_SESSION_HEADER),
            responses: OpenCodeResponsesProvider::new(
                api_key.clone(),
                model.clone(),
                base_url.clone(),
            )?,
            anthropic_messages: AnthropicProvider::new(api_key, model)?
                .with_base_url(anthropic_base_url(&base_url))
                .with_client_headers(crate::DEFAULT_USER_AGENT, OPENCODE_SESSION_HEADER),
            base_url,
        })
    }

    #[cfg(test)]
    fn with_strict(mut self, strict: bool) -> Self {
        self.chat_completions = self.chat_completions.with_strict(strict);
        self
    }

    /// Override the per-request timeout on the wrapped
    /// [`OpenAiCompatProvider`]. The factory calls this with
    /// `ProviderConfig::request_timeout_secs`.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.chat_completions = self.chat_completions.with_timeout_secs(secs);
        self.responses = self.responses.with_timeout_secs(secs);
        self.anthropic_messages = self.anthropic_messages.with_timeout_secs(secs);
        self
    }

    /// Forward reasoning effort to the OpenAI-compatible Zen/Go client.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.chat_completions = self.chat_completions.with_reasoning_effort(effort);
        self.responses = self.responses.with_reasoning_effort(effort);
        self.anthropic_messages = self.anthropic_messages.with_reasoning_effort(effort);
        self
    }

    /// Point the provider at a different OpenCode endpoint (Zen's general
    /// catalogue instead of Go's default). The factory calls this with
    /// `AI_MEMORY_LLM_BASE_URL`.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        self.chat_completions = self.chat_completions.with_base_url(url.clone());
        self.responses = self.responses.with_base_url(url.clone());
        self.anthropic_messages = self
            .anthropic_messages
            .with_base_url(anthropic_base_url(&url));
        self.base_url = url;
        self
    }

    /// Forward operator-configured headers (`AI_MEMORY_LLM_HEADERS`). The session
    /// header and user agent this provider sets are per-request defaults, so an
    /// operator entry for either name wins.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        self.chat_completions = self.chat_completions.with_extra_headers(headers.clone());
        self.responses = self.responses.with_extra_headers(headers.clone());
        self.anthropic_messages = self.anthropic_messages.with_extra_headers(headers);
        self
    }

    fn api(&self) -> OpenCodeApi {
        model_api(self.model(), &self.base_url)
    }

    #[cfg(test)]
    fn base_url(&self) -> &str {
        self.chat_completions.base_url()
    }
}

#[async_trait]
impl LlmProvider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn model(&self) -> &str {
        self.chat_completions.model()
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
        match self.api() {
            OpenCodeApi::ChatCompletions => {
                self.chat_completions
                    .complete_with_operation_id(request, operation_id)
                    .await
            }
            OpenCodeApi::Responses => self.responses.complete(request, operation_id).await,
            OpenCodeApi::AnthropicMessages => {
                self.anthropic_messages
                    .complete_with_operation_id(request, operation_id)
                    .await
            }
        }
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
        match self.api() {
            OpenCodeApi::ChatCompletions => {
                self.chat_completions
                    .complete_structured_raw_with_operation_id(request, schema, operation_id)
                    .await
            }
            OpenCodeApi::Responses => {
                self.responses
                    .complete_structured(request, schema, operation_id)
                    .await
            }
            OpenCodeApi::AnthropicMessages => {
                self.anthropic_messages
                    .complete_structured_raw_with_operation_id(request, schema, operation_id)
                    .await
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenCodeApi {
    Responses,
    AnthropicMessages,
    ChatCompletions,
}

fn model_api(model: &str, base_url: &str) -> OpenCodeApi {
    if is_go_base_url(base_url) {
        if matches_model(model, GO_RESPONSES_MODELS) {
            return OpenCodeApi::Responses;
        }
        if matches_model(model, GO_ANTHROPIC_MESSAGES_MODELS) {
            return OpenCodeApi::AnthropicMessages;
        }
    }
    if matches_model(model, RESPONSES_MODELS) {
        OpenCodeApi::Responses
    } else if matches_model(model, ANTHROPIC_MESSAGES_MODELS) {
        OpenCodeApi::AnthropicMessages
    } else {
        OpenCodeApi::ChatCompletions
    }
}

fn is_go_base_url(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).ok().is_some_and(|url| {
        let path = url.path().trim_end_matches('/');
        let base = path
            .strip_suffix("/chat/completions")
            .or_else(|| path.strip_suffix("/responses"))
            .or_else(|| path.strip_suffix("/messages"))
            .unwrap_or(path);
        base.ends_with("/zen/go/v1") || base.ends_with("/zen/go")
    })
}

fn matches_model(model: &str, models: &[&str]) -> bool {
    models
        .iter()
        .any(|candidate| model.eq_ignore_ascii_case(candidate))
}

fn anthropic_base_url(base_url: &str) -> String {
    base_url
        .trim_end_matches('/')
        .strip_suffix("/v1/messages")
        .or_else(|| base_url.trim_end_matches('/').strip_suffix("/v1"))
        .unwrap_or_else(|| base_url.trim_end_matches('/'))
        .to_string()
}

const GO_RESPONSES_MODELS: &[&str] = &["muse-spark-1.3-contributor", "muse-spark-1.2-contributor"];

const GO_ANTHROPIC_MESSAGES_MODELS: &[&str] = &["minimax-m3", "minimax-m2.7", "qwen3.8-max"];

const RESPONSES_MODELS: &[&str] = &[
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6.1-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.5-pro",
    "gpt-5.4",
    "gpt-5.4-pro",
    "gpt-5.4-mini",
    "gpt-5.4-nano",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.1",
    "gpt-5.1-codex",
    "gpt-5.1-codex-max",
    "gpt-5.1-codex-mini",
    "gpt-5",
    "gpt-5-codex",
    "gpt-5-nano",
    "grok-4.7",
    "grok-4.6",
    "grok-4.5",
    "grok-build-0.1",
    "muse-spark-1.3",
    "muse-spark-1.2",
    "muse-spark-1.3-contributor-free",
];

const ANTHROPIC_MESSAGES_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-opus-4-5",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "claude-sonnet-4-6",
    "claude-sonnet-4-5",
    "claude-sonnet-4",
    "claude-haiku-4-5",
    "qwen3.8-flash",
    "qwen3.7-max",
    "qwen3.7-plus",
    "qwen3.6-plus",
    "qwen3.5-plus",
];

struct OpenCodeResponsesProvider {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
    timeout: Duration,
    reasoning_effort: Option<ReasoningEffort>,
    extra_headers: ExtraHeaders,
}

impl OpenCodeResponsesProvider {
    fn new(api_key: SecretString, model: String, base_url: String) -> LlmResult<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            api_key,
            base_url,
            model,
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
        })
    }

    fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        self.extra_headers = headers;
        self
    }

    fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn complete(
        &self,
        request: ChatRequest,
        operation_id: LlmOperationId,
    ) -> LlmResult<ChatResponse> {
        let response = self
            .post(&self.build_request(&request, None), operation_id)
            .await?;
        Ok(ChatResponse {
            text: extract_output_text(&response).unwrap_or_default(),
            usage: response.usage.map(|usage| Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            }),
            model: response.model,
        })
    }

    async fn complete_structured(
        &self,
        request: ChatRequest,
        mut schema: serde_json::Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<serde_json::Value> {
        enforce_strict_object_schemas(&mut schema);
        let text = ResponsesText {
            format: ResponsesTextFormat::JsonSchema {
                name: STRUCTURED_OUTPUT_SCHEMA_NAME.into(),
                schema,
                strict: true,
            },
        };
        let response = self
            .post(&self.build_request(&request, Some(text)), operation_id)
            .await?;
        let text = extract_output_text(&response).unwrap_or_default();
        serde_json::from_str(&text).map_err(LlmError::from)
    }

    fn build_request<'a>(
        &'a self,
        request: &'a ChatRequest,
        text: Option<ResponsesText>,
    ) -> ResponsesRequest<'a> {
        ResponsesRequest {
            model: &self.model,
            instructions: request.system.as_deref(),
            input: request
                .messages
                .iter()
                .map(|message| ResponsesInputMessage {
                    role: message.role.as_str(),
                    content: vec![ResponsesInputContent {
                        kind: "input_text",
                        text: &message.content,
                    }],
                })
                .collect(),
            max_output_tokens: request.max_tokens,
            store: false,
            text,
            reasoning: self.reasoning_effort.map(|effort| ResponsesReasoning {
                effort: effort.openai_wire_effort(),
            }),
        }
    }

    async fn post<B: Serialize>(
        &self,
        body: &B,
        operation_id: LlmOperationId,
    ) -> LlmResult<ResponsesResponse> {
        let url = normalize_openai_base(&self.base_url, "responses");
        let builder = self
            .client
            .post(url)
            .timeout(self.timeout)
            .bearer_auth(self.api_key.expose_secret())
            .header("content-type", "application/json")
            .json(body);
        let mut headers = self.extra_headers.clone();
        headers.set_default(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(crate::DEFAULT_USER_AGENT),
        );
        let name = reqwest::header::HeaderName::from_static(OPENCODE_SESSION_HEADER);
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&operation_id.to_string()) {
            headers.set_default(name, value);
        }
        let response = headers.apply(builder).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body: provider_error_body(response).await,
            });
        }
        response_json_limited(response).await
    }
}

#[derive(Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: Vec<ResponsesInputMessage<'a>>,
    max_output_tokens: u32,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<ResponsesText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ResponsesReasoning>,
}

#[derive(Serialize)]
struct ResponsesText {
    format: ResponsesTextFormat,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponsesTextFormat {
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        strict: bool,
    },
}

#[derive(Serialize)]
struct ResponsesInputMessage<'a> {
    role: &'a str,
    content: Vec<ResponsesInputContent<'a>>,
}

#[derive(Serialize)]
struct ResponsesInputContent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct ResponsesReasoning {
    effort: ReasoningEffort,
}

#[derive(Deserialize)]
struct ResponsesResponse {
    model: String,
    #[serde(default)]
    output: Vec<ResponsesOutputItem>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
}

#[derive(Deserialize)]
struct ResponsesOutputItem {
    #[serde(default)]
    content: Vec<ResponsesOutputContent>,
}

#[derive(Deserialize)]
struct ResponsesOutputContent {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

fn extract_output_text(response: &ResponsesResponse) -> Option<String> {
    response
        .output
        .iter()
        .flat_map(|item| item.content.iter())
        .filter_map(|content| content.text.as_deref())
        .find(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn response_with_content(content: &str) -> serde_json::Value {
        json!({
            "model": "model-x",
            "choices": [{
                "message": { "content": content },
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 },
        })
    }

    fn responses_response_with_content(content: &str) -> serde_json::Value {
        json!({
            "model": "gpt-6-luna",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": content }],
            }],
            "usage": { "input_tokens": 1, "output_tokens": 1 },
        })
    }

    fn anthropic_response_with_content(content: &str) -> serde_json::Value {
        json!({
            "model": "claude-sonnet-5-5",
            "content": [{ "type": "text", "text": content }],
            "usage": { "input_tokens": 1, "output_tokens": 1 },
        })
    }

    fn header_value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
        request
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    #[test]
    fn provider_reports_opencode_name_and_configured_model() {
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x").unwrap();
        assert_eq!(provider.name(), "opencode");
        assert_eq!(provider.model(), "model-x");
    }

    #[test]
    fn the_default_base_url_is_gos_endpoint() {
        assert_eq!(OPENCODE_GO_BASE_URL, "https://opencode.ai/zen/go/v1");
        assert!(!OPENCODE_DEFAULT_MODEL.is_empty());
    }

    #[test]
    #[allow(deprecated)]
    fn the_deprecated_alias_still_resolves_to_go() {
        assert_eq!(OPENCODE_ZEN_BASE_URL, OPENCODE_GO_BASE_URL);
    }

    #[test]
    fn with_base_url_repoints_the_provider_at_zen() {
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x").unwrap();
        assert_eq!(provider.base_url(), OPENCODE_GO_BASE_URL);

        let provider = provider.with_base_url("https://opencode.ai/zen/v1");
        assert_eq!(provider.base_url(), "https://opencode.ai/zen/v1");
    }

    #[test]
    fn luna_models_select_the_responses_transport() {
        for model in ["gpt-5.6-luna", "gpt-6-luna", "GPT-6-LUNA"] {
            let provider = OpenCodeProvider::new(SecretString::from("sk-test"), model).unwrap();
            assert!(matches!(provider.api(), OpenCodeApi::Responses));
        }
    }

    #[test]
    fn published_models_select_their_documented_transports() {
        for model in RESPONSES_MODELS {
            let provider = OpenCodeProvider::new(SecretString::from("sk-test"), *model).unwrap();
            assert!(matches!(provider.api(), OpenCodeApi::Responses));
        }
        for model in ANTHROPIC_MESSAGES_MODELS {
            let provider = OpenCodeProvider::new(SecretString::from("sk-test"), *model).unwrap();
            assert!(matches!(provider.api(), OpenCodeApi::AnthropicMessages));
        }
    }

    #[test]
    fn catalogue_specific_models_select_the_documented_api() {
        let cases = [
            (
                "minimax-m3",
                OpenCodeApi::ChatCompletions,
                OpenCodeApi::AnthropicMessages,
            ),
            (
                "minimax-m2.7",
                OpenCodeApi::ChatCompletions,
                OpenCodeApi::AnthropicMessages,
            ),
            (
                "qwen3.8-max",
                OpenCodeApi::ChatCompletions,
                OpenCodeApi::AnthropicMessages,
            ),
            (
                "muse-spark-1.3-contributor",
                OpenCodeApi::ChatCompletions,
                OpenCodeApi::Responses,
            ),
            (
                "muse-spark-1.2-contributor",
                OpenCodeApi::ChatCompletions,
                OpenCodeApi::Responses,
            ),
            ("gpt-6-luna", OpenCodeApi::Responses, OpenCodeApi::Responses),
            (
                "qwen3.8-flash",
                OpenCodeApi::AnthropicMessages,
                OpenCodeApi::AnthropicMessages,
            ),
            (
                "deepseek-v4-flash",
                OpenCodeApi::ChatCompletions,
                OpenCodeApi::ChatCompletions,
            ),
        ];
        for (model, zen_api, go_api) in cases {
            for model in [model.to_string(), model.to_ascii_uppercase()] {
                let mut provider =
                    OpenCodeProvider::new(SecretString::from("sk-test"), &model).unwrap();
                assert_eq!(provider.api(), go_api, "{model}");
                for (url, expected) in [
                    ("https://opencode.ai/zen/v1", zen_api),
                    (OPENCODE_GO_BASE_URL, go_api),
                    ("https://opencode.ai/zen", zen_api),
                ] {
                    provider = provider.with_base_url(url);
                    assert_eq!(provider.api(), expected, "{model} at {url}");
                    assert_eq!(provider.model(), model);
                    assert_eq!(provider.base_url(), url);
                }
            }
        }
    }

    #[test]
    fn go_catalogue_detection_uses_url_paths() {
        for url in [
            "https://opencode.ai/zen/go",
            "https://opencode.ai/zen/go/v1/",
            "https://opencode.ai/zen/go/v1/messages",
            "https://opencode.ai/zen/go/v1/responses/",
            "https://opencode.ai/zen/go/v1/chat/completions",
            "http://localhost:1234/proxy/zen/go/v1",
            "https://opencode.ai/zen/go/v1?key=value",
        ] {
            assert!(is_go_base_url(url), "{url}");
        }
        for url in [
            "https://opencode.ai/zen/v1",
            "https://opencode.ai/zen/v1/messages",
            "https://opencode.ai/zen/go-other/v1",
            "https://opencode.ai/zen/go/v1/models",
            "https://proxy.example/v1?upstream=/zen/go/v1",
            "https://proxy.example/v1#/zen/go/v1",
            "http://localhost:1234/v1",
            "not-a-url/zen/go/v1",
        ] {
            assert!(!is_go_base_url(url), "{url}");
        }
    }

    #[tokio::test]
    async fn factory_routes_plain_and_structured_requests_for_both_catalogues() {
        use crate::{ProviderAuth, ProviderChoice, ProviderConfig, build_provider};

        for model in [
            "minimax-m3",
            "minimax-m2.7",
            "qwen3.8-max",
            "muse-spark-1.3-contributor",
            "muse-spark-1.2-contributor",
            "gpt-6-luna",
            "gpt-5.6-luna",
            "qwen3.8-flash",
            "deepseek-v4-flash",
        ] {
            for catalogue in ["zen", "zen/go"] {
                let server = MockServer::start().await;
                let base = format!("{}/{catalogue}/v1", server.uri());
                let api = match (model, catalogue) {
                    ("minimax-m3" | "minimax-m2.7" | "qwen3.8-max", "zen/go")
                    | ("qwen3.8-flash", _) => OpenCodeApi::AnthropicMessages,
                    ("muse-spark-1.3-contributor" | "muse-spark-1.2-contributor", "zen/go")
                    | ("gpt-6-luna" | "gpt-5.6-luna", _) => OpenCodeApi::Responses,
                    _ => OpenCodeApi::ChatCompletions,
                };
                let endpoint = match api {
                    OpenCodeApi::ChatCompletions => "chat/completions",
                    OpenCodeApi::Responses => "responses",
                    OpenCodeApi::AnthropicMessages => "messages",
                };
                Mock::given(method("POST"))
                    .and(path(format!("/{catalogue}/v1/{endpoint}")))
                    .respond_with(move |request: &Request| {
                        let body: serde_json::Value =
                            serde_json::from_slice(&request.body).unwrap();
                        let mut response = match api {
                            OpenCodeApi::ChatCompletions => response_with_content(r#"{"ok":true}"#),
                            OpenCodeApi::Responses => {
                                responses_response_with_content(r#"{"ok":true}"#)
                            }
                            OpenCodeApi::AnthropicMessages if body.get("tools").is_some() => {
                                json!({
                                    "content": [{"type": "tool_use", "input": {"ok": true}}],
                                })
                            }
                            OpenCodeApi::AnthropicMessages => {
                                anthropic_response_with_content(r#"{"ok":true}"#)
                            }
                        };
                        response["model"] = body["model"].clone();
                        ResponseTemplate::new(200).set_body_json(response)
                    })
                    .expect(2)
                    .mount(&server)
                    .await;
                let provider = build_provider(ProviderConfig {
                    provider: ProviderChoice::OpenCode,
                    model: model.into(),
                    auth: ProviderAuth::required_api_key_from_env(
                        "OPENCODE_API_KEY",
                        Some(SecretString::from("sk-test")),
                    ),
                    base_url: Some(base),
                    compat_strict: false,
                    compat_disable_thinking: false,
                    request_timeout_secs: 5,
                    reasoning_effort: Some(ReasoningEffort::High),
                    extra_headers: ExtraHeaders::parse(["x-test: routed"]).unwrap(),
                })
                .unwrap();
                let operation_id = LlmOperationId::new();
                let request = ChatRequest::user_prompt("hello");
                let response = provider
                    .complete_with_operation_id(request.clone(), operation_id)
                    .await
                    .unwrap();
                assert_eq!(response.model, model);
                let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]});
                let result = provider
                    .complete_structured_raw_with_operation_id(request, schema, operation_id)
                    .await
                    .unwrap();
                assert_eq!(result, json!({"ok": true}));
                let requests = server.received_requests().await.unwrap();
                assert_eq!(requests.len(), 2, "{model} at {catalogue}");
                for (index, request) in requests.iter().enumerate() {
                    assert_eq!(header_value(request, "x-test"), Some("routed"));
                    assert_eq!(
                        header_value(request, "user-agent"),
                        Some(crate::DEFAULT_USER_AGENT)
                    );
                    assert_eq!(
                        header_value(request, OPENCODE_SESSION_HEADER),
                        Some(operation_id.to_string().as_str())
                    );
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(body["model"], model);
                    match api {
                        OpenCodeApi::ChatCompletions => {
                            assert_eq!(
                                header_value(request, "authorization"),
                                Some("Bearer sk-test")
                            );
                            assert!(header_value(request, "x-api-key").is_none());
                            assert_eq!(body["messages"][0]["content"], "hello");
                            assert_eq!(body["reasoning_effort"], "high");
                            assert!(body.get("input").is_none());
                            assert!(body.get("tools").is_none());
                        }
                        OpenCodeApi::Responses => {
                            assert_eq!(
                                header_value(request, "authorization"),
                                Some("Bearer sk-test")
                            );
                            assert!(header_value(request, "x-api-key").is_none());
                            assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
                            assert_eq!(body["reasoning"]["effort"], "high");
                            assert!(body.get("messages").is_none());
                            if index == 1 {
                                assert_eq!(body["text"]["format"]["type"], "json_schema");
                            }
                        }
                        OpenCodeApi::AnthropicMessages => {
                            assert_eq!(header_value(request, "x-api-key"), Some("sk-test"));
                            assert_eq!(
                                header_value(request, "anthropic-version"),
                                Some(crate::anthropic::ANTHROPIC_VERSION)
                            );
                            assert!(header_value(request, "authorization").is_none());
                            assert_eq!(body["messages"][0]["content"], "hello");
                            assert!(body.get("input").is_none());
                            if index == 1 {
                                assert_eq!(body["tool_choice"]["name"], "result");
                                assert_eq!(
                                    body["tools"][0]["input_schema"]["properties"]["ok"]["type"],
                                    "boolean"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn repeated_catalogue_switches_preserve_operator_settings() {
        for model in ["minimax-m3", "muse-spark-1.3-contributor"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(|request: &Request| {
                    let response = match request.url.path() {
                        "/zen/go/v1/messages" => anthropic_response_with_content("ok"),
                        "/zen/go/v1/responses" => responses_response_with_content("ok"),
                        "/zen/v1/chat/completions" => response_with_content("ok"),
                        other => panic!("unexpected endpoint: {other}"),
                    };
                    ResponseTemplate::new(200).set_body_json(response)
                })
                .expect(3)
                .mount(&server)
                .await;
            let mut provider = OpenCodeProvider::new(SecretString::from("sk-test"), model)
                .unwrap()
                .with_timeout_secs(7)
                .with_reasoning_effort(Some(ReasoningEffort::High))
                .with_extra_headers(
                    ExtraHeaders::parse([
                        "user-agent: custom-client",
                        "x-opencode-session: custom-session",
                    ])
                    .unwrap(),
                );
            for catalogue in ["zen", "zen/go", "zen"] {
                provider = provider.with_base_url(format!("{}/{catalogue}/v1", server.uri()));
                provider
                    .complete(ChatRequest::user_prompt("hello"))
                    .await
                    .unwrap();
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 3);
            for request in requests {
                assert_eq!(header_value(&request, "user-agent"), Some("custom-client"));
                assert_eq!(
                    header_value(&request, OPENCODE_SESSION_HEADER),
                    Some("custom-session")
                );
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                if request.url.path().ends_with("chat/completions") {
                    assert_eq!(body["reasoning_effort"], "high");
                } else if request.url.path().ends_with("responses") {
                    assert_eq!(body["reasoning"]["effort"], "high");
                }
            }
        }
    }

    #[tokio::test]
    async fn timeout_survives_switching_catalogues_and_transports() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(response_with_content("ok"))
                    .set_delay(Duration::from_millis(50)),
            )
            .mount(&server)
            .await;
        for model in ["minimax-m3", "muse-spark-1.3-contributor"] {
            let mut provider = OpenCodeProvider::new(SecretString::from("sk-test"), model)
                .unwrap()
                .with_timeout_secs(0);
            for catalogue in ["zen", "zen/go", "zen"] {
                provider = provider.with_base_url(format!("{}/{catalogue}/v1", server.uri()));
                let error = provider
                    .complete(ChatRequest::user_prompt("hello"))
                    .await
                    .unwrap_err();
                assert!(matches!(error, LlmError::Http(error) if error.is_timeout()));
            }
        }
    }

    #[tokio::test]
    async fn catalogue_endpoints_normalize_bare_versioned_and_complete_urls() {
        for (model, catalogue, endpoint, api) in [
            (
                "minimax-m3",
                "zen/go",
                "messages",
                OpenCodeApi::AnthropicMessages,
            ),
            (
                "minimax-m3",
                "zen",
                "chat/completions",
                OpenCodeApi::ChatCompletions,
            ),
            ("gpt-6-luna", "zen/go", "responses", OpenCodeApi::Responses),
            ("gpt-6-luna", "zen", "responses", OpenCodeApi::Responses),
            (
                "claude-sonnet-5-5",
                "zen",
                "messages",
                OpenCodeApi::AnthropicMessages,
            ),
        ] {
            let server = MockServer::start().await;
            let response = match api {
                OpenCodeApi::ChatCompletions => response_with_content("ok"),
                OpenCodeApi::Responses => responses_response_with_content("ok"),
                OpenCodeApi::AnthropicMessages => anthropic_response_with_content("ok"),
            };
            Mock::given(method("POST"))
                .and(path(format!("/{catalogue}/v1/{endpoint}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(response))
                .expect(4)
                .mount(&server)
                .await;
            for suffix in [
                String::new(),
                "/v1/".into(),
                format!("/v1/{endpoint}"),
                format!("/v1/{endpoint}/"),
            ] {
                let provider = OpenCodeProvider::new(SecretString::from("sk-test"), model)
                    .unwrap()
                    .with_base_url(format!("{}/{catalogue}{suffix}", server.uri()));
                assert_eq!(provider.api(), api);
                let response = provider
                    .complete(ChatRequest::user_prompt("hello"))
                    .await
                    .unwrap();
                assert_eq!(response.text, "ok");
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 4);
        }
    }

    #[tokio::test]
    async fn catalogue_switches_preserve_provider_errors_without_retrying() {
        for (model, catalogue, endpoint) in [
            ("minimax-m3", "zen/go", "messages"),
            ("minimax-m3", "zen", "chat/completions"),
            ("muse-spark-1.3-contributor", "zen/go", "responses"),
        ] {
            for status in [401, 500] {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path(format!("/{catalogue}/v1/{endpoint}")))
                    .respond_with(ResponseTemplate::new(status).set_body_string("gateway error"))
                    .expect(2)
                    .mount(&server)
                    .await;
                let provider = OpenCodeProvider::new(SecretString::from("sk-test"), model)
                    .unwrap()
                    .with_base_url(format!("{}/{catalogue}/v1", server.uri()));
                let request = ChatRequest::user_prompt("hello");
                for error in [
                    provider.complete(request.clone()).await.unwrap_err(),
                    provider
                        .complete_structured_raw(request, json!({"type": "object"}))
                        .await
                        .unwrap_err(),
                ] {
                    assert!(
                        matches!(error, LlmError::Provider { status: code, body } if code == status && body == "gateway error")
                    );
                }
                assert_eq!(server.received_requests().await.unwrap().len(), 2);
            }
        }
    }

    #[test]
    fn default_model_selects_go_chat_completions_transport() {
        let provider =
            OpenCodeProvider::new(SecretString::from("sk-test"), OPENCODE_DEFAULT_MODEL).unwrap();
        assert!(matches!(provider.api(), OpenCodeApi::ChatCompletions));
        assert_eq!(provider.model(), "mimo-v2.6-flash");
        assert_eq!(provider.base_url(), OPENCODE_GO_BASE_URL);
    }

    #[test]
    fn chat_completions_models_keep_the_existing_transport() {
        let provider =
            OpenCodeProvider::new(SecretString::from("sk-test"), "deepseek-v4-flash").unwrap();
        assert!(matches!(provider.api(), OpenCodeApi::ChatCompletions));
    }

    #[test]
    fn responses_provider_preserves_custom_base_url() {
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "gpt-6-luna")
            .unwrap()
            .with_base_url("https://opencode.ai/zen/v1");
        assert_eq!(provider.base_url(), "https://opencode.ai/zen/v1");
    }

    #[test]
    fn responses_endpoint_normalizes_custom_base_urls() {
        assert_eq!(
            normalize_openai_base("https://opencode.ai/zen/v1", "responses"),
            "https://opencode.ai/zen/v1/responses"
        );
        assert_eq!(
            normalize_openai_base("https://opencode.ai/zen", "responses"),
            "https://opencode.ai/zen/v1/responses"
        );
        assert_eq!(
            normalize_openai_base("https://opencode.ai/zen/v1/responses", "responses"),
            "https://opencode.ai/zen/v1/responses"
        );
    }

    #[tokio::test]
    async fn gpt_6_luna_completion_uses_responses_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(responses_response_with_content("ok")),
            )
            .mount(&server)
            .await;

        let provider = OpenCodeProvider::new_with_base_url(
            SecretString::from("sk-test"),
            "gpt-6-luna",
            server.uri(),
        )
        .unwrap();
        let operation_id = LlmOperationId::new();
        let response = provider
            .complete_with_operation_id(ChatRequest::user_prompt("hello"), operation_id)
            .await
            .unwrap();

        assert_eq!(response.text, "ok");
        assert_eq!(response.model, "gpt-6-luna");
        assert_eq!(response.usage.unwrap().input_tokens, 1);
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["max_output_tokens"], 1024);
        assert!(body.get("temperature").is_none());
        assert_eq!(
            header_value(&requests[0], "user-agent"),
            Some(crate::DEFAULT_USER_AGENT)
        );
        assert_eq!(
            header_value(&requests[0], OPENCODE_SESSION_HEADER),
            Some(operation_id.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn claude_completion_uses_anthropic_messages_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/zen/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(anthropic_response_with_content("ok")),
            )
            .mount(&server)
            .await;

        let provider = OpenCodeProvider::new_with_base_url(
            SecretString::from("sk-test"),
            "claude-sonnet-5-5",
            format!("{}/zen/v1", server.uri()),
        )
        .unwrap();
        let operation_id = LlmOperationId::new();
        let response = provider
            .complete_with_operation_id(ChatRequest::user_prompt("hello"), operation_id)
            .await
            .unwrap();

        assert_eq!(response.text, "ok");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "claude-sonnet-5-5");
        assert_eq!(header_value(&requests[0], "x-api-key"), Some("sk-test"));
        assert_eq!(
            header_value(&requests[0], "user-agent"),
            Some(crate::DEFAULT_USER_AGENT)
        );
        assert_eq!(
            header_value(&requests[0], OPENCODE_SESSION_HEADER),
            Some(operation_id.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn default_model_completion_preserves_the_go_catalogue() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/zen/go/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_with_content("ok")))
            .mount(&server)
            .await;

        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), OPENCODE_DEFAULT_MODEL)
            .unwrap()
            .with_base_url(format!("{}/zen/go/v1", server.uri()));
        provider
            .complete(ChatRequest::user_prompt("hello"))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "mimo-v2.6-flash");
    }

    #[tokio::test]
    async fn luna_structured_completion_uses_responses_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(responses_response_with_content(r#"{"ok":true}"#)),
            )
            .mount(&server)
            .await;

        let provider = OpenCodeProvider::new_with_base_url(
            SecretString::from("sk-test"),
            "gpt-5.6-luna",
            server.uri(),
        )
        .unwrap();
        let value = provider
            .complete_structured_raw(
                ChatRequest::user_prompt("emit JSON"),
                json!({
                    "type": "object",
                    "properties": { "ok": { "type": "boolean" } },
                    "required": ["ok"],
                }),
            )
            .await
            .unwrap();

        assert_eq!(value, json!({ "ok": true }));
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert_eq!(body["text"]["format"]["schema"]["type"], "object");
    }

    #[tokio::test]
    async fn deepseek_completion_keeps_chat_completions_and_operation_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_with_content("ok")))
            .mount(&server)
            .await;

        let provider = OpenCodeProvider::new_with_base_url(
            SecretString::from("sk-test"),
            "deepseek-v4-flash",
            server.uri(),
        )
        .unwrap();
        let operation_id = LlmOperationId::new();
        provider
            .complete_with_operation_id(ChatRequest::user_prompt("hello"), operation_id)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            header_value(&requests[0], "user-agent"),
            Some(concat!("ai-memory/", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(
            header_value(&requests[0], "x-opencode-session"),
            Some(operation_id.to_string().as_str())
        );
    }

    #[tokio::test]
    async fn structured_fallback_reuses_its_logical_operation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(|request: &Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("request body is JSON");
                if body.get("response_format").is_some() {
                    ResponseTemplate::new(400)
                        .set_body_string("unsupported parameter: response_format")
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(response_with_content(r#"{"ok":true}"#))
                }
            })
            .mount(&server)
            .await;

        let provider = OpenCodeProvider::new_with_base_url(
            SecretString::from("sk-test"),
            "model-x",
            server.uri(),
        )
        .unwrap()
        .with_strict(true);
        let operation_id = LlmOperationId::new();
        let value = provider
            .complete_structured_raw_with_operation_id(
                ChatRequest::user_prompt("emit JSON"),
                json!({
                    "type": "object",
                    "properties": { "ok": { "type": "boolean" } },
                    "required": ["ok"],
                }),
                operation_id,
            )
            .await
            .unwrap();

        assert_eq!(value, json!({"ok": true}));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let expected_id = operation_id.to_string();
        assert_eq!(
            header_value(&requests[0], "x-opencode-session"),
            Some(expected_id.as_str())
        );
        assert_eq!(
            header_value(&requests[1], "x-opencode-session"),
            Some(expected_id.as_str())
        );
    }
}
