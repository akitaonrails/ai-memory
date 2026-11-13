//! LLM error type.

use std::time::Duration;

use thiserror::Error;

/// Result alias used throughout the LLM crate.
pub type LlmResult<T> = Result<T, LlmError>;

/// Errors raised by LLM providers.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LlmError {
    /// Underlying HTTP failure.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    /// Provider returned a non-2xx status.
    #[error("provider error {status}: {body}")]
    Provider {
        /// HTTP status code.
        status: u16,
        /// Response body (truncated).
        body: String,
        /// Provider-requested delay before another equivalent request.
        retry_after: Option<Duration>,
    },

    /// JSON (de)serialization failure.
    #[error("serde: {0}")]
    Serde(String),

    /// Provider gave a response with unexpected shape (e.g. no tool
    /// use block where structured output was requested).
    #[error("unexpected response shape: {0}")]
    UnexpectedShape(String),

    /// Structured response stopped at the provider's token budget
    /// (`finish_reason = "length"`): the engine ran out of output budget
    /// before finishing the JSON. Terminal — a retry reproduces the same
    /// truncation and doubles the spend. Carries only the configured
    /// model label and the provider-reported completion token count —
    /// never response content, prompt text, or secrets.
    #[error(
        "structured response truncated at token budget (finish_reason=length) for model {model}"
    )]
    TruncatedResponse {
        /// Configured model label.
        model: String,
        /// `usage.completion_tokens` when the provider reported it.
        completion_tokens: Option<u32>,
    },

    /// Structured request got HTTP 2xx but no usable message content
    /// (`message.content` missing, empty, or whitespace-only). Terminal
    /// and distinct from [`Self::TruncatedResponse`] and from a JSON
    /// parse error: nothing was produced to parse.
    #[error("provider returned no message content for model {model}")]
    EmptyContent {
        /// Configured model label.
        model: String,
    },

    /// Configured provider lacks the env var we need.
    #[error("provider not configured: {0}")]
    NotConfigured(String),

    /// Provider authentication failed or expired.
    #[error("auth: {0}")]
    Auth(String),

    /// Optional local work could not start without waiting behind already
    /// active work. Callers should use their non-provider fallback instead of
    /// retrying or reporting the provider unhealthy.
    #[error("{0} capacity is busy")]
    CapacityBusy(&'static str),

    /// JSON schema for structured output could not be derived.
    #[error("schema: {0}")]
    Schema(String),

    /// Every eligible candidate in an ordered LLM fallback chain failed, or
    /// every candidate's circuit was open. Carries only provider/model
    /// labels and error classes/status — never response bodies or secrets.
    /// See `docs/llm-provider-fallback.md`.
    #[error("llm fallback chain exhausted after {attempted} candidate(s): {summary}")]
    AllCandidatesFailed {
        /// Number of candidates actually attempted (skipped open circuits
        /// are excluded from this count).
        attempted: usize,
        /// Bounded `provider/model: class[ status]` summary, one entry per
        /// attempt, joined with `"; "`.
        summary: String,
    },
}

/// What a failed call means for retries and for an ordered fallback chain.
///
/// Computed by [`LlmError::failover_class`], the single classification used
/// by the in-process retry loops ([`LlmError::is_transient`]) and by
/// `FallbackLlmProvider` ([`LlmError::should_fail_over`]). See
/// `docs/llm-provider-fallback.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailoverClass {
    /// A short, bounded retry on the same provider may succeed (`429` rate
    /// limit, `5xx`, transport timeout or connect failure). The fallback
    /// chain advances and opens a short circuit on this candidate.
    Transient,
    /// The provider account is out of credit, quota or billing, or is
    /// disabled. Retrying the same provider is pointless until someone tops
    /// up or the quota window resets; the fallback chain advances and keeps
    /// this candidate out for the longer quota cooldown.
    QuotaExhausted,
    /// Deterministic for this request (bad request, invalid credential,
    /// schema, malformed or truncated response). No retry and no failover.
    Permanent,
}

impl FailoverClass {
    /// Stable, redacted wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::QuotaExhausted => "quota-exhausted",
            Self::Permanent => "permanent",
        }
    }
}

impl LlmError {
    /// Classify this failure for retries and fallback.
    ///
    /// A `Provider` error whose status and bounded, scrubbed body report
    /// exhausted credit, quota or billing (see `quota.rs`) is
    /// [`FailoverClass::QuotaExhausted`] whatever its status: Anthropic sends
    /// `400`, OpenAI `429`, OpenRouter `402`. Otherwise a `429` or any `5xx`
    /// and an `Http` timeout / connect failure are
    /// [`FailoverClass::Transient`], and everything else is
    /// [`FailoverClass::Permanent`].
    #[must_use]
    pub fn failover_class(&self) -> FailoverClass {
        match self {
            Self::Provider { status, body, .. } => {
                if crate::quota::is_quota_exhaustion(*status, body) {
                    FailoverClass::QuotaExhausted
                } else if Self::status_is_transient(*status) {
                    FailoverClass::Transient
                } else {
                    FailoverClass::Permanent
                }
            }
            Self::Http(e) if e.is_timeout() || e.is_connect() => FailoverClass::Transient,
            _ => FailoverClass::Permanent,
        }
    }

    /// Whether this failure is worth a short, bounded retry.
    ///
    /// True only for errors that a subsequent identical request could plausibly
    /// succeed on: a server-side `Provider` status (`429` or any `5xx`,
    /// including Cloudflare's `52x`), or an `Http` transport timeout / connect
    /// failure. Everything else — auth, schema, a malformed-request `4xx`, a
    /// deserialization or unexpected-shape error — is deterministic: retrying
    /// only burns another expensive call. Exhausted credit or quota
    /// ([`Self::is_quota_exhausted`]) is not transient either, even when it
    /// arrives as a `429`: a retry seconds later hits the same empty balance.
    /// Callers must keep the retry *short and bounded* (a few attempts,
    /// seconds apart); this is not a license for tenacity-style 8–128s
    /// backoff (see the cognee #2840 lesson in `lib.rs`).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        self.failover_class() == FailoverClass::Transient
    }

    /// Whether the provider reported exhausted credit, quota or billing, or
    /// a disabled account. See [`FailoverClass::QuotaExhausted`].
    #[must_use]
    pub fn is_quota_exhausted(&self) -> bool {
        self.failover_class() == FailoverClass::QuotaExhausted
    }

    /// Whether an ordered fallback chain should try its next candidate:
    /// a transient failure or exhausted credit/quota, never a deterministic
    /// one.
    #[must_use]
    pub fn should_fail_over(&self) -> bool {
        self.failover_class() != FailoverClass::Permanent
    }

    /// The status half of [`Self::is_transient`], for a call site that must
    /// pick an error variant before one exists — a token endpoint answering
    /// `429`/`5xx` has not rejected the credential, so it must not become an
    /// `Auth` error, which is deterministic. It ignores the body, so a `429`
    /// that reports exhausted quota still counts here; the resulting
    /// `Provider` error is then classified by [`Self::failover_class`].
    #[must_use]
    pub fn status_is_transient(status: u16) -> bool {
        status == 429 || (500..=599).contains(&status)
    }

    /// HTTP status captured from this error, when the failure carries one.
    #[must_use]
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Http(e) => e.status().map(|status| status.as_u16()),
            Self::Provider { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Provider-requested delay before another equivalent request, when the
    /// failed HTTP response carried a trustworthy, boundedly parsed hint.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Provider { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Redacted error category used for health reporting and the fallback
    /// chain's aggregate error — never a response body or secret. A provider
    /// error that reports exhausted credit or quota is `quota-exhausted`.
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::Http(_) => "http",
            Self::Provider { .. } if self.is_quota_exhausted() => "quota-exhausted",
            Self::Provider { .. } => "provider",
            Self::Serde(_) => "serde",
            Self::UnexpectedShape(_) => "unexpected-shape",
            Self::TruncatedResponse { .. } => "truncated-response",
            Self::EmptyContent { .. } => "empty-content",
            Self::NotConfigured(_) => "not-configured",
            Self::Auth(_) => "auth",
            Self::CapacityBusy(_) => "capacity-busy",
            Self::Schema(_) => "schema",
            Self::AllCandidatesFailed { .. } => "all-candidates-failed",
        }
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_covers_429_and_5xx_only() {
        assert!(
            LlmError::Provider {
                status: 429,
                body: String::new(),
                retry_after: None,
            }
            .is_transient()
        );
        for status in [500, 502, 503, 504, 520, 524] {
            assert!(
                LlmError::Provider {
                    status,
                    body: String::new(),
                    retry_after: None,
                }
                .is_transient(),
                "status {status} should be transient"
            );
        }
        // A malformed-request 4xx (not 429) is deterministic — do not retry.
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !LlmError::Provider {
                    status,
                    body: String::new(),
                    retry_after: None,
                }
                .is_transient(),
                "status {status} must not be treated as transient"
            );
        }
    }

    #[test]
    fn deterministic_errors_are_not_transient() {
        assert!(!LlmError::Auth("expired".into()).is_transient());
        assert!(!LlmError::CapacityBusy("embedding").is_transient());
        assert!(!LlmError::Schema("bad".into()).is_transient());
        assert!(!LlmError::Serde("nope".into()).is_transient());
        assert!(!LlmError::UnexpectedShape("no tool block".into()).is_transient());
        assert!(!LlmError::NotConfigured("no key".into()).is_transient());
    }

    // A `length` stop or an empty 2xx is a definitive answer about this
    // response: it is not transient, and the error must carry no response
    // content in its message.
    #[test]
    fn truncated_and_empty_content_are_terminal_and_content_free() {
        let truncated = LlmError::TruncatedResponse {
            model: "qwen3.8-27b".into(),
            completion_tokens: Some(4096),
        };
        assert_eq!(truncated.class(), "truncated-response");
        assert!(!truncated.is_transient());
        assert_eq!(truncated.http_status(), None);
        let rendered = truncated.to_string();
        assert!(rendered.contains("finish_reason=length"), "{rendered}");
        assert!(rendered.contains("qwen3.8-27b"), "{rendered}");

        let empty = LlmError::EmptyContent {
            model: "qwen3.8-27b".into(),
        };
        assert_eq!(empty.class(), "empty-content");
        assert!(!empty.is_transient());
        assert_eq!(empty.http_status(), None);
    }

    #[test]
    fn truncated_response_message_carries_no_content() {
        let err = LlmError::TruncatedResponse {
            model: "qwen3.8-27b".into(),
            completion_tokens: None,
        };
        let rendered = err.to_string();
        // Only the class, the wire fact, and the operator's own model
        // label may appear — never the (truncated) completion text.
        assert!(!rendered.contains('{'), "{rendered}");
        assert_eq!(rendered.matches('"').count(), 0, "{rendered}");
    }

    #[test]
    fn all_candidates_failed_is_not_transient() {
        // The aggregate error is terminal: nothing is left to advance to.
        assert!(
            !LlmError::AllCandidatesFailed {
                attempted: 2,
                summary: String::new(),
            }
            .is_transient()
        );
    }

    #[test]
    fn http_status_reads_provider_status_only() {
        assert_eq!(
            LlmError::Provider {
                status: 503,
                body: "boom".into(),
                retry_after: None,
            }
            .http_status(),
            Some(503)
        );
        assert_eq!(LlmError::Schema("bad".into()).http_status(), None);
    }

    #[test]
    fn class_is_a_stable_redacted_label_never_the_message() {
        let cases: &[(LlmError, &str)] = &[
            (
                LlmError::Provider {
                    status: 500,
                    body: "secret upstream body".into(),
                    retry_after: None,
                },
                "provider",
            ),
            (LlmError::Serde("nope".into()), "serde"),
            (
                LlmError::UnexpectedShape("no tool block".into()),
                "unexpected-shape",
            ),
            (
                LlmError::TruncatedResponse {
                    model: "qwen3.8-27b".into(),
                    completion_tokens: Some(4096),
                },
                "truncated-response",
            ),
            (
                LlmError::EmptyContent { model: "m".into() },
                "empty-content",
            ),
            (LlmError::NotConfigured("no key".into()), "not-configured"),
            (LlmError::Auth("expired".into()), "auth"),
            (LlmError::CapacityBusy("embedding"), "capacity-busy"),
            (LlmError::Schema("bad".into()), "schema"),
            (
                LlmError::AllCandidatesFailed {
                    attempted: 1,
                    summary: String::new(),
                },
                "all-candidates-failed",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.class(), *expected, "unexpected class for {err}");
            assert!(!err.class().contains("secret upstream body"));
        }
    }

    fn provider(status: u16, body: &str) -> LlmError {
        LlmError::Provider {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    #[test]
    fn quota_exhaustion_fails_over_but_is_not_retried_in_place() {
        let cases = [
            provider(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#,
            ),
            provider(
                429,
                r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
            ),
            provider(
                402,
                r#"{"error":{"message":"Insufficient credits","code":402}}"#,
            ),
        ];
        for err in cases {
            assert_eq!(err.failover_class(), FailoverClass::QuotaExhausted, "{err}");
            assert!(err.is_quota_exhausted(), "{err}");
            assert!(err.should_fail_over(), "{err}");
            assert!(!err.is_transient(), "{err}");
            assert_eq!(err.class(), "quota-exhausted");
        }
    }

    #[test]
    fn failover_class_keeps_the_existing_transient_and_permanent_split() {
        assert_eq!(
            provider(429, "rate limited").failover_class(),
            FailoverClass::Transient
        );
        assert_eq!(
            provider(503, "overloaded").failover_class(),
            FailoverClass::Transient
        );
        for status in [400, 401, 403, 404, 422] {
            let err = provider(status, "invalid x-api-key");
            assert_eq!(err.failover_class(), FailoverClass::Permanent, "{status}");
            assert!(!err.should_fail_over(), "{status}");
            assert_eq!(err.class(), "provider");
        }
        assert!(!LlmError::Auth("expired".into()).should_fail_over());
        assert!(!LlmError::Schema("bad".into()).should_fail_over());
        assert_eq!(FailoverClass::QuotaExhausted.as_str(), "quota-exhausted");
    }

    #[test]
    fn status_is_transient_matches_is_transient() {
        for status in [429u16, 500, 503, 529, 599] {
            assert!(LlmError::status_is_transient(status), "{status}");
            assert!(
                LlmError::Provider {
                    status,
                    body: String::new(),
                    retry_after: None,
                }
                .is_transient(),
                "{status} must agree with is_transient"
            );
        }
        for status in [400u16, 401, 403, 404, 422, 600] {
            assert!(!LlmError::status_is_transient(status), "{status}");
            assert!(
                !LlmError::Provider {
                    status,
                    body: String::new(),
                    retry_after: None,
                }
                .is_transient(),
                "{status} must agree with is_transient"
            );
        }
    }
}
