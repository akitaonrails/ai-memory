//! Provider-neutral request/response types.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifier shared by every HTTP attempt for one logical LLM operation.
///
/// Fresh per operation: the caller of one LLM operation (a consolidation
/// invocation, an auto-improve review) generates one value with
/// [`LlmOperationId::new`] and passes it to every attempt of that
/// operation. It is never derived from an agent session id — two
/// operations of the same session (or a crash-resumed or re-claimed run)
/// must not look like one operation on the provider side.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LlmOperationId {
    id: Uuid,
    workload: LlmWorkload,
    subject: Option<Arc<LlmSubject>>,
}

/// What one LLM operation reads and whom it acts for, carried next to its
/// workload for the opt-in `openai-compat` request metadata headers
/// (`llm_request_metadata_headers`).
///
/// Every field is an identifier, never page content, and is set only when
/// the call site knows a single value: a gateway reading the headers must be
/// able to trust that an absent one means "not one value", not a guess.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct LlmSubject {
    /// Workspace name the operation reads from.
    pub workspace: Option<String>,
    /// Project name the operation reads from.
    pub project: Option<String>,
    /// Username the operation acts for.
    pub user: Option<String>,
}

/// Closed workload labels for passive LLM telemetry.
///
/// These closed labels select provider routes and prevent prompts, project
/// names, or other caller-controlled text from becoming metric keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum LlmWorkload {
    /// A caller used the provider API without assigning a workload.
    #[default]
    Unspecified,
    /// Session-to-page consolidation.
    Consolidation,
    /// Post-retrieval relevance reranking.
    Rerank,
    /// Reviewed auto-improvement proposal generation.
    AutoImprove,
    /// Existing-project bootstrap extraction.
    Bootstrap,
    /// Opt-in query answer or project briefing synthesis.
    QueryAnswer,
    /// LLM-assisted wiki lint.
    Lint,
    /// Profile classification or extraction.
    Profile,
    /// Cross-session dream merge.
    Dream,
    /// Cross-trajectory experience abstraction.
    Experience,
}

impl LlmWorkload {
    /// Every workload in stable wire order.
    pub const ALL: [Self; 10] = [
        Self::Unspecified,
        Self::Consolidation,
        Self::Rerank,
        Self::AutoImprove,
        Self::Bootstrap,
        Self::QueryAnswer,
        Self::Lint,
        Self::Profile,
        Self::Dream,
        Self::Experience,
    ];

    /// Parse one of the supported snake-case task configuration keys.
    #[must_use]
    pub fn from_config_key(key: &str) -> Option<Self> {
        match key {
            "consolidation" => Some(Self::Consolidation),
            "rerank" => Some(Self::Rerank),
            "auto_improve" => Some(Self::AutoImprove),
            "bootstrap" => Some(Self::Bootstrap),
            "query_answer" => Some(Self::QueryAnswer),
            "lint" => Some(Self::Lint),
            "profile" => Some(Self::Profile),
            "dream" => Some(Self::Dream),
            "experience" => Some(Self::Experience),
            _ => None,
        }
    }

    /// Stable snake-case configuration key, including the unspecified label.
    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Consolidation => "consolidation",
            Self::Rerank => "rerank",
            Self::AutoImprove => "auto_improve",
            Self::Bootstrap => "bootstrap",
            Self::QueryAnswer => "query_answer",
            Self::Lint => "lint",
            Self::Profile => "profile",
            Self::Dream => "dream",
            Self::Experience => "experience",
        }
    }

    /// Stable kebab-case label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Consolidation => "consolidation",
            Self::Rerank => "rerank",
            Self::AutoImprove => "auto-improve",
            Self::Bootstrap => "bootstrap",
            Self::QueryAnswer => "query-answer",
            Self::Lint => "lint",
            Self::Profile => "profile",
            Self::Dream => "dream",
            Self::Experience => "experience",
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

impl LlmOperationId {
    /// Generate a fresh time-ordered operation identifier.
    #[must_use]
    pub fn new() -> Self {
        Self::for_workload(LlmWorkload::Unspecified)
    }

    /// Generate an operation identifier carrying a passive workload label.
    #[must_use]
    pub fn for_workload(workload: LlmWorkload) -> Self {
        Self {
            id: Uuid::now_v7(),
            workload,
            subject: None,
        }
    }

    /// Attach the scope and user this operation acts on. Shared, not copied,
    /// by every attempt; an empty subject is dropped.
    #[must_use]
    pub fn with_subject(mut self, subject: LlmSubject) -> Self {
        self.subject = (subject != LlmSubject::default()).then(|| Arc::new(subject));
        self
    }

    /// Workload assigned by the call site.
    #[must_use]
    pub const fn workload(&self) -> LlmWorkload {
        self.workload
    }

    /// Subject assigned by the call site, if it named one.
    #[must_use]
    pub fn subject(&self) -> Option<&LlmSubject> {
        self.subject.as_deref()
    }
}

impl Default for LlmOperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for LlmOperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.id.fmt(f)
    }
}

/// Message role in a chat turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Message from the user.
    User,
    /// Message from the assistant (the model).
    Assistant,
}

impl Role {
    /// Canonical lowercase wire string (`user` / `assistant`).
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One message in the chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role.
    pub role: Role,
    /// Message text.
    pub content: String,
}

impl ChatMessage {
    /// Convenience constructor.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    /// Convenience constructor.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// Provider-neutral chat completion request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Optional system prompt.
    pub system: Option<String>,
    /// User + assistant messages.
    pub messages: Vec<ChatMessage>,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature (0.0..2.0). `None` to defer to provider default.
    pub temperature: Option<f32>,
}

impl ChatRequest {
    /// Build a request from a single user prompt.
    #[must_use]
    pub fn user_prompt(prompt: impl Into<String>) -> Self {
        Self {
            system: None,
            messages: vec![ChatMessage::user(prompt)],
            max_tokens: 1024,
            temperature: None,
        }
    }

    /// Override the max-tokens cap.
    #[must_use]
    pub const fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }
}

/// Token usage report. Providers return at least input/output counts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    pub input_tokens: u32,
    /// Tokens generated.
    pub output_tokens: u32,
}

/// Provider-neutral response.
#[derive(Debug, Clone, Serialize)]
pub struct ChatResponse {
    /// Model-assistant text output.
    pub text: String,
    /// Token usage, if reported.
    pub usage: Option<Usage>,
    /// Model identifier echoed by the provider.
    pub model: String,
}

/// Operator-facing reasoning / thinking effort.
///
/// Serde rejects unknown values at config load so a typo does not become a
/// 400 on first consolidation. Each provider maps this onto its native
/// request field:
///
/// - OpenAI Chat Completions / generic openai-compat: `reasoning_effort`
///   (`ultra`/`persistent` clamp to `max`; those strings are not in the
///   official Chat Completions enum)
/// - OpenRouter: `reasoning.effort` (and excludes reasoning from `content`)
/// - xAI Grok Chat Completions: `reasoning_effort` (`low`/`medium`/`high`/
///   `xhigh`; reasoning cannot be disabled)
/// - Anthropic Messages: `output_config.effort` plus adaptive/disabled
///   `thinking` on models that accept those fields
/// - ChatGPT/Codex Responses: `reasoning.effort` (same OpenAI wire clamp)
///
/// Gemini and Copilot ignore the key.
///
/// Omitting the config key (Rust `None`) leaves the model default;
/// [`Self::None`] is the wire value `none` (disable reasoning where the
/// backend allows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Disable reasoning (`none`).
    None,
    /// Lowest billed reasoning (`minimal`).
    Minimal,
    /// Low reasoning effort.
    Low,
    /// Medium reasoning effort.
    Medium,
    /// High reasoning effort.
    High,
    /// Extra-high reasoning effort (`xhigh`).
    XHigh,
    /// Maximum reasoning effort (`max`).
    Max,
    /// Codex-advertised `ultra` effort.
    Ultra,
    /// Codex-advertised `persistent` effort.
    Persistent,
}

impl ReasoningEffort {
    /// Canonical lowercase wire-format string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::Persistent => "persistent",
        }
    }

    /// xAI Grok Chat Completions only accepts `low`/`medium`/`high`/`xhigh`
    /// and cannot disable reasoning. Closest supported value is used.
    #[must_use]
    pub const fn grok_chat_effort(self) -> Self {
        match self {
            Self::None | Self::Minimal => Self::Low,
            Self::Max | Self::Ultra | Self::Persistent => Self::XHigh,
            other => other,
        }
    }

    /// OpenAI Chat Completions / Responses and OpenRouter accept
    /// `none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`. `ultra` and
    /// `persistent` are not in that enum.
    #[must_use]
    pub const fn openai_wire_effort(self) -> Self {
        match self {
            Self::Ultra | Self::Persistent => Self::Max,
            other => other,
        }
    }

    /// Anthropic `output_config.effort` is `low`/`medium`/`high`/`xhigh`/
    /// `max`. `none` is represented by disabling thinking instead.
    #[must_use]
    pub const fn anthropic_output_effort(self) -> Option<Self> {
        match self {
            Self::None => None,
            Self::Minimal => Some(Self::Low),
            Self::Ultra | Self::Persistent => Some(Self::Max),
            other => Some(other),
        }
    }
}

/// Operator-supplied HTTP headers attached to every chat request.
///
/// Some gateways require a caller-identifying header: OpenCode asks
/// every tool on its endpoint to send `x-opencode-session` and to identify
/// itself with a specific `User-Agent`, and flags accounts whose traffic
/// carries neither. Rather than teach each provider one gateway's wire
/// quirks, the operator declares headers once (`AI_MEMORY_LLM_HEADERS`) and
/// every chat provider sends them.
///
/// Parsed and validated at the configuration boundary, so providers consume
/// typed header material and never re-parse operator strings — the rule
/// provider auth already follows. Header values are treated as sensitive:
/// they are marked as such on the wire and the [`std::fmt::Debug`] impl
/// prints names only, because this is a plausible place for an operator to
/// put a token.
#[derive(Clone, Default, PartialEq)]
pub struct ExtraHeaders(reqwest::header::HeaderMap);

/// Headers ai-memory sets itself on provider requests. Rejected as operator
/// input: `reqwest` appends rather than replaces, so a second `authorization`
/// or `content-type` breaks the request instead of annotating it. Failing at
/// startup beats failing on the first consolidation pass.
const RESERVED_HEADERS: &[&str] = &[
    "anthropic-beta",
    "anthropic-version",
    "authorization",
    "content-length",
    "content-type",
    "host",
    "openai-beta",
    "x-api-key",
    "x-goog-api-key",
];

impl ExtraHeaders {
    /// Parse `Name: Value` / `Name=Value` entries, skipping blank ones.
    ///
    /// # Errors
    /// [`LlmError::NotConfigured`] when an entry has no separator, carries an
    /// invalid header name or value, or names a header from
    /// [`RESERVED_HEADERS`].
    pub fn parse<I, S>(entries: I) -> crate::error::LlmResult<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        use reqwest::header::{HeaderName, HeaderValue};

        let mut map = reqwest::header::HeaderMap::new();
        for (index, entry) in entries.into_iter().enumerate() {
            let entry = entry.as_ref().trim();
            if entry.is_empty() {
                continue;
            }
            // 1-based: operators count list entries from one, and the index
            // is what the message can safely name — see `split_entry`.
            let (raw_name, raw_value) = split_entry(entry, index + 1)?;
            let name = HeaderName::try_from(raw_name).map_err(|_| {
                crate::error::LlmError::NotConfigured(format!(
                    "AI_MEMORY_LLM_HEADERS entry {}: {raw_name:?} is not a valid HTTP header name",
                    index + 1
                ))
            })?;
            if RESERVED_HEADERS.contains(&name.as_str()) {
                return Err(crate::error::LlmError::NotConfigured(format!(
                    "AI_MEMORY_LLM_HEADERS entry {}: {name} is set by ai-memory itself \
                     and cannot be overridden",
                    index + 1
                )));
            }
            let mut value = HeaderValue::try_from(raw_value).map_err(|_| {
                // Names the header, never the value.
                crate::error::LlmError::NotConfigured(format!(
                    "AI_MEMORY_LLM_HEADERS entry {}: the value for {name} is not a valid \
                     HTTP header value",
                    index + 1
                ))
            })?;
            value.set_sensitive(true);
            map.insert(name, value);
        }
        Ok(Self(map))
    }

    /// Set `name: value` only when the operator has not configured that
    /// header, so a provider default never overrides an explicit
    /// `AI_MEMORY_LLM_HEADERS` entry.
    pub(crate) fn set_default(
        &mut self,
        name: reqwest::header::HeaderName,
        value: reqwest::header::HeaderValue,
    ) {
        if !self.0.contains_key(&name) {
            self.0.insert(name, value);
        }
    }

    /// Insert `name: value`, replacing any existing value for that name.
    ///
    /// Unlike [`Self::set_default`], this overrides an operator entry: it is
    /// reserved for headers ai-memory sets itself per request (the
    /// openai-compat `x-request-id`, which carries the logical operation id).
    /// The factory refuses a static `AI_MEMORY_LLM_HEADERS` entry for those
    /// names up front, so on the configured path this can never silently
    /// replace operator material — and [`Self::apply`] replaces per name
    /// either way, so one value always goes on the wire.
    pub(crate) fn insert(
        &mut self,
        name: reqwest::header::HeaderName,
        value: reqwest::header::HeaderValue,
    ) {
        self.0.insert(name, value);
    }

    /// Whether `name` is configured (header-name comparison is
    /// case-insensitive, as on the wire).
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Attach the configured headers to `builder`.
    ///
    /// Uses `RequestBuilder::headers`, which *replaces* any value already set
    /// for the same name. `RequestBuilder::header` appends instead, which
    /// would send two `user-agent` values on the Copilot provider (its client
    /// carries one as a default) — a duplicate breaks the request rather than
    /// overriding it.
    pub(crate) fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.0.is_empty() {
            return builder;
        }
        builder.headers(self.0.clone())
    }

    /// Value configured for `name`. Test-visible so provider tests can assert
    /// what the boundary parsed without exposing values to normal callers.
    #[cfg(test)]
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|v| v.to_str().ok())
    }
}

/// Names only — never values. `ProviderConfig` derives `Debug` and is logged
/// on configuration errors.
impl std::fmt::Debug for ExtraHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

/// Split `Name: Value` or `Name=Value` on whichever separator appears first,
/// so a value containing the other character survives intact.
///
/// Errors name the entry by position rather than echoing it: an entry with no
/// separator at all may well be a bare token the operator pasted by mistake.
fn split_entry(entry: &str, position: usize) -> crate::error::LlmResult<(&str, &str)> {
    let at = match (entry.find(':'), entry.find('=')) {
        (Some(colon), Some(equals)) => colon.min(equals),
        (Some(colon), None) => colon,
        (None, Some(equals)) => equals,
        (None, None) => {
            return Err(crate::error::LlmError::NotConfigured(format!(
                "AI_MEMORY_LLM_HEADERS entry {position} is neither `Name: Value` nor `Name=Value`"
            )));
        }
    };
    let (name, rest) = entry.split_at(at);
    Ok((name.trim(), rest[1..].trim()))
}

#[cfg(test)]
mod tests {
    use super::{ExtraHeaders, LlmOperationId, LlmWorkload, ReasoningEffort};
    use rstest::rstest;

    const ALL_EFFORTS: [ReasoningEffort; 9] = [
        ReasoningEffort::None,
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
        ReasoningEffort::Persistent,
    ];

    #[test]
    fn workload_labels_are_stable_and_do_not_change_the_wire_operation_id() {
        for workload in LlmWorkload::ALL {
            let value = serde_json::to_value(workload).unwrap();
            assert_eq!(value, serde_json::json!(workload.as_str()));
            assert_eq!(
                serde_json::from_value::<LlmWorkload>(value).unwrap(),
                workload
            );
            let wire_id = LlmOperationId::for_workload(workload).to_string();
            assert_eq!(wire_id.len(), 36);
            assert_eq!(
                uuid::Uuid::parse_str(&wire_id).unwrap().get_version_num(),
                7
            );
            assert!(!wire_id.contains(workload.as_str()));
        }
    }

    #[test]
    fn task_config_keys_are_exact_and_reversible() {
        for workload in LlmWorkload::ALL {
            let expected = (workload != LlmWorkload::Unspecified).then_some(workload);
            assert_eq!(
                LlmWorkload::from_config_key(workload.config_key()),
                expected
            );
        }
        assert_eq!(LlmWorkload::from_config_key("auto-improve"), None);
        assert_eq!(LlmWorkload::from_config_key("Query_Answer"), None);
    }

    #[test]
    fn reasoning_effort_roundtrips() {
        assert_eq!(
            ALL_EFFORTS.map(|effort| effort.as_str()),
            [
                "none",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
                "persistent",
            ]
        );
        for effort in ALL_EFFORTS {
            let raw = effort.as_str();
            let parsed: ReasoningEffort = serde_json::from_value(serde_json::json!(raw)).unwrap();
            assert_eq!(parsed, effort);
            assert_eq!(
                serde_json::to_value(effort).unwrap(),
                serde_json::json!(raw)
            );
        }
    }

    #[rstest]
    #[case::uppercase("HIGH")]
    #[case::unknown("ludicrous")]
    fn reasoning_effort_rejects_invalid(#[case] raw: &str) {
        assert!(serde_json::from_value::<ReasoningEffort>(serde_json::json!(raw)).is_err());
    }

    #[rstest]
    #[case::none(ReasoningEffort::None, ReasoningEffort::Low)]
    #[case::minimal(ReasoningEffort::Minimal, ReasoningEffort::Low)]
    #[case::max(ReasoningEffort::Max, ReasoningEffort::XHigh)]
    #[case::ultra(ReasoningEffort::Ultra, ReasoningEffort::XHigh)]
    fn grok_clamps_unsupported_effort(
        #[case] input: ReasoningEffort,
        #[case] expected: ReasoningEffort,
    ) {
        assert_eq!(input.grok_chat_effort(), expected);
    }

    #[rstest]
    #[case::none(ReasoningEffort::None, None)]
    #[case::minimal(ReasoningEffort::Minimal, Some(ReasoningEffort::Low))]
    #[case::ultra(ReasoningEffort::Ultra, Some(ReasoningEffort::Max))]
    #[case::high(ReasoningEffort::High, Some(ReasoningEffort::High))]
    fn anthropic_maps_output_effort(
        #[case] input: ReasoningEffort,
        #[case] expected: Option<ReasoningEffort>,
    ) {
        assert_eq!(input.anthropic_output_effort(), expected);
    }

    #[rstest]
    #[case::ultra(ReasoningEffort::Ultra, ReasoningEffort::Max)]
    #[case::persistent(ReasoningEffort::Persistent, ReasoningEffort::Max)]
    #[case::xhigh(ReasoningEffort::XHigh, ReasoningEffort::XHigh)]
    fn openai_clamps_non_enum_effort(
        #[case] input: ReasoningEffort,
        #[case] expected: ReasoningEffort,
    ) {
        assert_eq!(input.openai_wire_effort(), expected);
    }
    #[test]
    fn parse_accepts_both_separators_and_trims() {
        let headers =
            ExtraHeaders::parse(["x-opencode-session = ses-1", "  x-tool: ai-memory  ", "  "])
                .expect("valid entries");
        assert_eq!(headers.get("x-opencode-session"), Some("ses-1"));
        assert_eq!(headers.get("x-tool"), Some("ai-memory"));
    }

    /// A value containing `=` must survive a `Name: Value` entry, and one
    /// containing `:` a `Name=Value` entry — the split takes the *first*
    /// separator, not whichever kind it finds.
    #[test]
    fn parse_splits_on_the_first_separator_only() {
        let headers =
            ExtraHeaders::parse(["x-a: k=v", "x-b=https://example.test/p"]).expect("valid entries");
        assert_eq!(headers.get("x-a"), Some("k=v"));
        assert_eq!(headers.get("x-b"), Some("https://example.test/p"));
    }

    #[test]
    fn parse_rejects_entry_without_a_separator() {
        let err = ExtraHeaders::parse(["sk-not-a-header"]).expect_err("must fail closed");
        let rendered = err.to_string();
        assert!(rendered.contains("entry 1"), "{rendered}");
        // The malformed entry may be a pasted secret; it must not be echoed.
        assert!(!rendered.contains("sk-not-a-header"), "{rendered}");
    }

    #[test]
    fn parse_rejects_invalid_header_name() {
        let err = ExtraHeaders::parse(["not a name: v"]).expect_err("must fail closed");
        assert!(err.to_string().contains("valid HTTP header name"));
    }

    /// Newlines are the header-injection vector; rejecting them closes it.
    /// The message names the header but never the offending value.
    #[test]
    fn parse_rejects_invalid_header_value_without_echoing_it() {
        let err = ExtraHeaders::parse(["x-a: bad\nvalue-smuggled"]).expect_err("must fail closed");
        let rendered = err.to_string();
        assert!(rendered.contains("value for x-a"), "{rendered}");
        assert!(!rendered.contains("smuggled"), "{rendered}");
    }

    #[rstest::rstest]
    #[case("authorization: Bearer x")]
    #[case("Content-Type: text/plain")]
    #[case("x-api-key: k")]
    #[case("anthropic-version: 2023-06-01")]
    fn parse_rejects_headers_ai_memory_owns(#[case] entry: &str) {
        let err = ExtraHeaders::parse([entry]).expect_err("reserved header must fail closed");
        assert!(err.to_string().contains("set by ai-memory itself"), "{err}");
    }

    #[test]
    fn set_default_never_overrides_an_operator_entry() {
        let mut headers = ExtraHeaders::parse(["user-agent: mine/1"]).expect("valid");
        headers.set_default(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("fallback/1"),
        );
        assert_eq!(headers.get("user-agent"), Some("mine/1"));
    }

    #[test]
    fn set_default_fills_an_absent_header() {
        let mut headers = ExtraHeaders::default();
        headers.set_default(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("fallback/1"),
        );
        assert_eq!(headers.get("user-agent"), Some("fallback/1"));
    }

    /// `insert` is the per-request override used for headers ai-memory owns
    /// (the openai-compat `x-request-id`): an existing entry is *replaced*,
    /// so exactly one value survives, and `contains` sees it case-insensitively.
    #[test]
    fn insert_replaces_an_existing_value() {
        let mut headers = ExtraHeaders::parse(["x-request-id: static-1"]).expect("valid");
        assert!(headers.contains("x-request-id"));
        headers.insert(
            reqwest::header::HeaderName::from_static("x-request-id"),
            reqwest::header::HeaderValue::from_static("dynamic-1"),
        );
        assert_eq!(headers.get("x-request-id"), Some("dynamic-1"));
        // The header-name match used by the factory's refuse check is
        // case-insensitive, so a mixed-case operator entry is caught too.
        let mixed = ExtraHeaders::parse(["X-Request-Id: static-2"]).expect("valid");
        assert!(mixed.contains("x-request-id"));
        assert!(!mixed.contains("x-opencode-session"));
    }

    /// `ProviderConfig` derives `Debug` and gets rendered into configuration
    /// errors; values must not ride along.
    #[test]
    fn debug_renders_names_but_not_values() {
        let headers = ExtraHeaders::parse(["x-opencode-session: ses-secret"]).expect("valid");
        let rendered = format!("{headers:?}");
        assert!(rendered.contains("x-opencode-session"), "{rendered}");
        assert!(!rendered.contains("ses-secret"), "{rendered}");
    }
}
