//! Wiki-layer error type.

use ai_memory_core::MemoryError;
use ai_memory_store::StoreError;
use thiserror::Error;

/// Result alias used throughout the wiki crate.
pub type WikiResult<T> = Result<T, WikiError>;

/// Errors raised by the wiki layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WikiError {
    /// Filesystem I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Atomic-write tempfile crate error.
    #[error(transparent)]
    Persist(#[from] tempfile::PersistError),

    /// Frontmatter could not be parsed as YAML.
    #[error("frontmatter yaml: {0}")]
    Yaml(String),

    /// Frontmatter could not be converted to JSON.
    #[error("frontmatter json: {0}")]
    Json(String),

    /// Domain-level error.
    #[error(transparent)]
    Memory(#[from] MemoryError),

    /// Store-level error.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// A move-project / similar operation refused to overwrite an existing
    /// destination directory. Surfaced as `409 Conflict` at the admin layer
    /// without string-matching the `io::Error` message. The wrapped path is
    /// the namespaced project root (`<wiki_root>/<ws>/<proj>/`) that
    /// already exists.
    #[error("destination dir already exists: {0}")]
    DestinationExists(String),
}

impl From<ai_memory_store::ContentError> for WikiError {
    fn from(value: ai_memory_store::ContentError) -> Self {
        use ai_memory_store::ContentError as CE;
        match value {
            CE::Io(e) => Self::Io(e),
            CE::Store(e) => Self::Store(e),
            CE::Parse(msg) => Self::Yaml(msg),
            CE::DestinationExists(path) => Self::DestinationExists(path),
            CE::Backend(msg) => Self::Io(std::io::Error::other(msg)),
        }
    }
}

impl From<WikiError> for ai_memory_store::ContentError {
    fn from(value: WikiError) -> Self {
        match value {
            WikiError::Io(e) => Self::Io(e),
            WikiError::Store(e) => Self::Store(e),
            WikiError::Yaml(msg) => Self::Parse(msg),
            WikiError::DestinationExists(path) => Self::DestinationExists(path),
            other => Self::Backend(other.to_string()),
        }
    }
}

impl From<serde_yaml::Error> for WikiError {
    fn from(value: serde_yaml::Error) -> Self {
        Self::Yaml(value.to_string())
    }
}

impl From<serde_json::Error> for WikiError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value.to_string())
    }
}
