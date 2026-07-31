//! Session and observation domain types.
//!
//! An *observation* is one ambient capture event from an agent CLI's
//! lifecycle hook — a session start, a user prompt, a tool use, etc.
//! Observations are the raw input to the consolidation pipeline; they
//! are never user-facing.

use std::path::PathBuf;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::ids::{AgentKind, ObservationId, ProjectId, SessionId, WorkspaceId};

/// Classification of a single observation, mirroring the headline
/// Claude Code / Codex lifecycle hook events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationKind {
    /// Session began (cwd captured, agent identified).
    SessionStart,
    /// User submitted a prompt.
    UserPrompt,
    /// Agent is about to use a tool.
    PreToolUse,
    /// Agent finished using a tool.
    PostToolUse,
    /// Compaction event (context window pressure).
    PreCompact,
    /// Post-compaction event (Devin-specific, after context compaction).
    PostCompaction,
    /// Agent emitted a notification.
    Notification,
    /// Agent finished its turn.
    Stop,
    /// Session ended (final).
    SessionEnd,
    /// Anything else.
    Other,
}

impl ObservationKind {
    /// Canonical kebab-case string for storage and wire format.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPrompt => "user-prompt",
            Self::PreToolUse => "pre-tool-use",
            Self::PostToolUse => "post-tool-use",
            Self::PreCompact => "pre-compact",
            Self::PostCompaction => "post-compaction",
            Self::Notification => "notification",
            Self::Stop => "stop",
            Self::SessionEnd => "session-end",
            Self::Other => "other",
        }
    }
}

impl std::str::FromStr for ObservationKind {
    type Err = crate::MemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "session-start" | "session_start" | "SessionStart" => Ok(Self::SessionStart),
            "user-prompt" | "user_prompt" | "UserPromptSubmit" => Ok(Self::UserPrompt),
            "pre-tool-use" | "pre_tool_use" | "PreToolUse" => Ok(Self::PreToolUse),
            "post-tool-use" | "post_tool_use" | "PostToolUse" => Ok(Self::PostToolUse),
            "pre-compact" | "pre_compact" | "PreCompact" => Ok(Self::PreCompact),
            "post-compaction" | "post_compaction" | "PostCompaction" => Ok(Self::PostCompaction),
            "notification" | "Notification" => Ok(Self::Notification),
            "stop" | "Stop" => Ok(Self::Stop),
            "session-end" | "session_end" | "SessionEnd" => Ok(Self::SessionEnd),
            "other" | "Other" => Ok(Self::Other),
            _ => Err(crate::MemoryError::MalformedRecord(format!(
                "unknown observation kind: {s}"
            ))),
        }
    }
}

/// Input for inserting an observation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewObservation {
    /// Owning session.
    pub session_id: SessionId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Event classification.
    pub kind: ObservationKind,
    /// Optional third-party extension namespace that supplied
    /// `source_event`. Core ai-memory event routing does not use
    /// this; it preserves custom vocabularies without expanding
    /// [`ObservationKind`].
    pub extension: Option<String>,
    /// Optional source event name from an opt-in extension vocabulary.
    /// Unknown hook events leave this unset unless the hook explicitly
    /// declared an extension namespace.
    pub source_event: Option<String>,
    /// Short title (auto-derived from event when blank).
    pub title: String,
    /// Sanitised body (may include excerpts of prompts or tool I/O).
    pub body: String,
    /// 1..=10. Default 5.
    pub importance: u8,
}

/// Materialised view of an observation row.
#[derive(Clone, Debug)]
pub struct Observation {
    /// Unique identifier.
    pub id: ObservationId,
    /// Owning session.
    pub session_id: SessionId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Event classification.
    pub kind: ObservationKind,
    /// Optional third-party extension namespace.
    pub extension: Option<String>,
    /// Optional source event name from an opt-in extension vocabulary.
    pub source_event: Option<String>,
    /// Short title.
    pub title: String,
    /// Sanitised body.
    pub body: String,
    /// 1..=10 importance.
    pub importance: u8,
    /// Wall-clock capture time.
    pub created_at: Timestamp,
}

/// Input for beginning a session row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewSession {
    /// Session identifier (caller-provided, idempotent).
    pub id: SessionId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Which agent CLI is running this session.
    pub agent_kind: AgentKind,
    /// Working directory at session start.
    pub cwd: Option<PathBuf>,
    /// Operator this session belongs to. `None` (no authenticated actor) keeps
    /// the session shared, which is the single-operator behaviour.
    #[serde(default)]
    pub actor_user: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_kind_round_trips() {
        for k in [
            ObservationKind::SessionStart,
            ObservationKind::UserPrompt,
            ObservationKind::PreToolUse,
            ObservationKind::PostToolUse,
            ObservationKind::PreCompact,
            ObservationKind::PostCompaction,
            ObservationKind::Notification,
            ObservationKind::Stop,
            ObservationKind::SessionEnd,
            ObservationKind::Other,
        ] {
            assert_eq!(k.as_str().parse::<ObservationKind>().unwrap(), k);
        }
    }

    #[test]
    fn observation_kind_accepts_snake_and_pascal() {
        assert_eq!(
            "PostToolUse".parse::<ObservationKind>().unwrap(),
            ObservationKind::PostToolUse,
        );
        assert_eq!(
            "session_start".parse::<ObservationKind>().unwrap(),
            ObservationKind::SessionStart,
        );
        assert_eq!(
            "PostCompaction".parse::<ObservationKind>().unwrap(),
            ObservationKind::PostCompaction,
        );
    }
}
