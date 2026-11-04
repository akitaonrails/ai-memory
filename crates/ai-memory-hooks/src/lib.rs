//! Agent lifecycle hook plumbing for ai-memory.
//!
//! Wire flow:
//!
//! 1. The agent CLI (Claude Code, Codex, OpenCode) emits a lifecycle event
//!    JSON over stdin to one of the vendored hook scripts under `hooks/`.
//! 2. Native hook commands spool events locally and drain them to
//!    `POST /hook/batch` (or `POST /hook?event=<kind>&agent=<kind>` for
//!    direct integrations) with short timeouts. Scripts exit 0 so the agent
//!    never blocks on us (lesson from agentmemory #221 — hooks that `await`
//!    REST round-trips can deadlock the engine under fan-out).
//! 3. The server parses the body as JSON, runs it through bounded ingest and
//!    the [`ai_memory_core::Sanitizer`] redaction layer, then forwards a
//!    [`ai_memory_core::Sanitized<NewObservation>`] to the store writer. On
//!    `SessionEnd` it also synthesises a wiki page summarising the session via
//!    [`synth`].
//!
//! Privacy strip is a *typed* boundary: there is no way to write an
//! observation without first passing through `Sanitized::new`.
//!
//! This crate does not read process environment directly; server configuration
//! is resolved once by `ai-memory-cli` and threaded in as typed state.

pub mod antigravity;
mod assistant_capture;
mod brief_tiers;
pub mod capture_policy;
pub mod drop_list;
mod file_field;
mod grants;
pub mod hermes;
pub mod idle_end;
mod json_repair;
pub mod log;
pub mod payload;
mod recap;
pub mod router;
pub mod synth;
pub mod workstream;

pub use antigravity::{
    MAX_ANTIGRAVITY_OUTPUT_BYTES, TranscriptCursor, TranscriptPrompt, TranscriptRead,
    antigravity_conversation_id, antigravity_transcript_path, enrich_antigravity_step_output,
    is_output_eligible, read_transcript_prompts,
};

// Re-export the sanitizer types from core so callers that grew up
// pointing at this crate's `sanitize` module keep working.
pub use ai_memory_core::{SanitizeConfig, Sanitized, Sanitizer};
// Client-side symbols used by the CLI crate; the server-side `apply_assistant_backstop`
// and the protocol/table internals stay crate-private (router reaches them via
// `crate::assistant_capture`).
pub use assistant_capture::{
    ClientAssistantTransform, strip_assistant_message_raw, transform_for_client,
};
pub use capture_policy::{
    CaptureConfig, CaptureDecision, CaptureDisposition, CaptureMode, CapturePolicy,
    CaptureProtocol, CaptureSource, ExtractionState, FileTarget, GlobStep, GlobWalk,
    LookupLocation, PolicyState, ToolFamily, absolute_tool_location_paths,
    capture_lookup_locations, describe_invalid_capture_config, file_target, glob_segment_matches,
    glob_walk, repository_admits_capture, tool_call_payload, valid_call_id,
};
pub use drop_list::IngestDropList;
pub use file_field::{FILE_FIELD_KEY, FileSkip, file_field};
pub use hermes::{hermes_is_delegated_child, hermes_is_first_turn, shape_hermes_lifecycle_body};
pub use idle_end::{
    IDLE_END_BATCH, IDLE_END_INTERVAL_SECS, IdleEndTickInput, IdleEndTickOutcome, run_idle_end_tick,
};
pub use json_repair::replace_lone_surrogate_escapes;
pub use payload::{
    HookEnvelope, HookEvent, NOTIFICATION_EXCERPT_MAX_BYTES, POST_COMPACTION_EXCERPT_MAX_BYTES,
    USER_PROMPT_EXCERPT_MAX_BYTES, agent_from_payload, cap_lifecycle_body_for_client,
    hook_payload_context, user_prompt_text, user_prompt_text_for_agent,
};
pub use recap::RECAP_BUDGET_MIN;
pub use router::{
    BACKFILL_MARKER_PREFIX, CwdProbes, DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
    DEFAULT_INGEST_GATE_MAX_ENTRIES, DEFAULT_PROJECT_CACHE_MAX_ENTRIES, HookState, IngestGates,
    IngestRateLimiter, ProjectCache, ProjectCacheStore, SubagentSessionSet, SubagentSessions,
    hook_router,
};
pub use synth::{SessionDigest, synthesize_session_page};
pub use workstream::{WorkstreamState, workstream_router};

/// Request header every first-party client (native hook and CLI, the shell and
/// PowerShell hook bundles, the generated TypeScript integrations) sends to the
/// server's machine routes. The value names the client kind and is
/// informational; the server only checks that the header is present.
///
/// A web page cannot attach a custom header to a cross-site request without a
/// CORS preflight, and the machine routes answer no preflight, so the header
/// separates a first-party client from a page in the operator's browser where
/// fetch metadata is absent (see `serve`'s browser guard).
pub const CLIENT_MARKER_HEADER: &str = "x-ai-memory-client";

// Integration tests compile into this crate's test harness instead of a
// separate binary: every test binary is another link and, on macOS and
// Windows, another first-run malware scan. They still exercise only the
// public API; `extern crate self` lets them keep addressing it by crate name.
#[cfg(test)]
extern crate self as ai_memory_hooks;
#[cfg(test)]
#[path = "../tests/suite/mod.rs"]
mod integration;
