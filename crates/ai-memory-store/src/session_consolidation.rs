//! Durable queue state for opt-in SessionEnd LLM consolidation.

use ai_memory_core::{ProjectId, SessionId, WorkspaceId};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};

/// Maximum provider attempts for one immutable observation generation.
pub const SESSION_CONSOLIDATION_MAX_ATTEMPTS: u32 = 5;

/// One atomically claimed session-consolidation job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConsolidationJob {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    generation: u64,
    attempts: u32,
    claim_id: Uuid,
    attempt_spent: bool,
}

impl SessionConsolidationJob {
    /// Session whose observations must be consolidated.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Owning workspace.
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Owning project.
    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Observation count covered by this immutable job.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// One-based provider attempt number.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }
}

/// Enqueue the current observation generation for an ended session.
///
/// Returns `true` only when a new generation was inserted. A duplicate end,
/// missing session, mismatched scope, open session, or observation-less session
/// returns `false` without creating work.
pub fn enqueue(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO session_consolidation_jobs \
         (session_id, workspace_id, project_id, generation, state, requested_at, \
          next_attempt_at, started_at, completed_at, attempts, claim_id, last_error) \
         SELECT s.id, s.workspace_id, s.project_id, COUNT(o.id), 'pending', ?4, \
                ?4, NULL, NULL, 0, NULL, NULL \
         FROM sessions s \
         JOIN observations o ON o.session_id = s.id \
         WHERE s.id = ?1 AND s.workspace_id = ?2 AND s.project_id = ?3 \
           AND s.ended_at IS NOT NULL \
         GROUP BY s.id, s.workspace_id, s.project_id \
         HAVING COUNT(o.id) > 0",
        params![
            session_id.as_bytes(),
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            now,
        ],
    )?;
    // A newer pending generation subsumes an older one because consolidation
    // reads the session's current observations. Running work keeps its lease:
    // it may already have read the older snapshot, so the new generation must
    // still run afterward.
    tx.execute(
        "UPDATE session_consolidation_jobs \
         SET state = 'superseded', completed_at = ?1, claim_id = NULL, \
             last_error = 'superseded by a newer observation generation' \
         WHERE session_id = ?2 AND state = 'pending' \
           AND generation < ( \
               SELECT COUNT(*) FROM observations WHERE session_id = ?2 \
           )",
        params![now, session_id.as_bytes()],
    )?;
    tx.commit()?;
    Ok(inserted == 1)
}

/// Claim the oldest due job, including a running job whose lease expired.
pub fn claim_next(
    conn: &mut Connection,
    now: i64,
    stale_before: i64,
) -> StoreResult<Option<SessionConsolidationJob>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row = tx
        .query_row(
            "SELECT session_id, workspace_id, project_id, generation, attempts, state \
             FROM session_consolidation_jobs \
             WHERE (state = 'pending' AND attempts < ?1 AND next_attempt_at <= ?2) \
                OR (state = 'running' AND attempts <= ?1 AND started_at <= ?3) \
             ORDER BY requested_at ASC, generation ASC \
             LIMIT 1",
            params![
                i64::from(SESSION_CONSOLIDATION_MAX_ATTEMPTS),
                now,
                stale_before
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((session, workspace, project, generation, attempts, state)) = row else {
        tx.commit()?;
        return Ok(None);
    };

    let session_id = parse_session_id(&session)?;
    let workspace_id = parse_workspace_id(&workspace)?;
    let project_id = parse_project_id(&project)?;
    let generation = u64::try_from(generation).map_err(|_| {
        StoreError::MalformedRecord("negative session consolidation generation".into())
    })?;
    let attempts = u32::try_from(attempts).map_err(|_| {
        StoreError::MalformedRecord("invalid session consolidation attempt count".into())
    })?;
    let claimed_attempts = match state.as_str() {
        "pending" => attempts.saturating_add(1),
        "running" => attempts,
        other => {
            return Err(StoreError::MalformedRecord(format!(
                "invalid claimable session consolidation state: {other}"
            )));
        }
    };
    let attempt_spent = state == "pending";
    let claim_id = Uuid::new_v4();
    let changed = tx.execute(
        "UPDATE session_consolidation_jobs \
         SET state = 'running', started_at = ?1, completed_at = NULL, \
             attempts = CASE WHEN state = 'pending' THEN attempts + 1 ELSE attempts END, \
             claim_id = ?2, last_error = NULL \
         WHERE session_id = ?3 AND generation = ?4 \
           AND attempts = ?5 \
           AND ((state = 'pending' AND next_attempt_at <= ?1) \
             OR (state = 'running' AND started_at <= ?6))",
        params![
            now,
            claim_id.as_bytes(),
            session_id.as_bytes(),
            i64::try_from(generation).unwrap_or(i64::MAX),
            i64::from(attempts),
            stale_before,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidState(
            "session consolidation job changed while being claimed".into(),
        ));
    }
    tx.commit()?;
    Ok(Some(SessionConsolidationJob {
        session_id,
        workspace_id,
        project_id,
        generation,
        attempts: claimed_attempts,
        claim_id,
        attempt_spent,
    }))
}

/// Mark a claimed job complete. A stale claim cannot complete a newer lease.
pub fn complete(conn: &mut Connection, job: &SessionConsolidationJob) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let changed = conn.execute(
        "UPDATE session_consolidation_jobs \
         SET state = 'completed', completed_at = ?1, claim_id = NULL, last_error = NULL \
         WHERE session_id = ?2 AND generation = ?3 \
           AND state = 'running' AND claim_id = ?4",
        params![
            now,
            job.session_id.as_bytes(),
            generation_i64(job),
            job.claim_id.as_bytes(),
        ],
    )?;
    require_claim_update(changed)
}

/// Record a provider failure and either schedule another attempt or terminate.
pub fn fail(
    conn: &mut Connection,
    job: &SessionConsolidationJob,
    error: &str,
    retry_at: Option<i64>,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let error = truncate_error(error);
    if job.attempts >= SESSION_CONSOLIDATION_MAX_ATTEMPTS || retry_at.is_none() {
        let changed = conn.execute(
            "UPDATE session_consolidation_jobs \
             SET state = 'failed', completed_at = ?1, claim_id = NULL, last_error = ?2 \
             WHERE session_id = ?3 AND generation = ?4 \
               AND state = 'running' AND claim_id = ?5",
            params![
                now,
                error,
                job.session_id.as_bytes(),
                generation_i64(job),
                job.claim_id.as_bytes(),
            ],
        )?;
        require_claim_update(changed)
    } else {
        let retry_at = retry_at.unwrap_or(now);
        let changed = conn.execute(
            "UPDATE session_consolidation_jobs \
             SET state = 'pending', next_attempt_at = ?1, started_at = NULL, \
                 claim_id = NULL, last_error = ?2 \
             WHERE session_id = ?3 AND generation = ?4 \
               AND state = 'running' AND claim_id = ?5",
            params![
                retry_at,
                error,
                job.session_id.as_bytes(),
                generation_i64(job),
                job.claim_id.as_bytes(),
            ],
        )?;
        require_claim_update(changed)
    }
}

/// Release an in-flight claim during graceful shutdown without spending an
/// attempt. The next server process can claim it immediately.
pub fn release(conn: &mut Connection, job: &SessionConsolidationJob) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let changed = conn.execute(
        "UPDATE session_consolidation_jobs \
         SET state = 'pending', next_attempt_at = ?1, started_at = NULL, \
             attempts = CASE WHEN ?2 = 1 AND attempts > 0 THEN attempts - 1 ELSE attempts END, \
             claim_id = NULL \
         WHERE session_id = ?3 AND generation = ?4 \
           AND state = 'running' AND claim_id = ?5",
        params![
            now,
            i64::from(job.attempt_spent),
            job.session_id.as_bytes(),
            generation_i64(job),
            job.claim_id.as_bytes(),
        ],
    )?;
    require_claim_update(changed)
}

fn require_claim_update(changed: usize) -> StoreResult<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::InvalidState(
            "session consolidation claim is no longer active".into(),
        ))
    }
}

fn generation_i64(job: &SessionConsolidationJob) -> i64 {
    i64::try_from(job.generation).unwrap_or(i64::MAX)
}

fn truncate_error(error: &str) -> String {
    error.chars().take(1_024).collect()
}

fn parse_session_id(bytes: &[u8]) -> StoreResult<SessionId> {
    SessionId::from_slice(bytes).map_err(|error| StoreError::MalformedRecord(error.to_string()))
}

fn parse_workspace_id(bytes: &[u8]) -> StoreResult<WorkspaceId> {
    WorkspaceId::from_slice(bytes).map_err(|error| StoreError::MalformedRecord(error.to_string()))
}

fn parse_project_id(bytes: &[u8]) -> StoreResult<ProjectId> {
    ProjectId::from_slice(bytes).map_err(|error| StoreError::MalformedRecord(error.to_string()))
}

#[cfg(test)]
mod tests {
    use ai_memory_core::{
        AgentKind, NewObservation, NewSession, ObservationKind, Sanitized, Sanitizer,
    };

    use super::*;
    use crate::Store;

    async fn ended_session() -> (tempfile::TempDir, Store, WorkspaceId, ProjectId, SessionId) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project_id = store
            .writer
            .get_or_create_project(workspace_id, "project", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id,
                project_id,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        insert_observation(&store, workspace_id, project_id, session_id).await;
        store.writer.end_session(session_id, None).await.unwrap();
        (tmp, store, workspace_id, project_id, session_id)
    }

    async fn insert_observation(
        store: &Store,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
    ) {
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id,
                    project_id,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "continue".into(),
                    importance: 8,
                },
                &Sanitizer::default(),
            ))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn queue_is_idempotent_per_observation_generation() {
        let (_tmp, store, workspace_id, project_id, session_id) = ended_session().await;
        assert!(
            store
                .writer
                .enqueue_session_consolidation(workspace_id, project_id, session_id)
                .await
                .unwrap()
        );
        assert!(
            !store
                .writer
                .enqueue_session_consolidation(workspace_id, project_id, session_id)
                .await
                .unwrap()
        );

        let now = Timestamp::now().as_microsecond();
        let job = store
            .writer
            .claim_session_consolidation(now, now - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.session_id(), session_id);
        assert_eq!(job.generation(), 1);
        assert_eq!(job.attempts(), 1);
        store
            .writer
            .complete_session_consolidation(job)
            .await
            .unwrap();
        assert!(
            store
                .writer
                .claim_session_consolidation(now, now - 1)
                .await
                .unwrap()
                .is_none()
        );

        insert_observation(&store, workspace_id, project_id, session_id).await;
        store.writer.end_session(session_id, None).await.unwrap();
        assert!(
            store
                .writer
                .enqueue_session_consolidation(workspace_id, project_id, session_id)
                .await
                .unwrap(),
            "resumed work creates a new immutable generation"
        );
        let job = store
            .writer
            .claim_session_consolidation(Timestamp::now().as_microsecond(), now - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.generation(), 2);
    }

    #[tokio::test]
    async fn failed_claim_retries_and_graceful_release_does_not_spend_attempt() {
        let (_tmp, store, workspace_id, project_id, session_id) = ended_session().await;
        store
            .writer
            .enqueue_session_consolidation(workspace_id, project_id, session_id)
            .await
            .unwrap();
        let now = Timestamp::now().as_microsecond();
        let first = store
            .writer
            .claim_session_consolidation(now, now - 1)
            .await
            .unwrap()
            .unwrap();
        store
            .writer
            .fail_session_consolidation(first, "provider unavailable".into(), Some(now + 100))
            .await
            .unwrap();
        assert!(
            store
                .writer
                .claim_session_consolidation(now + 99, now - 1)
                .await
                .unwrap()
                .is_none()
        );
        let second = store
            .writer
            .claim_session_consolidation(now + 100, now - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.attempts(), 2);
        store
            .writer
            .release_session_consolidation(second)
            .await
            .unwrap();
        let reclaim_at = Timestamp::now().as_microsecond() + 100;
        let reclaimed = store
            .writer
            .claim_session_consolidation(reclaim_at, now - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.attempts(), 2);
    }

    #[tokio::test]
    async fn newer_generation_supersedes_older_pending_work() {
        let (_tmp, store, workspace_id, project_id, session_id) = ended_session().await;
        store
            .writer
            .enqueue_session_consolidation(workspace_id, project_id, session_id)
            .await
            .unwrap();
        insert_observation(&store, workspace_id, project_id, session_id).await;
        store.writer.end_session(session_id, None).await.unwrap();
        store
            .writer
            .enqueue_session_consolidation(workspace_id, project_id, session_id)
            .await
            .unwrap();

        let now = Timestamp::now().as_microsecond();
        let latest = store
            .writer
            .claim_session_consolidation(now, now - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.generation(), 2);
        store
            .writer
            .complete_session_consolidation(latest)
            .await
            .unwrap();
        assert!(
            store
                .writer
                .claim_session_consolidation(now, now - 1)
                .await
                .unwrap()
                .is_none(),
            "the superseded first generation must not run"
        );
    }

    #[tokio::test]
    async fn expired_lease_rejects_completion_from_the_old_claim() {
        let (_tmp, store, workspace_id, project_id, session_id) = ended_session().await;
        store
            .writer
            .enqueue_session_consolidation(workspace_id, project_id, session_id)
            .await
            .unwrap();
        let now = Timestamp::now().as_microsecond();
        let first = store
            .writer
            .claim_session_consolidation(now, now - 1)
            .await
            .unwrap()
            .unwrap();
        let second = store
            .writer
            .claim_session_consolidation(now + 100, now)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .writer
                .complete_session_consolidation(first)
                .await
                .is_err(),
            "an expired claim must not complete its replacement's lease"
        );
        assert_eq!(second.attempts(), 1);
        store
            .writer
            .release_session_consolidation(second)
            .await
            .unwrap();
        let reclaimed = store
            .writer
            .claim_session_consolidation(Timestamp::now().as_microsecond() + 100, now - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reclaimed.attempts(),
            2,
            "release after a recovered lease must retain the original attempt count"
        );
        store
            .writer
            .complete_session_consolidation(reclaimed)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn expired_final_attempt_replays_without_spending_an_extra_attempt() {
        let (_tmp, store, workspace_id, project_id, session_id) = ended_session().await;
        store
            .writer
            .enqueue_session_consolidation(workspace_id, project_id, session_id)
            .await
            .unwrap();
        let now = Timestamp::now().as_microsecond();
        for expected_attempt in 1..SESSION_CONSOLIDATION_MAX_ATTEMPTS {
            let job = store
                .writer
                .claim_session_consolidation(now, now - 1)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(job.attempts(), expected_attempt);
            store
                .writer
                .fail_session_consolidation(job, "retry".into(), Some(now))
                .await
                .unwrap();
        }
        let final_claim = store
            .writer
            .claim_session_consolidation(now, now - 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_claim.attempts(), SESSION_CONSOLIDATION_MAX_ATTEMPTS);

        let recovered = store
            .writer
            .claim_session_consolidation(now + 1, now)
            .await
            .unwrap()
            .expect("a crashed final attempt must be recovered from its expired lease");
        assert_eq!(recovered.attempts(), SESSION_CONSOLIDATION_MAX_ATTEMPTS);
        store
            .writer
            .fail_session_consolidation(recovered, "terminal".into(), None)
            .await
            .unwrap();
    }
}
