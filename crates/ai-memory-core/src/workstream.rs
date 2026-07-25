//! Managed cross-harness workstream wire types.
//!
//! A workstream is the portable, append-only history shared by native harness
//! sessions launched through `ai-memory run`. Direct harness launches never
//! create or consume these records.

use serde::{Deserialize, Serialize};

use crate::{AgentKind, ManagedRunId, WorkstreamId};

/// Semantic event families preserved in the portable workstream ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkstreamEventKind {
    /// User, assistant, developer, system, or tool-authored message.
    Message,
    /// A historical tool invocation. It must never be replayed as pending.
    ToolCall,
    /// Completed or failed historical tool output.
    ToolResult,
    /// Native context compaction or summary boundary.
    Compaction,
    /// Repository state observed at a managed-run boundary.
    Checkpoint,
    /// Importer loss, redaction, or recovery note.
    Annotation,
}

impl WorkstreamEventKind {
    /// Canonical SQLite/wire representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Compaction => "compaction",
            Self::Checkpoint => "checkpoint",
            Self::Annotation => "annotation",
        }
    }
}

impl std::str::FromStr for WorkstreamEventKind {
    type Err = crate::MemoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "message" => Ok(Self::Message),
            "tool_call" => Ok(Self::ToolCall),
            "tool_result" => Ok(Self::ToolResult),
            "compaction" => Ok(Self::Compaction),
            "checkpoint" => Ok(Self::Checkpoint),
            "annotation" => Ok(Self::Annotation),
            other => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown workstream event kind: {other}"
            ))),
        }
    }
}

/// One normalized event uploaded from a native harness transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewWorkstreamEvent {
    /// Stable, source-derived identifier used to make retries idempotent.
    pub event_id: String,
    /// Harness that produced this event.
    pub agent: AgentKind,
    /// Native session that produced this event.
    pub native_session_id: String,
    /// Native record/block identifier when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_record_id: Option<String>,
    /// Semantic event family.
    pub kind: WorkstreamEventKind,
    /// Message role (`user`, `assistant`, `tool`, etc.) when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Sanitized, human-readable event content.
    pub content: String,
    /// Source timestamp as RFC 3339 when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    /// Adapter-specific, allow-listed metadata. Never contains credentials or
    /// opaque provider reasoning.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Repository state captured without mutating the checkout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkstreamCheckpoint {
    /// Current Git commit, when inside a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Current branch or detached-HEAD marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Stable hash of the porcelain status output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_hash: Option<String>,
    /// Changed and untracked paths, bounded by the local adapter.
    #[serde(default)]
    pub changed_paths: Vec<String>,
}

/// Request to open a lease-backed managed harness invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareManagedRunRequest {
    /// Workspace name resolved by the host CLI.
    pub workspace: String,
    /// Project name resolved by the host CLI.
    pub project: String,
    /// Canonical host working directory.
    pub cwd: String,
    /// Stable repository identity hash.
    pub repo_fingerprint: String,
    /// Stable worktree identity hash (distinct across linked worktrees).
    pub worktree_fingerprint: String,
    /// Harness being launched.
    pub agent: AgentKind,
    /// Resolve the harness from the established workstream when possible.
    /// The provisional `agent` is the newest checkout-local candidate.
    #[serde(default)]
    pub automatic_harness: bool,
    /// Checkout-local harnesses with resumable sessions. The server only uses
    /// these values when `automatic_harness` is true.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_agents: Vec<AgentKind>,
    /// Select an existing named workstream instead of the current selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream: Option<String>,
    /// Create and select a fresh named workstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_workstream: Option<String>,
    /// Diagnostic owner label (host and process id), not an authorization key.
    pub lease_owner: String,
}

/// Result of preparing a managed invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareManagedRunResponse {
    /// Selected logical workstream.
    pub workstream_id: WorkstreamId,
    /// Human-readable workstream name.
    pub workstream_name: String,
    /// Lease/run identifier exported to the child process.
    pub run_id: ManagedRunId,
    /// Harness selected by the server. Old servers omit this field; explicit
    /// harness launches remain compatible, while automatic launches fail safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_agent: Option<AgentKind>,
    /// Previously linked native session for this harness, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Adapter cursor from the last successful source import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cursor: Option<String>,
    /// First portable sequence not yet delivered to this native session.
    pub sync_after: i64,
    /// Portable high-water mark assigned to this launch.
    pub sync_through: i64,
    /// Whether this otherwise-empty workstream may adopt a pre-existing native
    /// session. Old servers omit this field, which safely defaults to fresh.
    #[serde(default)]
    pub may_adopt_existing_session: bool,
}

/// One-time startup context for harnesses without a SessionStart hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedRunContextResponse {
    /// Bounded portable context packet, or `None` when there is nothing new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Bind the actual native session selected or created by a managed launch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkManagedRunRequest {
    /// Harness-native session identifier.
    pub native_session_id: String,
}

/// Import and close request sent after the managed child exits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishManagedRunRequest {
    /// Native session observed by hooks or transcript discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Adapter-specific cursor after reading the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_cursor: Option<String>,
    /// Normalized unseen source events.
    #[serde(default)]
    pub events: Vec<NewWorkstreamEvent>,
    /// Whether this is the final import batch. Non-final batches keep the
    /// lease open and do not advance the durable source cursor.
    #[serde(default = "default_true")]
    pub complete: bool,
    /// Non-mutating repository checkpoint at child exit.
    pub checkpoint: WorkstreamCheckpoint,
    /// Explicit extraction/redaction losses.
    #[serde(default)]
    pub losses: Vec<String>,
    /// Native process exit code; absent when terminated by a signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

const fn default_true() -> bool {
    true
}

/// Result of an idempotent managed-run finish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinishManagedRunResponse {
    /// Number of new portable events inserted; duplicates are excluded.
    pub imported_events: usize,
    /// Current portable high-water mark.
    pub latest_sequence: i64,
}

/// Managed-run state returned to the host wrapper for transcript discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedRunStatus {
    /// Run identifier.
    pub run_id: ManagedRunId,
    /// Owning workstream.
    pub workstream_id: WorkstreamId,
    /// Harness being run.
    pub agent: AgentKind,
    /// Native session linked by SessionStart, if observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    /// Whether the SessionStart context packet was returned successfully.
    pub context_delivered: bool,
    /// Current run state (`active`, `finished`, or `expired`).
    pub state: String,
}

/// Stored workstream event returned by history reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkstreamEvent {
    /// Portable monotonic sequence within the workstream.
    pub sequence: i64,
    /// Stable source-derived event identifier.
    pub event_id: String,
    /// Source harness.
    pub agent: AgentKind,
    /// Source native session.
    pub native_session_id: String,
    /// Semantic event family.
    pub kind: WorkstreamEventKind,
    /// Optional message role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Sanitized event content.
    pub content: String,
    /// Source timestamp when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_prepare_response_defaults_to_no_adoption() {
        let response: PrepareManagedRunResponse = serde_json::from_value(serde_json::json!({
            "workstream_id": "018f0000-0000-7000-8000-000000000001",
            "workstream_name": "default",
            "run_id": "018f0000-0000-7000-8000-000000000002",
            "sync_after": 0,
            "sync_through": 0
        }))
        .unwrap();

        assert!(!response.may_adopt_existing_session);
        assert!(response.resolved_agent.is_none());
    }

    #[test]
    fn older_prepare_request_defaults_to_explicit_harness() {
        let request: PrepareManagedRunRequest = serde_json::from_value(serde_json::json!({
            "workspace": "default",
            "project": "memory",
            "cwd": "/repo",
            "repo_fingerprint": "repo",
            "worktree_fingerprint": "worktree",
            "agent": "codex",
            "lease_owner": "host:1"
        }))
        .unwrap();

        assert!(!request.automatic_harness);
        assert!(request.available_agents.is_empty());
    }
}
