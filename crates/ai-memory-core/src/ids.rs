//! Strongly-typed identifiers for the domain.
//!
//! The 3-tuple ([`WorkspaceId`], [`ProjectId`], [`PagePath`]) is the universal
//! identity coordinate for any memory. It is baked in from M0 even though v1
//! ships single-workspace, so we never inherit basic-memory's v0.20 retrofit
//! pain (issues #783, #834, #802 and friends — see
//! `docs/issues-basic-memory.md`).

use std::fmt;
use std::path::Component;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::MemoryError;

macro_rules! id_newtype {
    ($vis:vis $name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        $vis struct $name(pub Uuid);

        impl $name {
            /// Generate a fresh v7 (time-ordered) identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Borrow the raw 16-byte big-endian representation.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            /// Reconstruct from a 16-byte slice (typically read from
            /// SQLite as a `BLOB`).
            ///
            /// # Errors
            /// Returns [`MemoryError::MalformedRecord`] if the slice is
            /// not exactly 16 bytes.
            pub fn from_slice(bytes: &[u8]) -> Result<Self, MemoryError> {
                Uuid::from_slice(bytes)
                    .map(Self)
                    .map_err(|e| MemoryError::MalformedRecord(format!("invalid uuid bytes: {e}")))
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = MemoryError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::from_str(s)
                    .map(Self)
                    .map_err(|e| MemoryError::MalformedRecord(format!("invalid uuid: {e}")))
            }
        }
    };
}

id_newtype!(pub WorkspaceId, "Workspace identifier (top of the 3-tuple).");
id_newtype!(pub ProjectId, "Project identifier (middle of the 3-tuple).");
id_newtype!(pub SessionId, "Identifier for a single agent run.");

impl SessionId {
    /// Resolve a harness-native session id to the `SessionId` the store keys
    /// on: a UUID native id is used as-is (canonicalized); any other string
    /// (Codex, OpenCode, ...) is hashed to a deterministic UUID v5, so hook
    /// POSTs and later lookups by native id share one key. The hook router and
    /// any offline tool that matches transcripts by native id (e.g.
    /// `ai-memory repair-backfill-timestamps`) must agree on this one rule.
    #[must_use]
    pub fn from_native(raw: &str) -> Self {
        Self::from_str(raw)
            .unwrap_or_else(|_| Self(Uuid::new_v5(&Uuid::NAMESPACE_OID, raw.as_bytes())))
    }
}
id_newtype!(pub ObservationId, "Identifier for a single observation captured during a session.");
id_newtype!(pub PageId, "Identifier for a single wiki page version.");
id_newtype!(pub EntityId, "Identifier for one project-scoped entity.");
id_newtype!(pub HandoffId, "Identifier for a cross-agent handoff record.");
id_newtype!(pub MessageId, "Identifier for one cross-project agent message (inbox/queue).");
id_newtype!(pub WorkstreamId, "Identifier for a managed cross-harness workstream.");
id_newtype!(pub ManagedRunId, "Identifier for one `ai-memory run` invocation.");
id_newtype!(pub UserId, "Identifier for a registered user (multi-user attribution; see [`crate::actor`]).");
id_newtype!(pub ApiCredentialId, "Identifier for one native `aim_` API credential.");
id_newtype!(pub AutoImproveRunId, "Identifier for one auto-improvement review run.");
id_newtype!(pub AutoImproveProposalId, "Identifier for one staged auto-improvement proposal.");
id_newtype!(pub PageFeedbackId, "Identifier for one page-feedback signal (`memory_feedback`).");
id_newtype!(pub ProcessingJobId, "Identifier for one durable session-processing (chunking) job.");

/// Relative path of a page within the wiki tree.
///
/// Always uses `/` as the separator (POSIX-style), normalised on construction.
/// Never starts with a slash; never contains `..` or `.` components. This
/// invariant lets the store treat paths as flat keys without re-validating.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PagePath(String);

impl PagePath {
    /// Construct from a raw string. Rejects empty, absolute, Windows-prefix,
    /// backslash-separated, and dot-segment paths.
    ///
    /// # Errors
    /// Returns [`MemoryError::InvalidPagePath`] when the input is empty or
    /// contains a path component that would escape or alias the wiki root.
    pub fn new(raw: impl Into<String>) -> Result<Self, MemoryError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(MemoryError::InvalidPagePath("empty path".into()));
        }
        if raw.starts_with('/') {
            return Err(MemoryError::InvalidPagePath(format!(
                "leading slash: {raw}"
            )));
        }
        if raw.len() >= 3 {
            let bytes = raw.as_bytes();
            if bytes[1] == b':' && bytes[2] == b'/' && bytes[0].is_ascii_alphabetic() {
                return Err(MemoryError::InvalidPagePath(format!(
                    "windows drive prefix in {raw}"
                )));
            }
        }
        if raw.contains('\\') {
            return Err(MemoryError::InvalidPagePath(format!(
                "backslash separator in {raw}"
            )));
        }
        for component in std::path::Path::new(&raw).components() {
            match component {
                Component::Normal(_) => {}
                Component::CurDir | Component::ParentDir => {
                    return Err(MemoryError::InvalidPagePath(format!(
                        "dot segment in {raw}"
                    )));
                }
                Component::Prefix(_) | Component::RootDir => {
                    return Err(MemoryError::InvalidPagePath(format!(
                        "absolute path in {raw}"
                    )));
                }
            }
        }
        for segment in raw.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(MemoryError::InvalidPagePath(format!(
                    "invalid segment in {raw}"
                )));
            }
        }
        Ok(Self(raw))
    }

    /// Reject a path that cannot be materialised and checkpointed on every
    /// supported platform.
    ///
    /// Deliberately **not** part of [`PagePath::new`]. Persisted rows are
    /// reconstructed through that constructor on every read
    /// (`reader.rs` does so in the recency, search, vector and graph
    /// queries), so tightening it would make any already-stored
    /// non-portable page unreadable — and because those are list queries,
    /// one such page would break a whole listing rather than just itself.
    /// The rule therefore applies where a *new* path enters the system.
    ///
    /// The rule is the same on every platform on purpose. A wiki authored
    /// on Linux is expected to be usable on Windows by the same release;
    /// making the check platform-conditional would let a Linux session
    /// create pages a Windows session cannot read, which is the defect
    /// being fixed rather than a fix for it (#462).
    ///
    /// # Errors
    /// Returns [`MemoryError::InvalidPagePath`] naming the offending
    /// component and the reason.
    pub fn ensure_portable(&self) -> Result<(), MemoryError> {
        for component in self.as_str().split('/') {
            ensure_portable_component(component, self.as_str())?;
        }
        Ok(())
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PagePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PagePath").field(&self.0).finish()
    }
}

impl fmt::Display for PagePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The key a case-folding or Unicode-normalizing filesystem effectively
/// stores a page path under: two paths with the same key are one file on
/// macOS (APFS) and Windows (NTFS), whatever they look like here.
///
/// Lowercase first, then compose: `İ` lowercases to `i` + U+0307, which only
/// composes back to a single scalar after the fold.
#[must_use]
pub fn portable_page_key(path: &str) -> String {
    let lowered = path.to_lowercase();
    let nfc = icu_normalizer::ComposingNormalizer::new_nfc();
    if nfc.is_normalized(&lowered) {
        lowered
    } else {
        nfc.normalize(&lowered).into_owned()
    }
}

/// Discriminator for the agent CLI that captured an observation or handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    /// Anthropic Claude Code CLI.
    ClaudeCode,
    /// OpenAI Codex CLI.
    Codex,
    /// OpenCode (open-source coding agent).
    OpenCode,
    /// Cursor IDE agent.
    Cursor,
    /// Google Gemini CLI.
    GeminiCli,
    /// Anthropic Claude Desktop.
    ClaudeDesktop,
    /// OpenClaw personal AI gateway.
    #[serde(rename = "openclaw", alias = "open-claw")]
    OpenClaw,
    /// Google Antigravity CLI (`agy`).
    AntigravityCli,
    /// Oh My Pi (`omp`) coding agent.
    Omp,
    /// Pi coding agent.
    #[serde(rename = "pi")]
    Pi,
    /// Charmbracelet Crush coding agent.
    Crush,
    /// xAI Grok Build CLI (`grok`).
    Grok,
    /// Zero coding agent (Gitlawb/zero).
    Zero,
    /// Devin CLI (Cognition).
    Devin,
    /// Kimi Code CLI (Moonshot AI).
    KimiCode,
    /// AWS Kiro CLI (verified v2 lifecycle protocol).
    KiroCli,
    /// Command Code CLI.
    CommandCode,
    /// Hermes Agent (Nous Research).
    Hermes,
    /// Pool (Poolside Agent CLI).
    Pool,
    /// ZCode (z.ai) coding agent.
    Zcode,
    /// GitHub Copilot CLI (`copilot`), configured with PascalCase hook event
    /// names for a Claude Code/VS-Code-compatible payload shape (#1040).
    CopilotCli,
    /// GrizzyBot, a native macOS agent app that posts its own lifecycle events.
    Grizzybot,
    /// Factory Droid CLI (`droid`), whose hooks send Claude Code-shaped
    /// payloads (#1240).
    Droid,
    /// Meta Muse Code (`muse`), whose hooks send Claude Code-shaped payloads
    /// with Muse's own tool names.
    Muse,
    /// Tencent CodeBuddy Code (`codebuddy`). Hooks are Claude Code-shaped
    /// (PascalCase events; `session_id`, `cwd`, `tool_name` and `tool_input`
    /// on stdin). `SessionStart` stdout's `hookSpecificOutput.additionalContext`
    /// is appended to the session (#1299).
    #[serde(rename = "codebuddy", alias = "code-buddy")]
    CodeBuddy,
    /// IBM Bob IDE, whose lifecycle hooks send Claude Code-shaped payloads
    /// but consume `SessionStart` stdout as plain context text.
    #[serde(rename = "ibm-bob", alias = "bob")]
    IbmBob,
    /// DeepSeek Harness (DSH), whose Claude Code hooks bridge
    /// (`@deepseek-ai/dsh-hooks-claude-code`) fires Claude Code-shaped
    /// payloads for seven events (no `SessionEnd` or `PreCompact`).
    Dsh,
    /// Cline CLI, whose file hooks use `taskId` plus nested `tool_call` /
    /// `tool_result` payloads. Run-start hooks are detached by default.
    Cline,
    /// Qwen Code (`qwen`), whose hooks send Claude Code-shaped payloads
    /// (`session_id`, `cwd`, `tool_name`, `tool_input`, `tool_use_id`) and
    /// whose MCP servers use Gemini CLI's `httpUrl` field in
    /// `~/.qwen/settings.json`.
    #[serde(rename = "qwen-code", alias = "qwen")]
    QwenCode,
    /// Qoder CLI and IDE. Lifecycle hooks use Claude Code-shaped payloads
    /// and inject `SessionStart` additional context.
    Qoder,
    /// Anything else (manual capture, future agents).
    Other,
}

impl AgentKind {
    /// Every variant, for tests that must cover the full agent surface —
    /// e.g. the store-level check that the persisted `sessions.agent_kind`
    /// CHECK constraint accepts every kind (the Zero integration shipped
    /// with the enum variant but without the V26 migration and only a
    /// live test caught it). Extend together with the enum.
    pub const ALL: [Self; 31] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::OpenCode,
        Self::Cursor,
        Self::GeminiCli,
        Self::ClaudeDesktop,
        Self::OpenClaw,
        Self::AntigravityCli,
        Self::Omp,
        Self::Pi,
        Self::Crush,
        Self::Grok,
        Self::Zero,
        Self::Devin,
        Self::KimiCode,
        Self::KiroCli,
        Self::CommandCode,
        Self::Hermes,
        Self::Pool,
        Self::Zcode,
        Self::CopilotCli,
        Self::Grizzybot,
        Self::Droid,
        Self::Muse,
        Self::CodeBuddy,
        Self::IbmBob,
        Self::Dsh,
        Self::Cline,
        Self::QwenCode,
        Self::Qoder,
        Self::Other,
    ];

    /// Kebab-case wire string matching the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::OpenCode => "open-code",
            Self::Cursor => "cursor",
            Self::GeminiCli => "gemini-cli",
            Self::ClaudeDesktop => "claude-desktop",
            Self::OpenClaw => "openclaw",
            Self::AntigravityCli => "antigravity-cli",
            Self::Omp => "omp",
            Self::Pi => "pi",
            Self::Crush => "crush",
            Self::Grok => "grok",
            Self::Zero => "zero",
            Self::Devin => "devin",
            Self::KimiCode => "kimi-code",
            Self::KiroCli => "kiro-cli",
            Self::CommandCode => "command-code",
            Self::Hermes => "hermes",
            Self::Pool => "pool",
            Self::Zcode => "zcode",
            Self::CopilotCli => "copilot-cli",
            Self::Grizzybot => "grizzybot",
            Self::Droid => "droid",
            Self::Muse => "muse",
            Self::CodeBuddy => "codebuddy",
            Self::IbmBob => "ibm-bob",
            Self::Dsh => "dsh",
            Self::Cline => "cline",
            Self::QwenCode => "qwen-code",
            Self::Qoder => "qoder",
            Self::Other => "other",
        }
    }

    /// Parse a known wire string or common alias. Unknown values map
    /// to [`AgentKind::Other`], which keeps hook ingestion tolerant of
    /// typos or future agents.
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "claude-code" | "claude_code" | "claude" => Self::ClaudeCode,
            "codex" => Self::Codex,
            "open-code" | "opencode" | "opencode2" | "opencode-v2" | "open-code2" => Self::OpenCode,
            "cursor" => Self::Cursor,
            "gemini-cli" | "gemini" => Self::GeminiCli,
            "claude-desktop" | "claude_desktop" => Self::ClaudeDesktop,
            "openclaw" | "open-claw" => Self::OpenClaw,
            "antigravity-cli" | "antigravity" | "agy" => Self::AntigravityCli,
            "pi" => Self::Pi,
            "crush" => Self::Crush,
            "omp" | "oh-my-pi" => Self::Omp,
            "grok" => Self::Grok,
            "zero" => Self::Zero,
            "devin" => Self::Devin,
            "kimi-code" | "kimi" => Self::KimiCode,
            "kiro-cli" | "kiro" => Self::KiroCli,
            "command-code" | "commandcode" | "cmdc" | "cmd" => Self::CommandCode,
            "hermes" | "hermes-agent" => Self::Hermes,
            "pool" | "poolside" => Self::Pool,
            "zcode" | "zai" => Self::Zcode,
            "copilot-cli" | "copilot_cli" => Self::CopilotCli,
            "grizzybot" | "grizzy-bot" => Self::Grizzybot,
            "droid" | "factory-droid" | "factory_droid" => Self::Droid,
            "muse" | "muse-code" | "musecode" => Self::Muse,
            "codebuddy" | "code-buddy" => Self::CodeBuddy,
            "ibm-bob" | "bob" => Self::IbmBob,
            "dsh" | "deepseek-harness" | "deepseek_harness" => Self::Dsh,
            "cline" | "cline-cli" => Self::Cline,
            "qwen-code" | "qwen" | "qwencode" => Self::QwenCode,
            "qoder" => Self::Qoder,
            _ => Self::Other,
        }
    }

    /// Whether this agent injects the native `session-start` hook's stdout
    /// into the resuming session as context. Agents that consume it return
    /// `true` (Claude Code reads `hookSpecificOutput.additionalContext`).
    ///
    /// Grok ignores hook stdout on `SessionStart` (per Grok's hooks guide:
    /// stdout for that event is ignored). `PostToolUse` stdout is read and
    /// `additionalContext` is shown to the model after the tool result. See
    /// [`Self::post_tool_injects_handoff`].
    /// Zero's agent loop discards the sessionStart dispatch result entirely
    /// (`internal/agent/loop.go` ignores `Dispatch`'s return there), so
    /// the native hook must NOT fetch the handoff for it: the fetch is
    /// **destructive** (the server marks the handoff accepted) and the result
    /// would be discarded — silently losing the handoff. For such agents the
    /// handoff stays available on demand via the MCP `memory_handoff_accept`
    /// tool. Unknown future agents return `false` until we know their
    /// SessionStart stdout semantics; accepting a handoff is single-use and
    /// should fail safe.
    ///
    /// Kimi Code fires `SessionStart` but discards the hook's stdout/result
    /// entirely (`packages/agent-core/src/session/index.ts` ignores the
    /// dispatch return, verified in the v0.28.1 source), so the handoff is
    /// delivered on `UserPromptSubmit` instead — see
    /// [`Self::user_prompt_injects_handoff`].
    ///
    /// Pool (Poolside Agent CLI) tolerates plain hook stdout, but model-visible
    /// context injection from `SessionStart` stdout is not demonstrated
    /// (verified against Poolside CLI v1.0.16), so Pool fails safe like other
    /// unproven agents: the handoff stays available on demand via the MCP
    /// `memory_handoff_accept` tool.
    ///
    /// ZCode (z.ai) DOES inject: a `hookSpecificOutput.additionalContext`
    /// canary printed by the session-start hook appeared verbatim inside a
    /// `<system-reminder>` text block of the first user message sent to the
    /// model (verified live against the embedded engine v0.16.5, capture logs
    /// 2026-08-28), so its native hook fetches the handoff like Claude Code's.
    ///
    /// GitHub Copilot CLI injects `SessionStart` context from a top-level
    /// `additionalContext` key (GitHub's hooks reference), not Claude Code's
    /// `hookSpecificOutput` envelope, even in its PascalCase payload mode; the
    /// native hook prints that top-level envelope for it.
    ///
    /// Factory Droid documents `SessionStart` stdout as session context and
    /// parses a JSON stdout's `hookSpecificOutput.additionalContext`, the
    /// envelope the native hook prints for Claude Code (#1240).
    ///
    /// Meta Muse Code injects a `SessionStart` stdout's
    /// `hookSpecificOutput.additionalContext` as developer context (verified
    /// offline against Muse Code 1.4.7), so it takes the same envelope.
    ///
    /// CodeBuddy Code appends that same envelope to the session (reported
    /// against v2.164.0–v2.167.0, #1299), so the native hook fetches the
    /// handoff for it. The fetch stays the built-in SessionStart path; a
    /// separate `GET /handoff` client is not part of the installer.
    ///
    /// Qwen Code injects `hookSpecificOutput.additionalContext` from
    /// `SessionStart` stdout into the session (Qwen Code hooks reference:
    /// <https://qwenlm.github.io/qwen-code-docs/en/users/features/hooks/>).
    #[must_use]
    pub fn session_start_injects_handoff(self) -> bool {
        !matches!(
            self,
            Self::Crush
                | Self::Grok
                | Self::Zero
                | Self::KimiCode
                | Self::Hermes
                | Self::Pool
                // DSH's bridge runs SessionStart detached, so its stdout
                // cannot be relied on to deliver a fetched handoff.
                | Self::Dsh
                // Cline CLI runs TaskStart/TaskResume detached by default,
                // so accepting a single-use handoff there would discard it.
                | Self::Cline
                | Self::Other
        )
    }

    /// Whether this agent injects the native `user-prompt` (UserPromptSubmit)
    /// hook's stdout into the upcoming turn as context. Only Kimi Code does:
    /// its `SessionStart` stdout is discarded (see
    /// [`Self::session_start_injects_handoff`]), while `UserPromptSubmit`
    /// stdout is prepended to the turn as a user message with
    /// `origin: {kind: 'hook_result'}` (`agent/turn/index.ts`,
    /// `session/hooks/user-prompt.ts`, verified in the v0.28.1 source).
    /// Empty stdout injects nothing, so the hook prints the raw handoff body
    /// or nothing at all — never a JSON envelope.
    ///
    /// Grok Build also ignores `SessionStart` stdout, and it is not in this
    /// set. An allowing `UserPromptSubmit` discards stdout and has no
    /// `additionalContext`. `GET /handoff` marks the handoff accepted, so
    /// fetching on that event would burn the baton. Grok shows
    /// `PostToolUse` `additionalContext` to the model; that is
    /// [`Self::post_tool_injects_handoff`].
    #[must_use]
    pub fn user_prompt_injects_handoff(self) -> bool {
        matches!(self, Self::KimiCode)
    }

    /// Whether this agent adds a JSON `context` from the native `user-prompt`
    /// hook's stdout to the upcoming turn.
    ///
    /// Hermes Agent ignores `on_session_start` stdout but adds a
    /// `pre_llm_call` hook's `{"context": "..."}` to the user message, and
    /// (`agent/turn_context.py`, `agent/shell_hooks.py`, v0.22.2). The hook
    /// checks every primary-session prompt: a resumed Hermes session reports
    /// `extra.is_first_turn = false`, but still needs to receive a handoff
    /// produced while another harness was active.
    #[must_use]
    pub fn json_context_user_prompt_injects_handoff(self) -> bool {
        matches!(self, Self::Hermes)
    }

    /// Whether this agent adds a `UserPromptSubmit` hook's
    /// `hookSpecificOutput.additionalContext` to the turn it belongs to.
    ///
    /// Claude Code places it alongside the submitted prompt and Codex adds it
    /// as developer context (both hook references). Qwen Code sends it as a
    /// tagged text part
    /// (<https://qwenlm.github.io/qwen-code-docs/en/users/features/hooks/>).
    /// The compaction recap uses this channel. Grok discards an allowing
    /// `UserPromptSubmit`'s output, and Kimi Code injects raw stdout rather
    /// than this envelope, so neither is listed.
    #[must_use]
    pub fn user_prompt_injects_context(self) -> bool {
        matches!(
            self,
            Self::ClaudeCode | Self::Codex | Self::QwenCode | Self::Qoder
        )
    }

    /// Whether this agent adds a `SubagentStart` hook's
    /// `hookSpecificOutput.additionalContext` to the subagent's context.
    ///
    /// [`Self::ClaudeCode`] adds it before the subagent's first prompt, within
    /// the same 10,000-character limit as every hook context string (its hooks
    /// reference, `SubagentStart` section). No other supported harness
    /// documents a context channel on its subagent-start event, so the
    /// `[briefing] inject_on_subagent_start` brief goes to that agent only.
    #[must_use]
    pub fn subagent_start_injects_context(self) -> bool {
        matches!(self, Self::ClaudeCode)
    }

    /// Whether `PostToolUse` stdout is model-visible context.
    ///
    /// Grok Build reads that stdout and delivers `hookSpecificOutput.additionalContext`
    /// after the tool result (`10-hooks.md`, PostToolUse Output). The handoff
    /// is accepted on the first such event of a session, not on `SessionStart`
    /// or `UserPromptSubmit`, because those outputs never reach the model.
    /// The model sees the handoff after the first tool, not before the first
    /// prompt. A session that never calls a tool leaves the handoff open for
    /// `memory_handoff_accept`.
    #[must_use]
    pub fn post_tool_injects_handoff(self) -> bool {
        matches!(self, Self::Grok)
    }

    /// Whether this agent reuses one session id across a `SessionEnd` and a
    /// later restart of the same conversation.
    ///
    /// Grok does: the same session id comes back after an end, so an already-
    /// ended receiver row is a live restart, not a corpse, and
    /// `accept_handoff` reopens it. Kimi Code does too: a resumed session
    /// reuses the native session id after `SessionEnd` (same shape as Grok;
    /// observed in production 2026-10). For every other agent an ended
    /// session is
    /// final — a late/out-of-order startup fetch must not rebind it to a new
    /// handoff, so accepting into an ended session stays an error (this is what
    /// keeps a lifecycle-only receiver from reclaiming after it released and
    /// ended).
    #[must_use]
    pub fn reuses_session_id_after_end(self) -> bool {
        matches!(self, Self::Grok | Self::KimiCode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_path_accepts_simple() {
        assert_eq!(PagePath::new("foo/bar.md").unwrap().as_str(), "foo/bar.md");
    }

    #[test]
    fn page_path_rejects_empty() {
        assert!(PagePath::new("").is_err());
    }

    #[test]
    fn page_path_rejects_leading_slash() {
        assert!(PagePath::new("/foo").is_err());
    }

    #[test]
    fn page_path_rejects_dot_segments() {
        assert!(PagePath::new("a/./b").is_err());
        assert!(PagePath::new("a/../b").is_err());
    }

    /// A UUID native id round-trips as-is; a non-UUID native id (Codex,
    /// OpenCode) hashes to a deterministic UUID v5, so hook capture and any
    /// offline tool matching transcripts by native id agree on one key.
    #[test]
    fn session_id_from_native_hashes_non_uuid_ids_and_passes_uuids_through() {
        let uuid_native = "11111111-2222-3333-4444-555555555555";
        assert_eq!(SessionId::from_native(uuid_native).to_string(), uuid_native);

        let non_uuid_native = "codex-native-id-123";
        let expected = SessionId(Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            non_uuid_native.as_bytes(),
        ));
        assert_eq!(SessionId::from_native(non_uuid_native), expected);
        // Deterministic: repeated calls (and hence repeated hook deliveries,
        // or a repeated repair run) must resolve to the same id.
        assert_eq!(
            SessionId::from_native(non_uuid_native),
            SessionId::from_native(non_uuid_native)
        );
    }

    #[test]
    fn agent_kind_grok_round_trips() {
        assert_eq!(AgentKind::Grok.as_str(), "grok");
        assert_eq!(AgentKind::from_wire("grok"), AgentKind::Grok);
        // serde uses rename_all = "kebab-case" → "grok".
        assert_eq!(serde_json::to_string(&AgentKind::Grok).unwrap(), "\"grok\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"grok\"").unwrap(),
            AgentKind::Grok
        );
        // Unknown tags still degrade to Other.
        assert_eq!(AgentKind::from_wire("grok-2"), AgentKind::Other);
        // Grok cannot inject the session-start handoff (ignores hook stdout),
        // and must not fetch on UserPromptSubmit either (that stdout is discarded).
        assert!(!AgentKind::Grok.session_start_injects_handoff());
        assert!(!AgentKind::Grok.user_prompt_injects_handoff());
        assert!(AgentKind::Grok.post_tool_injects_handoff());
        assert!(!AgentKind::KimiCode.post_tool_injects_handoff());
        assert!(!AgentKind::ClaudeCode.post_tool_injects_handoff());
        assert!(!AgentKind::Zero.session_start_injects_handoff());
        assert!(AgentKind::ClaudeCode.session_start_injects_handoff());
        assert!(AgentKind::Codex.session_start_injects_handoff());
        assert!(!AgentKind::Other.session_start_injects_handoff());
    }

    #[test]
    fn agent_kind_devin_round_trips() {
        assert_eq!(AgentKind::Devin.as_str(), "devin");
        assert_eq!(AgentKind::from_wire("devin"), AgentKind::Devin);
        // serde uses rename_all = "kebab-case" → "devin".
        assert_eq!(
            serde_json::to_string(&AgentKind::Devin).unwrap(),
            "\"devin\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"devin\"").unwrap(),
            AgentKind::Devin
        );
        // Unknown tags still degrade to Other.
        assert_eq!(AgentKind::from_wire("devin-2"), AgentKind::Other);
        // Devin can inject the session-start handoff (consumes additionalContext).
        assert!(AgentKind::Devin.session_start_injects_handoff());
    }

    #[test]
    fn agent_kind_kimi_code_round_trips() {
        assert_eq!(AgentKind::KimiCode.as_str(), "kimi-code");
        assert_eq!(AgentKind::from_wire("kimi-code"), AgentKind::KimiCode);
        assert_eq!(AgentKind::from_wire("kimi"), AgentKind::KimiCode);
        // serde uses rename_all = "kebab-case" → "kimi-code".
        assert_eq!(
            serde_json::to_string(&AgentKind::KimiCode).unwrap(),
            "\"kimi-code\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"kimi-code\"").unwrap(),
            AgentKind::KimiCode
        );
        // Unknown tags still degrade to Other.
        assert_eq!(AgentKind::from_wire("kimi-2"), AgentKind::Other);
        // Kimi Code discards SessionStart stdout (verified in the v0.28.1
        // source), so the handoff is delivered on UserPromptSubmit instead.
        assert!(!AgentKind::KimiCode.session_start_injects_handoff());
        assert!(AgentKind::KimiCode.user_prompt_injects_handoff());
        // A resumed session reuses the native id after SessionEnd (observed
        // in production 2026-10), the same shape as Grok's restart.
        assert!(AgentKind::KimiCode.reuses_session_id_after_end());
        assert!(AgentKind::Grok.reuses_session_id_after_end());
        assert!(!AgentKind::ClaudeCode.reuses_session_id_after_end());
        assert!(!AgentKind::Codex.reuses_session_id_after_end());
    }

    #[test]
    fn agent_kind_hermes_round_trips_and_claims_handoffs_on_user_prompts() {
        assert_eq!(AgentKind::Hermes.as_str(), "hermes");
        assert_eq!(AgentKind::from_wire("hermes"), AgentKind::Hermes);
        assert_eq!(AgentKind::from_wire("hermes-agent"), AgentKind::Hermes);
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"hermes\"").unwrap(),
            AgentKind::Hermes
        );
        assert_eq!(
            serde_json::to_string(&AgentKind::Hermes).unwrap(),
            "\"hermes\""
        );
        // `on_session_start` stdout is ignored, so the destructive fetch must
        // not run there; a primary-session `pre_llm_call` carries it instead.
        assert!(!AgentKind::Hermes.session_start_injects_handoff());
        assert!(!AgentKind::Hermes.user_prompt_injects_handoff());
        assert!(AgentKind::Hermes.json_context_user_prompt_injects_handoff());
        for other in AgentKind::ALL {
            if other != AgentKind::Hermes {
                assert!(
                    !other.json_context_user_prompt_injects_handoff(),
                    "{other:?}"
                );
            }
        }
    }

    #[test]
    fn only_one_agent_kind_takes_subagent_start_context() {
        assert!(AgentKind::ClaudeCode.subagent_start_injects_context());
        for other in AgentKind::ALL {
            if other != AgentKind::ClaudeCode {
                assert!(!other.subagent_start_injects_context(), "{other:?}");
            }
        }
    }

    #[test]
    fn agent_kind_pool_round_trips_without_claiming_handoff_delivery() {
        assert_eq!(AgentKind::Pool.as_str(), "pool");
        assert_eq!(AgentKind::from_wire("pool"), AgentKind::Pool);
        assert_eq!(AgentKind::from_wire("poolside"), AgentKind::Pool);
        assert_eq!(serde_json::to_string(&AgentKind::Pool).unwrap(), "\"pool\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"pool\"").unwrap(),
            AgentKind::Pool
        );
        assert_eq!(AgentKind::from_wire("pool-2"), AgentKind::Other);
        // Pool's SessionStart stdout injection is not demonstrated, so the
        // destructive handoff fetch must not happen from its native hook.
        assert!(!AgentKind::Pool.session_start_injects_handoff());
        assert!(!AgentKind::Pool.user_prompt_injects_handoff());
    }

    #[test]
    fn agent_kind_dsh_round_trips_without_claiming_handoff_delivery() {
        assert_eq!(AgentKind::Dsh.as_str(), "dsh");
        assert_eq!(AgentKind::from_wire("dsh"), AgentKind::Dsh);
        assert_eq!(AgentKind::from_wire("deepseek-harness"), AgentKind::Dsh);
        assert_eq!(AgentKind::from_wire("deepseek_harness"), AgentKind::Dsh);
        // serde uses rename_all = "kebab-case" → "dsh".
        assert_eq!(serde_json::to_string(&AgentKind::Dsh).unwrap(), "\"dsh\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"dsh\"").unwrap(),
            AgentKind::Dsh
        );
        // Unknown tags still degrade to Other.
        assert_eq!(AgentKind::from_wire("dsh-2"), AgentKind::Other);
        // DSH's Claude Code hooks bridge runs SessionStart detached, so the
        // destructive handoff fetch must not happen from its hook bundle.
        assert!(!AgentKind::Dsh.session_start_injects_handoff());
        assert!(!AgentKind::Dsh.user_prompt_injects_handoff());
    }

    #[test]
    fn agent_kind_copilot_cli_round_trips_without_claiming_handoff_delivery() {
        assert_eq!(AgentKind::CopilotCli.as_str(), "copilot-cli");
        assert_eq!(AgentKind::from_wire("copilot-cli"), AgentKind::CopilotCli);
        assert_eq!(AgentKind::from_wire("copilot_cli"), AgentKind::CopilotCli);
        assert_eq!(
            serde_json::to_string(&AgentKind::CopilotCli).unwrap(),
            "\"copilot-cli\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"copilot-cli\"").unwrap(),
            AgentKind::CopilotCli
        );
        // The bare "copilot" word is deliberately NOT an alias here: it is
        // already claimed by the unrelated `McpClient::VsCodeCopilot` client
        // (#1040), and aliasing both to different things would be confusing.
        assert_eq!(AgentKind::from_wire("copilot"), AgentKind::Other);
        // Unknown tags still degrade to Other.
        assert_eq!(AgentKind::from_wire("copilot-cli-2"), AgentKind::Other);
        // Copilot CLI reads a top-level `additionalContext` at SessionStart,
        // which the native hook prints for it.
        assert!(AgentKind::CopilotCli.session_start_injects_handoff());
        assert!(!AgentKind::CopilotCli.user_prompt_injects_handoff());
    }

    #[test]
    fn agent_kind_droid_round_trips() {
        assert_eq!(AgentKind::Droid.as_str(), "droid");
        for alias in ["droid", "factory-droid", "factory_droid"] {
            assert_eq!(AgentKind::from_wire(alias), AgentKind::Droid, "{alias}");
        }
        assert_eq!(
            serde_json::to_string(&AgentKind::Droid).unwrap(),
            "\"droid\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"droid\"").unwrap(),
            AgentKind::Droid
        );
        // The bare vendor name is not an alias, and unknown tags still
        // degrade to Other.
        assert_eq!(AgentKind::from_wire("factory"), AgentKind::Other);
        assert_eq!(AgentKind::from_wire("droid-2"), AgentKind::Other);
        // Droid reads `hookSpecificOutput.additionalContext` from SessionStart
        // stdout, so the native hook fetches the handoff there.
        assert!(AgentKind::Droid.session_start_injects_handoff());
        assert!(!AgentKind::Droid.user_prompt_injects_handoff());
        assert!(!AgentKind::Droid.post_tool_injects_handoff());
        assert!(!AgentKind::Droid.reuses_session_id_after_end());
    }

    #[test]
    fn agent_kind_muse_round_trips() {
        assert_eq!(AgentKind::Muse.as_str(), "muse");
        for alias in ["muse", "muse-code", "musecode"] {
            assert_eq!(AgentKind::from_wire(alias), AgentKind::Muse, "{alias}");
        }
        assert_eq!(serde_json::to_string(&AgentKind::Muse).unwrap(), "\"muse\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"muse\"").unwrap(),
            AgentKind::Muse
        );
        assert_eq!(AgentKind::from_wire("meta"), AgentKind::Other);
        assert_eq!(AgentKind::from_wire("muse-2"), AgentKind::Other);
        // Muse injects SessionStart's `hookSpecificOutput.additionalContext`,
        // so the native hook fetches the handoff there.
        assert!(AgentKind::Muse.session_start_injects_handoff());
        assert!(!AgentKind::Muse.user_prompt_injects_handoff());
        assert!(!AgentKind::Muse.post_tool_injects_handoff());
        assert!(!AgentKind::Muse.reuses_session_id_after_end());
    }

    #[test]
    fn agent_kind_codebuddy_round_trips_and_injects_session_start_handoff() {
        assert_eq!(AgentKind::CodeBuddy.as_str(), "codebuddy");
        for alias in ["codebuddy", "code-buddy"] {
            assert_eq!(AgentKind::from_wire(alias), AgentKind::CodeBuddy, "{alias}");
        }
        assert_eq!(
            serde_json::to_string(&AgentKind::CodeBuddy).unwrap(),
            "\"codebuddy\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"codebuddy\"").unwrap(),
            AgentKind::CodeBuddy
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"code-buddy\"").unwrap(),
            AgentKind::CodeBuddy
        );
        assert_eq!(AgentKind::from_wire("codebuddy-2"), AgentKind::Other);
        assert_eq!(AgentKind::from_wire("tencent"), AgentKind::Other);
        // CodeBuddy appends SessionStart's hookSpecificOutput.additionalContext
        // (#1299), so the built-in fetch is the delivery path.
        assert!(AgentKind::CodeBuddy.session_start_injects_handoff());
        assert!(!AgentKind::CodeBuddy.user_prompt_injects_handoff());
        assert!(!AgentKind::CodeBuddy.post_tool_injects_handoff());
        assert!(!AgentKind::CodeBuddy.reuses_session_id_after_end());
    }

    #[test]
    fn agent_kind_ibm_bob_round_trips_and_injects_session_start_handoff() {
        assert_eq!(AgentKind::IbmBob.as_str(), "ibm-bob");
        for alias in ["ibm-bob", "bob"] {
            assert_eq!(AgentKind::from_wire(alias), AgentKind::IbmBob, "{alias}");
        }
        assert_eq!(
            serde_json::to_string(&AgentKind::IbmBob).unwrap(),
            "\"ibm-bob\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"bob\"").unwrap(),
            AgentKind::IbmBob
        );
        assert_eq!(AgentKind::from_wire("bob-shell-2"), AgentKind::Other);
        assert!(AgentKind::IbmBob.session_start_injects_handoff());
        assert!(!AgentKind::IbmBob.user_prompt_injects_handoff());
        assert!(!AgentKind::IbmBob.post_tool_injects_handoff());
        assert!(!AgentKind::IbmBob.reuses_session_id_after_end());
    }

    #[test]
    fn agent_kind_qwen_code_round_trips_and_injects_session_start_handoff() {
        assert_eq!(AgentKind::QwenCode.as_str(), "qwen-code");
        for alias in ["qwen-code", "qwen", "qwencode"] {
            assert_eq!(AgentKind::from_wire(alias), AgentKind::QwenCode, "{alias}");
        }
        assert_eq!(
            serde_json::to_string(&AgentKind::QwenCode).unwrap(),
            "\"qwen-code\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"qwen-code\"").unwrap(),
            AgentKind::QwenCode
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"qwen\"").unwrap(),
            AgentKind::QwenCode
        );
        assert_eq!(AgentKind::from_wire("qwen-code-2"), AgentKind::Other);
        assert!(AgentKind::QwenCode.session_start_injects_handoff());
        assert!(!AgentKind::QwenCode.user_prompt_injects_handoff());
        assert!(AgentKind::QwenCode.user_prompt_injects_context());
        assert!(!AgentKind::QwenCode.post_tool_injects_handoff());
        assert!(!AgentKind::QwenCode.reuses_session_id_after_end());
    }

    #[test]
    fn agent_kind_qoder_round_trips_and_injects_supported_context() {
        assert_eq!(AgentKind::Qoder.as_str(), "qoder");
        assert_eq!(AgentKind::from_wire("qoder"), AgentKind::Qoder);
        assert_eq!(
            serde_json::to_string(&AgentKind::Qoder).unwrap(),
            "\"qoder\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"qoder\"").unwrap(),
            AgentKind::Qoder
        );
        assert_eq!(AgentKind::from_wire("qoder-cli-2"), AgentKind::Other);
        assert!(AgentKind::Qoder.session_start_injects_handoff());
        assert!(!AgentKind::Qoder.user_prompt_injects_handoff());
        assert!(AgentKind::Qoder.user_prompt_injects_context());
        assert!(!AgentKind::Qoder.post_tool_injects_handoff());
        assert!(!AgentKind::Qoder.reuses_session_id_after_end());
    }

    #[test]
    fn agent_kind_grizzybot_round_trips() {
        assert_eq!(AgentKind::Grizzybot.as_str(), "grizzybot");
        assert_eq!(AgentKind::from_wire("grizzybot"), AgentKind::Grizzybot);
        assert_eq!(AgentKind::from_wire("grizzy-bot"), AgentKind::Grizzybot);
        assert_eq!(
            serde_json::to_string(&AgentKind::Grizzybot).unwrap(),
            "\"grizzybot\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"grizzybot\"").unwrap(),
            AgentKind::Grizzybot
        );
        assert_eq!(AgentKind::from_wire("grizzybot-2"), AgentKind::Other);
    }

    #[test]
    fn agent_kind_zcode_round_trips_and_injects_session_start_handoff() {
        assert_eq!(AgentKind::Zcode.as_str(), "zcode");
        assert_eq!(AgentKind::from_wire("zcode"), AgentKind::Zcode);
        assert_eq!(AgentKind::from_wire("zai"), AgentKind::Zcode);
        assert_eq!(
            serde_json::to_string(&AgentKind::Zcode).unwrap(),
            "\"zcode\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"zcode\"").unwrap(),
            AgentKind::Zcode
        );
        // Unknown tags still degrade to Other.
        assert_eq!(AgentKind::from_wire("zcode-2"), AgentKind::Other);
        // ZCode injects SessionStart stdout as additionalContext (live canary
        // capture against engine v0.16.5), so the destructive handoff fetch
        // is safe from its native hook.
        assert!(AgentKind::Zcode.session_start_injects_handoff());
        assert!(!AgentKind::Zcode.user_prompt_injects_handoff());
    }

    #[test]
    fn agent_kind_command_code_round_trips_and_injects_session_start_handoff() {
        assert_eq!(AgentKind::CommandCode.as_str(), "command-code");
        for alias in ["command-code", "commandcode", "cmdc", "cmd"] {
            assert_eq!(AgentKind::from_wire(alias), AgentKind::CommandCode);
        }
        assert_eq!(
            serde_json::to_string(&AgentKind::CommandCode).unwrap(),
            "\"command-code\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"command-code\"").unwrap(),
            AgentKind::CommandCode
        );
        assert!(AgentKind::CommandCode.session_start_injects_handoff());
        assert!(!AgentKind::CommandCode.user_prompt_injects_handoff());
    }

    #[test]
    fn agent_kind_kiro_cli_round_trips_and_injects_at_session_start() {
        assert_eq!(AgentKind::KiroCli.as_str(), "kiro-cli");
        assert_eq!(AgentKind::from_wire("kiro-cli"), AgentKind::KiroCli);
        assert_eq!(AgentKind::from_wire("kiro"), AgentKind::KiroCli);
        assert_eq!(
            serde_json::to_string(&AgentKind::KiroCli).unwrap(),
            "\"kiro-cli\""
        );
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"kiro-cli\"").unwrap(),
            AgentKind::KiroCli
        );
        assert!(AgentKind::KiroCli.session_start_injects_handoff());
        assert!(!AgentKind::KiroCli.user_prompt_injects_handoff());
    }

    #[test]
    fn agent_kind_opencode_wire_names_share_one_kind() {
        // OpenCode 2.0 ships side by side as `opencode2` against the same
        // config dir and session store, so its wire names resolve to the
        // existing kind — no new variant, no store migration.
        assert_eq!(AgentKind::OpenCode.as_str(), "open-code");
        for alias in [
            "open-code",
            "opencode",
            "opencode2",
            "opencode-v2",
            "open-code2",
        ] {
            assert_eq!(AgentKind::from_wire(alias), AgentKind::OpenCode);
        }
    }

    #[test]
    fn user_prompt_handoff_injection_is_kimi_only() {
        for agent in AgentKind::ALL {
            assert_eq!(
                agent.user_prompt_injects_handoff(),
                agent == AgentKind::KimiCode,
                "{}",
                agent.as_str()
            );
        }
    }

    #[test]
    fn user_prompt_context_injection_matches_supported_hook_protocols() {
        for agent in AgentKind::ALL {
            assert_eq!(
                agent.user_prompt_injects_context(),
                matches!(
                    agent,
                    AgentKind::ClaudeCode
                        | AgentKind::Codex
                        | AgentKind::QwenCode
                        | AgentKind::Qoder
                ),
                "{}",
                agent.as_str()
            );
        }
    }

    #[test]
    fn page_path_rejects_backslashes_and_windows_prefixes() {
        assert!(PagePath::new(r"notes\secret.md").is_err());
        assert!(PagePath::new(r"C:\Users\me\secret.md").is_err());
        assert!(PagePath::new("C:/Users/me/secret.md").is_err());
    }

    #[test]
    fn ids_are_unique() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn id_round_trips_through_string() {
        let id = SessionId::new();
        let s = id.to_string();
        let parsed: SessionId = s.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn id_round_trips_through_bytes() {
        let id = ProjectId::new();
        assert_eq!(ProjectId::from_slice(id.as_bytes()).unwrap(), id);
    }

    #[test]
    fn id_rejects_malformed_byte_lengths() {
        assert!(PageId::from_slice(&[0_u8; 15]).is_err());
        assert!(PageId::from_slice(&[0_u8; 17]).is_err());
    }

    #[test]
    fn agent_kind_serde() {
        let k = AgentKind::ClaudeCode;
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, "\"claude-code\"");
        let back: AgentKind = serde_json::from_str(&s).unwrap();
        assert_eq!(back, k);

        let omp = serde_json::to_string(&AgentKind::Omp).unwrap();
        assert_eq!(omp, "\"omp\"");
        assert_eq!(AgentKind::from_wire("omp"), AgentKind::Omp);
        assert_eq!(AgentKind::from_wire("oh-my-pi"), AgentKind::Omp);

        let pi = serde_json::to_string(&AgentKind::Pi).unwrap();
        assert_eq!(pi, "\"pi\"");
        assert_eq!(AgentKind::Pi.as_str(), "pi");
        assert_eq!(AgentKind::from_wire("pi"), AgentKind::Pi);
        assert_eq!(
            serde_json::from_str::<AgentKind>(&pi).unwrap(),
            AgentKind::Pi
        );

        let crush = serde_json::to_string(&AgentKind::Crush).unwrap();
        assert_eq!(crush, "\"crush\"");
        assert_eq!(AgentKind::from_wire("crush"), AgentKind::Crush);
        assert!(!AgentKind::Crush.session_start_injects_handoff());

        let openclaw = serde_json::to_string(&AgentKind::OpenClaw).unwrap();
        assert_eq!(openclaw, "\"openclaw\"");
        assert_eq!(
            serde_json::from_str::<AgentKind>("\"open-claw\"").unwrap(),
            AgentKind::OpenClaw
        );

        let antigravity = serde_json::to_string(&AgentKind::AntigravityCli).unwrap();
        assert_eq!(antigravity, "\"antigravity-cli\"");

        let devin = serde_json::to_string(&AgentKind::Devin).unwrap();
        assert_eq!(devin, "\"devin\"");
        assert_eq!(AgentKind::Devin.as_str(), "devin");
        assert_eq!(AgentKind::from_wire("devin"), AgentKind::Devin);
        assert_eq!(
            serde_json::from_str::<AgentKind>(&devin).unwrap(),
            AgentKind::Devin
        );
    }

    #[test]
    fn session_id_from_native_keeps_uuids_and_hashes_other_ids() {
        let uuid = SessionId::new();
        assert_eq!(SessionId::from_native(&uuid.to_string()), uuid);
        let hashed = SessionId::from_native("agy-session-1");
        assert_eq!(
            hashed,
            SessionId::from_native("agy-session-1"),
            "deterministic"
        );
        assert_eq!(
            hashed.0,
            Uuid::new_v5(&Uuid::NAMESPACE_OID, b"agy-session-1"),
            "the key hook POSTs have always used"
        );
    }
}

/// Names Windows reserves regardless of extension: `CON.md` is still the
/// console device. Windows also treats the superscript digits `¹ ² ³` as
/// port numbers, so `COM¹` is a serial port too.
const DOS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "com¹", "com²", "com³", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8",
    "lpt9", "lpt¹", "lpt²", "lpt³",
];

/// Whether `stem` (a file name without its extension) is a DOS device name
/// Windows resolves to a device instead of a file, compared case-insensitively.
/// Spaces between the name and its extension are ignored, as Windows ignores
/// them: `CON .md` is still the console.
#[must_use]
pub fn is_dos_device_name(stem: &str) -> bool {
    DOS_DEVICE_NAMES.contains(&stem.trim_end_matches(' ').to_ascii_lowercase().as_str())
}

/// Characters Windows refuses in a filename. `/` is the separator and is
/// handled by the caller; `\\` and a drive prefix are already rejected by
/// [`PagePath::new`].
const WINDOWS_RESERVED_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Longest file name, in UTF-8 bytes, that every supported filesystem stores.
/// ext4 and most Linux filesystems cap a name at 255 bytes, NTFS at 255 UTF-16
/// units; the byte count is never smaller than the UTF-16 count, so it is the
/// bound both agree on.
const MAX_PORTABLE_COMPONENT_BYTES: usize = 255;

/// Returns true if a path component is reserved by Git (`.git` case-insensitively,
/// or an 8.3 short-name alias like `git~1`..`git~4`).
#[must_use]
pub fn is_git_reserved_component(component: &str) -> bool {
    // Compare on bytes: a `str` slice at a fixed byte index panics on a
    // multibyte component (a 5-byte UTF-8 name like "abcé" has no char
    // boundary at 4), and this runs on untrusted write input.
    let b = component.as_bytes();
    component.eq_ignore_ascii_case(".git")
        || (b.len() == 5 && b[..4].eq_ignore_ascii_case(b"git~") && (b'1'..=b'4').contains(&b[4]))
}

fn ensure_portable_component(component: &str, full: &str) -> Result<(), MemoryError> {
    let invalid = |reason: &str| {
        Err(MemoryError::InvalidPagePath(format!(
            "page path {full:?}: component {component:?} {reason}"
        )))
    };

    // NTFS accepts a 100-character CJK name (103 UTF-16 units, 303 bytes)
    // that ext4 refuses, so a page written on Windows would vanish from a
    // Linux checkout of the same wiki.
    if component.len() > MAX_PORTABLE_COMPONENT_BYTES {
        return invalid(&format!(
            "is {} bytes long; Linux filesystems refuse a name over {MAX_PORTABLE_COMPONENT_BYTES} bytes",
            component.len()
        ));
    }

    if let Some(bad) = component
        .chars()
        .find(|c| WINDOWS_RESERVED_CHARS.contains(c))
    {
        return invalid(&format!(
            "contains {bad:?}, which Windows refuses in a filename"
        ));
    }
    if let Some(bad) = component.chars().find(|c| (*c as u32) < 0x20) {
        return invalid(&format!(
            "contains control character U+{:04X}, which is not a legal filename byte",
            bad as u32
        ));
    }
    // A trailing dot or space is silently stripped by the Win32 layer, so the
    // file lands under a different name than the one recorded in the index.
    if component.ends_with('.') || component.ends_with(' ') {
        return invalid("ends with a dot or space, which Windows strips on create");
    }
    // Device names match on the stem, so `CON`, `CON.md` and `con.markdown`
    // are all the console.
    let stem = component.split('.').next().unwrap_or(component);
    if is_dos_device_name(stem) {
        return invalid(&format!(
            "uses the reserved DOS device name {stem:?}; Windows resolves it to a device, not a file"
        ));
    }
    // Git reserves `.git` for repository metadata. Any tree entry named `.git`
    // or an 8.3 alias is refused by libgit2 with `GIT_EINVALIDPATH` and cannot
    // be checkpointed.
    if is_git_reserved_component(component) {
        return invalid("is reserved by Git for repository metadata and refused in tree entries");
    }
    Ok(())
}

#[cfg(test)]
mod portable_page_path_tests {
    use super::{PagePath, portable_page_key};

    /// Every shape #462 reproduced on native Windows. Each either fails late
    /// with a 500, or writes but cannot be checkpointed by libgit2 and cannot
    /// be deleted through the normal API — silent partial state, which is
    /// worse than a clean rejection.
    #[test]
    fn windows_hostile_paths_are_rejected() {
        for raw in [
            "CON.md",
            "notes/aux.md",
            "notes/NUL.md",
            "notes/com1.md",
            "notes/LPT9.txt",
            "notes/con .md",
            "notes/NUL  .txt",
            "notes/COM¹.md",
            "notes/lpt³.md",
            "notes/trailing./x.md",
            "notes/trailing /x.md",
            "notes/end.md.",
            "notes/end.md ",
            "notes/a<b.md",
            "notes/a>b.md",
            "notes/a\"b.md",
            "notes/a|b.md",
            "notes/a?b.md",
            "notes/a*b.md",
            "notes/a:b.md",
            "notes/a\u{1}b.md",
        ] {
            let path = PagePath::new(raw).expect("still constructible: reads must keep working");
            assert!(
                path.ensure_portable().is_err(),
                "{raw:?} is not portable and must be refused at write time"
            );
        }
    }

    #[test]
    fn git_reserved_paths_are_rejected() {
        for raw in [
            ".git",
            ".git/config",
            ".git/HEAD",
            "notes/.git",
            "notes/.git/sub.md",
            "notes/.GIT/sub.md",
            "notes/.Git/sub.md",
            "notes/git~1",
            "notes/git~1/foo.md",
            "notes/GIT~2/bar.md",
            "notes/git~4/config",
            "a/b/c/.git/deep.md",
        ] {
            let path = PagePath::new(raw).expect("still constructible: reads must keep working");
            assert!(
                path.ensure_portable().is_err(),
                "{raw:?} contains a git-reserved component and must be refused at write time"
            );
        }
    }

    /// The rule must not reject ordinary pages. `con` is only reserved as a
    /// whole component, so `concepts/` and `icon.md` are fine.
    #[test]
    fn ordinary_paths_stay_writable() {
        for raw in [
            "notes/portable.md",
            "concepts/no-impl-without-test.md",
            "sessions/2026-08-22.md",
            "notes/icon.md",
            "notes/console.md",
            "notes/con x.md",
            "notes/com10.md",
            "prn-notes/aux-iliary.md",
            "a/b/c/deep.md",
            "notes/dot.in.middle.md",
            "notes/UPPER.MD",
            "notes/.git.md",
            "notes/.gitignore",
            "notes/.gitattributes",
            "notes/github.md",
            "git-notes/index.md",
        ] {
            let path = PagePath::new(raw).expect("valid path");
            assert!(
                path.ensure_portable().is_ok(),
                "{raw:?} is portable and must stay writable"
            );
        }
    }

    /// A multibyte component whose byte length is 5 must not be mistaken for
    /// a `git~N` alias, and must not panic: `is_git_reserved_component` once
    /// sliced the string at byte index 4, which is not a char boundary in a
    /// name like "abcé" (5 bytes) or "a😀" (5 bytes). Since this runs on the
    /// write funnel for untrusted input, the panic was a crashable defect.
    #[test]
    fn multibyte_components_are_not_git_reserved_and_do_not_panic() {
        for component in ["abcé", "ab€", "a😀", "éé", "🦀🦀"] {
            assert!(
                !super::is_git_reserved_component(component),
                "{component:?} is an ordinary name, not a git-reserved alias"
            );
        }
        // The full write funnel (construct + ensure_portable) must accept a
        // page path with a 5-byte multibyte component without panicking.
        for raw in ["notes/abcé.md", "notes/a😀.md", "notes/ab€.md"] {
            let path = PagePath::new(raw).expect("valid non-ASCII path");
            assert!(
                path.ensure_portable().is_ok(),
                "{raw:?} is portable and must stay writable"
            );
        }
    }

    /// The name limit is counted in UTF-8 bytes, not characters: 85 CJK
    /// characters plus `.md` is 258 bytes, which Windows stores and Linux
    /// refuses.
    #[test]
    fn components_over_255_bytes_are_rejected() {
        let ascii_max = format!("notes/{}.md", "a".repeat(252));
        let cjk_max = format!("notes/{}.md", "日".repeat(84));
        for raw in [&ascii_max, &cjk_max] {
            let path = PagePath::new(raw.as_str()).expect("valid path");
            assert!(
                path.ensure_portable().is_ok(),
                "a 255-byte component is portable and must stay writable"
            );
        }

        let ascii_over = format!("notes/{}.md", "a".repeat(253));
        let cjk_over = format!("notes/{}.md", "日".repeat(85));
        let dir_over = format!("{}/x.md", "é".repeat(128));
        for raw in [&ascii_over, &cjk_over, &dir_over] {
            let path =
                PagePath::new(raw.as_str()).expect("still constructible: reads must keep working");
            assert!(
                path.ensure_portable().is_err(),
                "{raw:?} has a component over 255 bytes and must be refused at write time"
            );
        }
    }

    /// Reads must keep working for pages already stored under a
    /// non-portable name: `PagePath::new` stays tolerant so a listing does
    /// not break on one bad row.
    #[test]
    fn existing_non_portable_pages_remain_constructible() {
        for raw in [
            "CON.md",
            "notes/a|b.md",
            "notes/trailing./x.md",
            ".git/config",
            "notes/.git/sub.md",
            "notes/git~1/foo.md",
        ] {
            assert!(
                PagePath::new(raw).is_ok(),
                "{raw:?} must still construct so persisted rows stay readable"
            );
        }
    }

    #[test]
    fn portable_key_collapses_case_and_normalization() {
        for (a, b) in [
            ("concepts/alpha.md", "concepts/Alpha.md"),
            ("Concepts/alpha.md", "concepts/ALPHA.md"),
            ("concepts/caf\u{00e9}.md", "concepts/cafe\u{0301}.md"),
            ("concepts/CAF\u{00c9}.md", "concepts/cafe\u{0301}.md"),
        ] {
            assert_eq!(
                portable_page_key(a),
                portable_page_key(b),
                "{a:?} and {b:?} are one file on macOS/Windows"
            );
        }
    }

    #[test]
    fn portable_key_keeps_distinct_paths_distinct() {
        for (a, b) in [
            ("concepts/alpha.md", "concepts/alphabet.md"),
            ("concepts/alpha.md", "decisions/alpha.md"),
            ("concepts/nested/a.md", "concepts/a.md"),
            ("concepts/cafe.md", "concepts/caf\u{00e9}.md"),
        ] {
            assert_ne!(portable_page_key(a), portable_page_key(b));
        }
    }

    #[test]
    fn cline_wire_identity_fails_safe_for_detached_start_hooks() {
        assert_eq!(
            super::AgentKind::from_wire("cline"),
            super::AgentKind::Cline
        );
        assert_eq!(
            super::AgentKind::from_wire("cline-cli"),
            super::AgentKind::Cline
        );
        assert_eq!(super::AgentKind::Cline.as_str(), "cline");
        assert!(!super::AgentKind::Cline.session_start_injects_handoff());
    }
}
