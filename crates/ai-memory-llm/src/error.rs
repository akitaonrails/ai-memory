//! LLM error type.

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
    },

    /// JSON (de)serialization failure.
    #[error("serde: {0}")]
    Serde(String),

    /// Provider gave a response with unexpected shape (e.g. no tool
    /// use block where structured output was requested).
    #[error("unexpected response shape: {0}")]
    UnexpectedShape(String),

    /// Configured provider lacks the env var we need.
    #[error("provider not configured: {0}")]
    NotConfigured(String),

    /// Provider authentication failed or expired.
    #[error("auth: {0}")]
    Auth(String),

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

impl LlmError {
    /// Whether this failure is worth a short, bounded retry.
    ///
    /// True only for errors that a subsequent identical request could plausibly
    /// succeed on: a server-side `Provider` status (`429` or any `5xx`,
    /// including Cloudflare's `52x`), or an `Http` transport timeout / connect
    /// failure. Everything else — auth, schema, a malformed-request `4xx`, a
    /// deserialization or unexpected-shape error — is deterministic: retrying
    /// only burns another expensive call. Callers must keep the retry *short
    /// and bounded* (a few attempts, seconds apart); this is not a license for
    /// tenacity-style 8–128s backoff (see the cognee #2840 lesson in `lib.rs`).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Provider { status, .. } => *status == 429 || (500..=599).contains(status),
            Self::Http(e) => e.is_timeout() || e.is_connect(),
            _ => false,
        }
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

    /// Redacted error category used for health reporting and the fallback
    /// chain's aggregate error — never a response body or secret.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Http(_) => "http",
            Self::Provider { .. } => "provider",
            Self::Serde(_) => "serde",
            Self::UnexpectedShape(_) => "unexpected-shape",
            Self::NotConfigured(_) => "not-configured",
            Self::Auth(_) => "auth",
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
                body: String::new()
            }
            .is_transient()
        );
        for status in [500, 502, 503, 504, 520, 524] {
            assert!(
                LlmError::Provider {
                    status,
                    body: String::new()
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
                    body: String::new()
                }
                .is_transient(),
                "status {status} must not be treated as transient"
            );
        }
    }

    #[test]
    fn deterministic_errors_are_not_transient() {
        assert!(!LlmError::Auth("expired".into()).is_transient());
        assert!(!LlmError::Schema("bad".into()).is_transient());
        assert!(!LlmError::Serde("nope".into()).is_transient());
        assert!(!LlmError::UnexpectedShape("no tool block".into()).is_transient());
        assert!(!LlmError::NotConfigured("no key".into()).is_transient());
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
                body: "boom".into()
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
                },
                "provider",
            ),
            (LlmError::Serde("nope".into()), "serde"),
            (
                LlmError::UnexpectedShape("no tool block".into()),
                "unexpected-shape",
            ),
            (LlmError::NotConfigured("no key".into()), "not-configured"),
            (LlmError::Auth("expired".into()), "auth"),
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
}
