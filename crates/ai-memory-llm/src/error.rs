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
}
