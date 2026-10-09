//! OpenAI-compatible client (Ollama, vLLM, LM Studio, llama.cpp).
//!
//! Uses the same wire format as [`crate::openai::OpenAiProvider`] but
//! with a configurable base URL (and no key required for most local
//! deployments). Structured output defaults to "parse first JSON object
//! out of the text" because many local engines lack reliable
//! `response_format` honour; operators can opt into strict
//! `response_format=json_schema` for engines that support it.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use secrecy::SecretString;
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::openai::{
    OpenAiProvider, RequestDialect, enforce_strict_object_schemas, is_length_truncated,
};
use crate::provider::LlmProvider;
use crate::text::{suffix_within_bytes, truncate_with_ellipsis};
use crate::types::{ChatRequest, ChatResponse, LlmOperationId};

// Compiled once. Matches <think>, <thinking>, <analysis>, <reasoning> blocks
// (case-insensitive, non-greedy, DOTALL) that reasoning models emit before
// the JSON payload. These blocks often contain `{` braces that confuse the
// balanced-brace extractor when unstripped.
//
// Each tag is listed explicitly because the `regex` crate does not support
// backreferences — we cannot write `<(tag)>.*?</\1>`.
static REASONING_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)(?:<think>.*?</think>|<thinking>.*?</thinking>|<analysis>.*?</analysis>|<reasoning>.*?</reasoning>)",
    )
    .unwrap()
});

// Matches an outermost ```json ... ``` or ``` ... ``` markdown fence.
static CODE_FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\A\s*```(?:json)?\s*\n(.*?)\n\s*```\s*\z").unwrap());

/// Remove `<think>`, `<thinking>`, `<analysis>`, and `<reasoning>` blocks
/// (case-insensitive) from `s`, then unwrap any surrounding markdown code
/// fence. Returns a freshly allocated `String` so the caller can choose
/// between the cleaned and the raw form.
pub(crate) fn strip_reasoning_blocks(s: &str) -> String {
    let stripped = REASONING_BLOCK_RE.replace_all(s, "");
    // If the entire remaining payload is wrapped in a code fence, unwrap it
    // so `serde_json::from_str` can see the bare JSON directly.
    if let Some(caps) = CODE_FENCE_RE.captures(stripped.as_ref()) {
        caps[1].trim().to_string()
    } else {
        stripped.trim().to_string()
    }
}

/// OpenAI-compatible provider, parameterised by base URL.
pub struct OpenAiCompatProvider {
    inner: OpenAiProvider,
    name_tag: &'static str,
    /// When `true`, structured output sends a strict `response_format`
    /// instead of the tolerant parser. Set by the factory from
    /// `ProviderConfig::compat_strict` (sourced once by `Config::load`).
    strict: bool,
}

impl OpenAiCompatProvider {
    /// Construct a provider pointed at `base_url` (`LLM_BASE_URL` or
    /// `OLLAMA_HOST`). API key is optional; many local engines accept
    /// any non-empty string.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<SecretString>,
        model: impl Into<String>,
    ) -> LlmResult<Self> {
        let key = api_key.unwrap_or_else(|| SecretString::from("dummy"));
        // Local / proxy engines speak the legacy OpenAI wire format
        // only — no `max_completion_tokens`, no model-family caps, no
        // temperature massaging. Swap dialect so the inner provider's
        // per-request quirks don't leak into Ollama / vLLM setups.
        let inner = OpenAiProvider::new(key, model)?
            .with_base_url(base_url)
            .with_dialect(RequestDialect::Compat);
        Ok(Self {
            inner,
            name_tag: "openai-compat",
            strict: false,
        })
    }

    /// Set strict mode. The factory calls this with
    /// `ProviderConfig::compat_strict`; `new` defaults it off so the
    /// tolerant parser stays the zero-config behaviour.
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Send `chat_template_kwargs: {"enable_thinking": false}` with every
    /// request, for thinking-capable local engines (vLLM / SGLang serving
    /// Qwen3-class models): the engine otherwise spends the output budget
    /// on a reasoning pass before the structured payload and can truncate
    /// it. The factory calls this with
    /// `ProviderConfig::compat_disable_thinking`; `new` defaults it off so
    /// existing vLLM / Ollama / LM Studio setups are unchanged. Forwarded
    /// to the inner Chat Completions client, where the `Compat` dialect
    /// gate keeps it off the wire for every other dialect.
    #[must_use]
    pub fn with_disable_thinking(mut self, enabled: bool) -> Self {
        self.inner = self.inner.with_disable_thinking(enabled);
        self
    }

    /// Repoint the wrapped [`OpenAiProvider`] at another base URL.
    ///
    /// For a wrapper that constructs with a product default and lets the
    /// operator override it — [`crate::OpenCodeProvider`] defaults to Go and
    /// accepts Zen this way. `new` already takes a base URL, so a direct
    /// caller has no need for this.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.inner = self.inner.with_base_url(url);
        self
    }

    /// Override the per-request timeout on the wrapped
    /// [`OpenAiProvider`]. The factory calls this with
    /// `ProviderConfig::request_timeout_secs`.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.inner = self.inner.with_timeout_secs(secs);
        self
    }

    /// Forward reasoning effort to the inner Chat Completions client.
    /// OpenRouter and xAI hosts use their native request shapes.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<crate::ReasoningEffort>) -> Self {
        self.inner = self.inner.with_reasoning_effort(effort);
        self
    }

    /// Forward operator-configured headers to the inner client. Aggregators
    /// and gateways behind an OpenAI-compatible endpoint are the common case
    /// for caller-identifying headers.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: crate::ExtraHeaders) -> Self {
        self.inner = self.inner.with_extra_headers(headers);
        self
    }

    /// Forward the caller-identifying defaults to the inner client. Only
    /// [`crate::OpenCodeProvider`] opts in today; the values sit under any
    /// operator-configured header of the same name.
    pub(crate) fn with_client_headers(
        mut self,
        user_agent: &'static str,
        operation_id: &'static str,
    ) -> Self {
        self.inner = self.inner.with_client_headers(user_agent, operation_id);
        self
    }

    /// Send `X-Request-Id` with the logical operation id on every chat
    /// attempt, for gateways that record it (for example as `req=<id>`) and
    /// forward it to the engine. The id is shared by every attempt of one
    /// operation — the strict-to-tolerant fallback reuses it, so one
    /// operation is correlatable end to end. Enabled by the factory only
    /// for the configured `openai-compat` provider;
    /// [`crate::OpenCodeProvider`] keeps its own `x-opencode-session`
    /// contract and does not set it. A static `AI_MEMORY_LLM_HEADERS` entry
    /// for this name is refused at startup on that path.
    #[must_use]
    pub(crate) fn with_request_id_header(mut self) -> Self {
        self.inner = self.inner.with_request_id_header();
        self
    }
    /// Endpoint the inner client will call. Test-visible so
    /// [`crate::OpenCodeProvider`]'s tests can assert Go stays the default
    /// and a Zen override takes effect.
    #[cfg(test)]
    pub(crate) fn base_url(&self) -> &str {
        self.inner.base_url()
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &'static str {
        self.name_tag
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
        self.complete_structured(request, schema, operation_id)
            .await
    }
}

impl OpenAiCompatProvider {
    async fn complete_structured(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<serde_json::Value> {
        // Strict mode: modern local engines
        // honour `response_format=json_schema`. Normalise the schema
        // to the strict subset (additionalProperties:false, full required, no
        // oneOf / no $ref siblings) — the same rewrite the Official dialect
        // applies — and delegate to the inner provider, which sends the
        // response_format. The dialect stays Compat, so the api.openai.com
        // token/temperature quirks do NOT leak into the local engine.
        //
        // Note: `enforce_strict_object_schemas` is invoked here because the
        // inner provider's own normalisation is gated on `RequestDialect::
        // Official` (see openai.rs::complete_structured_raw). The wrapper
        // owns the Compat-side rewrite so a refactor of the inner cannot
        // silently drop it.
        //
        // Fallback path:
        // Fallback on *parse-shape* failures, or when a compatibility endpoint
        // explicitly rejects `response_format` / `json_schema` before model
        // execution. Other provider and transport failures propagate: a
        // retry would hit the same wall and may double latency or token spend.
        //
        // The fallback itself is a SECOND HTTP call to the model — the
        // strict raw call returns parsed JSON or an error, not raw text,
        // so we can't reuse the response body. Operators on engines that
        // routinely fall back (e.g. reasoning models with `<think>` in
        // `content`) should keep `strict=false` to avoid the double call.
        if self.strict {
            let mut strict_schema = schema.clone();
            enforce_strict_object_schemas(&mut strict_schema);
            let strict_result = self
                .inner
                .complete_structured_raw_with_operation_id(
                    request.clone(),
                    strict_schema,
                    operation_id,
                )
                .await;
            match strict_result {
                Ok(v) if v.is_object() => return Ok(v),
                Ok(_) => {
                    debug!("compat strict: non-object response, falling back to tolerant parser");
                }
                Err(err) if is_parse_shape_error(&err) => {
                    debug!(error = %err, "compat strict parse-shape mismatch, falling back to tolerant parser");
                }
                Err(err) if is_response_format_rejection(&err) => {
                    debug!(error = %err, "compat endpoint rejected response_format, falling back to tolerant parser");
                }
                Err(err) => {
                    // Transport / 5xx / auth / rate-limit: a tolerant retry
                    // would hit the same failure. Propagate so the caller
                    // sees the real cause and can back off appropriately.
                    return Err(err);
                }
            }
        }
        // Default (and strict fallback): most older local engines don't
        // honour `response_format`. Ask for JSON and extract the first
        // balanced `{…}` object from the text.
        let (res, finish_reason) = self
            .inner
            .complete_with_operation_id_and_finish_reason(request, operation_id)
            .await?;
        // A `length` stop means the engine hit its output budget before
        // the JSON could finish (the Qwen3-with-thinking-on case this
        // guard exists for). Terminal and content-free: a tolerant retry
        // would just re-truncate and double the token spend, and the old
        // `unexpected-shape` / `serde` classes pointed at the wrong cause.
        if is_length_truncated(finish_reason.as_deref()) {
            return Err(LlmError::TruncatedResponse {
                model: res.model,
                completion_tokens: res.usage.as_ref().map(|u| u.output_tokens),
            });
        }
        // No content at all (missing, empty, or whitespace-only) is a
        // distinct terminal failure: nothing was produced to parse.
        if res.text.trim().is_empty() {
            return Err(LlmError::EmptyContent { model: res.model });
        }
        // Reasoning models (DeepSeek, Qwen, MiniMax M2.7, …) prepend
        // `<think>…</think>` before the JSON. Strip those blocks (and any
        // surrounding markdown fences) before trying to parse — otherwise
        // `first_json_object` latches onto a `{` inside the reasoning text.
        let cleaned = strip_reasoning_blocks(&res.text);
        match serde_json::from_str::<serde_json::Value>(&cleaned) {
            Ok(v) if v.is_object() => Ok(v),
            _ => {
                let Some(slice) = first_json_object(&cleaned) else {
                    // Dump enough text to actually see what the model
                    // returned. 200 chars truncates inside code fences;
                    // 4 KB tells the full story for any reasonable
                    // structured-output response. Includes head + tail
                    // because some failures truncate the closing brace.
                    let head = truncate_with_ellipsis(&cleaned, 2000);
                    let tail = suffix_within_bytes(&cleaned, 2000);
                    debug!(
                        head = %head,
                        tail = %tail,
                        total_len = cleaned.len(),
                        "no balanced JSON object found"
                    );
                    return Err(LlmError::UnexpectedShape(
                        "openai-compat response did not contain a JSON object".into(),
                    ));
                };
                serde_json::from_str::<serde_json::Value>(slice).map_err(LlmError::from)
            }
        }
    }
}

/// True for errors where a *retry without structured-output coercion*
/// has a real chance of succeeding — i.e. the upstream returned a
/// response, but it wasn't in the shape we wanted. Transport / HTTP /
/// auth errors are excluded because they would just reproduce.
fn is_parse_shape_error(err: &LlmError) -> bool {
    matches!(err, LlmError::UnexpectedShape(_) | LlmError::Serde(_))
}

/// True only when a compatibility endpoint rejected structured-output syntax
/// before generation. Generic 4xx errors are deliberately excluded so auth,
/// quota, model, and malformed-request failures retain their original cause.
fn is_response_format_rejection(err: &LlmError) -> bool {
    let LlmError::Provider { status, body } = err else {
        return false;
    };
    if !matches!(status, 400 | 422) {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("response_format")
        || body.contains("json_schema")
        || body.contains("structured output")
        || body.contains("structured-output")
}

/// Find the first balanced `{...}` object in a string, skipping
/// braces that appear inside JSON string literals.
///
/// The naive implementation (only count `{` / `}`) breaks when the
/// model returns markdown content inside a JSON string value — the
/// content commonly contains `{` and `}` in code examples,
/// JSON-as-prose, etc. That throws the depth counter off and
/// either truncates the object early or never closes it. This
/// version tracks whether we're inside a `"..."` literal and
/// honours backslash escapes the JSON spec defines.
fn first_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escape = false;
    let bytes = s.as_bytes();
    for (i, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn chat_response_with_content(content: &str) -> serde_json::Value {
        json!({
            "model": "model-x",
            "choices": [{
                "message": { "content": content },
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 },
        })
    }

    fn request_id_of(request: &wiremock::Request) -> Option<String> {
        request
            .headers
            .get(crate::openai::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    /// Acceptance (a): the header is present on the wire in the accepted
    /// format — a 36-character `[A-Za-z0-9._:-]` value (UUID v7 display
    /// string) well inside the 64-character header contract — and it carries
    /// exactly the operation's id, as a single value.
    #[tokio::test]
    async fn the_request_id_header_carries_the_operation_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(chat_response_with_content("ok")),
            )
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(server.uri(), None, "model-x")
            .expect("provider builds")
            .with_request_id_header();
        let operation_id = LlmOperationId::new();
        provider
            .complete_with_operation_id(ChatRequest::user_prompt("hi"), operation_id)
            .await
            .expect("completion succeeds");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let value = request_id_of(&requests[0]).expect("x-request-id is on the wire");
        assert_eq!(value, operation_id.to_string());
        // Exactly one value: `reqwest` appends, so a duplicate would be two.
        assert_eq!(
            requests[0]
                .headers
                .get_all(crate::openai::REQUEST_ID_HEADER)
                .into_iter()
                .count(),
            1
        );
        // Accepted format: 36-char UUID v7, inside the <=64 char contract.
        assert_eq!(value.len(), 36, "UUID display form is 36 characters");
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')),
            "the gateway accepts [A-Za-z0-9._:-]: {value:?}"
        );
        let uuid = uuid::Uuid::parse_str(&value).expect("valid UUID");
        assert_eq!(uuid.get_version_num(), 7, "time-ordered v7 id");
    }

    /// Acceptance (b): the strict-to-tolerant fallback is a SECOND HTTP
    /// call of the same operation — both attempts must carry the same
    /// `x-request-id`, and only that value.
    #[tokio::test]
    async fn the_strict_fallback_reuses_the_request_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(|request: &wiremock::Request| {
                let body: serde_json::Value =
                    serde_json::from_slice(&request.body).expect("request body is JSON");
                if body.get("response_format").is_some() {
                    ResponseTemplate::new(400)
                        .set_body_string("unsupported parameter: response_format")
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(chat_response_with_content(r#"{"ok":true}"#))
                }
            })
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(server.uri(), None, "model-x")
            .expect("provider builds")
            .with_strict(true)
            .with_request_id_header();
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
            .expect("the tolerant fallback parses the JSON");

        assert_eq!(value, json!({ "ok": true }));
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 2, "strict attempt + tolerant fallback");
        let expected = operation_id.to_string();
        for request in &requests {
            assert_eq!(
                request_id_of(request),
                Some(expected.clone()),
                "every attempt of one operation carries the same id"
            );
            assert_eq!(
                request
                    .headers
                    .get_all(crate::openai::REQUEST_ID_HEADER)
                    .into_iter()
                    .count(),
                1
            );
        }
    }

    /// Acceptance (e): `opencode` routes through this wrapper internally but
    /// keeps its own `x-opencode-session` contract — it must NOT gain the
    /// `x-request-id` header.
    #[tokio::test]
    async fn the_opencode_transport_keeps_its_contract_without_a_request_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(chat_response_with_content("ok")),
            )
            .mount(&server)
            .await;

        let provider = crate::OpenCodeProvider::new(SecretString::from("sk-test"), "model-x")
            .expect("provider builds")
            .with_base_url(server.uri());
        let operation_id = LlmOperationId::new();
        provider
            .complete_with_operation_id(ChatRequest::user_prompt("hi"), operation_id)
            .await
            .expect("completion succeeds");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .headers
                .get(crate::opencode::OPENCODE_SESSION_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(operation_id.to_string().as_str()),
            "the x-opencode-session contract is preserved"
        );
        assert!(
            requests[0]
                .headers
                .get(crate::openai::REQUEST_ID_HEADER)
                .is_none(),
            "opencode must not gain the openai-compat x-request-id header"
        );
    }

    /// The low-level constructor stays tolerant so wrappers can choose their
    /// own compatibility policy; runtime configuration supplies the default.
    #[test]
    fn strict_defaults_off_and_can_be_overridden() {
        let p = OpenAiCompatProvider::new("http://localhost:11434/v1", None, "mistral-nemo")
            .expect("provider builds");
        assert!(!p.strict);
        let p = p.with_strict(true);
        assert!(p.strict);
    }

    /// The thinking switch is opt-in at the wrapper too: `new` leaves it
    /// off (existing vLLM / Ollama / LM Studio setups are unchanged) and
    /// `with_disable_thinking` forwards it to the inner client.
    #[test]
    fn disable_thinking_defaults_off_and_reaches_the_inner_provider() {
        let p = OpenAiCompatProvider::new("http://localhost:11434/v1", None, "qwen3.8-27b")
            .expect("provider builds");
        assert!(!p.inner.disable_thinking());
        let p = p.with_disable_thinking(true);
        assert!(p.inner.disable_thinking());
    }

    #[test]
    fn timeout_override_reaches_the_inner_provider() {
        let p = OpenAiCompatProvider::new("http://localhost:11434/v1", None, "mistral-nemo")
            .expect("provider builds")
            .with_timeout_secs(45);
        assert_eq!(
            p.inner.request_timeout(),
            std::time::Duration::from_secs(45)
        );
    }

    #[test]
    fn only_explicit_response_format_rejections_are_retryable() {
        assert!(is_response_format_rejection(&LlmError::Provider {
            status: 400,
            body: "unsupported parameter: response_format".into(),
        }));
        assert!(is_response_format_rejection(&LlmError::Provider {
            status: 422,
            body: "json_schema is not supported".into(),
        }));
        assert!(!is_response_format_rejection(&LlmError::Provider {
            status: 400,
            body: "unknown model".into(),
        }));
        assert!(!is_response_format_rejection(&LlmError::Provider {
            status: 401,
            body: "response_format requires authentication".into(),
        }));
    }

    /// Parse-shape failures are retryable. Provider capability rejection is
    /// classified separately; generic provider and transport failures are not.
    #[test]
    fn is_parse_shape_error_classifies_correctly() {
        // Shape failures → fall back.
        assert!(is_parse_shape_error(&LlmError::UnexpectedShape(
            "no JSON object".into()
        )));
        assert!(is_parse_shape_error(&LlmError::Serde(
            "trailing comma".into()
        )));
        // Transport / HTTP / auth → propagate.
        assert!(!is_parse_shape_error(&LlmError::Provider {
            status: 500,
            body: "engine boom".into()
        }));
        assert!(!is_parse_shape_error(&LlmError::Provider {
            status: 401,
            body: "unauthorized".into()
        }));
        assert!(!is_parse_shape_error(&LlmError::Provider {
            status: 429,
            body: "rate limited".into()
        }));
    }

    #[test]
    fn first_json_object_finds_balanced_object() {
        assert_eq!(first_json_object("noise {\"k\":1} more"), Some("{\"k\":1}"));
        assert_eq!(
            first_json_object("text {\"a\":{\"b\":2}} trailing"),
            Some("{\"a\":{\"b\":2}}"),
        );
        assert_eq!(first_json_object("no json here"), None);
    }

    // ── strip_reasoning_blocks ───────────────────────────────────────────

    /// A `<think>` block whose body contains braces must be stripped so the
    /// real JSON object is found rather than a brace inside the think text.
    #[test]
    fn strip_think_block_with_braces_before_json() {
        let input = "<think>maybe {a:1} or {b:2}?</think>\n{\"findings\":[]}";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value =
            serde_json::from_str(&cleaned).expect("should parse to the real JSON object");
        assert!(v.is_object());
        assert_eq!(v["findings"], serde_json::json!([]));
    }

    /// `<analysis>` prefix is also stripped.
    #[test]
    fn strip_analysis_block() {
        let input = "<analysis>I reviewed the pages.</analysis>\n{\"key\":\"value\"}";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value = serde_json::from_str(&cleaned).expect("parse");
        assert_eq!(v["key"], "value");
    }

    /// A lone ```` ```json\n{...}\n``` ```` fence is unwrapped so the JSON
    /// is directly parseable.
    #[test]
    fn strip_json_code_fence() {
        let input = "```json\n{\"answer\":42}\n```";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value = serde_json::from_str(&cleaned).expect("parse");
        assert_eq!(v["answer"], 42);
    }

    /// A plain JSON object with no reasoning prefix must survive unchanged
    /// (modulo trimming).
    #[test]
    fn plain_json_unchanged() {
        let input = "{\"hello\":\"world\"}";
        let cleaned = strip_reasoning_blocks(input);
        assert_eq!(cleaned, input);
    }

    /// Tag matching is case-insensitive — `<THINK>` is stripped just like
    /// `<think>`.
    #[test]
    fn strip_is_case_insensitive() {
        let input = "<THINK>uppercase</THINK>\n{\"ok\":true}";
        let cleaned = strip_reasoning_blocks(input);
        let v: serde_json::Value = serde_json::from_str(&cleaned).expect("parse");
        assert_eq!(v["ok"], true);
    }
}
