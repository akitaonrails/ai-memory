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
use reqwest::header::{HeaderName, HeaderValue};
use secrecy::SecretString;

use crate::error::LlmResult;
use crate::openai_compat::OpenAiCompatProvider;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, ExtraHeaders};

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
    /// Sent as `x-opencode-session` unless the operator configured that
    /// header. Generated once per provider instance — the provider is built
    /// once per `serve` process, so a run's requests correlate to one id,
    /// which is what OpenCode's metrics key on.
    session: HeaderValue,
}

impl OpenCodeProvider {
    /// Construct an OpenCode provider against Go, the default endpoint.
    /// Call [`Self::with_base_url`] for Zen.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let session = new_session_id();
        let inner = OpenAiCompatProvider::new(OPENCODE_GO_BASE_URL, Some(api_key), model.into())?
            .with_extra_headers(with_session_default(&session, ExtraHeaders::default()));
        Ok(Self { inner, session })
    }

    /// Point the provider at a different OpenCode endpoint — Zen's general
    /// catalogue (`https://opencode.ai/zen/v1`) instead of Go's default.
    ///
    /// The factory calls this with `ProviderConfig::base_url`
    /// (`AI_MEMORY_LLM_BASE_URL`). The session header and user agent are
    /// unaffected: both endpoints correlate requests the same way, so the
    /// override changes where requests go, not how they identify themselves.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.inner = self.inner.with_base_url(url);
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

    /// Forward operator-configured headers, keeping this provider's session
    /// default when the operator left `x-opencode-session` unset.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        let headers = with_session_default(&self.session, headers);
        self.inner = self.inner.with_extra_headers(headers);
        self
    }
}

/// A fresh session id. Prefixed so the value is self-describing in OpenCode's
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

    /// Overriding the endpoint must not cost the caller its identity: both
    /// endpoints correlate by the same header.
    #[test]
    fn a_zen_override_keeps_the_session_header() {
        let provider = OpenCodeProvider::new(SecretString::from("sk-test"), "model-x")
            .unwrap()
            .with_base_url("https://opencode.ai/zen/v1");
        assert!(
            provider
                .inner
                .extra_headers()
                .get(OPENCODE_SESSION_HEADER)
                .is_some_and(|v| v.starts_with("ai-memory-")),
            "session header lost when the base URL was overridden"
        );
    }

    /// OpenCode reports a request without this header as unattributable, so a
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
