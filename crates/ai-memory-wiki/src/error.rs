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

    /// The `wiki_migrations` table records a migration this binary does
    /// not know: the wiki was migrated by a newer ai-memory. Refusing to
    /// open it read-write prevents silent format mixing.
    #[error(
        "this wiki was migrated by a newer ai-memory (unknown wiki migration `{migration}`). \
         Upgrade the binary, or restore the pre-migration backup archive to go back."
    )]
    NewerWikiFormat {
        /// The unknown migration name found in `wiki_migrations`.
        migration: String,
    },

    /// A move-session refused to overwrite an existing page file at the
    /// destination (`<wiki_root>/<ws>/<proj>/sessions/<id>.md`). Surfaced as
    /// `409 Conflict` at the admin layer.
    #[error("destination page file already exists: {0}")]
    DestinationPageExists(String),

    /// libgit2 refused a wiki-repository operation because the repository is
    /// not owned by the account this process runs as (`code=Owner`, the
    /// CVE-2022-24765 dubious-ownership guard). Kept distinct from a generic
    /// I/O error so startup can surface it at ERROR with a fix: on a Windows
    /// LocalSystem service over a user-owned data dir every wiki commit fails
    /// this check, and it was previously logged WARN-only so nothing surfaced.
    /// The owner check is deliberately left enabled (disabling it is
    /// `unsafe` and reopens the CVE); the remedy is to run the service as the
    /// owning user.
    #[error(
        "wiki git repository is not owned by the current account \
         (libgit2 owner check, code=Owner): {0}"
    )]
    GitOwner(String),
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
