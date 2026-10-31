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

/// Splits a tool observation body into its metadata header (`tool_family:
/// …`, ids, `outcome: …`) and its output excerpt.
pub const TOOL_BODY_SEPARATOR: &str = "\n---\n";

/// Starts the header's outcome line; the outcome follows.
pub const TOOL_OUTCOME_LINE_PREFIX: &str = "\noutcome: ";

/// The header's last line when the harness reported the call as failed.
pub const FAILED_TOOL_OUTCOME_LINE: &str = "\noutcome: error";

/// Readers check the header on this many leading characters only. Every
/// header the router writes fits, so an output excerpt is never scanned.
pub const TOOL_BODY_HEADER_MAX_CHARS: usize = 512;

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
    /// Original event time in microseconds, when known (e.g. backfill
    /// replaying a transcript's own timestamps). `None` means "now" — the
    /// store fills it in at write time.
    #[serde(default)]
    pub occurred_at: Option<i64>,
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
    /// Operator this session belongs to, as an
    /// [`crate::IdentityKey::storage_key`] string. `None` (no authenticated
    /// actor, or a deployment that does not distinguish operators) keeps the
    /// session shared, which is the single-operator behaviour.
    #[serde(default)]
    pub actor_user: Option<String>,
    /// Original event time in microseconds, when known (e.g. backfill
    /// replaying a transcript's own timestamps). `None` means "now" — the
    /// store fills it in at write time.
    #[serde(default)]
    pub occurred_at: Option<i64>,
}

/// Reported cumulative token usage for one session (token-cost visibility).
/// Sourced client-side from the harness's own transcript at `session-end`;
/// see `ai-memory-cli`'s `session_usage` module. Values are cumulative
/// totals for the whole session, not deltas — the store's upsert keeps
/// `MAX(existing, reported)` per column so a replayed or out-of-order hook
/// can never lower a total (docs: session usage tracking).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NewSessionUsage {
    /// Session this usage belongs to.
    pub session_id: SessionId,
    /// Fresh (non-cached) input tokens.
    pub input_tokens: u64,
    /// Output tokens (including any reasoning/thinking breakdown the
    /// harness reports as part of the same total).
    pub output_tokens: u64,
    /// Tokens written to a prompt cache.
    pub cache_write_tokens: u64,
    /// Tokens served from a prompt cache.
    pub cache_read_tokens: u64,
    /// Last-seen model name, when the transcript records one.
    pub model: Option<String>,
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

/// Content-free usage of one model response, keyed by its native identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenCall {
    /// Stable response identity; retries update the same row.
    pub call_id: String,
    /// Fresh input tokens (cached input excluded).
    pub input_tokens: u64,
    /// Output tokens, including reasoning when reported within output.
    pub output_tokens: u64,
    /// Input tokens written to cache.
    pub cache_write_tokens: u64,
    /// Input tokens read from cache.
    pub cache_read_tokens: u64,
    /// Reported model, if available.
    pub model: Option<String>,
    /// Optional transcript provenance; absent on older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<TokenCallContext>,
}

/// Reported transcript coordinates, without prompts, tool arguments or outputs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenCallContext {
    /// Native user-turn identity, when explicitly present in the transcript.
    pub turn_id: Option<String>,
    /// Native provider response identity, when supplied independently of counters.
    pub response_id: Option<String>,
    /// RFC 3339 response timestamp reported by the harness.
    pub recorded_at: Option<String>,
    /// Bounded tool references with an explicit relationship.
    #[serde(default)]
    pub tools: Vec<TokenToolRef>,
}

/// A tool identity associated with a response or its turn; never its payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenToolRef {
    /// Native tool-call identity.
    pub id: String,
    /// Sanitized tool label.
    pub name: String,
    /// Strength and direction of the association.
    pub relation: TokenToolRelation,
    /// Explicit tool-result error flag, if the harness reported it.
    pub is_error: Option<bool>,
}

/// Exact response links remain distinct from a shared-turn association.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenToolRelation {
    /// The response itself requested this tool.
    Requested,
    /// A tool result was supplied directly before this response.
    Input,
    /// Tool activity was recorded in the same native turn; causality is unknown.
    Turn,
}

impl TokenToolRelation {
    /// Stable storage and wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Input => "input",
            Self::Turn => "turn",
        }
    }
}
