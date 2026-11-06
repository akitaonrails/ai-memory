//! Cross-agent handoff type.
//!
//! A handoff is a typed snapshot of "where we are" — created when one
//! agent CLI ends a session, accepted when the next one starts in the
//! same project. Stored explicitly (vs. inferring from the
//! observations log) because cross-agent continuity is the project's
//! headline feature and deserves a first-class schema.

use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    OwnerFilter,
    ids::{AgentKind, HandoffId, ProjectId, SessionId, WorkspaceId},
};

/// State machine of a single handoff row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    /// Created, not yet picked up by the next agent.
    Open,
    /// Another agent has called `memory_handoff_accept` on it.
    Accepted,
    /// Aged out (decay sweep).
    Expired,
}

impl HandoffState {
    /// Canonical wire string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Expired => "expired",
        }
    }
}

impl std::str::FromStr for HandoffState {
    type Err = crate::MemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "accepted" => Ok(Self::Accepted),
            "expired" => Ok(Self::Expired),
            other => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown handoff state: {other}"
            ))),
        }
    }
}

/// Input for inserting a new handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewHandoff {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session this handoff captures. An automatic handoff always names its
    /// session; an explicit `memory_handoff_begin` one names the caller's
    /// session only when its client forwards a hook session the server
    /// recorded in the handoff's own scope.
    pub from_session_id: Option<SessionId>,
    /// Agent CLI that produced this handoff.
    pub from_agent: AgentKind,
    /// Optional explicit target hint (`claude-code`, `codex`, …).
    pub to_agent: Option<AgentKind>,
    /// Working directory at handoff time. Used to match the next
    /// session's `memory_handoff_accept` call.
    pub cwd: Option<PathBuf>,
    /// One-paragraph summary of where we left off.
    pub summary: String,
    /// Open questions for the next agent.
    pub open_questions: Vec<String>,
    /// Suggested next steps.
    pub next_steps: Vec<String>,
    /// Files touched in the session.
    pub files_touched: Vec<String>,
    /// Operator this handoff belongs to, as an
    /// [`crate::IdentityKey::storage_key`] string. `None` publishes it to the
    /// whole project (the pre-ownership behaviour, and what a caller with no
    /// actor produces).
    #[serde(default)]
    pub owner_user: Option<String>,
}

/// Scope, ownership, and receiver metadata for an atomic handoff claim.
#[derive(Debug, Clone)]
pub struct HandoffAcceptance {
    /// Handoff being claimed.
    pub handoff_id: HandoffId,
    /// Workspace the caller resolved before the claim.
    pub workspace_id: WorkspaceId,
    /// Project the caller resolved before the claim.
    pub project_id: ProjectId,
    /// Agent CLI accepting the handoff.
    pub accepting_agent: AgentKind,
    /// Session accepting the handoff, when known.
    pub accepting_session: Option<SessionId>,
    /// Operator accepting the handoff, in [`crate::IdentityKey::storage_key`]
    /// form.
    pub accepting_user: Option<String>,
    /// Ownership boundary the caller is authorized to claim through.
    pub owner_filter: OwnerFilter,
    /// Working directory of the receiving session, used to bound automatic
    /// handoff supersession.
    pub receiving_cwd: Option<String>,
}

/// Row identity and tenancy for a handoff.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffScope {
    /// Stable identifier.
    pub id: HandoffId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
}

/// Where a handoff came from and who it's for.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffOrigin {
    /// Session that produced this handoff, if any.
    pub from_session_id: Option<SessionId>,
    /// Written explicitly through `memory_handoff_begin` rather than by the
    /// automatic SessionEnd or turn-checkpoint path. Kept off the wire: the
    /// row's shape on MCP and `/api/v1` predates it.
    #[serde(skip)]
    pub manual: bool,
    /// Agent CLI that produced this handoff.
    pub from_agent: AgentKind,
    /// Optional target hint.
    pub to_agent: Option<AgentKind>,
    /// Working directory at handoff time.
    pub cwd: Option<String>,
    /// Operator this handoff belongs to ([`crate::IdentityKey::storage_key`]
    /// form); `None` means shared with the project.
    pub owner_user: Option<String>,
}

/// The handoff payload itself.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffContent {
    /// Summary.
    pub summary: String,
    /// Open questions.
    pub open_questions: Vec<String>,
    /// Next steps.
    pub next_steps: Vec<String>,
    /// Files touched.
    pub files_touched: Vec<String>,
}

/// State machine and acceptance record for a handoff.
#[derive(Debug, Clone, Serialize)]
pub struct HandoffLifecycle {
    /// State.
    pub state: HandoffState,
    /// Creation timestamp.
    pub created_at: Timestamp,
    /// Agent CLI that accepted, if any.
    pub accepted_by: Option<AgentKind>,
    /// Acceptance timestamp.
    pub accepted_at: Option<Timestamp>,
    /// Session that accepted, if any.
    pub accepted_by_session: Option<SessionId>,
    /// Operator that accepted it. Unlike [`HandoffLifecycle::accepted_by`]
    /// (the agent CLI), this answers "which teammate took the baton".
    pub accepted_by_user: Option<String>,
}

impl HandoffOrigin {
    /// Whether this is a manual handoff: written explicitly, or left without a
    /// source session. Manual handoffs are project-wide for cwd matching, win
    /// delivery over automatic ones, and are never swept by them.
    #[must_use]
    pub const fn is_manual(&self) -> bool {
        self.manual || self.from_session_id.is_none()
    }
}

/// Materialised view of a handoff row.
///
/// Grouped into sub-structs (identity, origin, content, lifecycle) instead
/// of 18 flat fields; each is `#[serde(flatten)]`ed so the MCP wire JSON
/// stays exactly as flat as before this split.
#[derive(Debug, Clone, Serialize)]
pub struct Handoff {
    /// Row identity and tenancy.
    #[serde(flatten)]
    pub scope: HandoffScope,
    /// Where it came from and who it's for.
    #[serde(flatten)]
    pub origin: HandoffOrigin,
    /// The payload.
    #[serde(flatten)]
    pub content: HandoffContent,
    /// State machine and acceptance record.
    #[serde(flatten)]
    pub lifecycle: HandoffLifecycle,
}
