//! Store-level error type.

use thiserror::Error;

use ai_memory_core::MemoryError;

/// Result alias used throughout the store crate.
pub type StoreResult<T> = Result<T, StoreError>;

/// Errors raised by the store layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Underlying SQLite error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Migration runner failed.
    #[error("migration: {0}")]
    Migration(#[from] refinery::Error),

    /// The store's schema is newer than this binary: an applied migration is
    /// absent from the compiled-in set (refinery reports this as
    /// `MissingVersion`). The data was written by a newer ai-memory build than
    /// the one now running, so the store is left untouched rather than opened
    /// against a schema this binary does not understand. This replaces
    /// refinery's misleading raw wording ("migration V… is missing from the
    /// filesystem"), which reads as if a file were deleted.
    #[error(
        "memory database schema is newer than this ai-memory build: the store \
         has migration {applied} applied, but this build only ships migrations \
         through V{supported}. Run an ai-memory release at least as new as the \
         one that wrote this data; a newer store cannot be opened by an older \
         binary."
    )]
    DataSchemaAhead {
        /// The applied migration this binary does not know about, formatted as
        /// `V{version} ({name})` (e.g. `V28 (sessions_devin_agent_kind)`).
        applied: String,
        /// The highest schema version this binary ships.
        supported: u32,
    },

    /// I/O failed (e.g. opening the DB file).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON serialisation failure (frontmatter).
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// Writer actor has shut down.
    #[error("writer actor is no longer running")]
    WriterClosed,

    /// A hook tried to reuse a session id that belongs to another immutable
    /// session tuple or operator. Deliberately carries no row details.
    #[error("session collision")]
    SessionCollision,

    /// A `spawn_blocking` task panicked or was cancelled.
    #[error("reader pool task did not complete: {0}")]
    PoolPanic(String),

    /// Re-export of [`MemoryError`] for cross-crate propagation.
    #[error(transparent)]
    Memory(#[from] MemoryError),

    /// A project rename was rejected because the destination name is already
    /// in use by another project in the same workspace.
    #[error("project name '{0}' is already taken in this workspace")]
    ProjectNameTaken(String),

    /// The supplied project name failed validation (empty, slash, etc.).
    #[error("invalid project name: {0}")]
    InvalidProjectName(String),

    /// A lookup expected a row that was not present (e.g. moving a project
    /// that no longer exists in the source workspace — typically a race or
    /// caller-invariant violation).
    #[error("not found: {0}")]
    NotFound(String),
    /// A late-arriving event named a session that `purge_session` removed.
    /// Recreating it would silently undo a deletion the operator already
    /// performed and an application may already have reported to a user, so
    /// ingest refuses instead (#387).
    #[error("session {0} was purged and cannot be recreated")]
    SessionPurged(String),

    /// A workspace delete was refused because it still holds projects and the
    /// caller did not pass `force`. Carries the project count so the admin
    /// endpoint / CLI can report it before the operator retries with force.
    #[error("workspace still has {0} project(s); pass force to delete anyway")]
    WorkspaceNotEmpty(u64),

    /// A project purge was rejected because one of its managed workstreams
    /// still holds a live run lease. `workstreams` is `ON DELETE CASCADE` from
    /// `projects`, and `managed_runs` cascades from `workstreams`, so the
    /// purge would delete the lease row out from under a running agent — whose
    /// heartbeat then fails with `409 managed run lease is not active` for the
    /// rest of the session and whose transcript never reaches the ledger.
    /// Carries the offending workstream names so the operator can find the
    /// session before retrying with force.
    #[error(
        "project has {count} active managed run(s) in workstream(s) {workstreams}; \
         finish or cancel them first, or pass force to purge anyway (the running \
         agent will stop being able to save its history)"
    )]
    ManagedRunActive {
        /// How many `managed_runs` rows are still `state = 'active'`.
        count: u64,
        /// Comma-separated workstream names holding those runs.
        workstreams: String,
    },

    /// A workspace rename was rejected because the destination name is already
    /// in use by another workspace (`workspaces.name` is UNIQUE).
    #[error("workspace name '{0}' is already taken")]
    WorkspaceNameTaken(String),

    /// A session move was rejected because the destination scope already has
    /// a latest page at the session page path (`idx_pages_latest_path`), so
    /// re-stamping the source versions would collide. The caller retries with
    /// the regenerate mode or resolves the destination page first.
    #[error("page path '{path}' already has a latest version in the destination scope")]
    PagePathTaken {
        /// The colliding wiki-relative page path.
        path: String,
    },

    /// The supplied workspace name failed validation (empty, slash, etc.).
    #[error("invalid workspace name: {0}")]
    InvalidWorkspaceName(String),

    /// A UNIQUE constraint was violated by an insert (e.g. duplicate
    /// `users.username` / `users.email`). The string carries a
    /// human-readable explanation the CLI / admin endpoint surfaces
    /// verbatim.
    #[error("duplicate: {0}")]
    Duplicate(String),

    /// An OS primitive failed (e.g. the CSPRNG read inside
    /// [`crate::users::generate_token`]). Carries the OS error
    /// description.
    #[error("os error: {0}")]
    Os(String),

    /// A persisted row contains malformed data.
    #[error("malformed record: {0}")]
    MalformedRecord(String),

    /// A requested state transition is not allowed.
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// A live `ai-memory run` lease already owns the selected workstream.
    #[error("workstream is already active: {0}")]
    WorkstreamBusy(String),
}

impl StoreError {
    /// Whether a hook admission failed because its cached scope was deleted or
    /// moved after resolution.
    #[must_use]
    pub fn is_stale_session_scope_reference(&self) -> bool {
        let Self::Sqlite(rusqlite::Error::SqliteFailure(error, message)) = self else {
            return false;
        };

        error.code == rusqlite::ErrorCode::ConstraintViolation
            && (error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
                || (error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER
                    && message.as_deref()
                        == Some("sessions.workspace_id does not match the project's workspace")))
    }
}

#[cfg(test)]
mod tests {
    use super::StoreError;

    #[test]
    fn stale_session_scope_reference_only_matches_fk_or_exact_pairing_trigger() {
        let foreign_key = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            },
            Some("FOREIGN KEY constraint failed".into()),
        ));
        assert!(foreign_key.is_stale_session_scope_reference());

        let pairing_trigger = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
            },
            Some("sessions.workspace_id does not match the project's workspace".into()),
        ));
        assert!(pairing_trigger.is_stale_session_scope_reference());

        let other_trigger = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                extended_code: rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
            },
            Some("other trigger".into()),
        ));
        assert!(!other_trigger.is_stale_session_scope_reference());
        assert!(!StoreError::SessionCollision.is_stale_session_scope_reference());
        assert!(!StoreError::InvalidState("invalid".into()).is_stale_session_scope_reference());
    }
}
