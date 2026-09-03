//! OpenCode provider — Go by default, Zen on request.
//!
//! Thin wrapper around [`OpenAiCompatProvider`] that names the provider
//! `"opencode"` and accepts an `sk-...` API key from `OPENCODE_API_KEY`.
//!
//! OpenCode serves two endpoints, and they are different products rather
//! than two names for one: **Go** ([`OPENCODE_GO_BASE_URL`]) is a smaller,
//! cost-optimised model set, while **Zen** (`https://opencode.ai/zen/v1`)
//! serves the full catalogue. Go is the default here; an operator selects
//! Zen with `AI_MEMORY_LLM_BASE_URL`, which the factory passes to
//! [`OpenCodeProvider::with_base_url`]. Model ids are per catalogue, so an
//! override wants an explicit `AI_MEMORY_LLM_MODEL` too.
//!
//! Go's documentation covers third-party access and asks every tool using
//! it to (1) not generate abusive traffic and (2) "properly identify itself
//! (no broad user agents)". The agent string is
//! [`crate::DEFAULT_USER_AGENT`], applied to every provider; what is
//! specific to this one is `x-opencode-session`, which both endpoints
//! correlate requests by and which this provider defaults under anything
//! the operator set through `AI_MEMORY_LLM_HEADERS`.

use async_trait::async_trait;
use secrecy::SecretString;

use crate::error::LlmResult;
use crate::openai_compat::OpenAiCompatProvider;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, ExtraHeaders, LlmOperationId};

/// Public OpenCode **Go** OpenAI-compatible base URL, and this provider's
/// default endpoint.
///
/// Go is a distinct product from Zen's general catalogue, not another
/// spelling of it: Go serves a smaller, cost-optimised model set under
/// `zen/go/v1`, while Zen serves the full catalogue at
/// `https://opencode.ai/zen/v1`. Both authenticate with the same
/// `OPENCODE_API_KEY` and both correlate requests by
/// [`OPENCODE_SESSION_HEADER`], so reaching Zen is a base-URL override —
/// see [`OpenCodeProvider::with_base_url`].
pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Misnomer for [`OPENCODE_GO_BASE_URL`], kept so the released public API
/// keeps compiling: the value has always been Go's endpoint, never Zen's
/// general one.
#[deprecated(
    since = "2.1.0",
    note = "names Zen but holds Go's endpoint; use OPENCODE_GO_BASE_URL"
)]
pub const OPENCODE_ZEN_BASE_URL: &str = OPENCODE_GO_BASE_URL;

/// Default model when `AI_MEMORY_LLM_MODEL` is not set. Taken from Go's
/// catalogue, so an operator overriding the base URL should set the model
/// explicitly rather than assume this id exists on the other endpoint.
pub const OPENCODE_DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Session-correlation header OpenCode asks callers to send, on Zen and Go
/// alike. Requests arriving without it are reported as unattributable and
/// may be rejected.
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// OpenCode LLM provider, pointed at Go unless repointed at Zen.
///
/// Routes through `https://opencode.ai/zen/go/v1` using the OpenAI chat
/// completions wire format. Authenticate with the `sk-...` key obtained
/// from <https://opencode.ai/auth>.
pub struct OpenCodeProvider {
    inner: OpenAiCompatProvider,
}

impl OpenCodeProvider {
    /// Construct an OpenCode provider against Go, the default endpoint.
    /// Call [`Self::with_base_url`] for Zen.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        Self::new_with_base_url(api_key, model, OPENCODE_GO_BASE_URL)
    }

    /// Construct against an explicit endpoint. The session header carries a
    /// [`LlmOperationId`], so one logical operation keeps one id across
    /// retries and the strict/tolerant fallback — which is what OpenCode's
    /// metrics key on. Both values are defaults: an `AI_MEMORY_LLM_HEADERS`
    /// entry for either name wins.
    fn new_with_base_url(
        api_key: SecretString,
        model: impl Into<String>,
        base_url: impl Into<String>,
    ) -> LlmResult<Self> {
        let inner = OpenAiCompatProvider::new(base_url, Some(api_key), model.into())?
            .with_client_headers(crate::DEFAULT_USER_AGENT, OPENCODE_SESSION_HEADER);
        Ok(Self { inner })
    }

    /// Point the provider at a different OpenCode endpoint — Zen's general
    /// catalogue (`https://opencode.ai/zen/v1`) instead of Go's default.
    ///
    /// The factory calls this with `ProviderConfig::base_url`
    /// (`AI_MEMORY_LLM_BASE_URL`). Identification is unaffected: both
    /// endpoints correlate requests the same way, so the override changes
    /// where requests go, not how they identify themselves.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.inner = self.inner.with_base_url(url);
        self
    }

    #[cfg(test)]
    fn with_strict(mut self, strict: bool) -> Self {
        self.inner = self.inner.with_strict(strict);
        self
    }

    /// Override the per-request timeout on the wrapped
    /// [`OpenAiCompatProvider`]. The factory calls this with
    /// `ProviderConfig::request_timeout_secs`.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.inner = self.inner.with_timeout_secs(secs);
        self
    }

    /// Forward reasoning effort to the OpenAI-compatible OpenCode client.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<crate::ReasoningEffort>) -> Self {
        self.inner = self.inner.with_reasoning_effort(effort);
        self
    }

    /// Forward operator-configured headers. The session header and user
    /// agent this provider sets are defaults applied per request, so an
    /// `AI_MEMORY_LLM_HEADERS` entry for either name wins here.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        self.inner = self.inner.with_extra_headers(headers);
        self
    }
}

#[async_trait]
impl LlmProvider for OpenCodeProvider {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn model(&self) -> &str {
        self.inner.model()
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
        self.inner
            .complete_with_operation_id(request, operation_id)
            .await
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
        self.inner
            .complete_structured_raw_with_operation_id(request, schema, operation_id)
            .await
    }
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

    /// Go and Zen are different endpoints; the default is Go's.
    #[test]
    fn the_default_base_url_is_gos_endpoint() {
        assert_eq!(OPENCODE_GO_BASE_URL, "https://opencode.ai/zen/go/v1");
        assert!(!OPENCODE_DEFAULT_MODEL.is_empty());
    }

    /// The deprecated name kept its value, so code compiled against v2.0
    /// keeps hitting the endpoint it already hit.
    #[test]
    #[allow(deprecated)]
    fn the_deprecated_alias_still_resolves_to_go() {
        assert_eq!(OPENCODE_ZEN_BASE_URL, OPENCODE_GO_BASE_URL);
    }

    /// Zen's general catalogue is reachable, and is not the default — the
    /// bug this pair pins is a base URL that could not be overridden.
    #[test]
    fn with_base_url_repoints_the_provider_at_zen() {
        let go = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x").unwrap();
        assert_eq!(go.inner.base_url(), OPENCODE_GO_BASE_URL);

        let zen = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x")
            .unwrap()
            .with_base_url("https://opencode.ai/zen/v1");
        assert_eq!(zen.inner.base_url(), "https://opencode.ai/zen/v1");
    }

    #[tokio::test]
    async fn completion_identifies_ai_memory_and_its_logical_operation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_with_content("ok")))
            .mount(&server)
            .await;

        let provider = OpenCodeProvider::new_with_base_url(
            SecretString::from("sk-test"),
            "model-x",
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
