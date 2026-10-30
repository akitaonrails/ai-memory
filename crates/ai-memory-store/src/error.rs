//! Store-level error type.

use thiserror::Error;

use ai_memory_core::MemoryError;

/// Which of its keys makes a project answer to an ambiguous name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbiguousMatch {
    /// The project's current name.
    Name,
    /// The canonical path-style key derived from its repository identity.
    CanonicalKey,
    /// The v2 basename key derived from its repository identity.
    LegacyKey,
}

/// One project that answers to an ambiguous name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousProjectHolder {
    /// The project's current name, or `None` for a restricted project: the
    /// project list hides those names from callers without a grant, and the
    /// lookup that fails here runs before any caller is authorized.
    pub name: Option<String>,
    /// The key that matched.
    pub matched_by: AmbiguousMatch,
}

/// The projects behind a [`StoreError::ProjectNameAmbiguous`], rendered as
/// the tail of its message so the operator can see which project to rename,
/// purge or pin with a marker. Empty when the lookup did not collect them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AmbiguousProjectHolders(pub Vec<AmbiguousProjectHolder>);

impl std::fmt::Display for AmbiguousProjectHolders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let last = self.0.len().saturating_sub(1);
        for (index, holder) in self.0.iter().enumerate() {
            f.write_str(match index {
                0 => ": it is ",
                _ if index == last => " and ",
                _ => ", ",
            })?;
            f.write_str(match holder.matched_by {
                AmbiguousMatch::Name => "the name of ",
                AmbiguousMatch::CanonicalKey => "the canonical key of ",
                AmbiguousMatch::LegacyKey => "the legacy key of ",
            })?;
            match &holder.name {
                Some(name) => write!(f, "project '{name}'")?,
                None => f.write_str("a restricted project")?,
            }
        }
        Ok(())
    }
}

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

    /// A restore source would initialize a new store instead of opening a
    /// database that ai-memory already migrated.
    #[error(
        "backup has no initialized ai-memory database snapshot at db/memory.sqlite; \
         restore requires an existing snapshot with applied migration history"
    )]
    UninitializedSnapshot,

    /// The store's schema is newer than this binary: an applied migration is
    /// above the highest compiled-in version (refinery reports this as
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

    /// An embedded migration is absent from the store's history even though a
    /// later version is already applied. Opening remains refused; deciding how
    /// to repair a divergent history requires inspecting the actual schema.
    #[error(
        "memory database migration history has a gap: migration {missing} is \
         not recorded, but a later migration is already applied. Back up the \
         data directory and report the migration history for review before \
         attempting a repair."
    )]
    MigrationHistoryGap {
        /// Missing embedded migration, formatted as `V{version} ({name})`.
        missing: String,
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

    /// The read connection pool was closed at shutdown; no new read opens a
    /// connection.
    #[error("read connection pool is closed")]
    ReaderClosed,

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

    /// More than one project in a workspace matched a compatibility name.
    #[error("project name '{name}' is ambiguous in this workspace{holders}")]
    ProjectNameAmbiguous {
        /// The name the caller asked for.
        name: String,
        /// The projects that answer to it, when the lookup knows them.
        holders: AmbiguousProjectHolders,
    },

    /// The supplied project name failed validation (empty, slash, etc.).
    #[error("invalid project name: {0}")]
    InvalidProjectName(String),

    /// A lookup expected a row that was not present (e.g. moving a project
    /// that no longer exists in the source workspace — typically a race or
    /// caller-invariant violation).
    #[error("not found: {0}")]
    NotFound(String),
    /// An event or disk reindex named a session that `purge_session` removed.
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

    /// A project merge was refused because sessions can still receive hook
    /// observations under the source coordinate.
    #[error("project has {count} open session(s); finish them before merging the project")]
    ProjectSessionsOpen {
        /// Number of source sessions whose `ended_at` is still NULL.
        count: u64,
    },

    /// A workspace rename was rejected because the destination name is already
    /// in use by another workspace (`workspaces.name` is UNIQUE).
    #[error("workspace name '{0}' is already taken")]
    WorkspaceNameTaken(String),

    /// A page create was refused because another live page in the same scope
    /// differs from it only by case or Unicode normalization. Such a pair is
    /// one file on macOS and Windows, so creating the second destroys the
    /// first (#1/#2: the index kept both rows and served the survivor's body
    /// for either path).
    #[error(
        "page path '{requested}' collides with existing page '{existing}': they differ only in \
         case or Unicode normalization, so a case-insensitive or normalizing filesystem (macOS, \
         Windows) stores both at one file and this write would destroy the other page. Choose a \
         distinct path, or delete/rename the existing page first."
    )]
    PagePathCollides {
        /// The path this write asked for.
        requested: String,
        /// The live page it would land on.
        existing: String,
    },

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

    /// An authenticated finish authority failed an owner or project guard.
    #[error("forbidden: {0}")]
    Forbidden(&'static str),

    /// A persisted row contains malformed data.
    #[error("malformed record: {0}")]
    MalformedRecord(String),

    /// A requested state transition is not allowed.
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// A processing-job claim no longer owns its job: the lease was lost to a
    /// newer claim, or the job was paused, invalidated, purged or moved.
    #[error("processing claim is no longer active")]
    StaleProcessingClaim,

    /// A live `ai-memory run` lease already owns the selected workstream.
    #[error("workstream is already active: {0}")]
    WorkstreamBusy(String),

    /// A workstream rename was rejected because another workstream in the
    /// same checkout already holds the destination name. `workstreams` is
    /// UNIQUE on (workspace, project, repo, worktree, name), so the rename
    /// would violate that constraint; refusing by name gives the caller the
    /// collision instead of a bare SQLite error.
    #[error("workstream name '{0}' is already taken in this checkout")]
    WorkstreamNameTaken(String),
}

impl StoreError {
    /// Whether a processing-job write failed because its claim is stale: either
    /// the claim check of a job operation, or the V97 fence trigger aborting a
    /// page or proposal insert made through a fenced writer handle.
    #[must_use]
    pub fn is_stale_processing_claim(&self) -> bool {
        match self {
            Self::StaleProcessingClaim => true,
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, message)) => {
                error.code == rusqlite::ErrorCode::ConstraintViolation
                    && message.as_deref() == Some(crate::processing_jobs::STALE_CLAIM_MESSAGE)
            }
            _ => false,
        }
    }

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
