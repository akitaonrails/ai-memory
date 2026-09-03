//! OpenAI Chat Completions client (with `response_format` JSON schema for
//! structured output).

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited};
use reqwest::header::{HeaderName, HeaderValue};

use crate::types::{
    ChatRequest, ChatResponse, ExtraHeaders, LlmOperationId, ReasoningEffort, Usage,
};

/// Default OpenAI API base.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Name embedded in the `json_schema` envelope of every structured-output
/// request OpenAI / openai-compat send. OpenAI's docs use "Result" as the
/// canonical sample name; we standardise on the same literal so the
/// schema-name surface stays one source of truth across the
/// `OpenAiProvider`, the openai-compat strict path (which delegates to
/// this provider), the Copilot provider, and any future fork. Local
/// engines (vLLM / LM Studio) sometimes echo this name in error
/// messages and logs — naming it makes those messages discoverable.
pub(crate) const STRUCTURED_OUTPUT_SCHEMA_NAME: &str = "Result";

/// Build the full URL for an OpenAI-style endpoint. Tolerates the
/// conventions found in the wild:
///   * `https://api.openai.com`           (OpenAI's own docs)
///   * `https://openrouter.ai/api/v1`     (OpenRouter's docs)
///   * `http://localhost:11434/v1`        (Ollama's openai-compat path)
///   * `https://api.z.ai/api/coding/paas/v4` (Z.AI)
///
/// Without this, half the providers produce `…/v1/v1/…` 404s the
/// first time consolidation runs.
#[must_use]
pub fn normalize_openai_base(base: &str, endpoint: &str) -> String {
    let s = base.trim_end_matches('/');

    if s.ends_with(&format!("/{endpoint}")) {
        return s.to_string();
    }

    if last_segment_is_version(s) {
        return format!("{s}/{endpoint}");
    }

    format!("{s}/v1/{endpoint}")
}

fn last_segment_is_version(url: &str) -> bool {
    url.split('/').next_back().is_some_and(|seg| {
        let digits = seg.strip_prefix('v').unwrap_or("");
        !digits.is_empty() && digits.len() <= 2 && digits.chars().all(|c| c.is_ascii_digit())
    })
}

/// Request dialect — picks which OpenAI quirks the provider applies.
///
/// `Official` targets `api.openai.com` and honours the model-family
/// rules that the real OpenAI Chat Completions endpoint enforces:
/// `max_completion_tokens` for gpt-5 / o-series, model-family output
/// caps, omitted `temperature` for reasoning models, strict-mode JSON
/// schema normalisation.
///
/// `Compat` targets the OpenAI-compatible wire format spoken by
/// Ollama, vLLM, LM Studio, llama.cpp, and the long tail of local /
/// proxy backends. Those backends almost universally implement the
/// legacy `max_tokens` dialect, ignore OpenAI-specific output caps,
/// and accept any temperature value — so we keep the request shape
/// stable and let the engine clamp / coerce as it sees fit. Forcing
/// the official dialect onto compat backends would break working
/// Ollama / vLLM setups (issue raised in PR review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDialect {
    /// Official `api.openai.com`. Apply per-model quirks.
    Official,
    /// Local / proxy `openai-compat` (Ollama, vLLM, LM Studio, …).
    /// Legacy `max_tokens` only, no caps, no temperature massaging.
    Compat,
}

/// OpenAI Chat Completions-backed provider.
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
    dialect: RequestDialect,
    timeout: Duration,
    reasoning_effort: Option<ReasoningEffort>,
    /// Operator-configured headers from `AI_MEMORY_LLM_HEADERS`. Applied
    /// last, so an explicit entry wins over any default below.
    extra_headers: ExtraHeaders,
    /// Caller-identifying defaults a provider opts into. Layered *under*
    /// `extra_headers`.
    client_headers: Option<ClientHeaders>,
}

/// Defaults for a provider whose gateway wants the caller identified: an
/// agent string, and the header name to report the logical operation under.
#[derive(Debug, Clone, Copy)]
struct ClientHeaders {
    user_agent: &'static str,
    operation_id: &'static str,
}

impl OpenAiProvider {
    /// Construct a provider given an API key + model id. Defaults to
    /// the `Official` dialect (targeting `api.openai.com`). Override
    /// with [`with_dialect`] when wrapping for `openai-compat`.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let client = reqwest::Client::builder().build()?;
        Ok(Self {
            client,
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            model: model.into(),
            dialect: RequestDialect::Official,
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
            client_headers: None,
        })
    }

    /// Override the API base URL (tests; or pointing at an
    /// OpenAI-compatible mirror).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Override the per-request timeout (default
    /// [`crate::DEFAULT_REQUEST_TIMEOUT_SECS`]). Applied per request,
    /// so the HTTP client itself stays connection-pool friendly.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    /// Currently configured per-request timeout. Test-visible so
    /// wrapper tests (`OpenAiCompatProvider`, `OpenCodeProvider`)
    /// can assert the delegation without exposing the field.
    #[cfg(test)]
    pub(crate) fn request_timeout(&self) -> Duration {
        self.timeout
    }

    /// Switch request dialect. See [`RequestDialect`].
    #[must_use]
    pub fn with_dialect(mut self, dialect: RequestDialect) -> Self {
        self.dialect = dialect;
        self
    }

    /// Set reasoning effort. `None` omits the field so the model default
    /// applies. Official Chat Completions send `reasoning_effort`; OpenRouter
    /// and xAI hosts use their native shapes.
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

    /// Opt this provider into caller-identifying defaults: `user_agent` as
    /// the agent string, and `operation_id` as the header *name* to report
    /// each logical operation under. Both are defaults — an
    /// `AI_MEMORY_LLM_HEADERS` entry for either name takes precedence.
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

    /// Endpoint the client will call. Test-visible so wrappers that default
    /// the base URL and let it be overridden can assert which one is set.
    #[cfg(test)]
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[derive(Debug, Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAiMsg<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenAiResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAiReasoning>,
}

#[derive(Debug, Serialize)]
struct OpenAiReasoning {
    effort: ReasoningEffort,
    /// OpenRouter: keep thinking tokens out of `message.content` so
    /// structured-output parse is not polluted.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    exclude: bool,
}

#[derive(Debug, Serialize)]
struct OpenAiMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiResponseFormat {
    JsonSchema { json_schema: OpenAiJsonSchema },
}

#[derive(Debug, Serialize)]
struct OpenAiJsonSchema {
    name: String,
    schema: serde_json::Value,
    strict: bool,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
    model: String,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessageResponse,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessageResponse {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
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
        let response = self
            .post(&self.build_request(&request, None), operation_id)
            .await?;
        Ok(self.to_chat_response(response))
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
        self.complete_structured(request, schema, operation_id)
            .await
    }
}

impl OpenAiProvider {
    async fn complete_structured(
        &self,
        request: ChatRequest,
        mut schema: serde_json::Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<serde_json::Value> {
        // Strict-mode normalisation is an `Official` concern — compat
        // backends typically ignore `response_format` entirely and fall
        // back to "parse the first JSON object out of the text".
        if self.dialect == RequestDialect::Official {
            enforce_strict_object_schemas(&mut schema);
        }
        let response_format = OpenAiResponseFormat::JsonSchema {
            json_schema: OpenAiJsonSchema {
                name: STRUCTURED_OUTPUT_SCHEMA_NAME.into(),
                schema,
                strict: true,
            },
        };
        let response = self
            .post(
                &self.build_request(&request, Some(response_format)),
                operation_id,
            )
            .await?;
        let text = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .unwrap_or("");
        serde_json::from_str::<serde_json::Value>(text).map_err(LlmError::from)
    }

    fn build_request<'a>(
        &'a self,
        request: &'a ChatRequest,
        response_format: Option<OpenAiResponseFormat>,
    ) -> OpenAiRequest<'a> {
        let mut messages: Vec<OpenAiMsg<'a>> = Vec::new();
        if let Some(sys) = request.system.as_deref() {
            messages.push(OpenAiMsg {
                role: "system",
                content: sys,
            });
        }
        for m in &request.messages {
            messages.push(OpenAiMsg {
                role: m.role.as_str(),
                content: &m.content,
            });
        }
        // `Compat` backends (Ollama, vLLM, LM Studio, …) speak the
        // legacy OpenAI wire format only: always `max_tokens`, never
        // OpenAI-side caps, never temperature-omission. The engine
        // itself clamps oversized requests; forcing the official
        // dialect onto them is the regression Akita flagged in review.
        let (max_tokens, max_completion_tokens, temperature) = match self.dialect {
            RequestDialect::Compat => (Some(request.max_tokens), None, request.temperature),
            RequestDialect::Official => {
                let capped = request.max_tokens.min(max_output_tokens_for(&self.model));
                let (mt, mct) = if model_requires_max_completion_tokens(&self.model) {
                    (None, Some(capped))
                } else {
                    (Some(capped), None)
                };
                // gpt-5 and o-series reject any non-default temperature
                // with `Unsupported value: temperature does not support
                // 0.2 with this model. Only the default (1) is
                // supported.` The lint / consolidate / bootstrap call
                // sites all pass 0.1-0.2; omit the field entirely so
                // the API uses its model-specific default.
                let temp = if model_requires_default_temperature(&self.model) {
                    None
                } else {
                    request.temperature
                };
                (mt, mct, temp)
            }
        };
        let (reasoning_effort, reasoning) = self.chat_reasoning_fields();
        OpenAiRequest {
            model: &self.model,
            messages,
            max_tokens,
            max_completion_tokens,
            temperature,
            response_format,
            reasoning_effort,
            reasoning,
        }
    }

    /// Native reasoning payload for this host / dialect.
    ///
    /// Official OpenAI and generic openai-compat send top-level
    /// `reasoning_effort`. OpenRouter's Chat Completions docs use
    /// `reasoning: { effort, exclude }`. xAI Grok Chat Completions uses
    /// `reasoning_effort` with a clamped value set (cannot disable).
    fn chat_reasoning_fields(&self) -> (Option<ReasoningEffort>, Option<OpenAiReasoning>) {
        self.reasoning_effort
            .map(|effort| ReasoningHost::detect(self.dialect, &self.base_url).fields(effort))
            .unwrap_or((None, None))
    }

    fn to_chat_response(&self, response: OpenAiResponse) -> ChatResponse {
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

    async fn post<B: Serialize>(
        &self,
        body: &B,
        operation_id: LlmOperationId,
    ) -> LlmResult<OpenAiResponse> {
        let url = normalize_openai_base(&self.base_url, "chat/completions");
        debug!(url, "POST openai");
        let builder = self
            .client
            .post(&url)
            .timeout(self.timeout)
            .bearer_auth(self.api_key.expose_secret())
            .header("content-type", "application/json");
        // Both header layers go on in one `apply`, which replaces per name.
        // Adding them separately with `RequestBuilder::header` would *append*
        // — two `user-agent` values whenever the operator configures one, and
        // a duplicate is worse than either value alone. `set_default` leaves
        // an operator entry untouched, so the layering is explicit here
        // rather than dependent on call order.
        let request = match self.client_headers {
            None => self.extra_headers.apply(builder),
            Some(client) => {
                let mut headers = self.extra_headers.clone();
                headers.set_default(
                    reqwest::header::USER_AGENT,
                    HeaderValue::from_static(client.user_agent),
                );
                // Both are our own constants, so an invalid name is a bug to
                // surface in the first test run, not a runtime branch.
                let name = HeaderName::from_static(client.operation_id);
                // A UUID is always a valid header value; on the impossible
                // failure, omitting a correlation header beats failing the
                // consolidation pass that carries it.
                if let Ok(value) = HeaderValue::from_str(&operation_id.to_string()) {
                    headers.set_default(name, value);
                }
                headers.apply(builder)
            }
        };
        let resp = request.json(body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        response_json_limited::<OpenAiResponse>(resp).await
    }
}

/// Recursively normalise a JSON schema for OpenAI Structured Outputs
/// (`strict: true`). The endpoint rejects schemas missing either:
///
/// 1. `additionalProperties: false` on every object node — without it:
///    `'additionalProperties' is required to be supplied and to be false`.
///
/// 2. `required` listing **every** key in `properties` (strict mode does
///    not support optional fields; callers that need optionality express
///    it via a nullable type instead, e.g. `["string", "null"]`). Without
///    a complete `required` array: `'required' is required to be supplied
///    and to be an array including every key in properties`.
///
/// Both rules are unconditional here: this normalisation only runs on
/// the `Official` request dialect, which targets `api.openai.com`
/// where strict mode is mandatory. Any caller-supplied
/// `additionalProperties: true` or trimmed `required` array is
/// overwritten — preserving them would let invalid schemas through
/// and re-introduce the 400 this function exists to prevent. Callers
/// that need looser schemas should use the `Compat` dialect (which
/// skips this normalisation entirely) or a non-strict path.
pub(crate) fn enforce_strict_object_schemas(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // OpenAI's structured-output subset rejects any sibling
            // keyword next to `$ref` with a 400. schemars 1.x emits a
            // field-level `description` next to `$ref` for doc-commented
            // fields typed as external enums (e.g. `tier: Tier` on
            // `ConsolidatedPageUpdate`); without this strip,
            // `memory_consolidate multi_page=true` fails before the model
            // runs. In our generated schemas those siblings are
            // annotations, not validation constraints, so the referenced
            // definition remains the source of truth.
            if map.contains_key("$ref") {
                map.retain(|k, _| k == "$ref");
                return;
            }
            // OpenAI strict mode rejects `oneOf` outright but accepts
            // `anyOf`. schemars 1.x emits `oneOf` for closed Rust enums
            // such as `Tier` and `PageKind` under `$defs` in
            // `ConsolidatedBatch`; their const branches are disjoint, so
            // the rewrite preserves the generated schema's accepted set.
            if let Some(one_of) = map.remove("oneOf") {
                map.insert("anyOf".to_string(), one_of);
            }
            let is_object = map
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t == "object")
                || map.contains_key("properties");
            if is_object {
                // Force-set both: a caller-supplied `true` would defeat
                // the entire purpose of the strict-mode normalisation.
                map.insert("additionalProperties".to_string(), serde_json::json!(false));
                // OpenAI strict mode rejects ANY incomplete `required` —
                // even an explicit subset. The only way to express
                // optionality is via a nullable type at the value site
                // (e.g. `["string", "null"]`). Overwrite unconditionally
                // when `properties` is present so a caller-supplied
                // partial list doesn't sneak through.
                if let Some(props) = map.get("properties").and_then(|p| p.as_object()) {
                    let keys: Vec<serde_json::Value> =
                        props.keys().map(|k| serde_json::json!(k)).collect();
                    map.insert("required".to_string(), serde_json::Value::Array(keys));
                }
            }
            for (_, v) in map.iter_mut() {
                enforce_strict_object_schemas(v);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                enforce_strict_object_schemas(v);
            }
        }
        _ => {}
    }
}

/// Models that require `max_completion_tokens` instead of `max_tokens`.
/// OpenAI introduced this rename starting with the reasoning-capable o1
/// family and made it mandatory across the gpt-5 line. Sending the legacy
/// `max_tokens` to these models returns a 400 with
/// `Unsupported parameter: 'max_tokens'`.
fn model_requires_max_completion_tokens(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("gpt-5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

/// Models that reject any non-default `temperature` value.
///
/// gpt-5 and the o-series reasoning models accept only the model
/// default (1.0). Any caller-supplied value — including the 0.1-0.2
/// passed by lint / bootstrap / consolidation — returns a 400:
/// `Unsupported value: 'temperature' does not support 0.2 with this
/// model. Only the default (1) is supported.` Omitting the field
/// entirely lets the API apply its own default and unblocks those
/// models without forcing every call site to be model-aware.
fn model_requires_default_temperature(model: &str) -> bool {
    // Same family as `max_completion_tokens` — keep aligned: any future
    // family that adopts the new rename also tends to lock temperature.
    model_requires_max_completion_tokens(model)
}

fn is_openrouter_base(url: &str) -> bool {
    url.to_ascii_lowercase().contains("openrouter.ai")
}

fn is_xai_base(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("api.x.ai") || lower.contains("://x.ai/") || lower.contains(".x.ai/")
}

/// Which reasoning object this Chat Completions host expects.
#[derive(Debug, Clone, Copy)]
enum ReasoningHost {
    OpenAi,
    OpenRouter,
    Grok,
}

impl ReasoningHost {
    fn detect(dialect: RequestDialect, base_url: &str) -> Self {
        match dialect {
            RequestDialect::Compat if is_openrouter_base(base_url) => Self::OpenRouter,
            RequestDialect::Compat if is_xai_base(base_url) => Self::Grok,
            RequestDialect::Official | RequestDialect::Compat => Self::OpenAi,
        }
    }

    fn fields(self, effort: ReasoningEffort) -> (Option<ReasoningEffort>, Option<OpenAiReasoning>) {
        match self {
            Self::OpenRouter => (
                None,
                Some(OpenAiReasoning {
                    effort: effort.openai_wire_effort(),
                    exclude: true,
                }),
            ),
            Self::Grok => (Some(effort.grok_chat_effort()), None),
            Self::OpenAi => (Some(effort.openai_wire_effort()), None),
        }
    }
}

/// Per-model output-token ceiling for the `Official` dialect.
///
/// OpenAI rejects requests above the model's published limit with
/// `400 max_tokens is too large`, instead of silently truncating.
/// Callers (e.g. bootstrap) deliberately ask for very large budgets
/// (64K) so Anthropic / Haiku-class models don't truncate mid-JSON;
/// the same request blows up on gpt-4o-mini without this defensive
/// cap. The cap is informed but conservative: gpt-4-turbo's real
/// limit is 4096 (smaller than what we use here), so a max-budget
/// bootstrap call to gpt-4-turbo will still 400 with the same
/// model-specific message — at which point the operator can lower
/// `max_tokens` or switch model. The cap exists to unblock the
/// common case (gpt-4o family at 16384), not to paper over every
/// model. Reasoning models in the gpt-5 / o-series have much larger
/// caps (128K+), so we leave their requests untouched.
fn max_output_tokens_for(model: &str) -> u32 {
    if model_requires_max_completion_tokens(model) {
        // gpt-5 / o-series: documented at 128K output. Leave the
        // caller's value alone — they know what they're asking for.
        u32::MAX
    } else {
        // gpt-4o family published cap. gpt-4-turbo / gpt-3.5 have a
        // lower cap (4096) and will still 400 — this is intentional;
        // they're outside the strict-mode target audience.
        16_384
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OpenAiProvider, RequestDialect, enforce_strict_object_schemas,
        model_requires_max_completion_tokens, normalize_openai_base,
    };
    use crate::types::{ChatMessage, ChatRequest, ReasoningEffort, Role};
    use rstest::rstest;
    use schemars::JsonSchema;
    use secrecy::SecretString;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    fn provider_for(model: &str) -> OpenAiProvider {
        OpenAiProvider::new(SecretString::new("test-key".into()), model).unwrap()
    }

    #[test]
    fn request_timeout_defaults_and_is_overridable() {
        let provider = provider_for("gpt-4o-mini");
        assert_eq!(
            provider.timeout,
            std::time::Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS)
        );
        let provider = provider.with_timeout_secs(900);
        assert_eq!(provider.timeout, std::time::Duration::from_secs(900));
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "hi".to_string(),
            }],
            max_tokens: 256,
            temperature: None,
        }
    }

    #[test]
    fn enforce_strict_injects_additional_properties_false_on_root() {
        let mut schema = json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } },
            "required": ["summary"]
        });
        enforce_strict_object_schemas(&mut schema);
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn enforce_strict_recurses_into_nested_objects() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "page": {
                    "type": "object",
                    "properties": { "title": { "type": "string" } }
                },
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "name": { "type": "string" } }
                    }
                }
            }
        });
        enforce_strict_object_schemas(&mut schema);
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["page"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            schema["properties"]["tags"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn enforce_strict_fills_required_with_all_property_keys() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "body": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } }
            }
        });
        enforce_strict_object_schemas(&mut schema);
        let required = schema["required"].as_array().expect("required is array");
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"title"));
        assert!(names.contains(&"body"));
        assert!(names.contains(&"tags"));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn enforce_strict_overwrites_incomplete_required() {
        // OpenAI strict mode rejects partial `required` arrays — even an
        // explicit subset from the caller. Optionality at the value site
        // (nullable union types) is the only supported escape hatch.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "a": { "type": "string" },
                "b": { "type": "string" }
            },
            "required": ["a"]
        });
        enforce_strict_object_schemas(&mut schema);
        let required = schema["required"].as_array().expect("required is array");
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn enforce_strict_strips_sibling_keywords_next_to_ref() {
        // OpenAI's structured-output validator rejects any sibling
        // keyword next to `$ref` with a 400. schemars 1.x emits a
        // field-level `description` next to `$ref` whenever a
        // doc-commented field is typed as an external enum (e.g.
        // `tier: Tier` in `ConsolidatedPageUpdate`). Without this
        // strip, `memory_consolidate multi_page=true` 400s on every
        // call against gpt-4o-mini. In our generated schemas those
        // siblings are annotations, not validation constraints.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "tier": {
                    "$ref": "#/$defs/Tier",
                    "description": "Tier classification."
                }
            }
        });
        enforce_strict_object_schemas(&mut schema);
        let tier = &schema["properties"]["tier"];
        assert_eq!(tier["$ref"], json!("#/$defs/Tier"));
        assert!(
            tier.get("description").is_none(),
            "description must be stripped from a $ref node"
        );
        assert_eq!(
            tier.as_object().unwrap().len(),
            1,
            "only $ref should remain on the node"
        );
    }

    #[test]
    fn enforce_strict_renames_oneof_to_anyof() {
        // OpenAI structured-output strict mode rejects `oneOf` outright
        // ("In context=(), 'oneOf' is not permitted") while accepting
        // `anyOf`. schemars 1.x emits `oneOf` for every Rust enum with
        // tagged variants — e.g. the `Tier` and `PageKind` enums under
        // `$defs` in `ConsolidatedBatch`. For closed enum sets where
        // exactly one branch matches per value, `anyOf` is semantically
        // equivalent (no two branches overlap), so the rewrite is
        // lossless.
        let mut schema = json!({
            "type": "object",
            "$defs": {
                "Tier": {
                    "oneOf": [
                        { "type": "string", "const": "working" },
                        { "type": "string", "const": "episodic" }
                    ]
                }
            },
            "properties": { "tier": { "$ref": "#/$defs/Tier" } }
        });
        enforce_strict_object_schemas(&mut schema);
        let tier_def = &schema["$defs"]["Tier"];
        assert!(
            tier_def.get("oneOf").is_none(),
            "oneOf must be rewritten away"
        );
        let any_of = tier_def.get("anyOf").expect("oneOf must become anyOf");
        assert_eq!(any_of.as_array().unwrap().len(), 2);
    }

    #[test]
    fn enforce_strict_normalizes_schemars_enum_refs() {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        struct Fixture {
            /// Doc-commented enum field, matching the schemars shape that
            /// triggered OpenAI's `$ref` sibling rejection.
            tier: FixtureTier,
            /// Array of the same enum, covering `$ref` under `items`.
            tiers: Vec<FixtureTier>,
        }

        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        enum FixtureTier {
            Working,
            Episodic,
        }

        let mut schema = serde_json::to_value(schemars::schema_for!(Fixture)).unwrap();
        enforce_strict_object_schemas(&mut schema);

        assert_no_one_of(&schema);
        assert_no_ref_siblings(&schema);
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn enforce_strict_strips_ref_siblings_inside_array_items() {
        // OpenAI applies the same `$ref` sibling restriction anywhere a
        // ref appears, not just on direct object properties. schemars
        // emits this shape inside `items` for `Vec<EnumType>` too.
        let mut schema = json!({
            "type": "array",
            "items": {
                "$ref": "#/$defs/Tier",
                "description": "Each element classified."
            }
        });
        enforce_strict_object_schemas(&mut schema);
        let items = &schema["items"];
        assert_eq!(items["$ref"], json!("#/$defs/Tier"));
        assert!(items.get("description").is_none());
    }

    fn assert_no_one_of(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                assert!(map.get("oneOf").is_none(), "oneOf remains in {value}");
                for child in map.values() {
                    assert_no_one_of(child);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    assert_no_one_of(child);
                }
            }
            _ => {}
        }
    }

    fn assert_no_ref_siblings(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                if map.contains_key("$ref") {
                    assert_eq!(map.len(), 1, "$ref has siblings in {value}");
                }
                for child in map.values() {
                    assert_no_ref_siblings(child);
                }
            }
            serde_json::Value::Array(items) => {
                for child in items {
                    assert_no_ref_siblings(child);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn enforce_strict_overwrites_caller_additional_properties_true() {
        // OpenAI strict mode requires `additionalProperties: false` on
        // every object node — preserving an explicit `true` would
        // re-introduce the 400 this function exists to prevent. The
        // PR-review version of this test had the opposite assertion
        // and was incompatible with the function's own contract.
        let mut schema = json!({
            "type": "object",
            "properties": { "anything": { "type": "string" } },
            "additionalProperties": true
        });
        enforce_strict_object_schemas(&mut schema);
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "strict mode requires false; caller's true must be overwritten"
        );
    }

    #[test]
    fn enforce_strict_ignores_non_object_nodes() {
        let mut schema = json!({ "type": "string" });
        enforce_strict_object_schemas(&mut schema);
        assert!(schema.get("additionalProperties").is_none());
    }

    #[test]
    fn model_requires_max_completion_tokens_matches_gpt5_and_o_series() {
        assert!(model_requires_max_completion_tokens("gpt-5"));
        assert!(model_requires_max_completion_tokens("gpt-5-mini"));
        assert!(model_requires_max_completion_tokens("gpt-5.4-nano"));
        assert!(model_requires_max_completion_tokens("GPT-5"));
        assert!(model_requires_max_completion_tokens("o1-mini"));
        assert!(model_requires_max_completion_tokens("o3"));
        assert!(model_requires_max_completion_tokens("o4-mini"));
    }

    #[test]
    fn model_requires_max_completion_tokens_passes_gpt4_through() {
        assert!(!model_requires_max_completion_tokens("gpt-4o-mini"));
        assert!(!model_requires_max_completion_tokens("gpt-4-turbo"));
        assert!(!model_requires_max_completion_tokens("gpt-3.5-turbo"));
        assert!(!model_requires_max_completion_tokens("claude-haiku-4-5"));
    }

    #[test]
    fn build_request_uses_max_tokens_for_gpt4() {
        let p = provider_for("gpt-4o-mini");
        let req_input = chat_request();
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_tokens"], json!(256));
        assert!(json.get("max_completion_tokens").is_none());
    }

    #[test]
    fn build_request_uses_max_completion_tokens_for_gpt5() {
        let p = provider_for("gpt-5.4-nano");
        let req_input = chat_request();
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_completion_tokens"], json!(256));
        assert!(json.get("max_tokens").is_none());
    }

    #[test]
    fn build_request_caps_huge_max_tokens_on_gpt4o() {
        // Bootstrap requests 64K output to avoid mid-JSON truncation on
        // Anthropic Haiku-class models. OpenAI gpt-4o family caps at
        // 16384 and rejects above; cap silently so the caller doesn't
        // need to know per-model limits.
        let p = provider_for("gpt-4o-mini");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 64_000,
            temperature: None,
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_tokens"], json!(16_384));
    }

    #[test]
    fn build_request_omits_temperature_for_gpt5() {
        // gpt-5 / o-series reject any non-default temperature. The
        // `Official` dialect must omit the field so the API uses its
        // model-specific default.
        let p = provider_for("gpt-5.4-nano");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 256,
            temperature: Some(0.2),
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert!(
            json.get("temperature").is_none(),
            "temperature must be omitted for gpt-5/o-series under the Official dialect"
        );
    }

    #[test]
    fn build_request_keeps_temperature_for_gpt4() {
        // gpt-4 family accepts any temperature; forwarding the
        // caller's value is the legacy behaviour and stays.
        let p = provider_for("gpt-4o-mini");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 256,
            temperature: Some(0.2),
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        let temp = json["temperature"].as_f64().unwrap();
        assert!(
            (temp - 0.2).abs() < 1e-6,
            "temperature must be ~0.2, got {temp}"
        );
    }

    #[test]
    fn build_request_compat_dialect_keeps_max_tokens_and_temperature() {
        // `Compat` (Ollama / vLLM / LM Studio) speaks the legacy
        // wire format only — even when the model id starts with
        // `gpt-5*`, because the local engine doesn't implement the
        // new dialect. Akita flagged this regression in PR review.
        let p = OpenAiProvider::new(SecretString::new("dummy".into()), "gpt-5-mini")
            .unwrap()
            .with_dialect(RequestDialect::Compat);
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 64_000,
            temperature: Some(0.2),
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json["max_tokens"],
            json!(64_000),
            "compat dialect must use legacy max_tokens, uncapped"
        );
        assert!(
            json.get("max_completion_tokens").is_none(),
            "compat dialect must not emit max_completion_tokens"
        );
        let temp = json["temperature"].as_f64().unwrap();
        assert!(
            (temp - 0.2).abs() < 1e-6,
            "compat dialect must forward temperature unchanged, got {temp}"
        );
    }

    #[test]
    fn build_request_does_not_cap_gpt5() {
        // Reasoning models have a much larger output cap (128K+); leave
        // the caller's value alone.
        let p = provider_for("gpt-5.4-nano");
        let req_input = ChatRequest {
            system: None,
            messages: vec![ChatMessage {
                role: Role::User,
                content: "x".into(),
            }],
            max_tokens: 64_000,
            temperature: None,
        };
        let req = p.build_request(&req_input, None);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["max_completion_tokens"], json!(64_000));
    }

    fn chat_reasoning_json(
        dialect: RequestDialect,
        base_url: Option<&str>,
        model: &str,
        effort: Option<ReasoningEffort>,
    ) -> serde_json::Value {
        let mut provider = OpenAiProvider::new(SecretString::new("test-key".into()), model)
            .unwrap()
            .with_dialect(dialect)
            .with_reasoning_effort(effort);
        if let Some(url) = base_url {
            provider = provider.with_base_url(url);
        }
        serde_json::to_value(provider.build_request(&chat_request(), None)).unwrap()
    }

    #[rstest]
    #[case::official_low(
        RequestDialect::Official,
        None,
        "gpt-5.4-mini",
        Some(ReasoningEffort::Low),
        Some("low"),
        None
    )]
    #[case::official_unset(RequestDialect::Official, None, "gpt-5.4-mini", None, None, None)]
    #[case::openrouter_excludes_content(
        RequestDialect::Compat,
        Some("https://openrouter.ai/api/v1"),
        "anthropic/claude-sonnet-4.6",
        Some(ReasoningEffort::High),
        None,
        Some("high")
    )]
    #[case::xai_none_clamps_low(
        RequestDialect::Compat,
        Some("https://api.x.ai/v1"),
        "grok-4.6",
        Some(ReasoningEffort::None),
        Some("low"),
        None
    )]
    #[case::official_ultra_clamps_max(
        RequestDialect::Official,
        None,
        "gpt-5.4-mini",
        Some(ReasoningEffort::Ultra),
        Some("max"),
        None
    )]
    fn chat_request_uses_native_reasoning_shape(
        #[case] dialect: RequestDialect,
        #[case] base_url: Option<&str>,
        #[case] model: &str,
        #[case] effort: Option<ReasoningEffort>,
        #[case] reasoning_effort: Option<&str>,
        #[case] reasoning_object: Option<&str>,
    ) {
        let json = chat_reasoning_json(dialect, base_url, model, effort);
        match reasoning_effort {
            Some(expected) => assert_eq!(json["reasoning_effort"], json!(expected)),
            None => assert!(json.get("reasoning_effort").is_none()),
        }
        match reasoning_object {
            Some(expected) => {
                assert_eq!(json["reasoning"]["effort"], json!(expected));
                assert_eq!(json["reasoning"]["exclude"], json!(true));
            }
            None => assert!(json.get("reasoning").is_none()),
        }
    }

    #[test]
    fn normalize_openai_base_chat_completions() {
        let ep = "chat/completions";

        assert_eq!(
            normalize_openai_base("https://api.openai.com", ep),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("https://api.openai.com/", ep),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("https://openrouter.ai/api/v1", ep),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("http://localhost:11434/v1", ep),
            "http://localhost:11434/v1/chat/completions"
        );
        // /v123 must not be treated as a version segment.
        assert_eq!(
            normalize_openai_base("https://example.com/v123", ep),
            "https://example.com/v123/v1/chat/completions"
        );
        // Z.AI-style: non-v1 version segment in the path.
        assert_eq!(
            normalize_openai_base("https://api.z.ai/api/coding/paas/v4", ep),
            "https://api.z.ai/api/coding/paas/v4/chat/completions"
        );
        // Full endpoint URL already provided (Z.AI or GitHub Copilot style).
        assert_eq!(
            normalize_openai_base("https://api.z.ai/api/coding/paas/v4/chat/completions", ep),
            "https://api.z.ai/api/coding/paas/v4/chat/completions"
        );
        assert_eq!(
            normalize_openai_base("https://api.githubcopilot.com/chat/completions", ep),
            "https://api.githubcopilot.com/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_base_embeddings() {
        let ep = "embeddings";

        assert_eq!(
            normalize_openai_base("https://api.openai.com", ep),
            "https://api.openai.com/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("https://openrouter.ai/api/v1", ep),
            "https://openrouter.ai/api/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("http://localhost:11434/v1", ep),
            "http://localhost:11434/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("https://example.com/v123", ep),
            "https://example.com/v123/v1/embeddings"
        );
        assert_eq!(
            normalize_openai_base("https://api.z.ai/api/coding/paas/v4", ep),
            "https://api.z.ai/api/coding/paas/v4/embeddings"
        );
    }
}
