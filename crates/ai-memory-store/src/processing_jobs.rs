//! Durable session-processing (chunking) jobs.
//!
//! A job snapshots one session's observations as a manifest of identities,
//! order and body hashes (never body copies), then a worker claims it with a
//! renewable lease and a fence token, checkpoints bounded map/reduce nodes,
//! reserves every provider call attempt durably *before* the send, and
//! publishes through the existing page/proposal paths.
//!
//! Safety properties kept here rather than in the engine:
//!
//! - every worker mutation is guarded by `claim_id` + `fence`; a lost lease,
//!   purge (FK cascade), move/merge (explicit invalidation, same transaction)
//!   or pause makes the claim stale;
//! - page and proposal inserts made through [`WriterHandle::fenced`] abort in
//!   their own transaction when the claim is stale (`processing_write_fence`
//!   plus triggers from V97), so a worker holding an in-memory response cannot
//!   publish after purge, move, or lease loss;
//! - call attempts are reserved atomically and never refunded, so a restart
//!   after an uncertain send does not reset the budget;
//! - reads of manifest sources validate scope and body hash;
//! - a *new* job is admitted only below fixed unfinished-job limits
//!   ([`PROCESSING_MAX_UNFINISHED_JOBS`] server-wide,
//!   [`PROCESSING_MAX_UNFINISHED_JOBS_PER_OWNER`] per workspace/project/owner),
//!   checked in the creating transaction after the equivalent-job lookup, so a
//!   join always works at capacity and an accepted snapshot is never dropped.
//!
//! The state is derived and private; it is never needed to rebuild published
//! pages.

use std::time::Duration;

use ai_memory_core::{
    ActorContext, AutoImproveProposalId, AutoImproveRunId, IdentityKey, ObservationId,
    ObservationKind, ProcessingJobId, ProcessingJobKind, ProcessingJobState, ProcessingJobStatus,
    ProjectId, SessionId, UserId, WorkspaceId,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::reader::ReaderPool;
use crate::writer::WriterHandle;

/// Default retention for terminal or idle jobs before cleanup.
pub const PROCESSING_JOB_RETENTION_DEFAULT: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Most checkpoint nodes one job may hold.
pub const PROCESSING_MAX_NODES_PER_JOB: usize = 50_000;
/// Most unfinished jobs (queued, running, reducing, publishing, paused or over
/// budget) the server holds at once. A job that joins an equivalent one never
/// counts: admission only gates a *new* job.
pub const PROCESSING_MAX_UNFINISHED_JOBS: i64 = 128;
/// Most unfinished jobs one workspace/project/owner bucket holds at once.
pub const PROCESSING_MAX_UNFINISHED_JOBS_PER_OWNER: i64 = 16;

const MAX_JSON_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_TEXT_CHARS: usize = 1000;
const MAX_KEY_BYTES: usize = 128;
const MAX_EFFECTS_PER_JOB: i64 = 10_000;
const MAX_FINGERPRINT_BYTES: usize = 512;
const MAX_MANIFEST_PAGE: usize = 1000;
const MAX_SOURCE_PAGE: usize = 256;
const MAX_NODE_PAGE: usize = 500;
const CLAIM_SCAN_LIMIT: usize = 16;
const ACTIVE_STATES: &str = "'queued', 'running', 'reducing', 'publishing'";
const UNFINISHED_STATES: &str =
    "'queued', 'running', 'reducing', 'publishing', 'paused', 'budget_exceeded'";
const CLAIMED_STATES: &str = "'running', 'reducing', 'publishing'";
const STATUS_COLS: &str = "id, workspace_id, project_id, session_id, kind, state, chunks_total, \
     chunks_done, calls_used, max_calls, last_error, result_json, created_at, updated_at, \
     completed_at, wait_ms, exec_ms";

/// Marker text raised by the V97 fence triggers.
pub(crate) const STALE_CLAIM_MESSAGE: &str = "processing claim is stale";

/// Request to create (or coalesce into) a processing job.
#[derive(Debug, Clone)]
pub struct NewProcessingJob {
    /// Scope the observations are read from.
    pub workspace_id: WorkspaceId,
    /// Scope the observations are read from.
    pub project_id: ProjectId,
    /// Session to process.
    pub session_id: SessionId,
    /// Operation to run.
    pub kind: ProcessingJobKind,
    /// Attribution actor for the eventual publication.
    pub actor: ActorContext,
    /// Attribution author for the eventual publication.
    pub author_id: Option<UserId>,
    /// User whose access applies when the job reads or publishes
    /// (`None` = server-owned). Part of equivalence, distinct from `author_id`.
    pub viewer: Option<UserId>,
    /// Operator bucket (`None` = shared/unattributed). Part of equivalence.
    pub owner: Option<IdentityKey>,
    /// Input-affecting options. Part of equivalence.
    pub options_json: Value,
    /// Model/prompt/schema/policy/pipeline fingerprint. Part of equivalence.
    pub context_fingerprint: String,
    /// Call-attempt ceiling for the whole run. Not part of equivalence.
    pub max_calls: u32,
}

/// Outcome of [`WriterHandle::create_processing_job`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessingJobCreated {
    /// Status of the created or coalesced job.
    pub status: ProcessingJobStatus,
    /// `false` when an equivalent job already existed.
    pub created: bool,
}

/// Fence token proving a worker's current ownership of one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingClaim {
    /// Claimed job.
    pub job_id: ProcessingJobId,
    /// Unique per claim.
    pub claim_id: Uuid,
    /// Monotonic per job; a newer claim invalidates older ones.
    pub fence: u64,
}

/// A claimed job with the private data a worker needs.
#[derive(Debug, Clone)]
pub struct ClaimedProcessingJob {
    /// Token for every fenced call.
    pub claim: ProcessingClaim,
    /// Public status at claim time.
    pub status: ProcessingJobStatus,
    /// Attribution actor captured at start.
    pub actor: ActorContext,
    /// Attribution author captured at start.
    pub author_id: Option<UserId>,
    /// Viewer captured at start.
    pub viewer: Option<UserId>,
    /// Operator bucket captured at start.
    pub owner: Option<IdentityKey>,
    /// Options captured at start.
    pub options_json: Value,
    /// Context fingerprint captured at start.
    pub context_fingerprint: String,
    /// Fingerprint of the snapshot manifest.
    pub manifest_fingerprint: String,
    /// Observations in the manifest.
    pub manifest_count: u32,
    /// Total UTF-8 body bytes in the manifest.
    pub manifest_body_bytes: u64,
    /// Times the job has been claimed, this claim included.
    pub claim_count: u32,
    /// Publication identity, when publication already started.
    pub publication: Option<ProcessingPublication>,
    /// Lease end.
    pub lease_expires_at: Timestamp,
}

/// One manifest row, without the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingManifestEntry {
    /// Position in the snapshot order.
    pub ordinal: u32,
    /// Observation identity.
    pub observation_id: ObservationId,
    /// UTF-8 body length at snapshot time.
    pub body_bytes: u64,
    /// SHA-256 of the body at snapshot time.
    pub body_sha256: [u8; 32],
}

/// One validated source observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingSource {
    /// Position in the snapshot order.
    pub ordinal: u32,
    /// Observation identity.
    pub observation_id: ObservationId,
    /// Event classification.
    pub kind: ObservationKind,
    /// Stored title.
    pub title: String,
    /// Capture time.
    pub created_at: Timestamp,
    /// Whole stored body, hash-checked against the manifest.
    pub body: String,
    /// UTF-8 body length.
    pub body_bytes: u64,
    /// SHA-256 of the body.
    pub body_sha256: [u8; 32],
}

/// Checkpoint node state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingNodeState {
    /// Valid structured result persisted.
    Completed,
    /// Truncated or failed; must be subdivided or replanned, not repeated.
    Recoverable,
}

impl ProcessingNodeState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Recoverable => "recoverable",
        }
    }
}

/// Node checkpoint to write.
#[derive(Debug, Clone)]
pub struct ProcessingNodeWrite {
    /// Stable key, at most 128 bytes of `[A-Za-z0-9:_.-]`.
    pub key: String,
    /// Reduction level (0 = map).
    pub level: u32,
    /// Result state.
    pub state: ProcessingNodeState,
    /// Fingerprint of everything the node depends on.
    pub input_fingerprint: String,
    /// Sanitized, bounded (64 KiB) payload.
    pub json: Value,
}

/// Persisted node.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessingNode {
    /// Stable key.
    pub key: String,
    /// Reduction level.
    pub level: u32,
    /// Result state.
    pub state: ProcessingNodeState,
    /// Dependency fingerprint.
    pub input_fingerprint: String,
    /// Payload.
    pub json: Value,
    /// Last write.
    pub updated_at: Timestamp,
}

/// Atomic checkpoint of progress, state and nodes.
#[derive(Debug, Clone, Default)]
pub struct ProcessingCheckpoint {
    /// New phase; only `Running` or `Reducing`, never backwards from publishing.
    pub state: Option<ProcessingJobState>,
    /// Planned units.
    pub chunks_total: Option<u32>,
    /// Completed units.
    pub chunks_done: Option<u32>,
    /// Nodes to insert or replace.
    pub upsert_nodes: Vec<ProcessingNodeWrite>,
    /// Node keys to delete (dependent-node invalidation).
    pub delete_nodes: Vec<String>,
}

/// Result of reserving one provider call attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallReservation {
    /// The attempt is now durably spent; the send may proceed.
    Reserved {
        /// Attempt number across the whole job (1-based).
        attempt: u32,
        /// Attempts spent, this one included.
        calls_used: u32,
        /// Ceiling.
        max_calls: u32,
    },
    /// Budget reached: the job is `budget_exceeded` and the claim released.
    BudgetExhausted {
        /// Attempts spent.
        calls_used: u32,
        /// Ceiling.
        max_calls: u32,
    },
}

/// Publication identity stamped before any effect.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessingPublication {
    /// Idempotency identity of the publication/staging.
    pub identity: String,
    /// Planned effects, bounded.
    pub plan_json: Value,
    /// When the identity was stamped.
    pub started_at: Timestamp,
}

/// Confirmed publication effect.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessingEffect {
    /// Effect key chosen by the engine.
    pub key: String,
    /// Effect payload (ids, statuses).
    pub json: Value,
    /// When recorded.
    pub recorded_at: Timestamp,
}

/// How a claimed run ends.
#[derive(Debug, Clone)]
pub enum ProcessingOutcome {
    /// Done; stores the final result and drops intermediates.
    Completed {
        /// Final result (at most 256 KiB).
        result: Value,
    },
    /// Terminal failure; keeps derived nodes and the manifest until retention cleanup.
    Failed {
        /// Redacted reason.
        error: String,
    },
    /// Snapshot or fingerprints no longer valid; discards intermediates.
    Invalidated {
        /// Redacted reason.
        reason: String,
    },
    /// Stop without publishing; resumable through [`WriterHandle::resume_paused_processing_jobs`].
    Paused {
        /// Redacted reason.
        reason: String,
    },
    /// Budget reached; resumable only with a larger `max_calls`.
    BudgetExceeded {
        /// Redacted reason.
        reason: String,
    },
    /// Release for a later retry, keeping state, nodes and spent calls.
    Retry {
        /// Redacted reason.
        error: String,
        /// Earliest next claim; now when `None`.
        retry_at: Option<Timestamp>,
    },
}

/// Which owner buckets a status read may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingOwnerFilter {
    /// Every bucket; for root-only surfaces.
    Any,
    /// Exactly this bucket; `None` is the shared/unattributed one.
    Owner(Option<IdentityKey>),
}

/// Requests handled by the writer actor.
pub(crate) enum ProcessingOp {
    Create(Box<NewProcessingJob>),
    ClaimNext {
        lease: Duration,
    },
    Claim {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
        lease: Duration,
    },
    Renew {
        claim: ProcessingClaim,
        lease: Duration,
    },
    Release(ProcessingClaim),
    RecordCallTiming {
        claim: ProcessingClaim,
        wait_ms: u64,
        exec_ms: u64,
    },
    Checkpoint(ProcessingClaim, ProcessingCheckpoint),
    ReserveCall(ProcessingClaim),
    BeginPublication {
        claim: ProcessingClaim,
        identity: String,
        plan_json: Value,
    },
    RecordEffect {
        claim: ProcessingClaim,
        key: String,
        json: Value,
    },
    Finish(ProcessingClaim, ProcessingOutcome),
    PauseAll,
    ResumeAll,
    Cleanup {
        retention: Duration,
    },
}

/// Replies of [`ProcessingOp`].
pub(crate) enum ProcessingReply {
    Created(ProcessingJobCreated),
    Claimed(Option<Box<ClaimedProcessingJob>>),
    Unit,
    Status(ProcessingJobStatus),
    Reservation(CallReservation),
    Publication(ProcessingPublication),
    Count(u64),
}

pub(crate) fn dispatch(conn: &mut Connection, op: ProcessingOp) -> StoreResult<ProcessingReply> {
    Ok(match op {
        ProcessingOp::Create(job) => ProcessingReply::Created(create(conn, &job)?),
        ProcessingOp::ClaimNext { lease } => {
            ProcessingReply::Claimed(claim(conn, None, lease)?.map(Box::new))
        }
        ProcessingOp::Claim {
            workspace_id,
            project_id,
            job_id,
            lease,
        } => ProcessingReply::Claimed(
            claim(conn, Some((workspace_id, project_id, job_id)), lease)?.map(Box::new),
        ),
        ProcessingOp::Renew { claim, lease } => {
            renew(conn, &claim, lease)?;
            ProcessingReply::Unit
        }
        ProcessingOp::Release(claim) => {
            release(conn, &claim)?;
            ProcessingReply::Unit
        }
        ProcessingOp::RecordCallTiming {
            claim,
            wait_ms,
            exec_ms,
        } => {
            record_call_timing(conn, &claim, wait_ms, exec_ms)?;
            ProcessingReply::Unit
        }
        ProcessingOp::Checkpoint(claim, checkpoint) => {
            ProcessingReply::Status(checkpoint_job(conn, &claim, &checkpoint)?)
        }
        ProcessingOp::ReserveCall(claim) => {
            ProcessingReply::Reservation(reserve_call(conn, &claim)?)
        }
        ProcessingOp::BeginPublication {
            claim,
            identity,
            plan_json,
        } => ProcessingReply::Publication(begin_publication(conn, &claim, &identity, &plan_json)?),
        ProcessingOp::RecordEffect { claim, key, json } => {
            record_effect(conn, &claim, &key, &json)?;
            ProcessingReply::Unit
        }
        ProcessingOp::Finish(claim, outcome) => {
            ProcessingReply::Status(finish(conn, &claim, outcome)?)
        }
        ProcessingOp::PauseAll => ProcessingReply::Count(pause_all(conn)?),
        ProcessingOp::ResumeAll => ProcessingReply::Count(resume_all(conn)?),
        ProcessingOp::Cleanup { retention } => ProcessingReply::Count(cleanup(conn, retention)?),
    })
}

fn unexpected() -> StoreError {
    StoreError::InvalidState("unexpected processing reply".into())
}

impl WriterHandle {
    /// Create a job for one session snapshot, or return the equivalent one.
    ///
    /// One transaction validates the scope (the session row exists and at
    /// least one observation lies in the requested scope), snapshots
    /// observation ids, order, byte lengths and body hashes, and either
    /// inserts the job or coalesces into an existing non-failed equivalent
    /// (same scope, session, kind, owner bucket, options, context and
    /// manifest). A `budget_exceeded` equivalent with a smaller `max_calls`
    /// is raised and requeued.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for a missing or foreign session,
    /// [`StoreError::InvalidState`] for an empty session or invalid inputs.
    pub async fn create_processing_job(
        &self,
        job: NewProcessingJob,
    ) -> StoreResult<ProcessingJobCreated> {
        match self
            .processing_op(ProcessingOp::Create(Box::new(job)))
            .await?
        {
            ProcessingReply::Created(created) => Ok(created),
            _ => Err(unexpected()),
        }
    }

    /// Claim the oldest runnable job server-wide.
    ///
    /// # Errors
    /// SQL and writer errors.
    pub async fn claim_next_processing_job(
        &self,
        lease: Duration,
    ) -> StoreResult<Option<ClaimedProcessingJob>> {
        match self
            .processing_op(ProcessingOp::ClaimNext { lease })
            .await?
        {
            ProcessingReply::Claimed(job) => Ok(job.map(|job| *job)),
            _ => Err(unexpected()),
        }
    }

    /// Claim one job in a scope; `None` when foreign, held, paused or terminal.
    ///
    /// # Errors
    /// SQL and writer errors.
    pub async fn claim_processing_job(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
        lease: Duration,
    ) -> StoreResult<Option<ClaimedProcessingJob>> {
        match self
            .processing_op(ProcessingOp::Claim {
                workspace_id,
                project_id,
                job_id,
                lease,
            })
            .await?
        {
            ProcessingReply::Claimed(job) => Ok(job.map(|job| *job)),
            _ => Err(unexpected()),
        }
    }

    /// Extend the lease of a still-owned claim.
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost.
    pub async fn renew_processing_job(
        &self,
        claim: &ProcessingClaim,
        lease: Duration,
    ) -> StoreResult<()> {
        self.processing_unit(ProcessingOp::Renew {
            claim: claim.clone(),
            lease,
        })
        .await
    }

    /// Release a claim for graceful shutdown without losing progress.
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost.
    pub async fn release_processing_job(&self, claim: &ProcessingClaim) -> StoreResult<()> {
        self.processing_unit(ProcessingOp::Release(claim.clone()))
            .await
    }

    /// Atomically persist progress, phase and node changes under the fence.
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost,
    /// [`StoreError::InvalidState`] for invalid or oversized payloads.
    pub async fn checkpoint_processing_job(
        &self,
        claim: &ProcessingClaim,
        checkpoint: ProcessingCheckpoint,
    ) -> StoreResult<ProcessingJobStatus> {
        self.processing_status(ProcessingOp::Checkpoint(claim.clone(), checkpoint))
            .await
    }

    /// Add the measured queue wait and provider execution of one returned
    /// call (success or failure) to the job's cumulative `wait_ms` /
    /// `exec_ms`. Independent of [`Self::reserve_processing_call`]: it never
    /// touches `calls_used`, and a call lost to a crash simply stays
    /// unmeasured. Sums saturate at `i64::MAX`.
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost.
    pub async fn record_processing_call_timing(
        &self,
        claim: &ProcessingClaim,
        wait_ms: u64,
        exec_ms: u64,
    ) -> StoreResult<()> {
        self.processing_unit(ProcessingOp::RecordCallTiming {
            claim: claim.clone(),
            wait_ms,
            exec_ms,
        })
        .await
    }

    /// Reserve one provider call attempt before sending it. Never refunded.
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost.
    pub async fn reserve_processing_call(
        &self,
        claim: &ProcessingClaim,
    ) -> StoreResult<CallReservation> {
        match self
            .processing_op(ProcessingOp::ReserveCall(claim.clone()))
            .await?
        {
            ProcessingReply::Reservation(reservation) => Ok(reservation),
            _ => Err(unexpected()),
        }
    }

    /// Stamp the publication identity before the first effect and enter
    /// `publishing`. A job that already holds one returns the stored identity
    /// and plan; callers must use the returned value.
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost.
    pub async fn begin_processing_publication(
        &self,
        claim: &ProcessingClaim,
        identity: String,
        plan_json: Value,
    ) -> StoreResult<ProcessingPublication> {
        match self
            .processing_op(ProcessingOp::BeginPublication {
                claim: claim.clone(),
                identity,
                plan_json,
            })
            .await?
        {
            ProcessingReply::Publication(publication) => Ok(publication),
            _ => Err(unexpected()),
        }
    }

    /// Record a confirmed publication effect (idempotent per key).
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost.
    pub async fn record_processing_effect(
        &self,
        claim: &ProcessingClaim,
        key: String,
        json: Value,
    ) -> StoreResult<()> {
        self.processing_unit(ProcessingOp::RecordEffect {
            claim: claim.clone(),
            key,
            json,
        })
        .await
    }

    /// End a claimed run.
    ///
    /// # Errors
    /// [`StoreError::StaleProcessingClaim`] when ownership was lost.
    pub async fn finish_processing_job(
        &self,
        claim: &ProcessingClaim,
        outcome: ProcessingOutcome,
    ) -> StoreResult<ProcessingJobStatus> {
        self.processing_status(ProcessingOp::Finish(claim.clone(), outcome))
            .await
    }

    /// Pause every active job and clear every claim (flag off at startup).
    ///
    /// # Errors
    /// SQL and writer errors.
    pub async fn pause_processing_jobs(&self) -> StoreResult<u64> {
        self.processing_count(ProcessingOp::PauseAll).await
    }

    /// Lift [`Self::pause_processing_jobs`]: paused jobs return to their phase.
    ///
    /// # Errors
    /// SQL and writer errors.
    pub async fn resume_paused_processing_jobs(&self) -> StoreResult<u64> {
        self.processing_count(ProcessingOp::ResumeAll).await
    }

    /// Delete jobs idle for longer than `retention` (terminal, paused,
    /// budget-exceeded, abandoned), cascading manifests, nodes and effects.
    /// Jobs under a live lease are kept.
    ///
    /// # Errors
    /// SQL and writer errors.
    pub async fn cleanup_processing_jobs(&self, retention: Duration) -> StoreResult<u64> {
        self.processing_count(ProcessingOp::Cleanup { retention })
            .await
    }

    /// A handle whose every command runs with `claim` armed: page and
    /// proposal inserts abort with [`StoreError::is_stale_processing_claim`]
    /// when the claim is no longer the job's current one.
    #[must_use]
    pub fn fenced(&self, claim: &ProcessingClaim) -> Self {
        self.with_processing_fence(claim.clone())
    }

    async fn processing_unit(&self, op: ProcessingOp) -> StoreResult<()> {
        match self.processing_op(op).await? {
            ProcessingReply::Unit => Ok(()),
            _ => Err(unexpected()),
        }
    }

    async fn processing_status(&self, op: ProcessingOp) -> StoreResult<ProcessingJobStatus> {
        match self.processing_op(op).await? {
            ProcessingReply::Status(status) => Ok(status),
            _ => Err(unexpected()),
        }
    }

    async fn processing_count(&self, op: ProcessingOp) -> StoreResult<u64> {
        match self.processing_op(op).await? {
            ProcessingReply::Count(count) => Ok(count),
            _ => Err(unexpected()),
        }
    }
}

impl ReaderPool {
    /// Status of one job, `None` for an unknown id, a foreign scope or a
    /// foreign owner bucket. Works whatever the chunking flag says.
    ///
    /// # Errors
    /// SQL and pool errors.
    pub async fn processing_job_status(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
        owner: ProcessingOwnerFilter,
    ) -> StoreResult<Option<ProcessingJobStatus>> {
        let owner_key: Option<String> = match owner {
            ProcessingOwnerFilter::Any => None,
            ProcessingOwnerFilter::Owner(None) => Some(String::new()),
            ProcessingOwnerFilter::Owner(Some(identity)) => {
                if !identity.is_valid() {
                    return Ok(None);
                }
                Some(identity.storage_key())
            }
        };
        self.with_conn(move |conn| {
            query_status(
                conn,
                &format!(
                    "SELECT {STATUS_COLS} FROM processing_jobs \
                     WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
                       AND (?4 IS NULL OR owner_key = ?4)"
                ),
                params![
                    job_id.as_bytes(),
                    workspace_id.as_bytes(),
                    project_id.as_bytes(),
                    owner_key
                ],
            )
        })
        .await
    }

    /// Whether the owner's bucket holds an unfinished job (queued, running,
    /// reducing, publishing, paused or budget-exceeded) of `kind` for the
    /// session in this scope. Lets a legacy synchronous path refuse instead of
    /// bypassing a paused durable job. Scope and owner bucket are both part of
    /// the match, so another operator's or project's job is never reported.
    ///
    /// # Errors
    /// SQL and pool errors.
    pub async fn has_unfinished_processing_job(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        kind: ProcessingJobKind,
        owner: Option<IdentityKey>,
    ) -> StoreResult<bool> {
        if owner.as_ref().is_some_and(|identity| !identity.is_valid()) {
            return Ok(false);
        }
        let owner_key = owner
            .as_ref()
            .map(IdentityKey::storage_key)
            .unwrap_or_default();
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM processing_jobs \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND session_id = ?3 \
                   AND kind = ?4 AND owner_key = ?5 \
                   AND state IN ('queued', 'running', 'reducing', 'publishing', \
                                 'paused', 'budget_exceeded'))",
                params![
                    workspace_id.as_bytes().as_slice(),
                    project_id.as_bytes().as_slice(),
                    session_id.as_bytes().as_slice(),
                    kind.as_str(),
                    owner_key
                ],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Whether any job row exists in any scope.
    ///
    /// # Errors
    /// SQL and pool errors.
    pub async fn processing_jobs_exist(&self) -> StoreResult<bool> {
        self.with_conn(|conn| {
            Ok(
                conn.query_row("SELECT EXISTS(SELECT 1 FROM processing_jobs)", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .await
    }

    /// A bounded page of the manifest, ordinal order, without bodies.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown or foreign job.
    pub async fn processing_manifest_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
        after_ordinal: Option<u32>,
        limit: usize,
    ) -> StoreResult<Vec<ProcessingManifestEntry>> {
        self.with_conn(move |conn| {
            require_job_scope(conn, workspace_id, project_id, job_id)?;
            let mut stmt = conn.prepare_cached(
                "SELECT ordinal, observation_id, body_bytes, body_sha256 \
                 FROM processing_job_observations \
                 WHERE job_id = ?1 AND ordinal > ?2 ORDER BY ordinal LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![
                job_id.as_bytes(),
                after_param(after_ordinal),
                clamp_limit(limit, MAX_MANIFEST_PAGE)
            ])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(ProcessingManifestEntry {
                    ordinal: u32_col(row, 0)?,
                    observation_id: ObservationId::from_slice(&row.get::<_, Vec<u8>>(1)?)
                        .map_err(StoreError::from)?,
                    body_bytes: u64_col(row, 2)?,
                    body_sha256: hash_col(row, 3)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Read whole source bodies in manifest order, validated against the
    /// manifest hash and the job's scope. `max_bytes` bounds the batch, but a
    /// batch always carries at least one row so an oversized body can be
    /// split by the caller.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown or foreign job;
    /// [`StoreError::InvalidState`] when a source vanished, moved out of
    /// scope or changed (the job can no longer be completed) or the job was
    /// invalidated.
    pub async fn read_processing_sources(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
        after_ordinal: Option<u32>,
        limit: usize,
        max_bytes: usize,
    ) -> StoreResult<Vec<ProcessingSource>> {
        self.with_conn(move |conn| {
            let state = require_job_scope(conn, workspace_id, project_id, job_id)?;
            if state == ProcessingJobState::Invalidated {
                return Err(StoreError::InvalidState(
                    "processing job was invalidated".into(),
                ));
            }
            let mut stmt = conn.prepare_cached(
                "SELECT m.ordinal, m.observation_id, m.body_sha256, m.body_bytes, \
                        o.kind, o.title, o.created_at, o.body \
                 FROM processing_job_observations m \
                 JOIN processing_jobs j ON j.id = m.job_id \
                 LEFT JOIN observations o ON o.id = m.observation_id \
                      AND o.session_id = j.session_id \
                      AND o.workspace_id = j.workspace_id AND o.project_id = j.project_id \
                 WHERE m.job_id = ?1 AND m.ordinal > ?2 ORDER BY m.ordinal LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![
                job_id.as_bytes(),
                after_param(after_ordinal),
                clamp_limit(limit, MAX_SOURCE_PAGE)
            ])?;
            let mut out: Vec<ProcessingSource> = Vec::new();
            let mut total = 0usize;
            while let Some(row) = rows.next()? {
                let expected = hash_col(row, 2)?;
                let expected_bytes = u64_col(row, 3)?;
                let Some(kind) = row.get::<_, Option<String>>(4)? else {
                    return Err(source_changed());
                };
                let body: String = row.get(7)?;
                if body.len() as u64 != expected_bytes || sha256(body.as_bytes()) != expected {
                    return Err(source_changed());
                }
                if !out.is_empty() && total.saturating_add(body.len()) > max_bytes {
                    break;
                }
                total = total.saturating_add(body.len());
                out.push(ProcessingSource {
                    ordinal: u32_col(row, 0)?,
                    observation_id: ObservationId::from_slice(&row.get::<_, Vec<u8>>(1)?)
                        .map_err(StoreError::from)?,
                    kind: kind.parse::<ObservationKind>().map_err(StoreError::from)?,
                    title: row.get(5)?,
                    created_at: ts(row.get(6)?)?,
                    body,
                    body_bytes: expected_bytes,
                    body_sha256: expected,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Checkpoint nodes in key order.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown or foreign job.
    pub async fn processing_nodes(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
        after_key: Option<String>,
        limit: usize,
    ) -> StoreResult<Vec<ProcessingNode>> {
        self.with_conn(move |conn| {
            require_job_scope(conn, workspace_id, project_id, job_id)?;
            let mut stmt = conn.prepare_cached(
                "SELECT node_key, level, state, input_fingerprint, json, updated_at \
                 FROM processing_job_nodes WHERE job_id = ?1 AND node_key > ?2 \
                 ORDER BY node_key LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![
                job_id.as_bytes(),
                after_key.unwrap_or_default(),
                clamp_limit(limit, MAX_NODE_PAGE)
            ])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(ProcessingNode {
                    key: row.get(0)?,
                    level: u32_col(row, 1)?,
                    state: match row.get::<_, String>(2)?.as_str() {
                        "completed" => ProcessingNodeState::Completed,
                        "recoverable" => ProcessingNodeState::Recoverable,
                        other => {
                            return Err(StoreError::MalformedRecord(format!(
                                "unknown processing node state: {other}"
                            )));
                        }
                    },
                    input_fingerprint: row.get(3)?,
                    json: serde_json::from_str(&row.get::<_, String>(4)?)?,
                    updated_at: ts(row.get(5)?)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Confirmed effects of a job.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown or foreign job.
    pub async fn processing_effects(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
    ) -> StoreResult<Vec<ProcessingEffect>> {
        self.with_conn(move |conn| {
            require_job_scope(conn, workspace_id, project_id, job_id)?;
            let mut stmt = conn.prepare_cached(
                "SELECT effect_key, json, recorded_at FROM processing_job_effects \
                 WHERE job_id = ?1 ORDER BY effect_key",
            )?;
            let mut rows = stmt.query(params![job_id.as_bytes()])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(ProcessingEffect {
                    key: row.get(0)?,
                    json: serde_json::from_str(&row.get::<_, String>(1)?)?,
                    recorded_at: ts(row.get(2)?)?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// The auto-improve run a job staged, found through the
    /// `config_json.processing_job` stamp, with its proposal ids in staging
    /// order. Lets a restart reconcile a stage that committed before its
    /// effect was recorded.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown or foreign job.
    pub async fn auto_improve_run_for_processing_job(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
    ) -> StoreResult<Option<(AutoImproveRunId, Vec<AutoImproveProposalId>)>> {
        self.with_conn(move |conn| {
            require_job_scope(conn, workspace_id, project_id, job_id)?;
            let run: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT id FROM auto_improve_runs \
                     WHERE workspace_id = ?1 AND project_id = ?2 \
                       AND json_extract(config_json, '$.processing_job') = ?3 \
                     ORDER BY created_at, rowid LIMIT 1",
                    params![
                        workspace_id.as_bytes(),
                        project_id.as_bytes(),
                        job_id.0.to_string()
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(run) = run else {
                return Ok(None);
            };
            let mut stmt = conn.prepare_cached(
                "SELECT id FROM auto_improve_proposals WHERE run_id = ?1 \
                 ORDER BY staged_at, rowid",
            )?;
            let proposals = stmt
                .query_map(params![run], |row| row.get::<_, Vec<u8>>(0))?
                .map(|id| AutoImproveProposalId::from_slice(&id?).map_err(StoreError::from))
                .collect::<StoreResult<Vec<_>>>()?;
            Ok(Some((
                AutoImproveRunId::from_slice(&run).map_err(StoreError::from)?,
                proposals,
            )))
        })
        .await
    }

    /// The stored publication identity, if publication started.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for an unknown or foreign job.
    pub async fn processing_publication(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
    ) -> StoreResult<Option<ProcessingPublication>> {
        self.with_conn(move |conn| {
            require_job_scope(conn, workspace_id, project_id, job_id)?;
            publication_of(conn, job_id)
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// helpers

fn now_us() -> i64 {
    Timestamp::now().as_microsecond()
}

fn ts(us: i64) -> StoreResult<Timestamp> {
    Timestamp::from_microsecond(us)
        .map_err(|e| StoreError::MalformedRecord(format!("invalid timestamp: {e}")))
}

fn lease_us(lease: Duration) -> i64 {
    i64::try_from(lease.as_micros()).unwrap_or(i64::MAX / 4)
}

fn after_param(after: Option<u32>) -> i64 {
    after.map_or(-1, i64::from)
}

fn clamp_limit(limit: usize, max: usize) -> i64 {
    i64::try_from(limit.clamp(1, max)).unwrap_or(1)
}

fn u32_col(row: &Row<'_>, idx: usize) -> StoreResult<u32> {
    u32::try_from(row.get::<_, i64>(idx)?)
        .map_err(|_| StoreError::MalformedRecord("processing counter out of range".into()))
}

fn u64_col(row: &Row<'_>, idx: usize) -> StoreResult<u64> {
    u64::try_from(row.get::<_, i64>(idx)?)
        .map_err(|_| StoreError::MalformedRecord("processing counter out of range".into()))
}

fn hash_col(row: &Row<'_>, idx: usize) -> StoreResult<[u8; 32]> {
    row.get::<_, Vec<u8>>(idx)?
        .try_into()
        .map_err(|_| StoreError::MalformedRecord("processing body hash is not 32 bytes".into()))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn feed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn source_changed() -> StoreError {
    StoreError::InvalidState("processing source changed".into())
}

fn stale() -> StoreError {
    StoreError::StaleProcessingClaim
}

/// Control characters stripped and length bounded; callers already redact.
fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(MAX_TEXT_CHARS)
        .collect()
}

fn bounded_json(value: &Value, max: usize, what: &str) -> StoreResult<String> {
    let text = serde_json::to_string(value)?;
    if text.len() > max {
        return Err(StoreError::InvalidState(format!(
            "{what} exceeds {max} bytes"
        )));
    }
    Ok(text)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'-'))
}

fn owner_json(owner: &IdentityKey) -> Value {
    match owner {
        IdentityKey::Subject { issuer, subject } => {
            serde_json::json!({"issuer": issuer, "subject": subject})
        }
        IdentityKey::User(user) => serde_json::json!({"user": user}),
    }
}

fn owner_from_json(text: &str) -> StoreResult<IdentityKey> {
    let value: Value = serde_json::from_str(text)?;
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    let identity = match (field("issuer"), field("subject"), field("user")) {
        (Some(issuer), Some(subject), None) => IdentityKey::Subject { issuer, subject },
        (None, None, Some(user)) => IdentityKey::User(user),
        _ => {
            return Err(StoreError::MalformedRecord(
                "processing job owner is malformed".into(),
            ));
        }
    };
    if identity.is_valid() {
        Ok(identity)
    } else {
        Err(StoreError::MalformedRecord(
            "processing job owner is not normalized".into(),
        ))
    }
}

fn status_from_row(row: &Row<'_>) -> StoreResult<ProcessingJobStatus> {
    Ok(ProcessingJobStatus {
        job_id: ProcessingJobId::from_slice(&row.get::<_, Vec<u8>>(0)?)?,
        workspace_id: WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(1)?)?,
        project_id: ProjectId::from_slice(&row.get::<_, Vec<u8>>(2)?)?,
        session_id: SessionId::from_slice(&row.get::<_, Vec<u8>>(3)?)?,
        kind: row.get::<_, String>(4)?.parse()?,
        state: row.get::<_, String>(5)?.parse()?,
        chunks_total: u32_col(row, 6)?,
        chunks_done: u32_col(row, 7)?,
        calls_used: u32_col(row, 8)?,
        max_calls: u32_col(row, 9)?,
        last_error: row.get(10)?,
        result: row
            .get::<_, Option<String>>(11)?
            .map(|text| serde_json::from_str(&text))
            .transpose()?,
        created_at: ts(row.get(12)?)?,
        updated_at: ts(row.get(13)?)?,
        completed_at: row.get::<_, Option<i64>>(14)?.map(ts).transpose()?,
        wait_ms: u64_col(row, 15)?,
        exec_ms: u64_col(row, 16)?,
    })
}

fn query_status(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> StoreResult<Option<ProcessingJobStatus>> {
    conn.query_row(sql, params, |row| Ok(status_from_row(row)))
        .optional()?
        .transpose()
}

fn status_of(conn: &Connection, job_id: ProcessingJobId) -> StoreResult<ProcessingJobStatus> {
    query_status(
        conn,
        &format!("SELECT {STATUS_COLS} FROM processing_jobs WHERE id = ?1"),
        params![job_id.as_bytes()],
    )?
    .ok_or(StoreError::StaleProcessingClaim)
}

fn require_job_scope(
    conn: &Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    job_id: ProcessingJobId,
) -> StoreResult<ProcessingJobState> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM processing_jobs \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            params![
                job_id.as_bytes(),
                workspace_id.as_bytes(),
                project_id.as_bytes()
            ],
            |row| row.get(0),
        )
        .optional()?;
    state
        .ok_or_else(|| StoreError::NotFound("processing job not found".into()))?
        .parse()
        .map_err(StoreError::from)
}

fn publication_of(
    conn: &Connection,
    job_id: ProcessingJobId,
) -> StoreResult<Option<ProcessingPublication>> {
    conn.query_row(
        "SELECT publication_identity, publication_plan_json, publication_started_at \
         FROM processing_jobs WHERE id = ?1 AND publication_identity IS NOT NULL",
        params![job_id.as_bytes()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .optional()?
    .map(|(identity, plan, at)| {
        Ok(ProcessingPublication {
            identity,
            plan_json: serde_json::from_str(&plan)?,
            started_at: ts(at)?,
        })
    })
    .transpose()
}

/// Current claim, or [`StoreError::StaleProcessingClaim`].
fn check_claim(conn: &Connection, claim: &ProcessingClaim) -> StoreResult<ProcessingJobState> {
    let state: Option<String> = conn
        .query_row(
            &format!(
                "SELECT state FROM processing_jobs \
                 WHERE id = ?1 AND claim_id = ?2 AND fence = ?3 AND state IN ({CLAIMED_STATES})"
            ),
            params![
                claim.job_id.as_bytes(),
                claim.claim_id.as_bytes(),
                i64::try_from(claim.fence).unwrap_or(i64::MAX)
            ],
            |row| row.get(0),
        )
        .optional()?;
    state.ok_or_else(stale)?.parse().map_err(StoreError::from)
}

// ---------------------------------------------------------------------------
// writer-side operations

type Scan<'a> = &'a mut dyn FnMut(u32, &[u8], [u8; 32], u64) -> StoreResult<()>;

/// Stream the session's in-scope observations in snapshot order.
fn scan_snapshot(conn: &Connection, job: &NewProcessingJob, visit: Scan<'_>) -> StoreResult<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, body FROM observations \
         WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3 \
         ORDER BY created_at, rowid",
    )?;
    let mut rows = stmt.query(params![
        job.session_id.as_bytes(),
        job.workspace_id.as_bytes(),
        job.project_id.as_bytes()
    ])?;
    let mut ordinal = 0u32;
    while let Some(row) = rows.next()? {
        let id: Vec<u8> = row.get(0)?;
        let body = row
            .get_ref(1)?
            .as_str()
            .map_err(|e| StoreError::MalformedRecord(format!("observation body: {e}")))?;
        visit(ordinal, &id, sha256(body.as_bytes()), body.len() as u64)?;
        ordinal = ordinal
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidState("session manifest is too large".into()))?;
    }
    Ok(())
}

fn validate_new(job: &NewProcessingJob) -> StoreResult<()> {
    if job.max_calls == 0 {
        return Err(StoreError::InvalidState(
            "processing job max_calls must be positive".into(),
        ));
    }
    if job.owner.as_ref().is_some_and(|owner| !owner.is_valid()) {
        return Err(StoreError::InvalidState(
            "processing job owner is not a normalized identity".into(),
        ));
    }
    let fingerprint = job.context_fingerprint.trim();
    if fingerprint.is_empty() || job.context_fingerprint.len() > MAX_FINGERPRINT_BYTES {
        return Err(StoreError::InvalidState(
            "processing job context fingerprint is empty or too long".into(),
        ));
    }
    Ok(())
}

fn create(conn: &mut Connection, job: &NewProcessingJob) -> StoreResult<ProcessingJobCreated> {
    validate_new(job)?;
    let options = bounded_json(&job.options_json, MAX_JSON_BYTES, "processing job options")?;
    let actor = serde_json::to_string(&job.actor)?;
    let owner_key = job
        .owner
        .as_ref()
        .map(IdentityKey::storage_key)
        .unwrap_or_default();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let session_scope: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT workspace_id, project_id FROM sessions WHERE id = ?1",
            params![job.session_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let in_scope: i64 = tx.query_row(
        "SELECT COUNT(*) FROM observations \
         WHERE session_id = ?1 AND workspace_id = ?2 AND project_id = ?3",
        params![
            job.session_id.as_bytes(),
            job.workspace_id.as_bytes(),
            job.project_id.as_bytes()
        ],
        |row| row.get(0),
    )?;
    if in_scope == 0 {
        let row_in_scope = session_scope.as_ref().is_some_and(|(ws, proj)| {
            ws.as_slice() == job.workspace_id.as_bytes()
                && proj.as_slice() == job.project_id.as_bytes()
        });
        return Err(if row_in_scope {
            StoreError::InvalidState("session has no observations to process".into())
        } else {
            StoreError::NotFound("session not found in this workspace/project".into())
        });
    }
    if session_scope.is_none() {
        return Err(StoreError::NotFound(
            "session not found in this workspace/project".into(),
        ));
    }

    let mut hasher = Sha256::new();
    let mut count = 0u32;
    let mut bytes = 0u64;
    scan_snapshot(&tx, job, &mut |ordinal, id, hash, len| {
        hasher.update(ordinal.to_be_bytes());
        hasher.update(id);
        hasher.update(hash);
        hasher.update(len.to_be_bytes());
        count = ordinal + 1;
        bytes = bytes.saturating_add(len);
        Ok(())
    })?;
    let manifest_fingerprint = hex(&hasher.finalize());

    let mut key = Sha256::new();
    feed(&mut key, b"processing-job-v1");
    feed(&mut key, job.workspace_id.as_bytes());
    feed(&mut key, job.project_id.as_bytes());
    feed(&mut key, job.session_id.as_bytes());
    feed(&mut key, job.kind.as_str().as_bytes());
    feed(&mut key, owner_key.as_bytes());
    // The authorization viewer is part of what a job may do (it is re-checked
    // before every effect), so a request can only join a job that carries the
    // same one: never a weaker (absent) viewer, never another user's.
    feed(
        &mut key,
        job.viewer
            .as_ref()
            .map_or(&[][..], |viewer| viewer.as_bytes().as_slice()),
    );
    feed(&mut key, options.as_bytes());
    feed(&mut key, job.context_fingerprint.as_bytes());
    feed(&mut key, manifest_fingerprint.as_bytes());
    let equivalence_key = hex(&key.finalize());
    let now = now_us();

    let existing: Option<(Vec<u8>, String, i64)> = tx
        .query_row(
            "SELECT id, state, max_calls FROM processing_jobs \
             WHERE equivalence_key = ?1 AND state NOT IN ('failed', 'invalidated')",
            params![equivalence_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((id, state, max_calls)) = existing {
        let id = ProcessingJobId::from_slice(&id)?;
        if state == ProcessingJobState::BudgetExceeded.as_str()
            && i64::from(job.max_calls) > max_calls
        {
            tx.execute(
                "UPDATE processing_jobs SET max_calls = ?2, state = 'queued', \
                     paused_from = NULL, claim_id = NULL, lease_expires_at = NULL, \
                     next_run_at = ?3, last_error = NULL, updated_at = ?3 WHERE id = ?1",
                params![id.as_bytes(), i64::from(job.max_calls), now],
            )?;
        }
        let status = status_of(&tx, id)?;
        tx.commit()?;
        return Ok(ProcessingJobCreated {
            status,
            created: false,
        });
    }

    // Admission, in this same write transaction and only for a genuinely new
    // job (joins above work at capacity). Counts use idx_processing_jobs_due
    // (state) and idx_processing_jobs_scope; the single writer serializes
    // them with the insert below.
    let unfinished: i64 = tx.query_row(
        &format!("SELECT COUNT(*) FROM processing_jobs WHERE state IN ({UNFINISHED_STATES})"),
        [],
        |row| row.get(0),
    )?;
    if unfinished >= PROCESSING_MAX_UNFINISHED_JOBS {
        return Err(StoreError::ProcessingQueueFull("server-wide limit"));
    }
    let bucket: i64 = tx.query_row(
        &format!(
            "SELECT COUNT(*) FROM processing_jobs \
             WHERE workspace_id = ?1 AND project_id = ?2 AND owner_key = ?3 \
               AND state IN ({UNFINISHED_STATES})"
        ),
        params![
            job.workspace_id.as_bytes(),
            job.project_id.as_bytes(),
            owner_key
        ],
        |row| row.get(0),
    )?;
    if bucket >= PROCESSING_MAX_UNFINISHED_JOBS_PER_OWNER {
        return Err(StoreError::ProcessingQueueFull(
            "project and operator limit",
        ));
    }

    let id = ProcessingJobId::new();
    tx.execute(
        "INSERT INTO processing_jobs \
         (id, session_id, workspace_id, project_id, kind, state, owner_key, owner_json, \
          actor_json, author_id, viewer_id, options_json, context_fingerprint, \
          manifest_fingerprint, manifest_count, manifest_body_bytes, equivalence_key, \
          max_calls, next_run_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                 ?16, ?17, ?18, ?18, ?18)",
        params![
            id.as_bytes(),
            job.session_id.as_bytes(),
            job.workspace_id.as_bytes(),
            job.project_id.as_bytes(),
            job.kind.as_str(),
            owner_key,
            job.owner
                .as_ref()
                .map(|owner| owner_json(owner).to_string()),
            actor,
            job.author_id.map(|id| id.as_bytes().to_vec()),
            job.viewer.map(|id| id.as_bytes().to_vec()),
            options,
            job.context_fingerprint,
            manifest_fingerprint,
            i64::from(count),
            i64::try_from(bytes).unwrap_or(i64::MAX),
            equivalence_key,
            i64::from(job.max_calls),
            now,
        ],
    )?;
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO processing_job_observations \
             (job_id, ordinal, observation_id, body_sha256, body_bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut check = Sha256::new();
        let mut inserted = 0u32;
        scan_snapshot(&tx, job, &mut |ordinal, obs, hash, len| {
            insert.execute(params![
                id.as_bytes(),
                i64::from(ordinal),
                obs,
                hash.as_slice(),
                i64::try_from(len).unwrap_or(i64::MAX)
            ])?;
            check.update(ordinal.to_be_bytes());
            check.update(obs);
            check.update(hash);
            check.update(len.to_be_bytes());
            inserted = ordinal + 1;
            Ok(())
        })?;
        if inserted != count || hex(&check.finalize()) != manifest_fingerprint {
            return Err(StoreError::InvalidState(
                "session changed while the processing manifest was written".into(),
            ));
        }
    }
    let status = status_of(&tx, id)?;
    tx.commit()?;
    Ok(ProcessingJobCreated {
        status,
        created: true,
    })
}

/// Every manifest entry still resolves to an in-scope observation whose body
/// has the snapshot's byte length and SHA-256. Streams one body at a time.
fn manifest_intact(conn: &Connection, id: &[u8]) -> StoreResult<bool> {
    let expected: i64 = conn.query_row(
        "SELECT manifest_count FROM processing_jobs WHERE id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare_cached(
        "SELECT m.body_sha256, m.body_bytes, o.body \
         FROM processing_job_observations m \
         JOIN processing_jobs j ON j.id = m.job_id \
         LEFT JOIN observations o ON o.id = m.observation_id \
              AND o.session_id = j.session_id \
              AND o.workspace_id = j.workspace_id AND o.project_id = j.project_id \
         WHERE m.job_id = ?1 ORDER BY m.ordinal",
    )?;
    let mut rows = stmt.query(params![id])?;
    let mut seen = 0i64;
    while let Some(row) = rows.next()? {
        seen += 1;
        let hash = hash_col(row, 0)?;
        let bytes = u64_col(row, 1)?;
        let rusqlite::types::ValueRef::Text(body) = row.get_ref(2)? else {
            return Ok(false);
        };
        if body.len() as u64 != bytes || sha256(body) != hash {
            return Ok(false);
        }
    }
    Ok(seen == expected)
}

fn drop_intermediates(conn: &Connection, id: &[u8]) -> StoreResult<()> {
    conn.execute(
        "DELETE FROM processing_job_nodes WHERE job_id = ?1",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM processing_job_observations WHERE job_id = ?1",
        params![id],
    )?;
    Ok(())
}

fn claim(
    conn: &mut Connection,
    only: Option<(WorkspaceId, ProjectId, ProcessingJobId)>,
    lease: Duration,
) -> StoreResult<Option<ClaimedProcessingJob>> {
    for _ in 0..CLAIM_SCAN_LIMIT {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_us();
        let sql = format!(
            "SELECT id FROM processing_jobs \
             WHERE state IN ({ACTIVE_STATES}) AND next_run_at <= ?1 \
               AND (claim_id IS NULL OR lease_expires_at <= ?1) \
               AND (?2 IS NULL OR (id = ?2 AND workspace_id = ?3 AND project_id = ?4)) \
             ORDER BY next_run_at, created_at LIMIT 1"
        );
        let (job, ws, proj) = match &only {
            Some((ws, proj, job)) => (
                Some(job.as_bytes().to_vec()),
                Some(ws.as_bytes().to_vec()),
                Some(proj.as_bytes().to_vec()),
            ),
            None => (None, None, None),
        };
        let id: Option<Vec<u8>> = tx
            .query_row(&sql, params![now, job, ws, proj], |row| row.get(0))
            .optional()?;
        let Some(id) = id else {
            tx.commit()?;
            return Ok(None);
        };
        if !manifest_intact(&tx, &id)? {
            tx.execute(
                "UPDATE processing_jobs SET state = 'invalidated', claim_id = NULL, \
                     lease_expires_at = NULL, completed_at = ?2, updated_at = ?2, \
                     last_error = 'source observations changed or were removed' WHERE id = ?1",
                params![id, now],
            )?;
            drop_intermediates(&tx, &id)?;
            tx.commit()?;
            if only.is_some() {
                return Ok(None);
            }
            continue;
        }
        let claim_id = Uuid::new_v4();
        let lease_end = now.saturating_add(lease_us(lease));
        tx.execute(
            "UPDATE processing_jobs SET claim_id = ?2, fence = fence + 1, \
                 claim_count = claim_count + 1, lease_expires_at = ?3, updated_at = ?4, \
                 state = CASE WHEN state = 'queued' THEN 'running' ELSE state END \
             WHERE id = ?1",
            params![id, claim_id.as_bytes(), lease_end, now],
        )?;
        let claimed = load_claimed(&tx, &id, claim_id)?;
        tx.commit()?;
        return Ok(Some(claimed));
    }
    Ok(None)
}

fn load_claimed(conn: &Connection, id: &[u8], claim_id: Uuid) -> StoreResult<ClaimedProcessingJob> {
    let job_id = ProcessingJobId::from_slice(id)?;
    let status = status_of(conn, job_id)?;
    let row = conn.query_row(
        "SELECT fence, actor_json, author_id, viewer_id, owner_json, options_json, \
                context_fingerprint, manifest_fingerprint, manifest_count, \
                manifest_body_bytes, claim_count, lease_expires_at \
         FROM processing_jobs WHERE id = ?1",
        params![id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, i64>(11)?,
            ))
        },
    )?;
    let user = |bytes: Option<Vec<u8>>| -> StoreResult<Option<UserId>> {
        bytes
            .map(|b| UserId::from_slice(&b).map_err(StoreError::from))
            .transpose()
    };
    Ok(ClaimedProcessingJob {
        claim: ProcessingClaim {
            job_id,
            claim_id,
            fence: u64::try_from(row.0).unwrap_or(0),
        },
        status,
        actor: serde_json::from_str(&row.1)?,
        author_id: user(row.2)?,
        viewer: user(row.3)?,
        owner: row.4.as_deref().map(owner_from_json).transpose()?,
        options_json: serde_json::from_str(&row.5)?,
        context_fingerprint: row.6,
        manifest_fingerprint: row.7,
        manifest_count: u32::try_from(row.8).unwrap_or(0),
        manifest_body_bytes: u64::try_from(row.9).unwrap_or(0),
        claim_count: u32::try_from(row.10).unwrap_or(0),
        publication: publication_of(conn, job_id)?,
        lease_expires_at: ts(row.11)?,
    })
}

fn renew(conn: &mut Connection, claim: &ProcessingClaim, lease: Duration) -> StoreResult<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_claim(&tx, claim)?;
    let now = now_us();
    tx.execute(
        "UPDATE processing_jobs SET lease_expires_at = ?2, updated_at = ?3 WHERE id = ?1",
        params![
            claim.job_id.as_bytes(),
            now.saturating_add(lease_us(lease)),
            now
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn release(conn: &mut Connection, claim: &ProcessingClaim) -> StoreResult<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_claim(&tx, claim)?;
    tx.execute(
        "UPDATE processing_jobs SET claim_id = NULL, lease_expires_at = NULL, updated_at = ?2 \
         WHERE id = ?1",
        params![claim.job_id.as_bytes(), now_us()],
    )?;
    tx.commit()?;
    Ok(())
}

fn record_call_timing(
    conn: &mut Connection,
    claim: &ProcessingClaim,
    wait_ms: u64,
    exec_ms: u64,
) -> StoreResult<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_claim(&tx, claim)?;
    let clamp = |ms: u64| i64::try_from(ms).unwrap_or(i64::MAX);
    tx.execute(
        "UPDATE processing_jobs SET \
           wait_ms = CASE WHEN wait_ms > 9223372036854775807 - ?2 \
                          THEN 9223372036854775807 ELSE wait_ms + ?2 END, \
           exec_ms = CASE WHEN exec_ms > 9223372036854775807 - ?3 \
                          THEN 9223372036854775807 ELSE exec_ms + ?3 END, \
           updated_at = ?4 \
         WHERE id = ?1",
        params![
            claim.job_id.as_bytes(),
            clamp(wait_ms),
            clamp(exec_ms),
            now_us()
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn checkpoint_job(
    conn: &mut Connection,
    claim: &ProcessingClaim,
    checkpoint: &ProcessingCheckpoint,
) -> StoreResult<ProcessingJobStatus> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = check_claim(&tx, claim)?;
    let id = claim.job_id.as_bytes();
    let now = now_us();
    if let Some(next) = checkpoint.state {
        let allowed = match next {
            ProcessingJobState::Running | ProcessingJobState::Reducing => {
                current != ProcessingJobState::Publishing
            }
            _ => false,
        };
        if !allowed {
            return Err(StoreError::InvalidState(
                "processing checkpoint cannot enter that state".into(),
            ));
        }
    }
    for key in &checkpoint.delete_nodes {
        tx.execute(
            "DELETE FROM processing_job_nodes WHERE job_id = ?1 AND node_key = ?2",
            params![id, key],
        )?;
    }
    for node in &checkpoint.upsert_nodes {
        if !valid_key(&node.key) {
            return Err(StoreError::InvalidState(
                "processing node key is invalid".into(),
            ));
        }
        if node.input_fingerprint.len() > MAX_FINGERPRINT_BYTES {
            return Err(StoreError::InvalidState(
                "processing node fingerprint is too long".into(),
            ));
        }
        let json = bounded_json(&node.json, MAX_JSON_BYTES, "processing node payload")?;
        tx.execute(
            "INSERT INTO processing_job_nodes \
             (job_id, node_key, level, state, input_fingerprint, json, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(job_id, node_key) DO UPDATE SET level = excluded.level, \
                 state = excluded.state, input_fingerprint = excluded.input_fingerprint, \
                 json = excluded.json, updated_at = excluded.updated_at",
            params![
                id,
                node.key,
                i64::from(node.level),
                node.state.as_str(),
                node.input_fingerprint,
                json,
                now
            ],
        )?;
    }
    let nodes: i64 = tx.query_row(
        "SELECT COUNT(*) FROM processing_job_nodes WHERE job_id = ?1",
        params![id],
        |row| row.get(0),
    )?;
    if usize::try_from(nodes).unwrap_or(usize::MAX) > PROCESSING_MAX_NODES_PER_JOB {
        return Err(StoreError::InvalidState(
            "processing job exceeds its node limit".into(),
        ));
    }
    tx.execute(
        "UPDATE processing_jobs SET chunks_total = COALESCE(?2, chunks_total), \
             chunks_done = COALESCE(?3, chunks_done), state = COALESCE(?4, state), \
             updated_at = ?5 WHERE id = ?1",
        params![
            id,
            checkpoint.chunks_total.map(i64::from),
            checkpoint.chunks_done.map(i64::from),
            checkpoint.state.map(ProcessingJobState::as_str),
            now
        ],
    )?;
    let (total, done): (i64, i64) = tx.query_row(
        "SELECT chunks_total, chunks_done FROM processing_jobs WHERE id = ?1",
        params![id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if done > total {
        return Err(StoreError::InvalidState(
            "processing chunks_done exceeds chunks_total".into(),
        ));
    }
    let status = status_of(&tx, claim.job_id)?;
    tx.commit()?;
    Ok(status)
}

fn reserve_call(conn: &mut Connection, claim: &ProcessingClaim) -> StoreResult<CallReservation> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_claim(&tx, claim)?;
    let now = now_us();
    let reserved = tx.execute(
        "UPDATE processing_jobs SET calls_used = calls_used + 1, updated_at = ?2 \
         WHERE id = ?1 AND calls_used < max_calls",
        params![claim.job_id.as_bytes(), now],
    )?;
    let (used, max): (i64, i64) = tx.query_row(
        "SELECT calls_used, max_calls FROM processing_jobs WHERE id = ?1",
        params![claim.job_id.as_bytes()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (calls_used, max_calls) = (
        u32::try_from(used).unwrap_or(u32::MAX),
        u32::try_from(max).unwrap_or(u32::MAX),
    );
    if reserved == 1 {
        tx.commit()?;
        return Ok(CallReservation::Reserved {
            attempt: calls_used,
            calls_used,
            max_calls,
        });
    }
    tx.execute(
        "UPDATE processing_jobs SET state = 'budget_exceeded', paused_from = NULL, \
             claim_id = NULL, lease_expires_at = NULL, updated_at = ?2, \
             last_error = 'call budget exhausted' WHERE id = ?1",
        params![claim.job_id.as_bytes(), now],
    )?;
    tx.commit()?;
    Ok(CallReservation::BudgetExhausted {
        calls_used,
        max_calls,
    })
}

fn begin_publication(
    conn: &mut Connection,
    claim: &ProcessingClaim,
    identity: &str,
    plan: &Value,
) -> StoreResult<ProcessingPublication> {
    if identity.trim().is_empty() || identity.len() > MAX_FINGERPRINT_BYTES {
        return Err(StoreError::InvalidState(
            "processing publication identity is empty or too long".into(),
        ));
    }
    let plan_json = bounded_json(plan, MAX_RESULT_BYTES, "processing publication plan")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_claim(&tx, claim)?;
    let now = now_us();
    tx.execute(
        "UPDATE processing_jobs SET publication_identity = ?2, publication_plan_json = ?3, \
             publication_started_at = ?4 WHERE id = ?1 AND publication_identity IS NULL",
        params![claim.job_id.as_bytes(), identity, plan_json, now],
    )?;
    tx.execute(
        "UPDATE processing_jobs SET state = 'publishing', updated_at = ?2 WHERE id = ?1",
        params![claim.job_id.as_bytes(), now],
    )?;
    let publication = publication_of(&tx, claim.job_id)?.ok_or_else(stale)?;
    tx.commit()?;
    Ok(publication)
}

fn record_effect(
    conn: &mut Connection,
    claim: &ProcessingClaim,
    key: &str,
    json: &Value,
) -> StoreResult<()> {
    if !valid_key(key) {
        return Err(StoreError::InvalidState(
            "processing effect key is invalid".into(),
        ));
    }
    let json = bounded_json(json, MAX_JSON_BYTES, "processing effect payload")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_claim(&tx, claim)?;
    tx.execute(
        "INSERT INTO processing_job_effects (job_id, effect_key, json, recorded_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(job_id, effect_key) DO UPDATE SET json = excluded.json",
        params![claim.job_id.as_bytes(), key, json, now_us()],
    )?;
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM processing_job_effects WHERE job_id = ?1",
        params![claim.job_id.as_bytes()],
        |row| row.get(0),
    )?;
    if count > MAX_EFFECTS_PER_JOB {
        return Err(StoreError::InvalidState(
            "processing job exceeds its effect limit".into(),
        ));
    }
    tx.commit()?;
    Ok(())
}

fn finish(
    conn: &mut Connection,
    claim: &ProcessingClaim,
    outcome: ProcessingOutcome,
) -> StoreResult<ProcessingJobStatus> {
    let result_text = match &outcome {
        ProcessingOutcome::Completed { result } => {
            Some(bounded_json(result, MAX_RESULT_BYTES, "processing result")?)
        }
        _ => None,
    };
    let failed = matches!(outcome, ProcessingOutcome::Failed { .. });
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_claim(&tx, claim)?;
    let id = claim.job_id.as_bytes();
    let now = now_us();
    match outcome {
        ProcessingOutcome::Completed { .. } => {
            tx.execute(
                "UPDATE processing_jobs SET state = 'completed', result_json = ?2, \
                     last_error = NULL, claim_id = NULL, lease_expires_at = NULL, \
                     completed_at = ?3, updated_at = ?3 WHERE id = ?1",
                params![id, result_text, now],
            )?;
            drop_intermediates(&tx, id)?;
        }
        ProcessingOutcome::Failed { error: reason } | ProcessingOutcome::Invalidated { reason } => {
            let state = if failed { "failed" } else { "invalidated" };
            tx.execute(
                "UPDATE processing_jobs SET state = ?2, last_error = ?3, claim_id = NULL, \
                     lease_expires_at = NULL, completed_at = ?4, updated_at = ?4 WHERE id = ?1",
                params![id, state, clean_text(&reason), now],
            )?;
            // A failed job keeps its derived nodes and manifest for
            // inspection until the normal retention cleanup deletes the job
            // (its terminal state already frees queue capacity and the
            // equivalence slot). An invalidated job's inputs are no longer
            // trustworthy, so they are discarded with the success path.
            if !failed {
                drop_intermediates(&tx, id)?;
            }
        }
        ProcessingOutcome::Paused { reason } => {
            tx.execute(
                "UPDATE processing_jobs SET paused_from = state, state = 'paused', \
                     last_error = ?2, claim_id = NULL, lease_expires_at = NULL, \
                     updated_at = ?3 WHERE id = ?1",
                params![id, clean_text(&reason), now],
            )?;
        }
        ProcessingOutcome::BudgetExceeded { reason } => {
            tx.execute(
                "UPDATE processing_jobs SET state = 'budget_exceeded', paused_from = NULL, \
                     last_error = ?2, claim_id = NULL, lease_expires_at = NULL, \
                     updated_at = ?3 WHERE id = ?1",
                params![id, clean_text(&reason), now],
            )?;
        }
        ProcessingOutcome::Retry { error, retry_at } => {
            tx.execute(
                "UPDATE processing_jobs SET last_error = ?2, claim_id = NULL, \
                     lease_expires_at = NULL, next_run_at = ?3, updated_at = ?4 WHERE id = ?1",
                params![
                    id,
                    clean_text(&error),
                    retry_at.map_or(now, |at| at.as_microsecond()),
                    now
                ],
            )?;
        }
    }
    let status = status_of(&tx, claim.job_id)?;
    tx.commit()?;
    Ok(status)
}

fn pause_all(conn: &mut Connection) -> StoreResult<u64> {
    let changed = conn.execute(
        &format!(
            "UPDATE processing_jobs SET paused_from = state, state = 'paused', claim_id = NULL, \
                 lease_expires_at = NULL, last_error = 'chunking is disabled', updated_at = ?1 \
             WHERE state IN ({ACTIVE_STATES})"
        ),
        params![now_us()],
    )?;
    Ok(changed as u64)
}

fn resume_all(conn: &mut Connection) -> StoreResult<u64> {
    let now = now_us();
    let changed = conn.execute(
        "UPDATE processing_jobs SET state = COALESCE(paused_from, 'queued'), \
             paused_from = NULL, next_run_at = ?1, last_error = NULL, updated_at = ?1 \
         WHERE state = 'paused'",
        params![now],
    )?;
    Ok(changed as u64)
}

fn cleanup(conn: &mut Connection, retention: Duration) -> StoreResult<u64> {
    let now = now_us();
    let cutoff = now.saturating_sub(lease_us(retention));
    let deleted = conn.execute(
        "DELETE FROM processing_jobs WHERE updated_at < ?1 \
           AND NOT (claim_id IS NOT NULL AND lease_expires_at > ?2)",
        params![cutoff, now],
    )?;
    Ok(deleted as u64)
}

// ---------------------------------------------------------------------------
// writer plumbing

/// A claim is current when no other claimant took the job and it is still in
/// a phase a worker may publish from. Same predicate the temp triggers apply
/// inside the effect's own transaction.
const FENCE_STALE_WHEN: &str = "EXISTS (SELECT 1 FROM temp.processing_write_fence) \
    AND NOT EXISTS ( \
        SELECT 1 FROM temp.processing_write_fence f JOIN main.processing_jobs j \
          ON j.id = f.job_id AND j.claim_id = f.claim_id AND j.fence = f.fence \
         AND j.state IN ('running', 'reducing', 'publishing'))";

/// `(trigger suffix, event, table)` guarded while a fence is armed: every
/// table a publication or staging effect writes through the writer.
const FENCED_WRITES: &[(&str, &str, &str)] = &[
    ("pages_bi", "INSERT", "pages"),
    ("pages_bu", "UPDATE", "pages"),
    ("page_evidence_bi", "INSERT", "page_evidence"),
    (
        "sessions_summary_bu",
        "UPDATE OF summary_page_id",
        "sessions",
    ),
    ("auto_improve_runs_bi", "INSERT", "auto_improve_runs"),
    (
        "auto_improve_proposals_bi",
        "INSERT",
        "auto_improve_proposals",
    ),
    (
        "auto_improve_proposals_bu",
        "UPDATE",
        "auto_improve_proposals",
    ),
    (
        "auto_improve_proposal_events_bi",
        "INSERT",
        "auto_improve_proposal_events",
    ),
    (
        "auto_improve_rejections_bi",
        "INSERT",
        "auto_improve_rejections",
    ),
];

/// Install the connection-local fence: a TEMP table holding the armed claim
/// and TEMP triggers that abort a guarded write inside its own transaction
/// when that claim is no longer the job's current one.
///
/// Temp objects belong to one connection, so a second writer connection (a
/// second `Store`, another process) can neither clear nor replace this
/// writer's armed claim. Called once when the writer thread starts.
pub(crate) fn install_write_fence(conn: &Connection) -> StoreResult<()> {
    let mut sql = String::from(
        "CREATE TEMP TABLE IF NOT EXISTS processing_write_fence ( \
             id INTEGER PRIMARY KEY CHECK (id = 1), \
             job_id BLOB NOT NULL, claim_id BLOB NOT NULL, fence INTEGER NOT NULL);\n",
    );
    for (name, event, table) in FENCED_WRITES {
        sql.push_str(&format!(
            "CREATE TEMP TRIGGER IF NOT EXISTS processing_fence_{name} \
             BEFORE {event} ON main.{table} WHEN {FENCE_STALE_WHEN} \
             BEGIN SELECT RAISE(ABORT, '{STALE_CLAIM_MESSAGE}'); END;\n"
        ));
    }
    conn.execute_batch(&sql)?;
    clear_fence(conn)
}

/// Check the claim, then arm it for the command about to run. A stale claim
/// is refused before the command executes; the triggers still re-check inside
/// each effect's transaction, which is the authoritative guard.
pub(crate) fn prepare_fence(conn: &Connection, claim: &ProcessingClaim) -> StoreResult<()> {
    check_claim(conn, claim)?;
    conn.execute(
        "INSERT OR REPLACE INTO temp.processing_write_fence (id, job_id, claim_id, fence) \
         VALUES (1, ?1, ?2, ?3)",
        params![
            claim.job_id.as_bytes().as_slice(),
            claim.claim_id.as_bytes().as_slice(),
            i64::try_from(claim.fence).unwrap_or(i64::MAX)
        ],
    )?;
    Ok(())
}

/// Disarm after a fenced command, or at writer startup.
pub(crate) fn clear_fence(conn: &Connection) -> StoreResult<()> {
    conn.execute("DELETE FROM temp.processing_write_fence", [])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// lifecycle (same transaction as the caller)

/// Invalidate active jobs of a session whose scope is about to change, drop
/// their intermediates and re-stamp every job of the session into the target.
/// Returns the number of jobs invalidated.
pub(crate) fn invalidate_and_restamp_session(
    tx: &Connection,
    session_id: &[u8],
    to_workspace: &[u8],
    to_project: &[u8],
) -> StoreResult<u64> {
    let now = now_us();
    let ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM processing_jobs WHERE session_id = ?1 \
               AND NOT (workspace_id = ?2 AND project_id = ?3) \
               AND state NOT IN ('completed', 'failed', 'invalidated')",
        )?;
        stmt.query_map(params![session_id, to_workspace, to_project], |row| {
            row.get(0)
        })?
        .collect::<Result<_, _>>()?
    };
    invalidate_ids(tx, &ids, now)?;
    tx.execute(
        "UPDATE processing_jobs SET workspace_id = ?2, project_id = ?3 \
         WHERE session_id = ?1 AND NOT (workspace_id = ?2 AND project_id = ?3)",
        params![session_id, to_workspace, to_project],
    )?;
    Ok(ids.len() as u64)
}

/// Invalidate every active job of a project before its sessions move to
/// another workspace or project (`move_project_workspace`, `merge_project`).
/// The caller re-stamps the rows (terminal jobs keep their results).
pub(crate) fn invalidate_project_jobs(tx: &Connection, project_id: &[u8]) -> StoreResult<u64> {
    let now = now_us();
    let ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM processing_jobs WHERE project_id = ?1 \
               AND state NOT IN ('completed', 'failed', 'invalidated')",
        )?;
        stmt.query_map(params![project_id], |row| row.get(0))?
            .collect::<Result<_, _>>()?
    };
    invalidate_ids(tx, &ids, now)?;
    Ok(ids.len() as u64)
}

fn invalidate_ids(tx: &Connection, ids: &[Vec<u8>], now: i64) -> StoreResult<()> {
    for id in ids {
        tx.execute(
            "UPDATE processing_jobs SET state = 'invalidated', paused_from = NULL, \
                 claim_id = NULL, lease_expires_at = NULL, completed_at = ?2, updated_at = ?2, \
                 last_error = 'session scope changed' WHERE id = ?1",
            params![id, now],
        )?;
        drop_intermediates(tx, id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use ai_memory_core::{AgentKind, NewObservation, NewSession, Sanitized, Sanitizer};

    struct Seeded {
        _tmp: tempfile::TempDir,
        store: Store,
        ws: WorkspaceId,
        proj: ProjectId,
        session: SessionId,
        job: ProcessingJobId,
    }

    async fn seeded() -> Seeded {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let session = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: session,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    occurred_at: Some(1_700_000_000_000_000),
                    session_id: session,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "t".into(),
                    body: "body".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
        let job = store
            .writer
            .create_processing_job(NewProcessingJob {
                workspace_id: ws,
                project_id: proj,
                session_id: session,
                kind: ProcessingJobKind::Consolidate,
                actor: ActorContext::default(),
                author_id: None,
                viewer: None,
                owner: None,
                options_json: serde_json::json!({}),
                context_fingerprint: "ctx".into(),
                max_calls: 4,
            })
            .await
            .unwrap()
            .status
            .job_id;
        Seeded {
            _tmp: tmp,
            store,
            ws,
            proj,
            session,
            job,
        }
    }

    /// A raw connection with its own temp fence, as a second writer would have.
    fn raw_writer(s: &Seeded) -> Connection {
        let conn = Connection::open(s.store.db_path()).unwrap();
        conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        install_write_fence(&conn).unwrap();
        conn
    }

    /// Arm without the pre-check, so only the in-transaction trigger decides.
    fn arm(conn: &Connection, claim: &ProcessingClaim) {
        conn.execute(
            "INSERT OR REPLACE INTO temp.processing_write_fence (id, job_id, claim_id, fence) \
             VALUES (1, ?1, ?2, ?3)",
            params![
                claim.job_id.as_bytes().as_slice(),
                claim.claim_id.as_bytes().as_slice(),
                claim.fence as i64
            ],
        )
        .unwrap();
    }

    fn stale_of(claim: &ProcessingClaim) -> ProcessingClaim {
        ProcessingClaim {
            fence: claim.fence + 1,
            ..claim.clone()
        }
    }

    fn guarded_statements(
        s: &Seeded,
        page: &[u8],
        proposal: &[u8],
        run: &[u8],
    ) -> Vec<(String, Vec<rusqlite::types::Value>)> {
        use rusqlite::types::Value::{Blob, Integer};
        let (ws, proj) = (s.ws.as_bytes().to_vec(), s.proj.as_bytes().to_vec());
        let session = s.session.as_bytes().to_vec();
        vec![
            (
                "INSERT INTO pages (id, workspace_id, project_id, path, title, tier, body, \
                 body_sha256, frontmatter_json, is_latest, created_at, updated_at) \
                 VALUES (randomblob(16), ?1, ?2, 'new-' || hex(randomblob(4)) || '.md', 't', \
                 'semantic', 'b', zeroblob(32), '{}', 1, 1, 1)"
                    .into(),
                vec![Blob(ws.clone()), Blob(proj.clone())],
            ),
            (
                "UPDATE pages SET title = 'edited' WHERE id = ?1".into(),
                vec![Blob(page.to_vec())],
            ),
            (
                "INSERT OR IGNORE INTO page_evidence (page_id, source_kind, source_id, created_at) \
                 VALUES (?1, 'session', 'same-source', 1)"
                    .into(),
                vec![Blob(page.to_vec())],
            ),
            (
                "UPDATE sessions SET summary_page_id = ?1 WHERE id = ?2".into(),
                vec![Blob(page.to_vec()), Blob(session)],
            ),
            (
                "INSERT INTO auto_improve_runs (id, workspace_id, project_id, \
                 proposal_actor_json, created_at) VALUES (randomblob(16), ?1, ?2, '{}', 1)"
                    .into(),
                vec![Blob(ws.clone()), Blob(proj.clone())],
            ),
            (
                "INSERT INTO auto_improve_proposals (id, run_id, workspace_id, project_id, \
                 status, operation, target_path, kind, title, confidence, rationale, \
                 body_markdown, body_sha256, artifact_path, staged_at) \
                 VALUES (randomblob(16), ?1, ?2, ?3, 'approved', 'create', \
                 't-' || hex(randomblob(4)), 'k', 't', 0.5, 'r', 'b', zeroblob(32), 'a', 1)"
                    .into(),
                vec![Blob(run.to_vec()), Blob(ws.clone()), Blob(proj.clone())],
            ),
            (
                "UPDATE auto_improve_proposals SET rationale = 'again' WHERE id = ?1".into(),
                vec![Blob(proposal.to_vec())],
            ),
            (
                "INSERT INTO auto_improve_proposal_events (proposal_id, event, actor_json, at) \
                 VALUES (?1, 'staged', '{}', 1)"
                    .into(),
                vec![Blob(proposal.to_vec())],
            ),
            (
                "INSERT INTO auto_improve_rejections (id, workspace_id, project_id, reason, \
                 normalized_fingerprint, summary, created_at) \
                 VALUES (randomblob(16), ?1, ?2, 'r', 'f', 's', ?3)"
                    .into(),
                vec![Blob(ws), Blob(proj), Integer(1)],
            ),
        ]
    }

    #[tokio::test]
    async fn every_guarded_write_aborts_for_a_stale_claim_and_commits_for_the_current_one() {
        let s = seeded().await;
        let claim = s
            .store
            .writer
            .claim_next_processing_job(Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap()
            .claim;
        let conn = raw_writer(&s);
        let page = Uuid::new_v4().as_bytes().to_vec();
        let run = Uuid::new_v4().as_bytes().to_vec();
        let proposal = Uuid::new_v4().as_bytes().to_vec();
        // Unarmed seed rows (an ordinary writer is untouched by the fence).
        conn.execute(
            "INSERT INTO pages (id, workspace_id, project_id, path, title, tier, body, \
             body_sha256, frontmatter_json, is_latest, created_at, updated_at) \
             VALUES (?1, ?2, ?3, 'seed.md', 't', 'semantic', 'b', zeroblob(32), '{}', 1, 1, 1)",
            params![
                page,
                s.ws.as_bytes().as_slice(),
                s.proj.as_bytes().as_slice()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_improve_runs (id, workspace_id, project_id, proposal_actor_json, \
             created_at) VALUES (?1, ?2, ?3, '{}', 1)",
            params![
                run,
                s.ws.as_bytes().as_slice(),
                s.proj.as_bytes().as_slice()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_improve_proposals (id, run_id, workspace_id, project_id, status, \
             operation, target_path, kind, title, confidence, rationale, body_markdown, \
             body_sha256, artifact_path, staged_at) \
             VALUES (?1, ?2, ?3, ?4, 'approved', 'create', 'seed-target', 'k', 't', 0.5, 'r', \
             'b', zeroblob(32), 'a', 1)",
            params![
                proposal,
                run,
                s.ws.as_bytes().as_slice(),
                s.proj.as_bytes().as_slice()
            ],
        )
        .unwrap();

        for (sql, args) in guarded_statements(&s, &page, &proposal, &run) {
            arm(&conn, &stale_of(&claim));
            let err = StoreError::from(
                conn.execute(&sql, rusqlite::params_from_iter(args.iter()))
                    .expect_err(&sql),
            );
            assert!(err.is_stale_processing_claim(), "{sql}: {err:?}");
            arm(&conn, &claim);
            conn.execute(&sql, rusqlite::params_from_iter(args.iter()))
                .unwrap_or_else(|e| panic!("current claim must pass {sql}: {e}"));
            clear_fence(&conn).unwrap();
        }
    }

    #[tokio::test]
    async fn one_writers_fence_cannot_be_cleared_or_replaced_by_another_connection() {
        let s = seeded().await;
        let claim = s
            .store
            .writer
            .claim_next_processing_job(Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap()
            .claim;
        let stale_writer = raw_writer(&s);
        let current_writer = raw_writer(&s);
        let insert = "INSERT INTO auto_improve_runs (id, workspace_id, project_id, \
                      proposal_actor_json, created_at) VALUES (randomblob(16), ?1, ?2, '{}', 1)";
        let (ws, proj) = (s.ws.as_bytes().to_vec(), s.proj.as_bytes().to_vec());
        arm(&stale_writer, &stale_of(&claim));
        for _ in 0..20 {
            // The other connection arms, writes and disarms in between.
            arm(&current_writer, &claim);
            current_writer.execute(insert, params![ws, proj]).unwrap();
            clear_fence(&current_writer).unwrap();
            // The stale writer is still refused: nothing reset its context.
            let err =
                StoreError::from(stale_writer.execute(insert, params![ws, proj]).unwrap_err());
            assert!(err.is_stale_processing_claim(), "{err:?}");
        }
        // An empty context on the other connection is not an authorization.
        clear_fence(&stale_writer).unwrap();
        stale_writer.execute(insert, params![ws, proj]).unwrap();
    }

    #[tokio::test]
    async fn a_source_mutation_after_claim_invalidates_the_job_and_fences_the_claim() {
        let s = seeded().await;
        let claim = s
            .store
            .writer
            .claim_next_processing_job(Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap()
            .claim;
        s.store
            .writer
            .checkpoint_processing_job(
                &claim,
                ProcessingCheckpoint {
                    chunks_total: Some(1),
                    upsert_nodes: vec![ProcessingNodeWrite {
                        key: "m:0".into(),
                        level: 0,
                        state: ProcessingNodeState::Completed,
                        input_fingerprint: "fp".into(),
                        json: serde_json::json!({"facts": ["body"]}),
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let conn = raw_writer(&s);
        conn.execute(
            "UPDATE observations SET body = 'mutated' WHERE session_id = ?1",
            params![s.session.as_bytes().as_slice()],
        )
        .unwrap();
        // Same transaction as the mutation: job invalid, claim cleared, cache gone.
        let (state, claim_id, nodes): (String, Option<Vec<u8>>, i64) = conn
            .query_row(
                "SELECT state, claim_id, \
                   (SELECT COUNT(*) FROM processing_job_nodes WHERE job_id = ?1) \
                 FROM processing_jobs WHERE id = ?1",
                params![s.job.as_bytes().as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), claim_id, nodes), ("invalidated", None, 0));
        // The worker's cached response cannot reach a guarded table.
        arm(&conn, &claim);
        let err = StoreError::from(
            conn.execute(
                "INSERT INTO auto_improve_runs (id, workspace_id, project_id, \
                 proposal_actor_json, created_at) VALUES (randomblob(16), ?1, ?2, '{}', 1)",
                params![s.ws.as_bytes().as_slice(), s.proj.as_bytes().as_slice()],
            )
            .unwrap_err(),
        );
        assert!(err.is_stale_processing_claim(), "{err:?}");
        assert!(
            s.store
                .writer
                .claim_next_processing_job(Duration::from_secs(60))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_reclaim_streams_the_hash_even_when_the_mutation_boundary_was_bypassed() {
        let s = seeded().await;
        let first = s
            .store
            .writer
            .claim_next_processing_job(Duration::ZERO)
            .await
            .unwrap()
            .unwrap()
            .claim;
        s.store
            .writer
            .checkpoint_processing_job(
                &first,
                ProcessingCheckpoint {
                    chunks_total: Some(1),
                    upsert_nodes: vec![ProcessingNodeWrite {
                        key: "m:0".into(),
                        level: 0,
                        state: ProcessingNodeState::Completed,
                        input_fingerprint: "fp".into(),
                        json: serde_json::json!({"facts": ["body"]}),
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // A same-length, same-count mutation made while the trigger does not
        // run (restored backup, `PRAGMA writable_schema`, an older binary).
        let conn = raw_writer(&s);
        conn.execute("DROP TRIGGER observations_processing_invalidate_au", [])
            .unwrap();
        conn.execute(
            "UPDATE observations SET body = 'BODY' WHERE session_id = ?1",
            params![s.session.as_bytes().as_slice()],
        )
        .unwrap();
        assert!(
            s.store
                .writer
                .claim_next_processing_job(Duration::from_secs(60))
                .await
                .unwrap()
                .is_none(),
            "cached nodes must not be reused over a changed body"
        );
        let (state, nodes): (String, i64) = conn
            .query_row(
                "SELECT state, (SELECT COUNT(*) FROM processing_job_nodes) \
                 FROM processing_jobs WHERE id = ?1",
                params![s.job.as_bytes().as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), nodes), ("invalidated", 0));
    }
}
