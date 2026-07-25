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

mod assistant_capture;
pub mod capture_policy;
pub mod log;
pub mod payload;
pub mod router;
pub mod synth;
pub mod workstream;

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
    CaptureConfig, CaptureDecision, CaptureDisposition, CapturePolicy, CaptureProtocol,
    CaptureSource, ExtractionState, PolicyState, ToolFamily,
};
pub use payload::{HookEnvelope, HookEvent};
pub use router::{
    DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, DEFAULT_INGEST_GATE_MAX_ENTRIES,
    DEFAULT_PROJECT_CACHE_MAX_ENTRIES, HookState, IngestGates, IngestRateLimiter, ProjectCache,
    ProjectCacheStore, SubagentSessionSet, SubagentSessions, hook_router,
};
pub use synth::synthesize_session_page;
pub use workstream::{WorkstreamState, workstream_router};
