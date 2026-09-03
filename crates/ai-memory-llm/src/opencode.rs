//! OpenCode Zen/Go provider.
//!
//! Thin wrapper around [`OpenAiCompatProvider`] that bakes in the OpenCode
//! Zen API base URL (`https://opencode.ai/zen/go/v1`) and names the provider
//! `"opencode"`. Accepts an `sk-...` API key from `OPENCODE_API_KEY`.
//!
//! Zen/Go documents third-party access to these endpoints and asks every
//! tool using them to (1) not generate abusive traffic and (2) "properly
//! identify itself (no broad user agents)". The agent string is
//! [`crate::DEFAULT_USER_AGENT`], applied to every provider; what is
//! specific to this one is `x-opencode-session`, which Zen/Go correlates
//! requests by and which this provider defaults under anything the operator
//! set through `AI_MEMORY_LLM_HEADERS`.

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use secrecy::SecretString;

use crate::error::LlmResult;
use crate::openai_compat::OpenAiCompatProvider;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, ExtraHeaders};

/// Public OpenCode Zen/Go OpenAI-compatible base URL.
pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Default model when `AI_MEMORY_LLM_MODEL` is not set.
pub const OPENCODE_DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Session-correlation header Zen/Go asks callers to send. Requests arriving
/// without it are reported as unattributable and may be rejected.
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// OpenCode Zen/Go LLM provider.
///
/// Routes through `https://opencode.ai/zen/go/v1` using the OpenAI chat
/// completions wire format. Authenticate with the `sk-...` key obtained
/// from <https://opencode.ai/auth>.
pub struct OpenCodeProvider {
    inner: OpenAiCompatProvider,
    /// Sent as `x-opencode-session` unless the operator configured that
    /// header. Generated once per provider instance — the provider is built
    /// once per `serve` process, so a run's requests correlate to one id,
    /// which is what Zen/Go's metrics key on.
    session: HeaderValue,
}

impl OpenCodeProvider {
    /// Construct an OpenCode Zen/Go provider.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let session = new_session_id();
        let inner = OpenAiCompatProvider::new(OPENCODE_ZEN_BASE_URL, Some(api_key), model.into())?
            .with_extra_headers(with_session_default(&session, ExtraHeaders::default()));
        Ok(Self { inner, session })
    }

    /// Override the per-request timeout on the wrapped
    /// [`OpenAiCompatProvider`]. The factory calls this with
    /// `ProviderConfig::request_timeout_secs`.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.inner = self.inner.with_timeout_secs(secs);
        self
    }

    /// Forward reasoning effort to the OpenAI-compatible Zen/Go client.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<crate::ReasoningEffort>) -> Self {
        self.inner = self.inner.with_reasoning_effort(effort);
        self
    }

    /// Forward operator-configured headers, keeping this provider's session
    /// default when the operator left `x-opencode-session` unset.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        let headers = with_session_default(&self.session, headers);
        self.inner = self.inner.with_extra_headers(headers);
        self
    }
}

/// A fresh session id. Prefixed so the value is self-describing in Zen/Go's
/// metrics rather than a bare UUID.
fn new_session_id() -> HeaderValue {
    HeaderValue::try_from(format!("ai-memory-{}", uuid::Uuid::new_v4()))
        // A UUID is always header-safe; the fallback exists so a runtime
        // path never panics on it.
        .unwrap_or_else(|_| HeaderValue::from_static("ai-memory"))
}

/// Layer the session id *under* the operator's headers, so an explicit
/// `AI_MEMORY_LLM_HEADERS` entry always wins.
fn with_session_default(session: &HeaderValue, mut headers: ExtraHeaders) -> ExtraHeaders {
    headers.set_default(
        HeaderName::from_static(OPENCODE_SESSION_HEADER),
        session.clone(),
    );
    headers
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
        self.inner.complete(request).await
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.inner.complete_structured_raw(request, schema).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Zen/Go reports a request without this header as unattributable, so a
    /// zero-config `opencode` provider must supply one.
    #[test]
    fn zero_config_provider_sends_a_session_header() {
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x").unwrap();
        assert!(
            provider
                .inner
                .extra_headers()
                .get(OPENCODE_SESSION_HEADER)
                .is_some_and(|v| v.starts_with("ai-memory-")),
            "session header missing or unprefixed"
        );
    }

    #[test]
    fn an_operator_session_header_wins_over_the_default() {
        let operator = ExtraHeaders::parse(["x-opencode-session: ses-mine"]).expect("valid");
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x")
            .unwrap()
            .with_extra_headers(operator);
        assert_eq!(
            provider.inner.extra_headers().get(OPENCODE_SESSION_HEADER),
            Some("ses-mine")
        );
    }

    /// A default is filled in per header, not per map: operator headers that
    /// say nothing about the session must not drop it.
    #[test]
    fn unrelated_operator_headers_keep_the_session_default() {
        let operator = ExtraHeaders::parse(["x-opencode-client: ai-memory"]).expect("valid");
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x")
            .unwrap()
            .with_extra_headers(operator);
        let headers = provider.inner.extra_headers();
        assert_eq!(headers.get("x-opencode-client"), Some("ai-memory"));
        assert!(headers.get(OPENCODE_SESSION_HEADER).is_some());
    }

    #[test]
    fn session_id_is_stable_across_builder_calls_on_one_instance() {
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x").unwrap();
        let before = provider
            .inner
            .extra_headers()
            .get(OPENCODE_SESSION_HEADER)
            .expect("default session")
            .to_string();
        let provider = provider.with_timeout_secs(45).with_reasoning_effort(None);
        assert_eq!(
            provider.inner.extra_headers().get(OPENCODE_SESSION_HEADER),
            Some(before.as_str())
        );
    }

    #[test]
    fn separate_instances_get_separate_session_ids() {
        let a = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x").unwrap();
        let b = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x").unwrap();
        assert_ne!(
            a.inner.extra_headers().get(OPENCODE_SESSION_HEADER),
            b.inner.extra_headers().get(OPENCODE_SESSION_HEADER)
        );
    }
}
