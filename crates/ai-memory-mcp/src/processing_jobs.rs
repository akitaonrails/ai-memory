//! Shared plumbing for the durable processing-job surfaces
//! (`POST /admin/processing-jobs`, `GET /admin/processing-jobs/{job_id}`,
//! `memory_processing_job_start` and `memory_processing_job_status`).
//!
//! The engine owns the work ([`ai_memory_consolidate::ProcessingService`]);
//! this module only holds what both surfaces must agree on: the one wire
//! error mapping, the session-in-scope guard and the user-facing messages.

use ai_memory_consolidate::ProcessingError;
use ai_memory_core::{ProjectId, SessionId, WorkspaceId};
use ai_memory_store::{ReaderPool, StoreError};

/// Message of the explicit refusal for starting a job while `[chunking]` is off.
/// The CLI keys on the word "disabled".
pub(crate) const DISABLED_MESSAGE: &str =
    "processing jobs are disabled: set [chunking].enabled = true and restart the server";

/// HTTP-ish classification of an engine error, shared by both transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessingFailure {
    /// The feature is off, or the named session/job does not exist in scope.
    NotFound,
    /// The request cannot be processed as stated (empty session, bad budget).
    Invalid,
    /// An equivalent durable job is unfinished, so the request would shadow it.
    Conflict,
    /// The server has no LLM provider, so no job can be started or driven.
    Unavailable,
    /// Anything else; the message is already redacted.
    Internal,
}

/// Classify `error` and render its content-free message.
pub(crate) fn classify(error: &ProcessingError) -> (ProcessingFailure, String) {
    match error {
        ProcessingError::Disabled => (ProcessingFailure::NotFound, DISABLED_MESSAGE.to_string()),
        ProcessingError::PausedJob => (ProcessingFailure::Conflict, PAUSED_JOB_MESSAGE.to_string()),
        ProcessingError::NoProvider => (
            ProcessingFailure::Unavailable,
            "no LLM provider is configured on the server; set AI_MEMORY_LLM_PROVIDER".to_string(),
        ),
        ProcessingError::SessionNotFound => (
            ProcessingFailure::NotFound,
            session_not_found_message().to_string(),
        ),
        ProcessingError::Invalid(_)
        | ProcessingError::EmptySession
        | ProcessingError::Budget(_) => (ProcessingFailure::Invalid, error.redacted_summary()),
        _ => (ProcessingFailure::Internal, error.redacted_summary()),
    }
}

/// One message for unknown and foreign sessions, so the refusal does not tell
/// a caller whether a session id exists in another project.
pub(crate) const fn session_not_found_message() -> &'static str {
    "session not found in the requested project"
}

/// Message for an unknown job id and for a job id that belongs to another
/// scope or owner; the two are indistinguishable by design.
pub(crate) const fn job_not_found_message() -> &'static str {
    "processing job not found in the requested project"
}

/// Whether `session_id` belongs to `(workspace_id, project_id)`.
///
/// A session id reaches a repository without passing through scope
/// resolution, so a surface that authorized `(workspace_id, project_id)` must
/// still prove the session lives there before starting work on it. The
/// project comes from where the session's observations landed, falling back
/// to the session row, the same order `memory_write_page` uses.
///
/// # Errors
///
/// Any store failure.
pub(crate) async fn session_in_scope(
    reader: &ReaderPool,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> Result<bool, StoreError> {
    let scope = match reader.session_scope_from_observations(session_id).await? {
        Some(scope) => Some(scope),
        None => reader.session_project_ids(session_id).await?,
    };
    Ok(scope == Some((workspace_id, project_id)))
}

/// Why a legacy synchronous or queued run was refused: an equivalent durable
/// job is unfinished (typically paused when `[chunking]` was switched off).
pub const PAUSED_JOB_MESSAGE: &str = "an equivalent chunked processing job for this session is unfinished (paused while [chunking] is off); re-enable [chunking] and restart to resume it, or wait for its retention cleanup. The legacy path was not run";

/// Refuse to start a legacy generation while an equivalent durable job is
/// unfinished for the same scope, session, kind and any of `owners`.
///
/// Used by every dispatch that falls back to the legacy consolidator or
/// reviewer with `[chunking]` off (MCP, admin, the SessionEnd queue), so a
/// paused job is never shadowed by a second generation that spends provider
/// calls and publishes outside the job's checkpoints. Reads the store only:
/// no flag, provider or service is needed, and a scope with no such job
/// returns `Ok(false)` so ordinary legacy operation is preserved.
///
/// `owners` lists the staging buckets to treat as equivalent; callers that
/// cannot tell which bucket the durable flow would have used pass both the
/// shared bucket and the caller's own.
///
/// # Errors
///
/// Any store failure; callers must fail closed on it.
pub async fn has_equivalent_unfinished_job(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    kind: ai_memory_core::ProcessingJobKind,
    owners: &[Option<ai_memory_core::IdentityKey>],
) -> Result<bool, StoreError> {
    for owner in owners {
        if reader
            .has_unfinished_processing_job(
                workspace_id,
                project_id,
                session_id,
                kind,
                owner.clone(),
            )
            .await?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Owner buckets a legacy caller could share a durable job with: the shared
/// bucket and, when the actor is named, its own.
#[must_use]
pub fn candidate_owners(
    actor: &ai_memory_core::ActorContext,
) -> Vec<Option<ai_memory_core::IdentityKey>> {
    let mut owners = vec![None];
    if let Some(key) = actor.identity_key() {
        owners.push(Some(key));
    }
    owners
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_an_explicit_not_found_naming_the_flag() {
        let (kind, message) = classify(&ProcessingError::Disabled);
        assert_eq!(kind, ProcessingFailure::NotFound);
        assert!(message.contains("disabled"));
        assert!(message.contains("[chunking].enabled"));
    }

    #[test]
    fn unknown_and_foreign_sessions_share_one_message() {
        let (kind, message) = classify(&ProcessingError::SessionNotFound);
        assert_eq!(kind, ProcessingFailure::NotFound);
        assert_eq!(message, session_not_found_message());
    }

    #[test]
    fn caller_errors_are_invalid_and_the_rest_internal() {
        for error in [
            ProcessingError::EmptySession,
            ProcessingError::Invalid("x".into()),
            ProcessingError::Budget("x".into()),
        ] {
            assert_eq!(classify(&error).0, ProcessingFailure::Invalid);
        }
        assert_eq!(
            classify(&ProcessingError::Other("boom".into())).0,
            ProcessingFailure::Internal
        );
    }
}
