//! Core domain types and errors for ai-memory.
//!
//! This crate is the closure of the project's vocabulary: identifiers, agent
//! kinds, the workspace-wide error type, and the privacy strip (which is
//! pure-compute, no IO). Nothing in here performs I/O, which keeps it
//! trivially unit-testable and free of platform concerns.

pub mod active_project;
pub mod actor;
pub mod error;
pub mod handoff;
pub mod ids;
pub mod observation;
pub mod page;
pub mod routing_skills;
pub mod routing_snippet;
pub mod sanitize;
pub mod user;
mod workstream;

/// Default workspace name used by the single-workspace v1 flow.
pub const DEFAULT_WORKSPACE_NAME: &str = "default";

/// Defensive project fallback used only when no cwd/project is available.
pub const DEFAULT_PROJECT_NAME: &str = "scratch";

/// Reserved project holding user/team-level standing context — technology
/// preferences, code style, durable decisions that every project should
/// inherit (issue #154). Lives in [`DEFAULT_WORKSPACE_NAME`]. Default
/// `memory_query` reads union this scope with the current project; it is
/// written only through explicit `scope: "global"` requests. The leading
/// underscore follows the wiki's reserved-name convention (`_meta.md`,
/// `_pending/`, `_lint/`) and the hook router refuses to auto-attribute
/// event capture to it.
pub const GLOBAL_SCOPE_PROJECT: &str = "_global";

pub use active_project::{
    ActiveProject, ActiveProjectMode, ActorKey, DEFAULT_MAX_ENTRIES, DEFAULT_PER_KEY_TTL,
};
pub use actor::{ActorContext, AuthLevel, AuthzError, Capability};
pub use error::{MemoryError, MemoryResult};
pub use handoff::{Handoff, HandoffState, NewHandoff};
pub use ids::{
    AgentKind, AutoImproveProposalId, AutoImproveRunId, HandoffId, ManagedRunId, ObservationId,
    PageId, PagePath, ProjectId, SessionId, UserId, WorkspaceId, WorkstreamId,
};
pub use observation::{NewObservation, NewSession, Observation, ObservationKind};
pub use page::{LinkTarget, NewPage, Page, Tier};
pub use routing_snippet::{MARKER_END, MARKER_START, SNIPPET_BODY, find_marker_line, full_block};
pub use sanitize::{SanitizeConfig, Sanitized, Sanitizer};
pub use user::{MAX_EMAIL_LEN, MAX_USERNAME_LEN, NewUser, User, validate_email, validate_username};
pub use workstream::{
    FinishManagedRunRequest, FinishManagedRunResponse, LinkManagedRunRequest,
    ManagedRunContextResponse, ManagedRunStatus, NewWorkstreamEvent, PrepareManagedRunRequest,
    PrepareManagedRunResponse, WorkstreamCheckpoint, WorkstreamEvent, WorkstreamEventKind,
};
