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
pub mod ingest_metrics;
pub use ingest_metrics::{IngestMetrics, IngestMetricsSnapshot};
pub mod ids;
pub mod observation;
pub mod page;
pub mod routing_skills;
pub mod scaffolding;
pub use scaffolding::looks_like_scaffolding;
pub mod routing_snippet;
pub mod sanitize;
pub mod slots;
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
    MidSessionRouting,
};
pub use actor::{
    ActorContext, AuthLevel, AuthzError, Capability, IdentityKey, OwnerFilter,
    SKIP_ADMISSION_CHAIN_HEADER, owner_identity, owner_stamp, parse_skip_admission_chain,
    skip_admission_chain_for,
};
pub use error::{MemoryError, MemoryResult};
pub use handoff::{
    Handoff, HandoffAcceptance, HandoffContent, HandoffLifecycle, HandoffOrigin, HandoffScope,
    HandoffState, NewHandoff,
};
pub use ids::{
    AgentKind, AutoImproveProposalId, AutoImproveRunId, EntityId, HandoffId, ManagedRunId,
    ObservationId, PageFeedbackId, PageId, PagePath, ProjectId, SessionId, UserId, WorkspaceId,
    WorkstreamId,
};
pub use observation::{NewObservation, NewSession, Observation, ObservationKind};
pub use page::{
    FeedbackKind, LinkTarget, MAX_ENTITIES_PER_PAGE, MAX_ENTITY_LEN, NewPage, Page, Tier,
    normalize_entities, normalize_entity,
};
pub use routing_snippet::{MARKER_END, MARKER_START, SNIPPET_BODY, find_marker_line, full_block};
pub use sanitize::{
    OBSERVATION_BODY_MAX_BYTES, SanitizeConfig, Sanitized, Sanitizer, truncate_utf8_bytes,
    truncate_utf8_bytes_head_tail,
};
pub use slots::{
    SLOT_PREFIX, SlotPlacement, SlotVisibility, is_slot_named, is_slot_path, slot_owner,
    slot_placement,
};
pub use user::{MAX_EMAIL_LEN, MAX_USERNAME_LEN, NewUser, User, validate_email, validate_username};
pub use workstream::{
    FinishManagedRunRequest, FinishManagedRunResponse, LinkManagedRunRequest,
    ListManagedWorkstreamsRequest, MANAGED_WORKSTREAM_PACKET_MARKER, ManagedRunContextResponse,
    ManagedRunStatus, ManagedWorkstreamSummary, NewWorkstreamEvent, PrepareManagedRunRequest,
    PrepareManagedRunResponse, RenameManagedWorkstreamRequest, RenamedManagedWorkstream,
    UNTRUSTED_MEMORY_NOTICE, WorkstreamCheckpoint, WorkstreamEvent, WorkstreamEventKind,
};
