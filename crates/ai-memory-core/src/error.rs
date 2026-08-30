//! Workspace-wide error type.

use std::path::PathBuf;

use thiserror::Error;

/// Result alias used throughout the workspace.
pub type MemoryResult<T> = Result<T, MemoryError>;

/// Top-level error type for the ai-memory domain.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MemoryError {
    /// A path was outside the configured data root (defense in depth).
    #[error("path {0:?} escapes the configured data root")]
    PathEscape(PathBuf),

    /// A page identifier was malformed.
    #[error("invalid page path: {0}")]
    InvalidPagePath(String),

    /// A persisted record could not be parsed.
    #[error("malformed record in store: {0}")]
    MalformedRecord(String),

    /// A username failed the validation rules in
    /// [`crate::user::validate_username`] (empty, too long, contains
    /// whitespace / control / separator characters).
    #[error("invalid username: {0}")]
    InvalidUsername(String),

    /// An email failed the basic format check in
    /// [`crate::user::validate_email`] (missing `@`, whitespace in
    /// local or domain part, empty parts, …).
    #[error("invalid email: {0}")]
    InvalidEmail(String),

    /// A human password failed [`crate::user::validate_human_password`].
    /// The string is a generic policy message and never names the value.
    #[error("invalid password: {0}")]
    InvalidPassword(String),

    /// Wraps any underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Wraps a serde deserialization failure.
    #[error("serde: {0}")]
    Serde(String),
}

impl From<serde_json::Error> for MemoryError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value.to_string())
    }
}
