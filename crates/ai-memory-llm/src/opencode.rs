//! OpenCode Go provider.
//!
//! Selects the wire API published for each model under
//! `https://opencode.ai/zen/go/v1`: GPT-5.6 Luna uses the Responses API and
//! other models use OpenAI-compatible Chat Completions. Accepts an `sk-...`
//! API key from `OPENCODE_API_KEY`.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::error::{LlmError, LlmResult};
use crate::openai::{
    STRUCTURED_OUTPUT_SCHEMA_NAME, enforce_strict_object_schemas, normalize_openai_base,
};
use crate::openai_compat::OpenAiCompatProvider;
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited};
use crate::types::{ChatRequest, ChatResponse, LlmOperationId, ReasoningEffort, Usage};

/// Public OpenCode Zen/Go OpenAI-compatible base URL.
pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Default model when `AI_MEMORY_LLM_MODEL` is not set.
pub const OPENCODE_DEFAULT_MODEL: &str = "claude-sonnet-4-6";

const OPENCODE_USER_AGENT: &str = concat!("ai-memory/", env!("CARGO_PKG_VERSION"));
const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// OpenCode Go LLM provider.
///
/// Routes through `https://opencode.ai/zen/go/v1` using the model's published
/// Responses or Chat Completions wire format. Authenticate with the `sk-...`
/// key obtained from <https://opencode.ai/auth>.
pub struct OpenCodeProvider {
    transport: OpenCodeTransport,
}

enum OpenCodeTransport {
    ChatCompletions(OpenAiCompatProvider),
    Responses(OpenCodeResponsesProvider),
}

impl OpenCodeProvider {
    /// Construct an OpenCode Zen/Go provider.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        Self::new_with_base_url(api_key, model, OPENCODE_ZEN_BASE_URL)
    }

    fn new_with_base_url(
        api_key: SecretString,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> LlmResult<Self> {
        let model = model.into();
        let base_url = base_url.into();
        let transport = if model_uses_responses_api(&model) {
            OpenCodeTransport::Responses(OpenCodeResponsesProvider::new(api_key, model, base_url)?)
        } else {
            OpenCodeTransport::ChatCompletions(
                OpenAiCompatProvider::new(base_url, Some(api_key), model)?
                    .with_client_headers(OPENCODE_USER_AGENT, OPENCODE_SESSION_HEADER),
            )
        };
        Ok(Self { transport })
    }

    #[cfg(test)]
    fn with_strict(mut self, strict: bool) -> Self {
        self.transport = match self.transport {
            OpenCodeTransport::ChatCompletions(provider) => {
                OpenCodeTransport::ChatCompletions(provider.with_strict(strict))
            }
            responses => responses,
        };
        self
    }

    /// Override the per-request timeout on the wrapped
    /// [`OpenAiCompatProvider`]. The factory calls this with
    /// `ProviderConfig::request_timeout_secs`.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.transport = match self.transport {
            OpenCodeTransport::ChatCompletions(provider) => {
                OpenCodeTransport::ChatCompletions(provider.with_timeout_secs(secs))
            }
            OpenCodeTransport::Responses(provider) => {
                OpenCodeTransport::Responses(provider.with_timeout_secs(secs))
            }
        };
        self
    }

    /// Forward reasoning effort to the OpenAI-compatible Zen/Go client.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.transport = match self.transport {
            OpenCodeTransport::ChatCompletions(provider) => {
                OpenCodeTransport::ChatCompletions(provider.with_reasoning_effort(effort))
            }
            OpenCodeTransport::Responses(provider) => {
                OpenCodeTransport::Responses(provider.with_reasoning_effort(effort))
            }
        };
        self
    }
}

#[async_trait]
impl LlmProvider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn model(&self) -> &str {
        match &self.transport {
            OpenCodeTransport::ChatCompletions(provider) => provider.model(),
            OpenCodeTransport::Responses(provider) => &provider.model,
        }
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
        match &self.transport {
            OpenCodeTransport::ChatCompletions(provider) => {
                provider
                    .complete_with_operation_id(request, operation_id)
                    .await
            }
            OpenCodeTransport::Responses(provider) => {
                provider.complete(request, operation_id).await
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
        match &self.transport {
            OpenCodeTransport::ChatCompletions(provider) => {
                provider
                    .complete_structured_raw_with_operation_id(request, schema, operation_id)
                    .await
            }
            OpenCodeTransport::Responses(provider) => {
                provider
                    .complete_structured(request, schema, operation_id)
                    .await
            }
        }
    }
}

fn model_uses_responses_api(model: &str) -> bool {
    // OpenCode Go publishes Luna only through /responses. Its
    // /chat/completions route returns an internal-server error for these calls.
    model.eq_ignore_ascii_case("gpt-5.6-luna")
}

struct OpenCodeResponsesProvider {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
    timeout: Duration,
    reasoning_effort: Option<ReasoningEffort>,
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
        let response = self
            .client
            .post(url)
            .timeout(self.timeout)
            .bearer_auth(self.api_key.expose_secret())
            .header("content-type", "application/json")
            .header(reqwest::header::USER_AGENT, OPENCODE_USER_AGENT)
            .header(OPENCODE_SESSION_HEADER, operation_id.to_string())
            .json(body)
            .send()
            .await?;
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
            "model": "gpt-5.6-luna",
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": content }],
            }],
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
    fn public_constants_point_at_zen_base_url() {
        assert_eq!(OPENCODE_ZEN_BASE_URL, "https://opencode.ai/zen/go/v1");
        assert!(!OPENCODE_DEFAULT_MODEL.is_empty());
    }

    #[tokio::test]
    async fn luna_completion_uses_responses_api() {
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
            "gpt-5.6-luna",
            server.uri(),
        )
        .unwrap();
        let operation_id = LlmOperationId::new();
        let response = provider
            .complete_with_operation_id(ChatRequest::user_prompt("hello"), operation_id)
            .await
            .unwrap();

        assert_eq!(response.text, "ok");
        assert_eq!(response.model, "gpt-5.6-luna");
        assert_eq!(response.usage.unwrap().input_tokens, 1);
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["max_output_tokens"], 1024);
        assert!(body.get("temperature").is_none());
        assert_eq!(
            header_value(&requests[0], "user-agent"),
            Some(OPENCODE_USER_AGENT)
        );
        assert_eq!(
            header_value(&requests[0], OPENCODE_SESSION_HEADER),
            Some(operation_id.to_string().as_str())
        );
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
