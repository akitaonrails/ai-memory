//! Public vocabulary of durable session-processing (chunking) jobs.
//!
//! These are the only job types that cross the store boundary to clients. The
//! store keeps owner/identity buckets, claim tokens, fingerprints and raw
//! checkpoints private; [`ProcessingJobStatus`] never carries them.

use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::ids::{ProcessingJobId, ProjectId, SessionId, WorkspaceId};

/// Which existing operation a job runs once the whole session was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingJobKind {
    /// Single-page `memory_consolidate`.
    Consolidate,
    /// Multi-page `memory_consolidate`.
    ConsolidateMulti,
    /// Auto-improve session review and staging.
    AutoImprove,
}

impl ProcessingJobKind {
    /// Snake-case wire and storage form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Consolidate => "consolidate",
            Self::ConsolidateMulti => "consolidate_multi",
            Self::AutoImprove => "auto_improve",
        }
    }
}

impl fmt::Display for ProcessingJobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProcessingJobKind {
    type Err = MemoryError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "consolidate" => Ok(Self::Consolidate),
            "consolidate_multi" => Ok(Self::ConsolidateMulti),
            "auto_improve" => Ok(Self::AutoImprove),
            other => Err(MemoryError::MalformedRecord(format!(
                "unknown processing job kind: {other}"
            ))),
        }
    }
}

/// Lifecycle state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingJobState {
    /// Waiting for a worker claim.
    Queued,
    /// Map phase in progress.
    Running,
    /// Reduce phase in progress.
    Reducing,
    /// Final publication started; confirmed effects are preserved.
    Publishing,
    /// Finished; `result` is available.
    Completed,
    /// Stopped without publishing (flag turned off or operator pause).
    Paused,
    /// `max_calls` reached; resumes only through a larger compatible budget.
    BudgetExceeded,
    /// Terminal failure; `last_error` is redacted.
    Failed,
    /// The snapshot became invalid (purge, move, merge, changed content,
    /// incompatible fingerprints). Never resumed.
    Invalidated,
}

impl ProcessingJobState {
    /// Snake-case wire and storage form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Reducing => "reducing",
            Self::Publishing => "publishing",
            Self::Completed => "completed",
            Self::Paused => "paused",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Failed => "failed",
            Self::Invalidated => "invalidated",
        }
    }

    /// `true` for states no worker may leave: completed, failed, invalidated.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Invalidated)
    }
}

impl fmt::Display for ProcessingJobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProcessingJobState {
    type Err = MemoryError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "reducing" => Ok(Self::Reducing),
            "publishing" => Ok(Self::Publishing),
            "completed" => Ok(Self::Completed),
            "paused" => Ok(Self::Paused),
            "budget_exceeded" => Ok(Self::BudgetExceeded),
            "failed" => Ok(Self::Failed),
            "invalidated" => Ok(Self::Invalidated),
            other => Err(MemoryError::MalformedRecord(format!(
                "unknown processing job state: {other}"
            ))),
        }
    }
}

/// Client-visible job status. Carries no prompts, raw responses, checkpoints,
/// owner buckets or claim tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessingJobStatus {
    /// Job identifier.
    pub job_id: ProcessingJobId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session being processed.
    pub session_id: SessionId,
    /// Operation the job runs.
    pub kind: ProcessingJobKind,
    /// Current lifecycle state.
    pub state: ProcessingJobState,
    /// Chunk/node units planned so far.
    pub chunks_total: u32,
    /// Units with a valid checkpoint.
    pub chunks_done: u32,
    /// Provider call attempts reserved durably, uncertain sends included.
    pub calls_used: u32,
    /// Per-run call-attempt ceiling.
    pub max_calls: u32,
    /// Milliseconds provider calls spent queued behind other calls for the
    /// admission gate. Cumulative over calls that returned (success or
    /// failure); a call in flight when the process died is never measured.
    #[serde(default)]
    pub wait_ms: u64,
    /// Milliseconds provider calls spent executing once admitted, with the
    /// same cumulative scope as `wait_ms`.
    #[serde(default)]
    pub exec_ms: u64,
    /// Redacted, bounded reason of the last pause/failure/invalidation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Final result of a completed job (never prompts or raw responses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last durable change.
    pub updated_at: Timestamp,
    /// Terminal timestamp, when reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<Timestamp>,
}
