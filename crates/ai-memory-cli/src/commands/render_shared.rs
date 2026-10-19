//! Shared rendering helpers for the install-* / setup-agent commands.
//!
//! These three subcommands (`install-hooks`, `install-mcp`,
//! `setup-agent`) all emit configuration snippets that share two
//! pieces of state:
//!
//! 1. The per-agent lifecycle-hook event lists ai-memory wires up
//!    (Claude/Grok share `CLAUDE_CODE_EVENTS`; Codex, Cursor, Gemini,
//!    and Antigravity define their own profiles) — kept in sync between
//!    hook-bundle generation (setup-agent) and config rendering
//!    (install-hooks).
//! 2. The optional `Authorization: Bearer <token>` header used by
//!    both MCP client configs (install-mcp) and hook env blocks
//!    (install-hooks / setup-agent).
//!
//! Each subcommand still owns its per-client output formatting (the
//! commentary that frames the JSON snippet differs from client to
//! client and is the part that makes the printout readable). What
//! lives here is only the *data* both consume.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use ai_memory_hooks::capture_policy;
use base64::Engine as _;
use serde_json::{Value, json};

use crate::commands::path_util::strip_windows_verbatim_prefix;

/// Claude Code lifecycle events ai-memory hooks. Each pair is
/// `(event-name-in-Claude-Code-settings, POSIX hook-script-filename)`.
///
/// Adding a hook event means updating this list AND adding the matching `.sh`
/// and `.ps1` files under the agents that use this profile (`claude-code` and
/// `grok`). Agents with different vocabularies keep their own event arrays
/// below. The install-hooks parity test fails if a bundle drifts.
pub(crate) const CLAUDE_CODE_EVENTS: [(&str, &str); 9] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
    // Subagent boundaries — let the server seed/forget a subagent session id so
    // `drop_subagent_captures` can drop the whole nested session (Claude Code +
    // grok both emit these; other agents keep their own event lists).
    ("SubagentStart", "subagent-start.sh"),
    ("SubagentStop", "subagent-stop.sh"),
];

/// Claude Code otherwise gives SessionEnd hooks a 1.5-second shared budget.
/// A settings-file timeout raises that budget (up to Claude's 60-second cap),
/// leaving room for a slow local spool fsync without making other hooks wait.
pub(crate) const CLAUDE_SESSION_END_TIMEOUT_SECS: u64 = 15;

/// Kimi Code lifecycle events ai-memory hooks. Claude Code's 9-event
/// vocabulary (`CLAUDE_CODE_EVENTS`) plus `PostToolUseFailure`: Kimi Code
/// fires `PostToolUse` on successful calls only and reports failures
/// separately. The failure entry reuses the post-tool-use script — the
/// server aliases `PostToolUseFailure` to `PostToolUse` and reads the
/// outcome from the payload.
///
/// Kimi Code wires hooks as `[[hooks]]` entries in
/// `$KIMI_CODE_HOME/config.toml` (TOML) instead of a JSON settings file,
/// so its payload comes from [`kimi_code_hook_commands`] rather than the
/// JSON hook shapes.
///
/// Adding a hook event means updating this list AND adding the matching
/// `.sh` and `.ps1` files under `hooks/kimi-code/` (a reused script like
/// post-tool-use needs no new file). The install-hooks parity test fails
/// if the bundle drifts.
pub(crate) const KIMI_CODE_EVENTS: [(&str, &str); 10] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PostToolUseFailure", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
    ("SubagentStart", "subagent-start.sh"),
    ("SubagentStop", "subagent-stop.sh"),
];

/// GitHub Copilot CLI's lifecycle events, configured with PascalCase event
/// names so the payload comes in the Claude Code/VS-Code-compatible shape
/// (`session_id`, `cwd`, `tool_name`, `tool_input`, `tool_result`) instead of
/// its native camelCase one — this lets it free-ride on the same key-based
/// payload extraction as Claude Code/Codex/Zcode (see `tool_observation_metadata`
/// in `capture_policy.rs`) rather than needing a dedicated parser (#1040).
/// Same vocabulary as `KIMI_CODE_EVENTS`: Claude Code's 9 events plus
/// `PostToolUseFailure`, which Copilot (per
/// <https://docs.github.com/en/copilot/reference/hooks-configuration>) fires
/// instead of `PostToolUse` when a tool call errors — the server already
/// aliases it to `PostToolUse` (`HookEvent::parse`), so it reuses that script.
pub(crate) const COPILOT_CLI_EVENTS: [(&str, &str); 10] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PostToolUseFailure", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
    ("SubagentStart", "subagent-start.sh"),
    ("SubagentStop", "subagent-stop.sh"),
];

/// Factory Droid CLI lifecycle events ai-memory hooks (#1240). Each pair is
/// `(event-name-in-~/.factory/hooks.json, native hook-event script stem)`;
/// Droid has no script bundle, so the `.sh` name only carries the event
/// token the native command is rendered with.
///
/// Droid's documented vocabulary is Claude Code's minus `SubagentStart`
/// (0.240.0). `SubagentStop` is left out with it: without the start
/// boundary there is no subagent session to forget, and its payload
/// (`task_name`, `task_result`) is not Claude's. `Notification` carries no
/// capture. Droid fires a real `SessionEnd`, so no `finalize-session` step is
/// needed.
pub(crate) const DROID_EVENTS: [(&str, &str); 7] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
];

/// Meta Muse Code lifecycle events ai-memory hooks. Each pair is
/// `(event-name-in-settings.json, native hook-event script stem)`; Muse has
/// no script bundle, so the `.sh` name only carries the event token the
/// native command is rendered with.
///
/// Claude Code's vocabulary plus `PostToolUseFailure`, which Muse fires
/// instead of `PostToolUse` when a tool fails (the server aliases it to
/// `post-tool-use` and reads the outcome from `error`). `SubagentStart` and
/// `SubagentStop` are left out: Muse Code 1.4.7 fires them for its built-in
/// reminder observers on every turn, each under its own session id, so wiring
/// them would open several empty sessions per prompt. Muse fires a real
/// `SessionEnd`, so no `finalize-session` step is needed.
pub(crate) const MUSE_EVENTS: [(&str, &str); 8] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PostToolUseFailure", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
];

/// Tencent CodeBuddy Code lifecycle events ai-memory hooks (#1299). Each pair
/// is `(event-name-in-~/.codebuddy/settings.json, native hook-event script
/// stem)`; CodeBuddy has no script bundle, so the `.sh` name only carries the
/// event token the native command is rendered with.
///
/// The reporter's mapping on v2.164–v2.167: Claude Code's vocabulary minus
/// the subagent events. `PostToolUseFailure` is not in that mapping.
/// `SessionEnd` is wired for the interactive exit; headless `codebuddy -p`
/// is a separate follow-up.
pub(crate) const CODEBUDDY_EVENTS: [(&str, &str); 7] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
];

/// IBM Bob IDE's common lifecycle vocabulary. Bob Shell also supports these
/// five events; its additional compaction events are intentionally omitted so
/// one global settings file remains valid in both products. Bob has no
/// `SessionEnd`, and `Stop` is an agent-turn boundary rather than a reliable
/// process-exit boundary.
pub(crate) const IBM_BOB_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("Stop", "stop.sh"),
];

/// Seconds Muse waits for one ai-memory hook. Muse reads `timeout` in
/// seconds, like the Claude Code plugins it loads. The capture POST is
/// fire-and-forget; `SessionStart` also fetches a handoff.
pub(crate) const MUSE_HOOK_TIMEOUT_SECS: u64 = 30;

/// Kiro CLI v2-engine lifecycle events. Each pair is
/// `(trigger-name-in-agent-config, POSIX hook-script-filename)`.
///
/// The v2 engine embeds hooks in agent configs (`~/.kiro/agents/*.json`)
/// keyed by camelCase triggers, per kiro.dev/docs/hooks and the
/// shipping `HookTrigger` serde in aws/amazon-q-developer-cli
/// (`crates/chat-cli/src/cli/agent/hook.rs`). The vocabulary is exactly
/// these five events — there is no PreCompact/SessionEnd/subagent
/// equivalent. Adding a hook event means updating this list AND adding
/// the matching `.sh` and `.ps1` files under `hooks/kiro-cli/`; the
/// install-hooks parity test fails if the bundle drifts.
///
pub(crate) const KIRO_CLI_V2_EVENTS: [(&str, &str); 5] = [
    ("agentSpawn", "session-start.sh"),
    ("userPromptSubmit", "user-prompt-submit.sh"),
    ("preToolUse", "pre-tool-use.sh"),
    ("postToolUse", "post-tool-use.sh"),
    ("stop", "stop.sh"),
];

/// Kiro CLI v3-engine lifecycle events. V3 uses PascalCase triggers in a
/// standalone versioned hook file rather than v2's camelCase agent field.
pub(crate) const KIRO_CLI_V3_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("Stop", "stop.sh"),
];

/// Devin lifecycle events ai-memory hooks. Each pair is
/// `(event-name-in-Devin-settings, POSIX hook-script-filename)`.
///
/// Devin uses the same event vocabulary as Claude Code, but with two differences:
/// - `PostCompaction` instead of `PreCompact` (triggers *after* compaction with a `summary` field)
/// - No `SubagentStart`/`SubagentStop` (Devin does not expose subagent boundaries as hook events)
pub(crate) const DEVIN_EVENTS: [(&str, &str); 7] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PostCompaction", "post-compaction.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
];

/// Pool (Poolside Agent CLI) lifecycle events ai-memory hooks. Each pair is
/// `(event-name-in-.poolside-settings, POSIX hook-script-filename)`.
///
/// Pool's hook vocabulary uses Claude-shaped PascalCase names but is exactly
/// these five events (verified against Poolside CLI v1.0.16, hooks api 1.0):
/// no `SessionEnd`, no `PreCompact`, no subagent events. `Stop` is a turn
/// boundary, not a session end — `ai-memory finalize-session --agent pool`
/// is the close path. Adding a hook event means updating this list AND
/// adding the matching `.sh` and `.ps1` files under `hooks/pool/`; the
/// install-hooks parity test fails if the bundle drifts.
pub(crate) const POOL_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("Stop", "stop.sh"),
];

/// Format an `Authorization: Bearer <token>` header value, or `None`
/// when no token is supplied. Used by every MCP client renderer in
/// `install-mcp` and every hook-config renderer that wants to
/// embed an auth token.
///
/// Centralised because the prefix is `Bearer` per RFC 7235 / OAuth
/// 2.1 / the MCP spec — if anyone ever decides to support a
/// different scheme (e.g. `DPoP`) this is the one place that
/// changes.
#[must_use]
pub(crate) fn bearer_header_value(token: Option<&str>) -> Option<String> {
    token.map(|t| format!("Bearer {t}"))
}

/// Emit a TypeScript string literal containing `s`. Escapes
/// backslashes, double quotes, and common control characters. This is
/// sufficient for URL, auth-token, and path strings embedded into
/// generated TypeScript integrations.
#[must_use]
pub(crate) fn ts_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The single capture-policy-v1 implementation embedded in every generated
/// JavaScript integration.  It deliberately has no package dependency: these
/// extensions run in several hosts with different module loaders.
///
/// `capture_mode` is the baked `--capture-mode` value ("allowlist" or
/// "denylist", the same string [`crate::commands::install_hooks`]'s
/// `persist_capture_mode` returns) and is spliced into the emitted
/// `CAPTURE_MODE` constant that `capturePolicy` gates on. Under allowlist a
/// repository with no `.ai-memory.toml` marker must emit nothing — mirroring
/// the native admit gate in `commands/hook.rs` (`repository_admits_capture`)
/// — so the check runs before any disposition logic, for every event kind.
///
/// Ahead of even that marker scan sits the external-ownership gate: when
/// `AI_MEMORY_CAPTURE_OWNER` holds any value that is non-empty after trimming,
/// `capturePolicy` returns `drop` immediately, so a generated consumer never
/// queues, spools, or POSTs a *new* capture event. It mirrors the native gate
/// in `commands/hook.rs` and leaves handoff fetching untouched, which is the
/// whole point: an external producer replaces capture without losing native
/// context delivery.
///
/// Two things it deliberately does not do. It does not suppress the consumer's
/// routing work around the call (`applyMarkerParams` reads the marker before
/// `capturePolicy` is even reached), and it does not erase an existing spool
/// backlog: a consumer that reaches `drainHookQueue` — on dispose, for
/// instance — still kicks off `requestSpoolDrain`, so events spooled before the
/// handover can still be delivered.
#[must_use]
pub(crate) fn ts_capture_policy_v1(capture_mode: &str) -> String {
    const TEMPLATE: &str = r##"// capture-policy-v1 (generated; do not fork between adapters)
const CAPTURE_POLICY_V1 = 1;
const CAPTURE_MODE: "allowlist" | "denylist" = "__AI_MEMORY_CAPTURE_MODE__";
const CAPTURE_MARKER_MAX_BYTES = 64 * 1024;
const CAPTURE_MAX_PATTERNS = 128;
const CAPTURE_MAX_PATTERN_CHARS = 1024;
const CAPTURE_MAX_PATH_CHARS = 4096;
const CAPTURE_MAX_CANDIDATES = 32;
const CAPTURE_MAX_WORK = 1000000;
const CAPTURE_MAX_SHELL_NESTING = 4;
const CAPTURE_MAX_CALL_ID_CHARS = 128;
// Generated from `capture_policy.rs` (`FILE_TOOL_NAMES` and its siblings, `MAX_SHELL_COMMAND_CHARS`) so the two front doors cannot drift; never edit them here.
const CAPTURE_FILE_TOOLS: string[] = __AI_MEMORY_FILE_TOOLS__;
const CAPTURE_SEARCH_TOOLS: string[] = __AI_MEMORY_SEARCH_TOOLS__;
const CAPTURE_NON_FILE_TOOLS: string[] = __AI_MEMORY_NON_FILE_TOOLS__;
const CAPTURE_INLINE_CODE_TOOLS: string[] = __AI_MEMORY_INLINE_CODE_TOOLS__;
const CAPTURE_OUTPUT_READER_TOOLS: string[] = __AI_MEMORY_OUTPUT_READER_TOOLS__;
const CAPTURE_MAX_COMMAND_CHARS = __AI_MEMORY_MAX_COMMAND_CHARS__;
// Bounds on the other markers one call may make the policy read; mirror `capture_lookup_locations` and `foreign_capture_decision`.
const CAPTURE_MAX_LOOKUP_LOCATIONS = 4096;
const CAPTURE_MAX_FOREIGN_MARKERS = 16;
const CAPTURE_MAX_GLOB_DIRECTORIES = 256;
const CAPTURE_MAX_GLOB_ENTRIES = 32768;
const CAPTURE_MAX_GLOB_MS = 50;
const CAPTURE_MAX_LOOKUP_PROBES = 16384;
// The clock the walk and lookup budget reads; a test stands in a slow filesystem with it.
let captureNow: () => number = Date.now;
// Readings of one glob whose `..` follows a `**`, and words of one brace expansion; mirror `MAX_GLOBSTAR_READINGS` and `MAX_BRACE_ALTERNATIVES`.
const CAPTURE_MAX_GLOBSTAR_READINGS = 16;
const CAPTURE_MAX_BRACE_ALTERNATIVES = 64;
// macOS's default filesystem ignores case and Unicode composition, so `NOTES/X.md` reads `notes/x.md`; mirrors `fold_case`.
let captureFoldCase = typeof process !== "undefined" && process.platform === "darwin";
// A path as a case-folding host compares it: NFC, then the full lowercase mapping; mirrors `fold_path`.
function captureFold(path: string): string { return captureFoldCase ? path.normalize("NFC").toLowerCase() : path; }

type CaptureDisposition = "keep" | "drop" | "metadata-only";
type CaptureProtocol = { version: 1; disposition: CaptureDisposition; policy_state: "inactive" | "active" | "invalid"; tool_family: "file" | "search-list" | "non-file" | "unknown"; path_count: number; extraction_state: "not-applicable" | "extracted" | "missing-or-malformed" | "unsupported-schema" };

type CaptureConfig = { state: "inactive" | "active" | "invalid"; patterns: { path: string; windows: boolean; directory?: string; prefix: string }[]; base: string; windowsHost: boolean; root?: string };
function readFileSync(path: string, encoding?: "utf8"): any { if (encoding) return readMarkerText(path, encoding); const fd = openSync(path, "r"); try { const bytes = Buffer.allocUnsafe(CAPTURE_MARKER_MAX_BYTES + 1); const count = readSync(fd, bytes, 0, bytes.length, 0); if (count > CAPTURE_MARKER_MAX_BYTES) throw new Error("marker too large"); const result = bytes.subarray(0, count); new TextDecoder("utf-8", { fatal: true }).decode(result); return result; } finally { closeSync(fd); } }
function captureTrimComment(line: string): string { let quote = ""; let escaped = false; for (let i = 0; i < line.length; i++) { const c = line[i]; if (escaped) { escaped = false; continue; } if (c === "\\" && quote === '"') { escaped = true; continue; } if ((c === '"' || c === "'") && (!quote || quote === c)) quote = quote ? "" : c; else if (c === "#" && !quote) { line = line.slice(0, i); break; } } if (quote) throw new Error("unterminated string"); return line; }
// `windowsHost` is omitted for self-determining callers (the cwd/marker base,
// and ignore_paths patterns, which are allowed an explicit UNC/drive form
// regardless of host). It is passed explicitly, from the already-resolved
// host base, only when normalizing an untrusted tool-argument candidate: on a
// POSIX host a leading `//` there is an ordinary doubled separator, not a UNC
// root, and must collapse before flavor detection or it escapes every POSIX
// `ignore_paths` pattern via a flavor mismatch (GHSA-vh98).
function captureNormalize(path: string, windowsHost?: boolean): { path: string; windows: boolean } | undefined { const raw = windowsHost === false && path.startsWith("//") ? `/${path.replace(/^\/+/, "")}` : path; const p = raw.replace(/\\/g, "/"); let root: string; let tail: string[]; if (p.startsWith("//")) { const x = p.slice(2).split("/").filter(Boolean); if (x.length < 2) return undefined; root = `//${x.shift()}/${x.shift()}`; tail = x; } else if (/^[A-Za-z]:\//.test(p)) { root = `${p[0].toUpperCase()}:/`; tail = p.slice(3).split("/"); } else if (p.startsWith("/")) { root = "/"; tail = p.slice(1).split("/"); } else return undefined; const out: string[] = []; for (const x of tail) { if (!x || x === ".") continue; if (x === "..") out.pop(); else out.push(x); } return { path: root + (out.length ? (root.endsWith("/") ? "" : "/") + out.join("/") : ""), windows: root !== "/" }; }
// Every reading of a glob whose `..` steps back over a `**`, which matches no directory as well as many: the `..` then steps back from the segment before it, or the `**` stays; a directory segment an older bash can expand to `..` is read as `..` too. Undefined when the path is not usable, false past CAPTURE_MAX_GLOBSTAR_READINGS; mirrors `normalize_readings`.
function captureReadings(path: string, windowsHost?: boolean): { path: string; windows: boolean }[] | undefined | false { const raw = windowsHost === false && path.startsWith("//") ? `/${path.replace(/^\/+/, "")}` : path; const p = raw.replace(/\\/g, "/"); let root: string; let tail: string[]; if (p.startsWith("//")) { const x = p.slice(2).split("/").filter(Boolean); if (x.length < 2) return undefined; root = `//${x.shift()}/${x.shift()}`; tail = x; } else if (/^[A-Za-z]:\//.test(p)) { root = `${p[0].toUpperCase()}:/`; tail = p.slice(3).split("/"); } else if (p.startsWith("/")) { root = "/"; tail = p.slice(1).split("/"); } else return undefined; let readings: string[][] = [[]]; let overflow = false; const segments = tail.filter((x) => x && x !== "."); segments.forEach((x, index) => { const parent = index + 1 < segments.length && x !== ".." && captureCouldNameParent(x); if (!parent && x !== "..") { for (const parts of readings) parts.push(x); return; } const next: string[][] = []; const push = (parts: string[]) => { if (!next.some((other) => other.length === parts.length && other.every((part, i) => part === parts[i]))) next.push(parts); }; for (const parts of readings) { if (parent) push(parts.slice(0, -1)); if (x !== "..") push([...parts, x]); else if (parts.length && parts[parts.length - 1].includes("**")) { push(parts.slice(0, -2)); push(parts); } else push(parts.slice(0, -1)); } if (next.length > CAPTURE_MAX_GLOBSTAR_READINGS) overflow = true; readings = next; }); if (overflow) return false; return readings.map((out) => ({ path: root + (out.length ? (root.endsWith("/") ? "" : "/") + out.join("/") : ""), windows: root !== "/" })); }
// Whether a glob segment can match `..` where a leading-dot pattern matches the `.` and `..` entries (bash before 5.2): `.` then `*`, `?`, a class admitting `.` (`[[:punct:]]` too) or an extglob group with an alternative matching `.` (`.@(.)`, `.!(x)`); mirrors `could_name_parent`.
function captureCouldNameParent(segment: string): boolean { if (!segment.startsWith(".")) return false; const rest = [...segment.slice(1)]; return /[*?\[(]/.test(segment.slice(1)) && captureClassGlob(rest, ["."]); }
// A one-segment glob over `*`, `?`, `[…]` classes (`!`/`^` negates, ranges, a leading `]` is a member, `[:name:]`, `[=c=]`, `[.c.]`) and extglob groups (`@(…)`, `?(…)`, `*(…)`, `+(…)`, `!(…)`) matched against a short `text`; an unclosed `[` or `(` is literal. Mirrors `class_glob_matches`.
function captureClassGlob(pattern: string[], text: string[]): boolean { if (!pattern.length) return !text.length; const head = pattern[0]; if ("@?*+!".includes(head) && pattern[1] === "(") { let depth = 0; let close = -1; for (let i = 1; i < pattern.length; i++) { if (pattern[i] === "(") depth++; else if (pattern[i] === ")" && --depth === 0) { close = i; break; } } if (close >= 0) { const alternatives: string[][] = []; let start = 2; depth = 0; for (let i = 2; i < close; i++) { if (pattern[i] === "(") depth++; else if (pattern[i] === ")") depth = Math.max(depth - 1, 0); else if (pattern[i] === "|" && depth === 0) { alternatives.push(pattern.slice(start, i)); start = i + 1; } } alternatives.push(pattern.slice(start, close)); const once = (t: string[]) => alternatives.some((alternative) => captureClassGlob(alternative, t)); const repeated = (t: string[]): boolean => { for (let split = 1; split <= t.length; split++) if (once(t.slice(0, split)) && (split === t.length || repeated(t.slice(split)))) return true; return false; }; const group = (t: string[]) => head === "@" ? once(t) : head === "?" ? !t.length || once(t) : head === "*" ? !t.length || repeated(t) : head === "+" ? repeated(t) : !once(t); for (let split = 0; split <= text.length; split++) if (group(text.slice(0, split)) && captureClassGlob(pattern.slice(close + 1), text.slice(split))) return true; return false; } } if (head === "*") { for (let skip = 0; skip <= text.length; skip++) if (captureClassGlob(pattern.slice(1), text.slice(skip))) return true; return false; } if (head === "?") return text.length > 0 && captureClassGlob(pattern.slice(1), text.slice(1)); if (head === "[") { const negated = pattern[1] === "!" || pattern[1] === "^"; const start = negated ? 2 : 1; const members: ((c: string) => boolean)[] = []; let i = start; if (pattern[i] === "]") { members.push((c) => c === "]"); i++; } let closed = false; while (i < pattern.length) { const c = pattern[i]; if (c === "]") { closed = true; break; } if (c === "[" && (pattern[i + 1] === ":" || pattern[i + 1] === "=" || pattern[i + 1] === ".")) { const kind = pattern[i + 1]; let end = -1; for (let at = i + 2; at + 1 < pattern.length; at++) if (pattern[at] === kind && pattern[at + 1] === "]") { end = at; break; } if (end >= 0) { const name = pattern.slice(i + 2, end).join(""); members.push(kind === ":" ? (x) => captureNamedClass(name, x) : (x) => [...name].length !== 1 || name === x); i = end + 2; continue; } members.push((x) => x === "["); i++; continue; } if (pattern[i + 1] === "-" && pattern[i + 2] !== undefined && pattern[i + 2] !== "]") { const low = c; const high = pattern[i + 2]; members.push((x) => low <= x && x <= high); i += 3; continue; } members.push((x) => x === c); i++; } if (!closed) return text[0] === "[" && captureClassGlob(pattern.slice(1), text.slice(1)); if (!text.length) return false; const found = members.some((member) => member(text[0])); return found !== negated && captureClassGlob(pattern.slice(i + 1), text.slice(1)); } return text[0] === head && captureClassGlob(pattern.slice(1), text.slice(1)); }
// Whether the POSIX class `[:name:]` admits `c`; an unknown class may admit anything. Mirrors `ClassMember::admits`.
function captureNamedClass(name: string, c: string): boolean { const code = c.codePointAt(0)!; const ascii = code < 128; switch (name) { case "alnum": return /[\p{L}\p{N}]/u.test(c); case "alpha": return /\p{L}/u.test(c); case "blank": return c === " " || c === "\t"; case "cntrl": return /\p{Cc}/u.test(c); case "digit": return /[0-9]/.test(c); case "lower": return /\p{Ll}/u.test(c); case "space": return /\s/.test(c); case "upper": return /\p{Lu}/u.test(c); case "word": return /[\p{L}\p{N}_]/u.test(c); case "xdigit": return /[0-9A-Fa-f]/.test(c); case "punct": return ascii && /[!-\/:-@\[-`{-~]/.test(c); case "graph": return code > 32 && code < 127; case "print": return code >= 32 && code < 127; case "ascii": return ascii; default: return true; } }
function captureJoin(base: string, child: string): string { if (/^[^A-Za-z]?:|^[A-Za-z]:[^/\\]/.test(child)) return child; return `${base.replace(/[\\/]+$/, "")}/${child}`; }
// Under a Windows host, every absolute path Windows can open for `path` from `base`, `/`-separated with its `.` and `..` still in place: the `\\?\`, `\\.\` and `\??\` prefixes and the local administrative share (`\\localhost\C$`) name the drive or share (any other device is no path), Git Bash/MSYS2/Cygwin `/c/x` and `/cygdrive/c/x` are `C:/x`, any other rooted path is on the base's drive or share, and each segment loses an alternate data stream suffix and its trailing dots and spaces. A native tool reads `/c/x` and `/cygdrive/c/x` from the root of the base's drive instead, so those give that path too. Undefined when it is not usable; mirrors `windows_spellings`.
function captureWindowsSpellings(path: string, base: string): string[] | undefined { if (/^[\x00-\x7f]:/.test(path) && !/^[A-Za-z]:[\\/]/.test(path)) return undefined; const b = captureNormalize(base)?.path ?? ""; let p = path.replace(/\\/g, "/"); let alternate: string | undefined; if (/^(?:\/\/[?.]\/|\/\?\?\/)/.test(p)) { const rest = p.slice(4); if (/^unc\//i.test(rest)) p = `//${rest.slice(4)}`; else if (/^[A-Za-z]:/.test(rest)) p = rest; else return undefined; } if (/^[A-Za-z]:$/.test(p)) p += "/"; if (p.startsWith("//")) { const [server = "", share = "", ...rest] = p.slice(2).split("/"); if ((/^localhost$/i.test(server) || server === "127.0.0.1") && /^[A-Za-z]\$$/.test(share)) p = `${share[0]}:/${rest.join("/")}`; } else if (p.startsWith("/")) { const forward = path.startsWith("/"); const [first, ...after] = p.slice(1).split("/"); const driveRoot = /^[A-Za-z]:/.test(b) ? b.slice(0, 2) : /^\/\/[^/]+\/[^/]+/.exec(b)?.[0]; if (driveRoot === undefined) return undefined; const native = driveRoot + p; if (forward && /^[A-Za-z]$/.test(first)) { p = `${first.toUpperCase()}:/${after.join("/")}`; alternate = native; } else if (forward && first.toLowerCase() === "cygdrive" && /^[A-Za-z]$/.test(after[0] ?? "")) { p = `${after[0].toUpperCase()}:/${after.slice(1).join("/")}`; alternate = native; } else p = native; } else if (!/^[A-Za-z]:\//.test(p)) { if (!b) return undefined; p = `${b.replace(/\/+$/, "")}/${p}`; } const segments = (p: string): string | undefined => { let root: string; let tail: string[]; if (p.startsWith("//")) { const x = p.slice(2).split("/").filter(Boolean); if (x.length < 2) return undefined; root = `//${x.shift()}/${x.shift()}`; tail = x; } else if (/^[A-Za-z]:\//.test(p)) { root = `${p[0].toUpperCase()}:/`; tail = p.slice(3).split("/"); } else return undefined; const parts = tail.map(captureWindowsSegment).filter((x): x is string => x !== undefined); return root + (parts.length ? (root.endsWith("/") ? "" : "/") + parts.join("/") : ""); }; const spellings: string[] = []; for (const reading of alternate === undefined ? [p] : [p, alternate]) { const spelled = segments(reading); if (spelled === undefined) return undefined; spellings.push(spelled); } return spellings; }
// The first of `captureWindowsSpellings`: Git Bash's reading of `/c/x`; mirrors `windows_spelling`.
function captureWindowsSpelling(path: string, base: string): string | undefined { return captureWindowsSpellings(path, base)?.[0]; }
// An absolute directory (cwd, marker directory) or pattern, normalized; a Windows one is read as Windows opens it, so it shares one spelling with every candidate compared with it (`\\?\C:\repo` is `C:/repo`); mirrors `normalize_root`.
function captureRoot(path: string): { path: string; windows: boolean } | undefined { if (!captureHostWindows(path)) return captureNormalize(path); const spelled = captureWindowsSpelling(path, ""); return spelled === undefined ? undefined : captureNormalize(spelled); }
// One segment as Windows reads it; undefined when nothing is left. Mirrors `windows_segment`.
function captureWindowsSegment(segment: string): string | undefined { if (/[*?[\]{}()|]/.test(segment)) return segment; const name = segment.split(":")[0]; const unspaced = name.replace(/ +$/, ""); if (unspaced === "." || unspaced === "..") return unspaced; return name.replace(/[. ]+$/, "") || undefined; }
// PowerShell opens `FileSystem::C:\x` and `Microsoft.PowerShell.Core\FileSystem::C:\x` (any case) as `C:\x`; read only on a Windows host. Mirrors `strip_powershell_provider`.
function captureStripProvider(path: string): string { let rest = path; for (;;) { const prefix = /^(?:microsoft\.powershell\.core[\\/])?filesystem::/i.exec(rest); if (!prefix) return rest; rest = rest.slice(prefix[0].length); } }
// A Windows shell word naming a file the matcher cannot place, so its command proves no miss: a drive-relative path (`C:docs\x`, a bare `C:`) or a device-namespace path naming no drive or share (`\\?\GLOBALROOT\...`, `\\.\HarddiskVolume3\...`). Mirrors `windows_unplaceable`.
function captureWindowsUnplaceable(word: string, base: string): boolean { const w = captureStripProvider(word); if (/^[A-Za-z]:(?![\\/])/.test(w)) return true; return /^(?:[\\/]{2}[?.][\\/]|[\\/]\?\?[\\/])/.test(w) && captureWindowsSpellings(w, base) === undefined; }
// Every path a candidate names from `base`, read as Windows reads it on a Windows host.
function captureResolve(path: string, base: string, windowsHost: boolean): string[] | undefined { if (windowsHost) return captureWindowsSpellings(captureStripProvider(path), base); return [/^(?:\/|\\\\|[A-Za-z]:[\\/])/.test(path) ? path : captureJoin(base, path)]; }
// Every reading (`captureReadings`) of every path `resolved` holds: undefined when one is unusable, false past CAPTURE_MAX_GLOBSTAR_READINGS in all.
function captureAllReadings(resolved: string[] | undefined, windowsHost: boolean): { path: string; windows: boolean }[] | undefined | false { if (resolved === undefined) return undefined; const all: { path: string; windows: boolean }[] = []; for (const path of resolved) { const readings = captureReadings(path, windowsHost); if (!readings) return readings; all.push(...readings); if (all.length > CAPTURE_MAX_GLOBSTAR_READINGS) return false; } return all; }
// Longer than CAPTURE_MAX_COMMAND_CHARS characters summed over its argv elements; mirrors `ShellCommand::oversized`.
function captureCommandOversized(command: string | string[]): boolean { let total = 0; for (const item of typeof command === "string" ? [command] : command) for (const _c of item) if (++total > CAPTURE_MAX_COMMAND_CHARS) return true; return false; }
// A TOML key (`a`, `"a"`, `'a'`, dotted `a . "b"`) at the start of `text`: its parts and the index past it and any blanks, or undefined. Enough of the key grammar to see every spelling of `capture`.
function captureTomlKey(text: string): { parts: string[]; end: number } | undefined { const parts: string[] = []; let i = 0; for (;;) { while (text[i] === " " || text[i] === "\t") i++; if (text[i] === '"') { let j = i + 1; while (j < text.length && text[j] !== '"') j += text[j] === "\\" ? 2 : 1; if (j >= text.length) return undefined; const decoded = captureParseArray(`[${text.slice(i, j + 1)}]`); if (!decoded) return undefined; parts.push(decoded[0]); i = j + 1; } else if (text[i] === "'") { const j = text.indexOf("'", i + 1); if (j < 0) return undefined; parts.push(text.slice(i + 1, j)); i = j + 1; } else { const bare = /^[A-Za-z0-9_-]+/.exec(text.slice(i)); if (!bare) return undefined; parts.push(bare[0]); i += bare[0].length; } while (text[i] === " " || text[i] === "\t") i++; if (text[i] !== ".") return { parts, end: i }; i++; } }
function captureValidGlob(p: string): boolean { return !!p && [...p].length <= CAPTURE_MAX_PATTERN_CHARS && !/[!{}\[\]()|^$%]/.test(p) && !p.includes("${") && !p.includes("***") && !p.replace(/\\/g, "/").split("/").includes("..") && (!p.startsWith("~") || p.startsWith("~/")) && !/^[^A-Za-z]?:/.test(p) && !/^[A-Za-z]:[^/\\]/.test(p); }
function captureParseArray(value: string): string[] | undefined { let i = 0; const out: string[] = []; const ws = () => { while (/\s/.test(value[i] ?? "")) i++; }; const basic = { b: "\b", t: "\t", n: "\n", f: "\f", r: "\r", '"': '"', "\\": "\\" } as Record<string, string>; ws(); if (value[i++] !== "[") return undefined; for (;;) { ws(); if (value[i] === "]") { i++; ws(); return i === value.length ? out : undefined; } const quote = value[i++]; if (quote !== '"' && quote !== "'") return undefined; let s = ""; for (;;) { if (i >= value.length) return undefined; const c = value[i++]; if (c === quote) break; if (c === "\\" && quote === '"') { const e = value[i++]; if (e in basic) s += basic[e]; else if (e === "u" || e === "U") { const count = e === "u" ? 4 : 8; const hex = value.slice(i, i + count); if (!new RegExp(`^[0-9A-Fa-f]{${count}}$`).test(hex)) return undefined; const n = Number.parseInt(hex, 16); if (n > 0x10ffff || (n >= 0xd800 && n <= 0xdfff)) return undefined; s += String.fromCodePoint(n); i += count; } else return undefined; } else if (c === "\n" || c === "\r") return undefined; else s += c; } out.push(s); ws(); if (value[i] === ",") { i++; continue; } if (value[i] === "]") continue; return undefined; } }
// Follows the arrays and inline tables a TOML value leaves open across lines, each open `[` or `{` on `stack`. False when `text` cannot be (part of) a TOML 1.0 value: a close that matches nothing open, a `=` directly inside an array (a key line swallowed by one), text after the outermost close of a `continued` value, or an inline table still open at the end of the line.
function captureValueScan(text: string, stack: string[], continued: boolean): boolean { let quote = ""; for (let i = 0; i < text.length; i++) { const c = text[i]; if (quote) { if (c === "\\" && quote === '"') i++; else if (c === quote) quote = ""; continue; } if (continued && !stack.length && !/\s/.test(c)) return false; if (c === '"' || c === "'") quote = c; else if (c === "[" || c === "{") stack.push(c); else if (c === "]" || c === "}") { if (stack.pop() !== (c === "]" ? "[" : "{")) return false; } else if (c === "=" && stack[stack.length - 1] === "[") return false; } return stack[stack.length - 1] !== "{"; }
function captureHostWindows(path: string): boolean { return path.startsWith("\\\\") || path.startsWith("//") || /^[A-Za-z]:/.test(path); }
function captureConfig(cwd: string | undefined): CaptureConfig { return captureConfigAt(findMarker(cwd), cwd); }
// The policy one marker declares, for an event reported from `cwd`.
function captureConfigAt(marker: string | undefined, cwd: string | undefined): CaptureConfig {
  // Host flavor comes from the cwd STRING itself, never from `resolve(cwd)`:
  // `resolve` re-roots a drive/UNC-shaped cwd through the real OS's own path
  // module, which only agrees on the OS that actually owns that path. The
  // cwd the hook reports is already absolute and already names its own host,
  // exactly like the native hook's pure-string `flavor_of(cwd)`; a Windows
  // cwd is normalized lexically for the same reason.
  const windowsHost = captureHostWindows(cwd ?? "");
  const candidateBase = (windowsHost ? captureRoot(cwd ?? "") : captureNormalize(cwd ? resolve(cwd) : ""))?.path ?? "";
  if (!marker) return { state: "inactive", patterns: [], base: candidateBase, windowsHost };
  try {
    const bytes = readFileSync(marker);
    // A Windows marker's directory is cut lexically and spelled like the cwd.
    const markerBase = (windowsHost ? captureRoot(marker.replace(/[\\/][^\\/]*$/, "")) : captureNormalize(dirname(marker)))?.path ?? candidateBase;
    if (bytes.byteLength > CAPTURE_MARKER_MAX_BYTES) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
    // Every TOML spelling of the capture table the native parser honours
    // (`[capture]`, `[ "capture" ]`, a top-level dotted `capture.ignore_paths`)
    // is read; any other shape of a `capture` key (an inline table,
    // `[[capture]]`, `[capture.x]`, a second definition) is invalid, as
    // `read_capture_config` finds it.
    // A value's arrays may span lines (`stack`), and a line inside one,
    // `["docs"]` among them, is never a table header.
    let section: "top" | "capture" | "other" = "top";
    let value = "";
    let collecting = false;
    const stack: string[] = [];
    let seen = false;
    let defined = false;
    for (const raw of bytes.toString("utf8").split(/\r?\n/)) {
      const line = captureTrimComment(raw).trim();
      if (!line) continue;
      if (stack.length) {
        if (!captureValueScan(line, stack, true)) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
        if (collecting) value += ` ${line}`;
        collecting &&= stack.length > 0;
        continue;
      }
      if (line.startsWith("[")) {
        const array = line.startsWith("[[");
        const close = array ? "]]" : "]";
        const inner = line.slice(array ? 2 : 1, line.length - close.length);
        const header = line.endsWith(close) ? captureTomlKey(inner) : undefined;
        if (!header || header.end !== inner.length) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
        if (header.parts[0] !== "capture") { section = "other"; continue; }
        if (array || header.parts.length !== 1 || defined) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
        section = "capture";
        defined = true;
        continue;
      }
      const key = captureTomlKey(line);
      const kv = key && line[key.end] === "=" ? key : undefined;
      // Neither a header nor a key/value line: not TOML, as toml_edit finds it.
      if (!kv || !captureValueScan(line.slice(kv.end + 1), stack, false)) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
      if (section === "other") continue;
      if (section === "top" && kv.parts[0] !== "capture") continue;
      const path = kv ? (section === "top" ? kv.parts.slice(1) : kv.parts) : [];
      if (!kv || seen || path.length !== 1 || path[0] !== "ignore_paths") return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
      seen = true;
      defined = true;
      value = line.slice(kv.end + 1).trim();
      collecting = stack.length > 0;
    }
    if (stack.length) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
    if (!seen) return { state: "inactive", patterns: [], base: candidateBase, windowsHost };
    const strings = captureParseArray(value);
    if (!strings || strings.length > CAPTURE_MAX_PATTERNS) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
    const home = homedir();
    const patterns = strings.map((source) => {
      if (!captureValidGlob(source)) return undefined;
      const expanded = source.startsWith("~/")
        ? captureJoin(home, source.slice(2))
        : /^(?:\/|\\\\|[A-Za-z]:[\\/])/.test(source)
          ? source
          : captureJoin(markerBase, source);
      const parsed = captureRoot(expanded);
      // A Windows session's candidates are all Windows-flavored, so a POSIX pattern (`/c/repo/x/**`) would match nothing while the policy reports active; it fails closed, as `compile` rejects it.
      if (!parsed || (windowsHost && !parsed.windows)) return undefined;
      const normalized = { path: captureFold(parsed.path), windows: parsed.windows };
      return { path: normalized.path, windows: normalized.windows, directory: normalized.path.endsWith("/**") ? (normalized.path.slice(0, -3) || "/") : undefined, prefix: captureLiteralPrefix(normalized.path) };
    });
    if (patterns.some((p) => !p)) return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
    return patterns.length
      ? { state: "active", patterns: patterns as CaptureConfig["patterns"], base: candidateBase, windowsHost, root: markerBase }
      : { state: "inactive", patterns: [], base: candidateBase, windowsHost };
  } catch (_e) {
    return { state: "invalid", patterns: [], base: candidateBase, windowsHost };
  }
}
function captureGlob(pattern: string, candidate: string, insensitive: boolean, budget: { work: number }): boolean | undefined { const states = captureGlobStates(pattern, candidate, insensitive, budget); return states && states[states.length - 1]; }
// Whether `pattern` matches some path strictly below `directory`: a prefix still alive after `directory/` can always be completed.
function captureGlobBelow(pattern: string, directory: string, insensitive: boolean, budget: { work: number }): boolean | undefined { const states = captureGlobStates(pattern, directory.endsWith("/") ? directory : `${directory}/`, insensitive, budget); return states && states.some(Boolean); }
function captureGlobStates(pattern: string, candidate: string, insensitive: boolean, budget: { work: number }): boolean[] | undefined { const p = [...pattern]; const c = [...candidate]; const previous = new Array<boolean>(p.length + 1).fill(false); previous[0] = true; for (let j = 1; j <= p.length; j++) previous[j] = p[j - 1] === "*" && p[j] !== "*" && previous[j - 1]; for (const ch of c) { const current = new Array<boolean>(p.length + 1).fill(false); for (let j = 1; j <= p.length; j++) { if (++budget.work > CAPTURE_MAX_WORK) return undefined; const x = p[j - 1]; current[j] = x === "*" && p[j] === "*" ? false : x === "*" && j >= 2 && p[j - 2] === "*" ? current[j - 2] || previous[j] : x === "*" ? current[j - 1] || (ch !== "/" && previous[j]) : x === "?" ? ch !== "/" && previous[j - 1] : captureCharEq(x, ch, insensitive) && previous[j - 1]; } for (let j = 0; j <= p.length; j++) previous[j] = current[j]; } return previous; }
function captureCharEq(a: string, b: string, insensitive: boolean): boolean { return insensitive && a.charCodeAt(0) < 128 && b.charCodeAt(0) < 128 ? a.toLowerCase() === b.toLowerCase() : a === b; }
function captureLiteralPrefix(path: string): string { const glob = path.search(/[*?]/); if (glob < 0) return path; const slash = path.lastIndexOf("/", glob); if (slash < 0) return ""; const head = path.slice(0, slash + 1); const trimmed = head.slice(0, -1); return trimmed && !trimmed.endsWith(":") ? trimmed : head; }
function captureStartsWith(path: string, prefix: string, insensitive: boolean): boolean { const p = path[Symbol.iterator](); for (const x of prefix) { const c = p.next(); if (c.done || !captureCharEq(x, c.value, insensitive)) return false; } return true; }
function captureGlobReaches(glob: string, prefix: string, insensitive: boolean, budget: { work: number }): boolean | undefined { const target = prefix.replace(/\/+$/, ""); const depth = target.split("/").filter(Boolean).length; if (!depth) return true; let seen = 0; let offset = 0; for (const part of glob.split("/")) { offset += part.length; if (part && ++seen === depth) return captureGlob(glob.slice(0, offset), target, insensitive, budget); offset++; } return false; }
// An argv element is one word as given and is also tokenized on its own
// (`bash -lc "<script>"`); joining elements would re-split paths with spaces.
// Elements over 256 chars are scripts, so only their tokens count.
function captureShellCommand(args: Record<string, unknown> | undefined): string | string[] | undefined { if (!args || typeof args !== "object" || Array.isArray(args)) return undefined; const value = "command" in args ? args.command : ("cmd" in args ? args.cmd : args.CommandLine); if (typeof value === "string") return value; if (Array.isArray(value) && value.every((x) => typeof x === "string")) return value as string[]; return undefined; }
// Split only when a policy is active: this runs for every tool event. Every
// word, and again every word inside a word that holds more than one
// (`bash -c 'cat x'`, `"$(cat x)"`), down to CAPTURE_MAX_SHELL_NESTING levels;
// a script nested deeper is opaque. Mirrors `ShellCommand::split`.
// `windows` also reads the command without PowerShell backquote and cmd caret
// escapes. A `~-`, `~N` or `~name` prefix names a directory only the running
// shell knows, which makes the command unprovable. Each word carries whether a
// `~` in it was outside quotes: a quoted tilde, or one in an argv element or a
// quoted message, is literal, except under PowerShell, which reads `~` itself.
// `expanded` holds the words of each script with such syntax as the shell expands them where that changes the directory a path names (a zsh glob group kept in its word, ANSI-C numeric escapes decoded), for the marker lookup only; mirrors `ShellWords::expanded`.
type CaptureSplit = { words: [string, boolean][]; opaque: boolean; expanded: [string, boolean][] };
function captureShellSplit(command: string | string[], windows = false): CaptureSplit { const split: CaptureSplit = { words: [], opaque: false, expanded: [] }; const readings = (text: string): [string, boolean][] => { const texts = [text]; const plain = windows ? captureWithoutWindowsEscapes(text) : undefined; if (plain !== undefined) texts.push(plain); const out: [string, boolean][] = []; for (const t of texts) { out.push([t, false]); if (windows && t.includes("\\")) out.push([t, true]); } return out; }; if (typeof command === "string") { split.opaque = captureUnevaluatedExpansion(command); if (split.opaque) split.expanded.push(...captureMarkedWords(command, true)); for (const [text, literal] of readings(command)) captureNestedWords(captureMarkedWords(text, false, literal), CAPTURE_MAX_SHELL_NESTING, split, literal); } else { split.opaque = command.some((item) => captureUnevaluatedExpansion(item)) || captureInlineCode(command); for (const item of command) if (captureUnevaluatedExpansion(item)) split.expanded.push(...captureMarkedWords(item, true)); command.forEach((item, index) => { const script = captureShellScript(command, index); for (const [text, literal] of readings(item)) captureArgvItem(text, script, split, literal); }); } if (windows) for (const word of split.words) word[1] = true; if (split.words.some(([word, tilde]) => tilde && captureShellArguments(word).some(([argument]) => captureUnknownDirectory(argument)))) split.opaque = true; return split; }
function captureArgvItem(item: string, script: boolean, split: CaptureSplit, literal = false): void { const tokens = captureMarkedWords(item, false, literal); if (tokens.length === 1 && tokens[0][0] === item) { capturePushWord(item, false, split); return; } if ([...item].length <= 256) split.words.push([item, false]); captureNestedWords(captureLiteralTildes(tokens, script), CAPTURE_MAX_SHELL_NESTING - 1, split, literal); }
// Only a script a shell runs expands a tilde; mirrors `literal_tildes`.
function captureLiteralTildes(tokens: [string, boolean][], script: boolean): [string, boolean][] { return script ? tokens : tokens.map(([token]) => [token, false] as [string, boolean]); }
// PowerShell's backquote and cmd's caret each stand for the next character; mirrors `without_windows_escapes`.
function captureWithoutWindowsEscapes(command: string): string | undefined { if (!/[`^]/.test(command)) return undefined; const chars = [...command]; let plain = ""; for (let i = 0; i < chars.length; i++) { if (chars[i] === "`" || chars[i] === "^") { i++; if (i < chars.length) plain += chars[i]; } else plain += chars[i]; } return plain; }
function captureUnknownDirectory(argument: string): boolean { if (!argument.startsWith("~")) return false; const prefix = argument.slice(1).split(/[\/\\]/)[0]; return prefix !== "" && prefix !== "+"; }
// Only a script a shell runs expands braces, classes and tildes; a quoted message or pattern does not.
function captureNestedWords(words: [string, boolean][], depth: number, split: CaptureSplit, literal = false): void { const texts = words.map(([word]) => word); if (captureInlineCode(texts)) split.opaque = true; words.forEach(([word, tilde], index) => { capturePushWord(word, tilde, split); const tokens = captureMarkedWords(word, false, literal); if (tokens.length === 1 && tokens[0][0] === word) return; if (depth === 0) { split.opaque = true; return; } const script = word.includes("$(") || word.includes("`") || captureShellScript(texts, index); if (script && captureUnevaluatedExpansion(word)) { split.opaque = true; split.expanded.push(...captureMarkedWords(word, true)); } captureNestedWords(captureLiteralTildes(tokens, script), depth - 1, split, literal); }); }
// Also adds the POSIX reading of a word with `\o` escapes, which the shell reads as `o`, and the reading of each with every parameter default (`${X:-docs/x.md}`) replaced by its operand; a default nested deeper than the matcher reads makes the command unprovable.
function capturePushWord(word: string, tilde: boolean, split: CaptureSplit): void { capturePushReading(word, tilde, split); if (!word.includes("\\")) return; const chars = [...word]; let plain = ""; for (let i = 0; i < chars.length; i++) { if (chars[i] === "\\") { i++; if (i < chars.length) plain += chars[i]; } else plain += chars[i]; } if (plain !== word) capturePushReading(plain, tilde, split); }
function capturePushReading(word: string, tilde: boolean, split: CaptureSplit): void { split.words.push([word, tilde]); const operands = captureParameterOperands(word); if (operands === undefined) return; split.words.push([operands[0], tilde]); if (operands[1]) split.opaque = true; }
// Mirrors `parameter_operands`: `${NAME:-w}`, `${NAME:=w}`, `${NAME:+w}` (or without `:`) become `w`, nested down to CAPTURE_MAX_SHELL_NESTING levels; the flag is set when one is nested deeper still.
function captureParameterOperands(word: string): [string, boolean] | undefined { let current = captureReplaceOperands(word); if (current === undefined) return undefined; for (let i = 1; i < CAPTURE_MAX_SHELL_NESTING; i++) { const next = captureReplaceOperands(current); if (next === undefined) return [current, false]; current = next; } return [current, captureReplaceOperands(current) !== undefined]; }
function captureReplaceOperands(word: string): string | undefined { let output = ""; let rest = word; let changed = false; for (;;) { const start = rest.indexOf("${"); if (start < 0) break; output += rest.slice(0, start); const body = rest.slice(start + 2); const name = (/^[A-Za-z0-9_]*/.exec(body) ?? [""])[0].length; const after = body.slice(name); const operator = /^:[-=+]/.test(after) ? 2 : /^[-=+]/.test(after) ? 1 : 0; const operand = after.slice(operator); let depth = 0; let end = -1; for (let i = 0; i < operand.length; i++) { const c = operand[i]; if (c === "{") depth++; else if (c === "}") { if (depth === 0) { end = i; break; } depth--; } } if (end >= 0 && name > 0 && operator > 0) { output += operand.slice(0, end); changed = true; rest = operand.slice(end + 1); } else { output += "${"; rest = body; } } output += rest; return changed ? output : undefined; }
function captureCommandName(word: string): string { const name = word.slice(Math.max(word.lastIndexOf("/"), word.lastIndexOf("\\")) + 1).replace(/[A-Z]/g, (c) => c.toLowerCase()); return name.endsWith(".exe") ? name.slice(0, -4) : name; }
// Skips options and their values (`bash -o pipefail -c`); mirrors `is_shell_script`/`takes_shell_value`.
function captureShellScript(words: string[], index: number): boolean { let flagged = false; for (let i = index - 1; i >= 0; i--) { const word = words[i]; if (word.startsWith("-")) { flagged ||= !word.startsWith("--") && word.slice(1).includes("c"); continue; } if (word.length > 1 && word.startsWith("+")) continue; if (i > 0 && captureShellValueOption(words[i - 1])) continue; const name = captureCommandName(word); return (flagged && ["sh","bash","zsh","dash","ksh","mksh","ash","yash","fish","su","script"].includes(name)) || (i + 1 === index && name === "eval"); } return false; }
function captureShellValueOption(flag: string): boolean { return ["--rcfile","--init-file","--init-command","-C"].includes(flag) || (flag.length > 1 && (flag.startsWith("+") || (flag.startsWith("-") && !flag.startsWith("--"))) && /[oO]$/.test(flag)); }
// Inline interpreter code (`python3 -c`, `node -e`, `ruby -e`, `perl -e`, `php -r`, `lua -e`, `Rscript -e`, `osascript -e`, `deno eval`) builds paths at run time; mirrors `runs_inline_code`.
function captureInlineCode(words: string[]): boolean { return words.some((word, index) => { const name = captureCommandName(word); const versioned = (prefix: string) => name.startsWith(prefix) && /^[0-9.]*$/.test(name.slice(prefix.length)); if (name === "deno") return words[index + 1] === "eval"; const spec: [string, string[]] | undefined = versioned("python") || versioned("pypy") || name === "py" ? ["c", ["-W","-X","--check-hash-based-pycs"]] : versioned("perl") ? ["eE", ["-I","-M","-m"]] : versioned("ruby") ? ["e", ["-I","-r","-C","-E"]] : ["node","nodejs","bun"].includes(name) ? ["ep", ["-r","--require","--import","--loader","--experimental-loader","-C","--conditions","--input-type","--title","--env-file"]] : versioned("php") ? ["r", ["-d","-c","-z"]] : ["lua","luajit","rscript","osascript"].includes(name) ? ["e", ["-l"]] : undefined; if (!spec) return false; const [letters, values] = spec; for (let i = index + 1; i < words.length; i++) { const flag = words[i]; if (!flag.startsWith("-")) return false; if (["--eval","--print"].includes(flag) || flag.startsWith("--eval=") || flag.startsWith("--print=") || (!flag.startsWith("--") && [...flag.slice(1)].some((c) => letters.includes(c)))) return true; if (values.includes(flag)) i++; } return false; }); }
// Each word with whether it holds a `~` outside quotes and not after a backslash; mirrors `marked_shell_words`. With `expand`, a zsh glob group stays in its word and ANSI-C numeric and control escapes are decoded; mirrors `shell_tokens`. With `literal`, every backslash is an ordinary character, as PowerShell and cmd read it; mirrors `literal_shell_words`.
function captureMarkedWords(command: string, expand = false, literal = false): [string, boolean][] { const special = (c: string) => /\s/.test(c) || "|&;<>()`".includes(c); const chars = [...command]; const words: [string, boolean][] = []; let word = ""; let tilde = false; let inWord = false; let quote = ""; for (let i = 0; i < chars.length; i++) { const c = chars[i]; const next = chars[i + 1]; if (quote === "$") { if (c === "'") quote = ""; else if (c === "\\" && next !== undefined && !literal) { const plain = captureAnsiEscape(next); const numeric = plain === undefined && expand ? captureAnsiNumeric(chars, i + 1) : undefined; if (plain !== undefined) { word += plain; i++; } else if (numeric) { word += numeric[0]; i = numeric[1]; } else { word += c + next; i++; } } else word += c; } else if (quote) { if (c === quote) quote = ""; else if (!literal && quote === '"' && c === "\\" && next === "\n") i++; else if (!literal && quote === '"' && c === "\\" && (next === '"' || next === "\\" || next === "$" || next === "`")) { word += next; i++; } else word += c; } else if (c === "'" || c === '"') { quote = c; inWord = true; } else if (c === "$" && (next === "'" || next === '"')) { quote = next === "'" ? "$" : '"'; i++; inWord = true; } else if (!literal && c === "\\" && next === "\n") { i++; } else if (!literal && c === "\\" && next !== undefined && (special(next) || next === "'" || next === '"' || next === "\\")) { word += next; i++; inWord = true; } else if (c === "(" && expand && (!inWord || !/[$=]$/.test(word)) && captureGlobGroup(chars, i, inWord) !== undefined) { const end = captureGlobGroup(chars, i, inWord)!; word += chars.slice(i, end + 1).join(""); i = end; inWord = true; } else if (special(c)) { if (inWord) words.push([word, tilde]); word = ""; tilde = false; inWord = false; } else { if (c === "~" && !word.endsWith("\\")) tilde = true; word += c; inWord = true; } } if (inWord) words.push([word, tilde]); return words; }
// The index of the `)` closing the zsh glob group `(` opens at `open`: inside a word, or at a word's start when a word character follows; undefined for a subshell or when whitespace, a quote, an escape or another operator comes first. Mirrors `glob_group`.
function captureGlobGroup(chars: string[], open: number, inWord: boolean): number | undefined { let depth = 0; for (let i = open; i < chars.length; i++) { const c = chars[i]; if (/\s/.test(c) || "'\"\\;&<>`".includes(c)) return undefined; if (c === "(") depth++; else if (c === ")" && --depth === 0) { const next = chars[i + 1]; return inWord || (next !== undefined && !/\s/.test(next) && !"|&;<>()`".includes(next)) ? i : undefined; } } return undefined; }
// A numeric or control ANSI-C escape whose letter or first digit is at `at` (`\xHH`, `\uHHHH`, `\UHHHHHHHH`, `\NNN`, `\cX`): the character and the index of its last source character, or undefined. Mirrors `ansi_c_numeric_escape`.
function captureAnsiNumeric(chars: string[], at: number): [string, number] | undefined { const escape = chars[at]; if (escape === "c") { const control = chars[at + 1]; return control !== undefined && control.charCodeAt(0) < 128 ? [String.fromCharCode(control.charCodeAt(0) & 0x1f), at + 1] : undefined; } const shape = escape === "x" ? [16, 2] : escape === "u" ? [16, 4] : escape === "U" ? [16, 8] : /^[0-7]$/.test(escape ?? "") ? [8, 2] : undefined; if (!shape) return undefined; const [radix, most] = shape; const digit = radix === 16 ? /^[0-9A-Fa-f]$/ : /^[0-7]$/; let value = radix === 8 ? Number(escape) : 0; let end = at; for (let n = 0; n < most && digit.test(chars[end + 1] ?? ""); n++) { end++; value = value * radix + parseInt(chars[end], radix); } if (radix === 16 && end === at) return undefined; return value <= 0x10ffff && !(value >= 0xd800 && value <= 0xdfff) ? [String.fromCodePoint(value), end] : undefined; }
// `$'…'` escapes; numeric and control escapes stay as written. Mirrors `ansi_c_escape`.
function captureAnsiEscape(escape: string): string | undefined { const decoded: Record<string, string> = { a: "\u0007", b: "\b", e: "\u001b", E: "\u001b", f: "\f", n: "\n", r: "\r", t: "\t", v: "\v", "\\": "\\", "'": "'", '"': '"', "?": "?" }; return Object.prototype.hasOwnProperty.call(decoded, escape) ? decoded[escape] : undefined; }
// Whether an unquoted word holds syntax lexical matching does not evaluate: a brace expansion (`{a,b}`, `{1..3}`), a bracket class (`[ab]`, `[]a]`), a zsh glob group or extglob (`docs/(a|b)`, `(a|b)/x`), or an ANSI-C numeric/control escape; mirrors `has_unevaluated_expansion`.
function captureUnevaluatedExpansion(command: string): boolean { const operator = (c: string) => "|&;<>()`".includes(c); const isWord = (c: string | undefined) => c !== undefined && !/\s/.test(c) && !operator(c); const chars = [...command]; let quote = ""; let previous = ""; let inWord = false; let parameter = false; let brace: boolean | undefined; let klass: number | undefined; const groups: boolean[] = []; for (let i = 0; i < chars.length; i++) { const c = chars[i]; if (quote) { if (((quote === "$" || quote === "'") && c === "'") || (quote === '"' && c === '"')) quote = ""; else if (quote === "$" && c === "\\") { const escape = chars[++i]; if (escape !== undefined && /^[xuUc0-7]$/.test(escape)) return true; } else if (quote === '"' && c === "\\") i++; if (klass !== undefined) klass++; previous = ""; inWord = true; continue; } if (c === "'" && previous === "$") quote = "$"; else if (c === "'" || c === '"') quote = c; else if (c === "\\") { i++; if (klass !== undefined) klass++; previous = ""; inWord = true; continue; } else if (c === "(") { if (inWord && previous !== "$" && previous !== "=") return true; groups.push(!inWord && previous !== "<" && previous !== ">"); parameter = false; brace = undefined; klass = undefined; inWord = false; } else if (c === ")") { if (groups.pop() === true && isWord(chars[i + 1])) return true; parameter = false; brace = undefined; klass = undefined; inWord = false; } else if (c === "|" && groups[groups.length - 1] === true && inWord && isWord(chars[i + 1])) return true; else if (/\s/.test(c) || operator(c)) { parameter = false; brace = undefined; klass = undefined; inWord = false; } else if (c === "{" && previous === "$") parameter = true; else if (c === "}" && parameter) parameter = false; else if (parameter) { /* parameter expansion */ } else if (c === "{") brace = false; else if (c === "," && brace !== undefined) brace = true; else if (c === "." && brace !== undefined && previous === ".") brace = true; else if (c === "}" && brace !== undefined) { if (brace) return true; brace = undefined; } else if (c === "[" && klass === undefined) klass = 0; else if (c === "]" && klass === 0) klass = 1; else if (c === "]" && klass !== undefined) return true; else if (klass !== undefined) klass++; if (!/\s/.test(c) && !operator(c)) inWord = true; previous = c; } return false; }
// Also a value glued to a short flag (`-fFILE`), the path of a git `REV:path` object name, flagged because git resolves it from the repository root, and the path of any of these that is a `file:` URL; mirrors `shell_arguments`.
function captureShellArguments(word: string): [string, boolean][] { const flag = word.startsWith("-"); const out: [string, boolean][] = flag ? [] : [[word, false]]; const eq = word.indexOf("="); const colon = flag && eq < 0 ? word.indexOf(":") : -1; if (eq >= 0) out.push([word.slice(eq + 1), false]); else if (colon >= 0) out.push([word.slice(colon + 1), false]); if (flag && !word.startsWith("--")) { const glued = [...word.slice(1)].slice(1).join(""); if (glued) out.push([glued, false]); } const object = captureGitObjectPath(word); if (object !== undefined) out.push([object, true]); for (const [argument] of [...out]) { const url = captureFileUrlPath(argument); if (url !== undefined) out.push([url, false]); } return out.filter(([argument]) => argument.trim() !== ""); }
// The local path of a `file:` URL in any case, cut at its query or fragment and then ASCII percent-escapes decoded; mirrors `file_url_path`.
function captureFileUrlPath(argument: string): string | undefined { if (argument.slice(0, 5).toLowerCase() !== "file:") return undefined; const rest = argument.slice(5).split(/[?#]/)[0]; let path = rest; if (rest.startsWith("//")) { const slash = rest.indexOf("/", 2); if (slash < 0) return undefined; path = rest.slice(slash); } if (/^\/[A-Za-z]:/.test(path)) path = path.slice(1); return path.replace(/%([0-7][0-9A-Fa-f])/g, (_match, hex: string) => String.fromCharCode(Number.parseInt(hex, 16))); }
function captureGitObjectPath(word: string): string | undefined { if (word.startsWith("-")) return undefined; const colon = word.indexOf(":"); if (colon < 0) return undefined; const revision = word.slice(0, colon); let path = word.slice(colon + 1); if (!revision && /^[0-3]:/.test(path)) path = path.slice(2); return !/^[A-Za-z]$/.test(revision) && /^[A-Za-z0-9._\/~^@{}-]*$/.test(revision) && !path.startsWith("//") ? path : undefined; }
// Lexical only, like the native hook: nothing is expanded or executed, so
// variables, command substitution, and `cd` state are not followed. A tool's
// own `workdir` replaces the event cwd for relative arguments. On a Windows host
// a word the matcher cannot place (`captureWindowsUnplaceable`) proves no miss,
// unless it is only a POSIX shell's reading of a placeable native path
// (`C:\repo\x` read as `C:repox`); mirrors `match_command`.
function captureMatchCommand(words: [string, boolean][], config: CaptureConfig, workdir?: string): { match: boolean | undefined; spool: boolean } { const budget = { work: 0 }; const home = homedir(); const base = captureBase(config, workdir); let spool = false; const driveRelative: string[] = []; const placed = new Set<string>(); for (const [word, tilde] of words) for (const [argument, fromRoot] of captureShellArguments(word)) { const expanded = captureExpandTilde(argument, tilde, home, base); if (config.windowsHost) { if (!captureWindowsUnplaceable(expanded, base)) { if (expanded.includes("\\")) placed.add(expanded.replace(/\\(.?)/gs, "$1")); } else if (/^[A-Za-z]:/.test(captureStripProvider(expanded))) driveRelative.push(expanded); else spool = true; } if ([...expanded].length > CAPTURE_MAX_PATH_CHARS) continue; const absolute = /^(?:\/|\\\\|[A-Za-z]:[\\/])/.test(expanded); for (const root of fromRoot && !absolute && config.root ? [base, config.root] : [base]) { if (!absolute && !root) continue; const readings = captureAllReadings(captureResolve(expanded, root, config.windowsHost), config.windowsHost); if (readings === false) return { match: undefined, spool }; if (!readings) continue; for (const candidate of readings) { const spooled = captureOutputSpool(candidate.path, budget); if (spooled === undefined) return { match: undefined, spool }; if (spooled) spool = true; const match = captureMatchCandidate(candidate, false, config, budget); if (match !== false) return { match, spool }; } } } if (driveRelative.some((word) => !placed.has(word))) spool = true; return { match: false, spool }; }
// The directory a shell tool's relative arguments resolve from; mirrors `command_base`.
function captureBase(config: CaptureConfig, workdir?: string): string { const spelled = workdir !== undefined && config.windowsHost && config.base ? captureWindowsSpelling(captureStripProvider(workdir), config.base) : undefined; if (spelled !== undefined) return spelled; return workdir === undefined ? config.base : /^(?:\/|\\\\|[A-Za-z]:[\\/])/.test(workdir) ? workdir : config.base && captureJoin(config.base, workdir); }
// A leading unquoted `~/` (PowerShell `~\`) is home and `~+` the working directory; mirrors `expand_tilde`.
function captureExpandTilde(argument: string, tilde: boolean, home: string, base: string): string { return !tilde ? argument : /^~[\/\\]/.test(argument) ? captureJoin(home, argument.slice(2)) : argument === "~+" ? base : argument.startsWith("~+/") && base ? captureJoin(base, argument.slice(3)) : argument; }
// A harness's saved copy of an earlier tool's output, read back like `BashOutput`: Claude Code `~/.claude/projects/<project>/<session>/tool-results/<id>.txt` and `<tmp>/claude-<uid>/<project>/<session>/tasks/<id>.output`, OpenCode `<data>/opencode/tool-output/tool_<id>`, Pi `<tmp>/pi-bash-<id>.log`. A glob segment fits a named one when the two share a match, and at least one fitting segment must spell literal text. Each segment is fitted to each shape once and the work is charged to `budget`; undefined past CAPTURE_MAX_WORK. Mirrors `output_spool`.
function captureOutputSpool(path: string, budget: { work: number }): boolean | undefined { const segments = path.split("/").filter(Boolean).map((segment) => [...segment.replace(/[A-Z]/g, (c) => c.toLowerCase())]); const file = segments.pop(); if (file === undefined) return false; const fits = (segment: string[], shape: string): boolean | undefined => { budget.work += (segment.length + 1) * ([...shape].length + 1); return budget.work > CAPTURE_MAX_WORK ? undefined : captureGlobsIntersect(segment, [...shape]); }; const spells = (segment: string[], word: string) => captureSpells(segment.join(""), word); const count = segments.length; let after: [boolean, boolean][] | undefined; let claudeResults = false; for (let index = 1; index < count; index++) { const claude = segments[index - 1]; const projects = segments[index]; const c = fits(claude, "*claude*"); if (c === undefined) return undefined; if (!c) continue; const p = fits(projects, "projects"); if (p === undefined) return undefined; if (!p) continue; if (!after) { after = new Array<[boolean, boolean]>(count); let seen: [boolean, boolean] = [false, false]; for (let at = count - 1; at >= 0; at--) { after[at] = seen; const results = fits(segments[at], "tool-results"); if (results === undefined) return undefined; if (results) seen = [true, seen[1] || spells(segments[at], "tool-results")]; } } const [fitting, spelled] = after[index]; if (spelled || (fitting && (spells(claude, "claude") || spells(projects, "projects")))) { claudeResults = true; break; } } let claudeTask = false; if (count > 0) { const parent = segments[count - 1]; const tasks = fits(parent, "tasks"); if (tasks === undefined) return undefined; if (tasks) { const output = fits(file, "*.output"); if (output === undefined) return undefined; if (output) { const named = spells(parent, "tasks") || spells(file, ".output"); for (const claude of segments) { const c = fits(claude, "claude*"); if (c === undefined) return undefined; if (c && (named || spells(claude, "claude"))) { claudeTask = true; break; } } } } } let opencode = false; if (count >= 2) { const data = fits(segments[count - 2], "opencode"); if (data === undefined) return undefined; if (data) { const output = fits(segments[count - 1], "tool-output"); if (output === undefined) return undefined; opencode = output && (spells(segments[count - 2], "opencode") || spells(segments[count - 1], "tool-output")); } } const pi = fits(file, "pi-bash-*.log"); if (pi === undefined) return undefined; return claudeResults || claudeTask || opencode || (pi && spells(file, "pi-bash-")); }
// Whether a one-segment glob spells part of `word` in literal text: a run between wildcards sharing three characters in a row with it; mirrors `spells`.
function captureSpells(segment: string, word: string): boolean { const needed = Math.min([...word].length, 3); return segment.split(/[*?]/).some((run) => { const chars = [...run]; for (let i = 0; i + needed <= chars.length; i++) if (word.includes(chars.slice(i, i + needed).join(""))) return true; return false; }); }
// Whether two one-segment globs over `*` and `?` match a common string; mirrors `globs_intersect`.
function captureGlobsIntersect(left: string[], right: string[]): boolean { const common = Array.from({ length: left.length + 1 }, () => new Array<boolean>(right.length + 1).fill(false)); common[left.length][right.length] = true; for (let i = left.length; i >= 0; i--) for (let j = right.length; j >= 0; j--) { if (i === left.length && j === right.length) continue; common[i][j] = left[i] === "*" ? common[i + 1][j] || (j < right.length && common[i][j + 1]) : right[j] === "*" ? common[i][j + 1] || (i < left.length && common[i + 1][j]) : i < left.length && j < right.length ? (left[i] === "?" || right[j] === "?" || left[i] === right[j]) && common[i + 1][j + 1] : false; } return common[0][0]; }
// Gemini CLI `read_many_files` globs expand case-insensitively with `**`
// recursing to any depth: a literal path matches like a file tool's, a glob
// drops when it could expand into an ignored path, and glob syntax the matcher
// cannot evaluate (classes, braces, extglobs, negation) fails closed.
function captureMatchGlobs(paths: string[], config: CaptureConfig): boolean | undefined { const budget = { work: 0 }; for (const path of paths) { if (/[\[\]{}()!]/.test(path)) return undefined; const readings = captureAllReadings(captureResolve(path, config.base, config.windowsHost), config.windowsHost); if (!readings) return undefined; for (const candidate of readings) { const match = captureMatchCandidate(candidate, true, config, budget); if (match !== false) return match; } } return false; }
function captureMatchCandidate(raw: { path: string; windows: boolean }, toolGlob: boolean, config: CaptureConfig, budget: { work: number }): boolean | undefined { const candidate = { path: captureFold(raw.path), windows: raw.windows }; const glob = /[*?]/.test(candidate.path); const directory = toolGlob && !glob; for (const pattern of config.patterns) { if (candidate.windows !== pattern.windows) continue; const insensitive = toolGlob || pattern.windows || captureFoldCase; const under = captureStartsWith(candidate.path, pattern.prefix, insensitive); if (!under && !glob && !directory) continue; if (under) { const directory = pattern.directory ? captureGlob(pattern.directory, candidate.path, insensitive, budget) : false; if (directory !== false) return directory; const match = captureGlob(pattern.path, candidate.path, insensitive, budget); if (match !== false) return match; } if (glob) { const reaches = captureGlobstarReaches(candidate.path, pattern.prefix, insensitive, budget); if (reaches !== false) return reaches; } if (directory) { const below = captureGlobBelow(pattern.path, candidate.path, insensitive, budget); if (below !== false) return below; } } return false; }
function captureGlobstarReaches(glob: string, prefix: string, insensitive: boolean, budget: { work: number }): boolean | undefined { const star = glob.indexOf("**"); if (star < 0) return captureGlobReaches(glob, prefix, insensitive, budget); const slash = glob.lastIndexOf("/", star); const head = slash < 0 ? "" : glob.slice(0, slash); const depth = head.split("/").filter(Boolean).length; let seen = 0; let offset = 0; for (const part of prefix.split("/")) { if (seen === depth) break; offset += part.length; if (part) seen++; if (seen < depth) offset++; } if (seen < depth) return captureGlobReaches(glob, prefix, insensitive, budget); return captureGlob(head, prefix.slice(0, offset), insensitive, budget); }
function captureTool(payload: Record<string, unknown>): { family: CaptureProtocol["tool_family"]; paths?: string[]; extraction: CaptureProtocol["extraction_state"]; callID?: string; command?: string | string[]; shell?: boolean; powershell?: boolean; inline?: boolean; workdir?: string; globs?: boolean; reader?: boolean; search?: { roots: string[]; patterns: string[] }; patch?: string[] } { const name = typeof payload.tool === "string" ? payload.tool.toLowerCase() : ""; const args = payload.args as Record<string, unknown> | undefined; const call = ["tool_use_id","toolUseId","tool_call_id","toolCallId","call_id","callId","callID"].map((k) => payload[k]).find((v): v is string => typeof v === "string" && /^[A-Za-z0-9_.-]{1,128}$/.test(v)); if (CAPTURE_SEARCH_TOOLS.includes(name)) return { family: "search-list", extraction: "not-applicable", callID: call, search: captureSearchScope(name, args) }; if (CAPTURE_NON_FILE_TOOLS.includes(name)) { const workdir = ["workdir","dir_path","directory"].map((k) => args?.[k]).find((v) => v !== undefined); return { family: "non-file", extraction: "extracted", callID: call, command: captureShellCommand(args), shell: name !== "web_search", powershell: name === "powershell", inline: CAPTURE_INLINE_CODE_TOOLS.includes(name), workdir: typeof workdir === "string" && workdir.trim() ? workdir : undefined }; } if (!CAPTURE_FILE_TOOLS.includes(name)) return { family: "unknown", extraction: "extracted", callID: call, reader: CAPTURE_OUTPUT_READER_TOOLS.includes(name) }; const direct = (o: any): string[] | undefined => { if (!o || typeof o !== "object") return undefined; const r: string[] = []; for (const k of ["file_path","filePath","path","absolute_path","AbsolutePath","notebook_path","TargetFile"]) if (k in o) { if (typeof o[k] !== "string") return undefined; r.push(o[k]); } if ("paths" in o) { if (!Array.isArray(o.paths) || o.paths.some((x: unknown) => typeof x !== "string")) return undefined; r.push(...o.paths); } return r.length && r.length <= CAPTURE_MAX_CANDIDATES ? r : undefined; }; let paths = direct(args); if (["multi_edit","multiedit","replace_all"].includes(name)) { const entries = args?.edits ?? args?.replacements; if (!Array.isArray(entries) || !entries.length || entries.length > CAPTURE_MAX_CANDIDATES) paths = undefined; else { paths = paths ?? []; for (const entry of entries) { const more = direct(entry); if (!more || paths.length + more.length > CAPTURE_MAX_CANDIDATES) { paths = undefined; break; } paths.push(...more); } } } if (name === "read_many_files" && args && "include" in args) { const include = args.include; if (!Array.isArray(include) || include.some((x: unknown) => typeof x !== "string") || (paths?.length ?? 0) + include.length > CAPTURE_MAX_CANDIDATES) paths = undefined; else paths = [...(paths ?? []), ...(include as string[])]; } if ((name === "read" || name === "fs_read") && args && "operations" in args) { const entries = args.operations; if (!Array.isArray(entries) || !entries.length || entries.length > CAPTURE_MAX_CANDIDATES) paths = undefined; else { paths = paths ?? []; for (const entry of entries) { const more = direct(entry); if (!more || paths.length + more.length > CAPTURE_MAX_CANDIDATES) { paths = undefined; break; } paths.push(...more); } } } if (!paths || !paths.length || paths.some((p) => !p.trim() || [...p].length > CAPTURE_MAX_PATH_CHARS)) return { family: "file", extraction: "missing-or-malformed", callID: call, patch: capturePatchPaths(name, payload.args) }; return { family: "file", paths, extraction: "extracted", callID: call, globs: name === "read_many_files" }; }
// Where a search/list tool reads, for the cross-checkout lookup: its directory and file arguments, a glob tool's `pattern`, and a content search's file filters (a grep `pattern` is a regular expression). Undefined when one is not a string; mirrors `search_scope`.
function captureSearchScope(name: string, args: unknown): { roots: string[]; patterns: string[] } | undefined { if (args === undefined || args === null) return { roots: [], patterns: [] }; if (typeof args !== "object" || Array.isArray(args)) return undefined; const o = args as Record<string, unknown>; const roots: string[] = []; const patterns: string[] = []; for (const k of ["file_path","filePath","path","absolute_path","AbsolutePath","notebook_path","TargetFile","dir_path","directory","DirectoryPath","SearchDirectory","SearchPath"]) if (k in o) { if (typeof o[k] !== "string") return undefined; roots.push(o[k] as string); } if ("paths" in o) { if (!Array.isArray(o.paths) || o.paths.some((x: unknown) => typeof x !== "string")) return undefined; roots.push(...(o.paths as string[])); } for (const k of [...(["glob","find","find_by_name"].includes(name) ? ["pattern","Pattern"] : []), "glob","include","file_pattern","file_glob","Includes"]) { if (!(k in o)) continue; const v = o[k]; if (typeof v === "string") patterns.push(v); else if (Array.isArray(v) && v.every((x) => typeof x === "string")) patterns.push(...(v as string[])); else return undefined; } return roots.length + patterns.length <= CAPTURE_MAX_CANDIDATES && [...roots, ...patterns].every((p) => p.trim() && [...p].length <= CAPTURE_MAX_PATH_CHARS) ? { roots, patterns } : undefined; }
// The files an `apply_patch` patch names in its Add/Update/Delete File and Move to headers, for the cross-checkout lookup only; mirrors `patch_paths`.
function capturePatchPaths(name: string, args: unknown): string[] | undefined { if (name !== "apply_patch" && name !== "patch") return undefined; const o = args as Record<string, unknown> | undefined; const text = typeof args === "string" ? args : o && typeof o === "object" ? ["command","patch","patchText","input"].map((k) => o[k]).find((v): v is string => typeof v === "string") : undefined; if (text === undefined) return undefined; const paths: string[] = []; for (const line of text.split(/\r?\n/)) { const trimmed = line.trimStart(); const header = ["*** Add File:","*** Update File:","*** Delete File:","*** Move to:"].find((h) => trimmed.startsWith(h)); if (!header) continue; const path = trimmed.slice(header.length).trim(); if (!path || [...path].length > CAPTURE_MAX_PATH_CHARS || paths.length === CAPTURE_MAX_CANDIDATES) return undefined; paths.push(path); } return paths.length ? paths : undefined; }
// How to follow a lookup location with glob syntax: null when it has none (a parameter expansion's braces are not syntax; the shell split also reads each default in its place), undefined when a brace or group spans a separator or an alternative of `..` or `~…` leaves the root (one a brace expansion builds included), else its literal root and one step per remaining segment ("*" any child directory, "?<glob>" a child the `*`/`?` glob names without regard to ASCII case, "**" any depth, ending the walk; anything else that child); mirrors `glob_walk`.
function captureGlobWalk(location: string): { root: string; steps: string[] } | null | undefined { if (location.includes("${") || !/[*?\[{(]/.test(location)) return null; const parsed = captureNormalize(location); if (!parsed) return null; const p = parsed.path; let root: string; let tail: string; if (p.startsWith("//")) { const parts = p.slice(2).split("/"); root = `//${parts[0]}/${parts[1]}`; tail = parts.slice(2).join("/"); } else if (/^[A-Za-z]:\//.test(p)) { root = p.slice(0, 3); tail = p.slice(3); } else { root = "/"; tail = p.slice(1); } const segments = tail.split("/").filter(Boolean); const syntax = (segment: string) => /[*?\[{(]/.test(segment); const first = segments.findIndex(syntax); if (first < 0) return null; const steps: string[] = []; for (let index = first; index < segments.length; index++) { const segment = segments[index]; if (!syntax(segment)) { steps.push(segment); continue; } const count = (c: string) => segment.split(c).length - 1; const leaves = (text: string) => text.split(/[{},()|@!+]/).some((piece) => piece === ".." || piece.startsWith("~")); const alternatives = segment.includes("{") ? captureBraceAlternatives(segment) : []; if (leaves(segment) || (alternatives === undefined ? captureBraceOverflowLeaves(segment) : alternatives.some((alternative) => /[\/\\]/.test(alternative) || leaves(alternative) || (index < segments.length - 1 && captureCouldNameParent(alternative)))) || count("{") !== count("}") || count("(") !== count(")")) return undefined; if (segment.includes("**")) { steps.push("**"); break; } steps.push(/[\[{(]/.test(segment) ? "*" : `?${segment}`); } for (const segment of segments.slice(0, first)) root += `${root.endsWith("/") ? "" : "/"}${segment}`; return { root: parsed.windows ? root.replace(/\//g, "\\") : root, steps }; }
// Every word a brace expansion makes of `text` (a comma list or an integer or single-character sequence, nested and repeated); any other `{…}` stays. Undefined past CAPTURE_MAX_BRACE_ALTERNATIVES; mirrors `brace_alternatives`.
function captureBraceAlternatives(text: string): string[] | undefined { if (text.split("{").length - 1 > CAPTURE_MAX_BRACE_ALTERNATIVES) return undefined; const words: string[] = []; const sequence = (body: string): string[] | null | undefined => { const parts = body.split(".."); if (parts.length < 2 || parts.length > 3) return null; let step = 1; if (parts.length === 3) { if (!/^[+-]?\d+$/.test(parts[2])) return null; step = Math.max(Math.abs(Number(parts[2])), 1); } const integer = (part: string) => /^[+-]?\d+$/.test(part); let from: number; let to: number; let numeric: boolean; if (integer(parts[0]) && integer(parts[1])) { from = Number(parts[0]); to = Number(parts[1]); numeric = true; } else { const a = [...parts[0]]; const b = [...parts[1]]; if (a.length !== 1 || b.length !== 1) return null; from = a[0].codePointAt(0)!; to = b[0].codePointAt(0)!; numeric = false; } const count = Math.floor(Math.abs(to - from) / step) + 1; if (count > CAPTURE_MAX_BRACE_ALTERNATIVES) return undefined; const out: string[] = []; for (let i = 0, value = from; i < count; i++, value += from <= to ? step : -step) out.push(numeric ? String(value) : String.fromCodePoint(value)); return out; }; const expand = (chars: string[]): boolean => { for (let open = 0; open < chars.length; open++) { if (chars[open] !== "{") continue; let depth = 0; let close = -1; const commas: number[] = []; for (let i = open; i < chars.length; i++) { if (chars[i] === "{") depth++; else if (chars[i] === "}") { depth--; if (depth === 0) { close = i; break; } } else if (chars[i] === "," && depth === 1) commas.push(i); } if (close < 0) continue; let pieces: string[][]; if (!commas.length) { const words = sequence(chars.slice(open + 1, close).join("")); if (words === undefined) return false; if (words === null) continue; pieces = words.map((word) => [...word]); } else { pieces = []; let start = open + 1; for (const end of [...commas, close]) { pieces.push(chars.slice(start, end)); start = end + 1; } } for (const piece of pieces) if (!expand([...chars.slice(0, open), ...piece, ...chars.slice(close + 1)])) return false; return true; } if (words.length === CAPTURE_MAX_BRACE_ALTERNATIVES) return false; words.push(chars.join("")); return true; }; return expand([...text]) ? words : undefined; }
// The bounds of a brace sequence body (`1..3`, `a..e`, `x..y..n`): first and last value and whether they are integers (else code points); undefined when it is not one. Mirrors `brace_sequence_bounds`.
function captureBraceSequenceBounds(body: string): [number, number, boolean] | undefined { const parts = body.split(".."); if (parts.length < 2 || parts.length > 3) return undefined; if (parts.length === 3 && !/^[+-]?\d+$/.test(parts[2])) return undefined; const integer = (part: string) => /^[+-]?\d+$/.test(part); if (integer(parts[0]) && integer(parts[1])) return [Number(parts[0]), Number(parts[1]), true]; const a = [...parts[0]]; const b = [...parts[1]]; if (a.length !== 1 || b.length !== 1) return undefined; return [a[0].codePointAt(0)!, b[0].codePointAt(0)!, false]; }
// Whether a segment whose brace expansion is too large to list could still build a word that leaves the root (a separator, `~`, a group, `..` or a dot-glob an older bash expands to it): integer and letter sequences cannot, nor a `.` where every word also holds another literal character; a character sequence over a range with `.`, a separator, `~` or a group character can. Mirrors `brace_overflow_could_leave`.
function captureBraceOverflowLeaves(segment: string): boolean { if (/[\/\\~()|]/.test(segment)) return true; const chars = [...segment]; const close: (number | undefined)[] = []; const comma: boolean[] = []; const open: number[] = []; chars.forEach((c, index) => { if (c === "{") open.push(index); else if (c === "}") { const start = open.pop(); if (start !== undefined) close[start] = index; } else if (c === "," && open.length) comma[open[open.length - 1]] = true; }); const glob = ".*?[]"; let dot = false; let anchor = false; const ends: number[] = []; for (let index = 0; index < chars.length;) { const c = chars[index]; if (ends.length && ends[ends.length - 1] === index) { ends.pop(); index++; continue; } const end = c === "{" ? close[index] : undefined; if (end !== undefined) { if (comma[index]) { ends.push(end); index++; continue; } const bounds = captureBraceSequenceBounds(chars.slice(index + 1, end).join("")); if (bounds) { const [from, to, numeric] = bounds; const holds = (set: string) => [...set].some((x) => { const code = x.codePointAt(0)!; return Math.min(from, to) <= code && code <= Math.max(from, to); }); if (!numeric && holds("./\\~()|")) return true; if (!ends.length && (numeric || !holds(glob))) anchor = true; index = end + 1; continue; } } if (c === "," && ends.length) { index++; continue; } if (c === ".") dot = true; if (!ends.length && !glob.includes(c)) anchor = true; index++; } return dot && !anchor; }
// The marker files of every directory `steps` reach below `root` (root excluded), never following a symlink; nothing when `root` lies inside `checkout`, and a "**" lists nothing inside it. Each directory is read one entry at a time, so a huge one costs no more than the entry bound. Undefined past the directory or entry bounds (a "**" counts directories as it lists them) or the time budget; mirrors `GlobBudget::markers_below`.
type CaptureGlobBudget = { dirs: number; entries: number; deadline: number; now: () => number; probes?: number };
function captureMarkersBelow(root: string, steps: string[], checkout: string | undefined, budget: CaptureGlobBudget): string[] | undefined { const inside = (dir: string) => checkout !== undefined && dir.startsWith(checkout) && (dir.length === checkout.length || /[\\/]/.test(dir[checkout.length]) || /[\\/]$/.test(checkout)); if (inside(root)) return []; const children = (dir: string): string[] | undefined => { if (budget.now() > budget.deadline) return undefined; let handle: { readSync(): { name: string; isDirectory(): boolean } | null; closeSync(): void }; try { handle = opendirSync(dir); } catch (_e) { return []; } const out: string[] = []; try { for (let entry = handle.readSync(); entry; entry = handle.readSync()) { if (++budget.entries > CAPTURE_MAX_GLOB_ENTRIES) return undefined; if (entry.isDirectory()) out.push(join(dir, entry.name)); } } catch (_e) { /* the rest of an unreadable directory lists nothing */ } finally { handle.closeSync(); } return out; }; const found: string[] = []; let current = [root]; for (const step of steps) { const next: string[] = []; for (const dir of current) { if (step === "*" || step.startsWith("?")) { const more = children(dir); if (!more) return undefined; next.push(...(step === "*" ? more : more.filter((child) => captureGlob(step.slice(1), basename(child), true, { work: 0 }) !== false))); } else if (step === "**") { if (inside(dir)) continue; const listed = children(dir); if (!listed) return undefined; const pending = listed.filter((child) => !inside(child)); while (pending.length) { const below = pending.pop()!; if (++budget.dirs > CAPTURE_MAX_GLOB_DIRECTORIES) return undefined; const more = children(below); if (!more) return undefined; pending.push(...more.filter((child) => !inside(child))); next.push(below); } } else { const child = join(dir, step); try { if (statSync(child).isDirectory()) next.push(child); } catch (_e) { /* the glob matches nothing there */ } } } for (const dir of next) { if (step !== "**" && ++budget.dirs > CAPTURE_MAX_GLOB_DIRECTORIES) return undefined; const marker = join(dir, ".ai-memory.toml"); try { if (statSync(marker).isFile()) found.push(marker); } catch (_e) { /* no marker here */ } } if (!next.length) break; current = next; } return found; }
// Whether a checkout root counts as the event's own for the no-walk rule and the lookup order: not the filesystem root, and not at or above the home directory (a dotfiles repository); mirrors `own_checkout`.
function captureOwnCheckout(root: string): boolean { const home = homedir(); const above = !!home && (resolve(home) === root || resolve(home).startsWith(join(root, "x").slice(0, -1))); return dirname(root) !== root && !above; }
// A missing path answers without an exception, which a deep tree's many misses would pay for.
function captureExists(path: string): boolean { try { return statSync(path, { throwIfNoEntry: false }) !== undefined; } catch (_e) { return false; } }
// `name` inside the resolved directory `dir`, without `join`'s normalizing pass over a long path.
const captureSep = join("a", "b").charAt(1);
function captureChild(dir: string, name: string): string { return dir.endsWith(captureSep) ? dir + name : dir + captureSep + name; }
// Counts one filesystem probe of the marker lookup against the walk's budget; false past CAPTURE_MAX_LOOKUP_PROBES or the deadline. Mirrors `GlobBudget::probe`.
function captureProbe(budget: CaptureGlobBudget): boolean { budget.probes = (budget.probes ?? 0) + 1; return budget.probes <= CAPTURE_MAX_LOOKUP_PROBES && budget.now() <= budget.deadline; }
// `findMarker` for the many directories one call names: every directory a lookup passes keeps its answer and its checkout root, so a deep tree costs one probe per directory; each probe is counted. Null when there is none, undefined past the budget. Mirrors `MarkerMemo`.
type CaptureMarkerMemo = { governing: Map<string, string | null>; checkouts: Map<string, string | null>; known?: Set<string> };
// Records that `dir` exists, and with it every ancestor; mirrors `MarkerMemo::note_directory`.
function captureMemoNote(dir: string, memo: CaptureMarkerMemo): void { memo.known ??= new Set(); for (let current = resolve(dir); !memo.known.has(current); current = dirname(current)) { memo.known.add(current); if (dirname(current) === current) break; } }
function captureMemoCheckout(dir: string, memo: CaptureMarkerMemo, budget: CaptureGlobBudget): string | null | undefined { const chain: string[] = []; let current = dir; let found: string | null; for (;;) { const known = memo.checkouts.get(current); if (known !== undefined) { found = known; break; } if (dirname(current) === current) { found = null; break; } if (!captureProbe(budget)) return undefined; chain.push(current); if (captureExists(captureChild(current, ".git"))) { found = current; break; } current = dirname(current); } for (const d of chain) memo.checkouts.set(d, found); return found; }
// Records the marker an earlier `findMarker` found from `dir` for every directory that walk passed: up to the marker's directory, or to its boundary when it found none (home inside it, else the checkout root, else `dir` alone). Mirrors `MarkerMemo::remember`.
function captureMemoRemember(dir: string, marker: string | undefined, memo: CaptureMarkerMemo): void { const home = homedir(); const start = resolve(dir); const within = (d: string, stop: string) => d === stop || d.startsWith(join(stop, "x").slice(0, -1)); let stop: string | null; if (marker) stop = dirname(marker); else if (!home) stop = null; else if (within(start, home)) stop = home; else stop = memo.checkouts.get(start) ?? start; if (stop !== null && !within(start, stop)) stop = start; for (let current = start; ; current = dirname(current)) { memo.governing.set(current, marker ?? null); if (current === stop || dirname(current) === current) break; } }
function captureMemoMarker(start: string, memo: CaptureMarkerMemo, budget: CaptureGlobBudget): string | null | undefined { const home = homedir(); const homePrefix = home ? join(home, "x").slice(0, -1) : ""; const chain: string[] = []; let current = resolve(start); let found: string | null; for (;;) { const known = memo.governing.get(current); if (known !== undefined) { found = known; break; } if (dirname(current) === current) { found = null; break; } if (!captureProbe(budget)) return undefined; chain.push(current); const marker = captureChild(current, ".ai-memory.toml"); if (captureExists(marker)) { found = marker; break; } let boundary = false; if (home && (current === home || current.startsWith(homePrefix))) boundary = current === home; else if (home) { if (memo.checkouts.get(dirname(current)) === null) boundary = true; else { const root = captureMemoCheckout(current, memo, budget); if (root === undefined) return undefined; boundary = root === null || root === current; } } if (boundary) { found = null; break; } current = dirname(current); } for (const d of chain) memo.governing.set(d, found); return found; }
// The nearest existing directory at or above `path`, probing only what is not known already: null when there is none, undefined past the budget; mirrors `MarkerMemo::existing_dir`.
function captureExistingDirWithin(path: string, memo: CaptureMarkerMemo, budget: CaptureGlobBudget): string | null | undefined { let probe = path; for (;;) { if (memo.known?.has(probe)) return probe; if (!captureProbe(budget)) return undefined; try { if (statSync(probe, { throwIfNoEntry: false })?.isDirectory()) { captureMemoNote(probe, memo); return probe; } } catch (_e) { /* not there yet */ } const parent = dirname(probe); if (parent === probe || parent === ".") return null; probe = parent; } }
// An external lifecycle owner (`AI_MEMORY_CAPTURE_OWNER`, any value that is
// non-empty after trimming) takes over capture for this process: the gate runs
// before the capture-policy marker scan, so no disposition work, no marker read
// by this policy, and no queue write, spool write, or capture POST downstream.
// It does not reach the consumer's own routing work (`applyMarkerParams` runs
// earlier in postHook), and context delivery (`fetchHandoff`) is a separate
// path that stays live.
function captureOwnedExternally(): boolean { const owner = typeof process === "undefined" ? undefined : process.env?.AI_MEMORY_CAPTURE_OWNER; return typeof owner === "string" && owner.trim() !== ""; }
// Server profiles (`server = "<name>"`, #992) are routed only by the native
// hook. Any marker up the tree that selects one makes this integration emit
// nothing rather than deliver that repository's capture to the install default.
// Mirrors the native walk: no payload cwd falls back to the process cwd; inside
// home the walk stops at home, outside it continues past the checkout root to an
// organisation-level marker; an unreadable marker counts as a selection.
function captureServerRouted(cwd: string | undefined): boolean { let dir = resolve(cwd ?? process.cwd()); const home = homedir(); let boundary: string | undefined; for (let probe = dir; ; probe = dirname(probe)) { if (probe === home) { boundary = home; break; } if (probe === dirname(probe)) break; } for (;;) { try { if (/^\s*server\s*=/m.test(readFileSync(join(dir, ".ai-memory.toml"), "utf8"))) return true; } catch (e) { if (!["ENOENT", "ENOTDIR", "EISDIR"].includes((e as { code?: string })?.code ?? "")) return true; } if (dir === boundary || dir === dirname(dir)) return false; dir = dirname(dir); } }
type CaptureTool = ReturnType<typeof captureTool>;
// One policy's disposition for one call, shared by the event's own marker and every other marker it reaches.
function captureDecide(tool: CaptureTool, config: CaptureConfig): CaptureDisposition { if (config.state === "invalid" && (tool.family === "file" || tool.family === "search-list" || tool.shell || tool.reader)) return "metadata-only"; if (config.state !== "active") return "keep"; if (tool.reader) return "metadata-only"; if (tool.family === "search-list") return "drop"; if (tool.family === "file") { if (!tool.paths) return "metadata-only"; const parsed = tool.paths.map((p) => { const resolved = captureResolve(p, config.base, config.windowsHost); const normalized = resolved?.map((path) => captureNormalize(path, config.windowsHost)); return normalized?.every(Boolean) ? normalized as { path: string; windows: boolean }[] : undefined; }); if (parsed.some((p) => !p)) return "metadata-only"; const candidates = (parsed as { path: string; windows: boolean }[][]).flat(); const spoolBudget = { work: 0 }; if (tool.globs) { const match = captureMatchGlobs(tool.paths, config); if (match !== false) return match ? "drop" : "metadata-only"; for (const p of tool.paths) { const readings = captureAllReadings(captureResolve(p, config.base, config.windowsHost), config.windowsHost); if (!readings) continue; for (const candidate of readings) if (captureOutputSpool(candidate.path, spoolBudget) !== false) return "metadata-only"; } return "keep"; } const budget = { work: 0 }; for (const raw of candidates) { const candidate = { path: captureFold(raw.path), windows: raw.windows }; for (const pattern of config.patterns) { if (candidate.windows !== pattern.windows) continue; if (pattern.directory && captureGlob(pattern.directory, candidate.path, pattern.windows || captureFoldCase, budget)) return "drop"; const match = captureGlob(pattern.path, candidate.path, pattern.windows || captureFoldCase, budget); if (match === undefined) return "metadata-only"; if (match) return "drop"; } } for (const candidate of candidates) if (captureOutputSpool(candidate.path, spoolBudget) !== false) return "metadata-only"; return "keep"; } if (tool.family === "non-file" && (tool.inline || (tool.command !== undefined && captureCommandOversized(tool.command)))) return "metadata-only"; if (tool.family === "non-file" && tool.command !== undefined) { const split = captureShellSplit(tool.command, tool.powershell || config.windowsHost); const matched = captureMatchCommand(split.words, config, tool.workdir); if (matched.match !== false) return "drop"; if (split.opaque || matched.spool || (config.windowsHost && tool.workdir !== undefined && captureWindowsUnplaceable(tool.workdir, config.base))) return "metadata-only"; } return "keep"; }
const captureRank: Record<CaptureDisposition, number> = { keep: 0, "metadata-only": 1, drop: 2 };
// Every other marker governing a path the call names (a file-tool target or the files an `apply_patch` names, a search/list directory, file or path-like glob argument, or a shell argument with a separator resolved from its workdir, plus that directory) inspects it too, so a checkout's `ignore_paths` hold for a call made from elsewhere. Lexical; each directory is looked up once and remembered for every lookup that passes it; a glob location is looked up from its literal root (before the walk, whose one listing may spend the time budget) and, when that root lies outside the event's own checkout, every directory it could pass through is checked for a marker of its own; a glob whose `..` follows a `**` is looked up in every reading. Locations outside the own checkout come first: there unreadable targets, glob syntax the walk cannot follow, and anything past the bounds are unprovable, while running out of budget inside the own checkout ends the lookup. Mirrors `capture_lookup_locations` and `foreign_capture_decision`.
function captureForeignConfigs(tool: CaptureTool, config: CaptureConfig, cwd: string | undefined, own: string | undefined): CaptureConfig[] { const invalid: CaptureConfig[] = [{ state: "invalid", patterns: [], base: config.base, windowsHost: config.windowsHost }]; const locations = new Set<string>(); let overflow = false; const add = (path: string, base: string): string[] | undefined => { const absolute = /^(?:\/|\\\\|[A-Za-z]:[\\/])/.test(path); if (!absolute && !base) return undefined; const readings = captureAllReadings(captureResolve(path, base, config.windowsHost), config.windowsHost); if (readings === false) { overflow = true; return undefined; } if (!readings) return undefined; return readings.map((candidate) => { const location = candidate.windows ? candidate.path.replace(/\//g, "\\") : candidate.path; locations.add(location); return location; }); }; if (tool.family === "file") { const paths = tool.paths ?? tool.patch; if (!paths) return invalid; for (const path of paths) if (add(path, config.base) === undefined) return invalid; } else if (tool.family === "search-list") { if (!tool.search) return invalid; const roots: string[] = []; for (const root of tool.search.roots) { const found = add(root, config.base); if (found === undefined) return invalid; roots.push(...found); } if (!roots.length) roots.push(config.base); for (const pattern of tool.search.patterns.filter((p) => /[\/\\]/.test(p))) for (const root of roots) if (add(pattern, root) === undefined) return invalid; } else if (tool.family === "non-file" && tool.command !== undefined) { if (captureCommandOversized(tool.command)) return invalid; const base = captureBase(config, tool.workdir); if (base) add(base, base); const home = homedir(); const split = captureShellSplit(tool.command, tool.powershell || config.windowsHost); for (const [word, tilde] of [...split.words, ...split.expanded]) for (const [argument] of captureShellArguments(word)) { const expanded = captureExpandTilde(argument, tilde, home, base); if (/[\/\\]/.test(expanded)) add(expanded, base); } } if (overflow || locations.size > CAPTURE_MAX_LOOKUP_LOCATIONS) return invalid; const markers: string[] = []; const memo: CaptureMarkerMemo = { governing: new Map(), checkouts: new Map() }; let checkout: string | undefined; if (cwd) { const root = captureMemoCheckout(resolve(cwd), memo, { dirs: 0, entries: 0, deadline: Infinity, now: Date.now, probes: -Infinity }); checkout = root && captureOwnCheckout(root) ? root : undefined; captureMemoRemember(cwd, own, memo); if (statSync(resolve(cwd), { throwIfNoEntry: false })?.isDirectory()) captureMemoNote(cwd, memo); } const budget: CaptureGlobBudget = { dirs: 0, entries: 0, deadline: captureNow() + CAPTURE_MAX_GLOB_MS, now: captureNow, probes: 0 }; const remember = (marker: string | undefined): boolean => { if (!marker || marker === own || markers.includes(marker)) return true; if (markers.length === CAPTURE_MAX_FOREIGN_MARKERS) return false; markers.push(marker); return true; }; const insideOwn = (location: string) => checkout !== undefined && (location === checkout || location.startsWith(join(checkout, "x").slice(0, -1))); const ordered = [...locations].filter((location) => !insideOwn(location)); const insideFrom = ordered.length; ordered.push(...[...locations].filter(insideOwn).sort((a, b) => a.split(/[\\/]/).length - b.split(/[\\/]/).length)); for (let index = 0; index < ordered.length; index++) { const location = ordered[index]; const walk = captureGlobWalk(location); if (walk === undefined) return invalid; const lookup = walk ? walk.root : location; const dir = captureExistingDirWithin(lookup, memo, budget); if (dir === undefined) { if (index >= insideFrom) break; return invalid; } const marker = dir === null ? null : captureMemoMarker(dir, memo, budget); if (marker === undefined) { if (index >= insideFrom) break; return invalid; } if (!remember(marker ?? undefined)) return invalid; if (walk) { const found = captureMarkersBelow(walk.root, walk.steps, checkout, budget); if (!found) return invalid; for (const marker of found) if (!remember(marker)) return invalid; } } return markers.map((marker) => captureConfigAt(marker, cwd)); }
function capturePolicy(payload: Record<string, unknown>, cwd: string | undefined): { disposition: CaptureDisposition; protocol?: CaptureProtocol; payload: Record<string, unknown> } { if (captureOwnedExternally()) return { disposition: "drop", payload }; if (captureServerRouted(cwd)) return { disposition: "drop", payload }; const markerPresent = !!findMarker(cwd); if (CAPTURE_MODE === "allowlist" && !markerPresent) return { disposition: "drop", payload }; const own = findMarker(cwd); let config = captureConfigAt(own, cwd); const tool = captureTool(payload); let disposition = captureDecide(tool, config); if (disposition !== "drop" && tool.family !== "unknown") for (const other of captureForeignConfigs(tool, config, cwd, own)) { const decided = captureDecide(tool, other); if (captureRank[decided] > captureRank[disposition]) { disposition = decided; config = other; } } if (config.state === "inactive") return { disposition, payload }; const extraction = config.state === "active" && tool.family === "search-list" ? "not-applicable" : tool.extraction === "not-applicable" ? "extracted" : tool.extraction; const protocol: CaptureProtocol = { version: CAPTURE_POLICY_V1, disposition, policy_state: config.state, tool_family: tool.family, path_count: tool.paths?.length ?? 0, extraction_state: extraction }; if (disposition === "metadata-only") { const session = payload.sessionID ?? payload.sessionId ?? payload.session_id; const routing = typeof payload.cwd === "string" ? payload.cwd : cwd; return { disposition, protocol, payload: { ...(typeof session === "string" ? { session_id: session } : {}), ...(typeof routing === "string" ? { cwd: routing } : {}), tool_family: tool.family, tool_name: tool.family, ...(tool.callID ? { tool_call_id: tool.callID } : {}), _ai_memory_capture: protocol } }; } if (disposition === "keep") return { disposition, protocol, payload: { ...payload, _ai_memory_capture: protocol } }; return { disposition, protocol, payload }; }
"##;
    let names = |names: &[&str]| {
        let quoted: Vec<String> = names.iter().map(|name| ts_string_literal(name)).collect();
        format!("[{}]", quoted.join(", "))
    };
    TEMPLATE
        .replace("__AI_MEMORY_CAPTURE_MODE__", capture_mode)
        .replace(
            "__AI_MEMORY_FILE_TOOLS__",
            &names(capture_policy::FILE_TOOL_NAMES),
        )
        .replace(
            "__AI_MEMORY_SEARCH_TOOLS__",
            &names(capture_policy::SEARCH_LIST_TOOL_NAMES),
        )
        .replace(
            "__AI_MEMORY_NON_FILE_TOOLS__",
            &names(capture_policy::NON_FILE_TOOL_NAMES),
        )
        .replace(
            "__AI_MEMORY_INLINE_CODE_TOOLS__",
            &names(capture_policy::INLINE_CODE_TOOL_NAMES),
        )
        .replace(
            "__AI_MEMORY_OUTPUT_READER_TOOLS__",
            &names(capture_policy::OUTPUT_READER_TOOL_NAMES),
        )
        .replace(
            "__AI_MEMORY_MAX_COMMAND_CHARS__",
            &capture_policy::MAX_SHELL_COMMAND_CHARS.to_string(),
        )
}

/// Build the Claude Code `settings.json` fragment that wires the
/// lifecycle hooks (`CLAUDE_CODE_EVENTS`). Used by both:
/// - `install-hooks --agent claude-code` (script paths are
///   wherever the user told us via `--hooks-dir`)
/// - `setup-agent --agent claude-code` (script paths are where
///   `--host-prefix` says they'll live on the host)
///
/// `emit_root` is the directory that will contain hook scripts; it is
/// expected to be an absolute path on the system that will run the
/// agent CLI. This function does NOT verify the path exists on the
/// local filesystem — that decision belongs to the caller because
/// the docker case legitimately renders host paths that don't yet
/// exist in the container.
///
/// `auth_token`, when set, lands in each hook's `env` block as
/// `AI_MEMORY_AUTH_TOKEN`, which the shell scripts forward as
/// `Authorization: Bearer …` to the server.
#[must_use]
pub(crate) fn build_claude_code_payload(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    with_claude_session_end_timeout(build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_script_runner(),
            "claude-code",
            None,
            None,
        ),
    ))
}

pub(crate) fn build_claude_code_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
    capture_assistant: bool,
) -> serde_json::Value {
    build_claude_code_payload_with_data_dir_for_platform(
        emit_root,
        server_url,
        auth_token,
        data_dir,
        project_strategy,
        capture_assistant,
        HookCommandPlatform::for_bash_runner(),
    )
}

#[cfg(test)]
pub(crate) fn build_claude_code_script_payload_for_test(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
    capture_assistant: bool,
) -> serde_json::Value {
    build_claude_code_payload_with_data_dir_for_platform(
        emit_root,
        server_url,
        auth_token,
        data_dir,
        project_strategy,
        capture_assistant,
        HookCommandPlatform::Posix,
    )
}

fn build_claude_code_payload_with_data_dir_for_platform(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
    capture_assistant: bool,
    platform: HookCommandPlatform,
) -> serde_json::Value {
    with_claude_session_end_timeout(build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(platform, "claude-code", data_dir, project_strategy)
            .allow_claude_windows_exec()
            .with_capture_assistant(capture_assistant),
    ))
}

fn with_claude_session_end_timeout(mut payload: Value) -> Value {
    if let Some(handler) = payload
        .pointer_mut("/hooks/SessionEnd/0/hooks/0")
        .and_then(Value::as_object_mut)
    {
        handler.insert(
            "timeout".into(),
            Value::from(CLAUDE_SESSION_END_TIMEOUT_SECS),
        );
    }
    payload
}

/// Grok Build CLI hook payload for docker/setup-agent script snippets.
/// Grok shares Claude Code's JSON shape and event vocabulary, but uses
/// its own script bundle so script fallback keeps `agent=grok` and never
/// destructively fetches handoffs on SessionStart.
#[must_use]
pub(crate) fn build_grok_payload(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_script_runner(),
            "grok",
            None,
            None,
        ),
    )
}

/// Zero's hook events → ai-memory event names (issue #156). Zero has no
/// user-prompt or pre-compact equivalents; its `specialistStart`/`Stop`
/// map onto the subagent events the router already tracks for Claude Code.
pub(crate) const ZERO_EVENTS: [(&str, &str); 6] = [
    ("sessionStart", "session-start"),
    ("sessionEnd", "session-end"),
    ("beforeTool", "pre-tool-use"),
    ("afterTool", "post-tool-use"),
    ("specialistStart", "subagent-start"),
    ("specialistStop", "subagent-stop"),
];

/// Zero hooks.json config (issue #156): `{"enabled": true, "hooks": [..]}`
/// with one entry per lifecycle event. Zero executes `command` + `args`
/// directly (exec form, JSON payload on the hook's stdin) — no shell is
/// spawned, and our native `ai-memory hook` command reads exactly that
/// stdin shape, so Zero gets the local spool + OIDC fallback with zero
/// glue scripts. Entry ids carry the `ai-memory-` prefix so apply/uninstall
/// can merge around third-party hooks in the same file.
pub(crate) fn build_zero_hooks_config(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    let exe = hook_embedded_exe_path().to_string_lossy().into_owned();
    let hooks: Vec<serde_json::Value> = ZERO_EVENTS
        .iter()
        .map(|(zero_event, our_event)| {
            let mut args: Vec<String> = Vec::new();
            if let Some(dir) = data_dir {
                args.push("--data-dir".into());
                args.push(dir.to_string_lossy().into_owned());
            }
            args.extend(
                [
                    "hook",
                    "--event",
                    our_event,
                    "--agent",
                    "zero",
                    "--server-url",
                    server_url,
                ]
                .map(String::from),
            );
            if let Some(token) = auth_token {
                args.push("--auth-token".into());
                args.push(token.to_string());
            }
            if let Some(strategy) = project_strategy {
                args.push("--project-strategy".into());
                args.push(strategy.to_string());
            }
            serde_json::json!({
                "id": format!("ai-memory-{our_event}"),
                "name": format!("ai-memory {our_event}"),
                "event": zero_event,
                "command": exe,
                "args": args,
                "enabled": true,
            })
        })
        .collect();
    serde_json::json!({ "enabled": true, "hooks": hooks })
}

/// ZCode (z.ai) lifecycle events ai-memory hooks (#512). Each pair is
/// `(event-name-in-config.json, native `hook --event` value)`.
///
/// ZCode's hook vocabulary is exactly these six triggers (verified live
/// against the embedded engine v0.16.5): no `SessionEnd` — `Stop` fires at
/// the end of every turn, so `ai-memory finalize-session --agent zcode` is
/// the close path. `PermissionRequest` is deliberately not installed: its
/// hook chain races the interactive permission client and the loser is
/// aborted (`racePermissionResponders`), so a fast client decision silently
/// kills the hook — passive capture of that event is unreliable by design.
/// `PostToolUseFailure` fires *instead of* `PostToolUse` when the tool
/// throws; it forwards onto the same `post-tool-use` channel and the
/// payload's `error` field marks the outcome.
pub(crate) const ZCODE_EVENTS: [(&str, &str); 6] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PreToolUse", "pre-tool-use"),
    ("PostToolUse", "post-tool-use"),
    ("PostToolUseFailure", "post-tool-use"),
    ("Stop", "stop"),
];

/// ZCode hooks config (#512): the root `hooks` block of
/// `~/.zcode/cli/config.json` —
/// `{"enabled": true, "maxOutputBytes": …, "events": {Event: [{hooks: […]}]}}`.
///
/// Entries are exec-form `{"type":"process","command":…,"args":[…]}`: ZCode
/// spawns them without a shell and pipes the event JSON to stdin, which the
/// native `ai-memory hook` command reads directly — same wiring as Zero, so
/// ZCode gets the local spool + OIDC fallback with zero glue scripts. Every
/// key is from ZCode's documented hook schema (`type`, `command`, `args`,
/// `enabled`, `timeoutMs`, `statusMessage`); ZCode drops entries with
/// undocumented keys, so none are emitted. `statusMessage` doubles as the
/// ownership marker apply/uninstall merge around. `timeoutMs` bounds each
/// invocation (the capture POST is fire-and-forget; session-start also
/// fetches a handoff), and `maxOutputBytes` is raised above the 32 KiB
/// default so a fetched handoff is not truncated mid-JSON before ZCode
/// injects it as session context.
pub(crate) fn build_zcode_hooks_config(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    let exe = hook_embedded_exe_path().to_string_lossy().into_owned();
    let mut events = serde_json::Map::new();
    for (zcode_event, our_event) in ZCODE_EVENTS {
        let mut args: Vec<String> = Vec::new();
        if let Some(dir) = data_dir {
            args.push("--data-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        args.extend(
            [
                "hook",
                "--event",
                our_event,
                "--agent",
                "zcode",
                "--server-url",
                server_url,
            ]
            .map(String::from),
        );
        if let Some(token) = auth_token {
            args.push("--auth-token".into());
            args.push(token.to_string());
        }
        if let Some(strategy) = project_strategy {
            args.push("--project-strategy".into());
            args.push(strategy.to_string());
        }
        let entry = serde_json::json!({
            "type": "process",
            "command": exe,
            "args": args,
            "enabled": true,
            "timeoutMs": ZCODE_HOOK_TIMEOUT_MS,
            "statusMessage": "ai-memory capture",
        });
        events.insert(
            (*zcode_event).to_string(),
            serde_json::json!([{ "hooks": [entry] }]),
        );
    }
    serde_json::json!({
        "enabled": true,
        "maxOutputBytes": ZCODE_HOOK_MAX_OUTPUT_BYTES,
        "events": events,
    })
}

/// Per-hook wall-clock bound for ZCode invocations of the native command.
/// Generous enough for the synchronous session-start handoff fetch on a
/// slow machine; capture POSTs themselves are fire-and-forget.
pub(crate) const ZCODE_HOOK_TIMEOUT_MS: u64 = 10_000;

/// Session-start stdout ceiling for ZCode (`maxOutputBytes`). ZCode
/// truncates hook stdout beyond this before injecting it; 64 KiB
/// comfortably covers a handoff plus a `[briefing]`-budgeted brief
/// (same reasoning as Kiro v2's `max_output_size`).
pub(crate) const ZCODE_HOOK_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Hermes Agent lifecycle events ai-memory hooks. Each pair is
/// `(event-name-in-~/.hermes/config.yaml, native `hook --event` value)`.
///
/// Verified against Hermes v0.21.4 (`agent/shell_hooks.py`): a configured
/// `command` is split into argv by `shlex.split` and executed **without a
/// shell**, with the event JSON on stdin — so the generated block invokes the
/// native `hook` subcommand, the same shape Zero and ZCode use, and no
/// `.sh`/`.ps1` bundle is staged. Only the two tool events are wired: their
/// payload carries `tool_name` / `tool_input`, the envelope the router already
/// maps for `agent=hermes`. Session lifecycle stays with the memory-provider
/// plugin (`on_session_end`), so a hook-driven `session-end` cannot
/// double-close a Hermes session.
pub(crate) const HERMES_EVENTS: [(&str, &str); 2] = [
    ("pre_tool_call", "pre-tool-use"),
    ("post_tool_call", "post-tool-use"),
];

/// Wall-clock bound written into each Hermes hook entry. Capture POSTs are
/// fire-and-forget, so this only bounds a hung `hook` invocation.
pub(crate) const HERMES_HOOK_TIMEOUT_SECONDS: u64 = 20;

/// The ready-to-paste `hooks:` block for `~/.hermes/config.yaml`.
///
/// `command:` must be a bare argv line — Hermes splits it itself, so the
/// `KEY=value script` prefix every shell-run harness gets would be parsed as
/// extra argv entries. The native platform already emits the argv form
/// (`<exe> [--data-dir …] hook --event … --agent hermes --server-url …`), and
/// the YAML single-quote keeps any POSIX quoting inside it intact. ai-memory
/// deliberately does not write the file: it is YAML the installer would have
/// to splice, and Hermes gates user hooks behind its own acceptance prompt.
#[must_use]
pub(crate) fn build_hermes_hooks_yaml(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> String {
    build_hermes_hooks_yaml_for_platform(
        server_url,
        auth_token,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_runner(),
            "hermes",
            data_dir,
            project_strategy,
        ),
    )
}

/// Platform-forced variant, so a test can pin POSIX/Windows instead of
/// asserting whatever the machine running the suite happens to be.
fn build_hermes_hooks_yaml_for_platform(
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> String {
    let mut out = String::from("hooks:\n");
    for (hermes_event, our_event) in HERMES_EVENTS {
        // The native platforms derive the event token from the script stem, so
        // the synthetic filename carries the ai-memory event name.
        let command = hook_command(
            Path::new(&format!("{our_event}.sh")),
            server_url,
            auth_token,
            context,
        );
        out.push_str(&format!(
            "  {hermes_event}:\n    - command: {}\n      timeout: {HERMES_HOOK_TIMEOUT_SECONDS}\n",
            yaml_single_quote(&command)
        ));
    }
    out
}

/// Devin hook payload for docker/setup-agent script snippets.
/// Devin uses HookShape::Nested (same as Claude Code/Grok) but with
/// DEVIN_EVENTS (PostCompaction instead of PreCompact, no subagent events).
#[must_use]
pub(crate) fn build_devin_payload(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &DEVIN_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_script_runner(),
            "devin",
            None,
            None,
        ),
    )
}

/// Grok Build CLI hook payload for apply/render paths. Native commands are the
/// default; explicit script fallback still points at the Grok script bundle.
pub(crate) fn build_grok_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_runner(),
            "grok",
            data_dir,
            project_strategy,
        ),
    )
}

/// Devin hook payload for apply/render paths. Native commands are the
/// default; explicit script fallback still points at the Devin script bundle.
/// Devin uses HookShape::Nested (same as Claude Code/Grok) but with
/// DEVIN_EVENTS (PostCompaction instead of PreCompact, no subagent events).
#[must_use]
pub(crate) fn build_devin_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &DEVIN_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_runner(),
            "devin",
            data_dir,
            project_strategy,
        ),
    )
}

/// `{"hooks": <event map>}` for Factory Droid (#1240). `~/.factory/hooks.json`
/// holds the event map itself, with no wrapper key, in Claude Code's nested
/// shape (`"Event": [{"matcher": "", "hooks": [{"type": "command",
/// "command": …}]}]`). Droid runs each `command` through
/// a shell with the Claude Code-shaped event JSON on stdin, so every entry
/// invokes the native `ai-memory hook` command; there is no script bundle to
/// fall back to, so the platform is always a native one, whatever
/// `AI_MEMORY_HOOK_PLATFORM` asks for. An empty matcher fires on every tool
/// and every SessionStart source.
pub(crate) fn build_droid_hooks_payload(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> Value {
    build_hook_payload_for_platform(
        &DROID_EVENTS,
        Path::new(""),
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::native(),
            "droid",
            data_dir,
            project_strategy,
        ),
    )
}

/// `{"hooks": <event map>}` for Meta Muse Code: the `hooks` key of
/// `${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`, in Claude Code's nested
/// shape with an empty matcher (every tool, every `SessionStart` source).
/// Muse runs each `command` through a shell with a cleared environment and
/// the Claude Code-shaped event JSON on stdin, so every entry invokes the
/// native `ai-memory hook` command with its server, data dir and agent baked
/// in; there is no script bundle, whatever `AI_MEMORY_HOOK_PLATFORM` asks
/// for. Each handler carries [`MUSE_HOOK_TIMEOUT_SECS`].
pub(crate) fn build_muse_hooks_payload(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> Value {
    let mut payload = build_hook_payload_for_platform(
        &MUSE_EVENTS,
        Path::new(""),
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::native(),
            "muse",
            data_dir,
            project_strategy,
        ),
    );
    if let Some(events) = payload.get_mut("hooks").and_then(Value::as_object_mut) {
        for groups in events.values_mut().filter_map(Value::as_array_mut) {
            for handler in groups
                .iter_mut()
                .filter_map(|group| group.get_mut("hooks").and_then(Value::as_array_mut))
                .flatten()
                .filter_map(Value::as_object_mut)
            {
                handler.insert("timeout".into(), Value::from(MUSE_HOOK_TIMEOUT_SECS));
            }
        }
    }
    payload
}

/// `{"hooks": <event map>}` for Tencent CodeBuddy Code (#1299). The `hooks`
/// key of `~/.codebuddy/settings.json` holds Claude Code's nested shape
/// (`"Event": [{"matcher": "", "hooks": [{"type": "command", "command": …}]}]`).
/// CodeBuddy runs each `command` with the Claude Code-shaped event JSON on
/// stdin, so every entry invokes the native `ai-memory hook` command; there
/// is no script bundle, whatever `AI_MEMORY_HOOK_PLATFORM` asks for. An empty
/// matcher fires on every tool and every SessionStart source.
pub(crate) fn build_codebuddy_hooks_payload(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> Value {
    build_hook_payload_for_platform(
        &CODEBUDDY_EVENTS,
        Path::new(""),
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::native(),
            "codebuddy",
            data_dir,
            project_strategy,
        ),
    )
}

/// `{"hooks": <event map>}` for IBM Bob's
/// `~/.bob/settings/settings.json`. Bob's outer event groups have the same
/// nested shape as Claude Code, but `matcher` is valid only on tool events;
/// omitting it matches every tool and keeps lifecycle entries schema-valid.
/// Bob runs the command through the platform shell with the event JSON on
/// stdin, so each entry invokes ai-memory's native hook command directly.
pub(crate) fn build_ibm_bob_hooks_payload(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> Value {
    build_hook_payload_for_platform(
        &IBM_BOB_EVENTS,
        Path::new(""),
        server_url,
        auth_token,
        HookShape::NestedWithoutMatcher,
        HookCommandContext::new(
            HookCommandPlatform::native(),
            "ibm-bob",
            data_dir,
            project_strategy,
        ),
    )
}

/// Pool (Poolside Agent CLI) `.poolside/settings.yaml` `hooks:` block for
/// install-hooks' print path. Pool's hook config is a project-scoped YAML
/// file at the repo root — there is no user-global hook file for ai-memory
/// to merge, so the installer renders a ready-to-paste snippet instead of
/// writing into the user's project checkouts.
///
/// One entry per event, shell command string on `command:`, `matcher: "*"`
/// and a bounded `timeout` per Pool's documented schema. YAML single-quote
/// escaping (`'` → `''`) keeps the embedded shell quoting intact.
#[must_use]
pub(crate) fn build_pool_settings_yaml_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> String {
    build_pool_settings_yaml_for_platform(
        emit_root,
        server_url,
        auth_token,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_runner(),
            "pool",
            data_dir,
            project_strategy,
        ),
    )
}

/// Script-fallback variant for `setup-agent` / docker-host snippets: the
/// copied `.sh` scripts are the artifact, so the commands reference them
/// rather than a host-local native binary.
#[must_use]
pub(crate) fn build_pool_settings_yaml(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> String {
    build_pool_settings_yaml_for_platform(
        emit_root,
        server_url,
        auth_token,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_script_runner(),
            "pool",
            None,
            None,
        ),
    )
}

fn build_pool_settings_yaml_for_platform(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> String {
    let mut out = String::from("hooks:\n");
    for (event, script) in &POOL_EVENTS {
        let resolved = script_for_platform(script, context.platform);
        let abs = emit_root.join(resolved.as_ref());
        let command = hook_command(&abs, server_url, auth_token, context);
        let stem = script.strip_suffix(".sh").unwrap_or(script);
        out.push_str(&format!(
            "  {event}:\n    - name: ai-memory-{stem}\n      matcher: \"*\"\n      command: {}\n      timeout: 20\n",
            yaml_single_quote(&command)
        ));
    }
    out
}

/// Emit a YAML single-quoted scalar: wrap in `'…'`, doubling any embedded
/// `'`. Single-quote style is the only YAML form in which the POSIX shell
/// quoting inside the hook command survives byte-for-byte.
pub(crate) fn yaml_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Different agents nest hook entries differently. Two shapes
/// cover everyone we support:
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookShape {
    /// Claude Code / Codex / Gemini CLI:
    /// `"E": [ { "matcher":"", "hooks":[ {"type":"command",
    /// "command":"..."} ] } ]`
    /// Gemini CLI tolerates (but doesn't require) a sibling
    /// `sequential` key at the outer level — we don't set it.
    Nested,
    /// Command Code's nested handler shape without an outer matcher. Its
    /// stable hook schema treats omission as "all tools" and does not fire
    /// SessionStart/Stop when a matcher is present.
    NestedWithoutMatcher,
    /// Cursor: `"e": [ { "type":"command", "command":"...",
    /// "matcher":"" } ]` (no inner `hooks` array). Cursor's
    /// `hooks.json` also requires a sibling `version: 1` key at
    /// the top level — handled by the caller's apply path.
    Flat,
    /// GitHub Copilot CLI: `"E": [ { "type":"command", "command":"..." } ]`.
    /// Entries sit directly in the event array like Cursor's, but with no
    /// `matcher`: Copilot rejects it on `sessionStart`/`sessionEnd`/
    /// `userPromptSubmitted`/`agentStop`/`subagentStop`/`errorOccurred` and
    /// treats it as an optional filter elsewhere, so omitting it everywhere
    /// means "fire on every invocation". The file's top-level `version: 1` is
    /// handled by the caller's apply path, as for Cursor.
    FlatWithoutMatcher,
}

/// One hook profile = (event vocabulary, JSON shape). Each agent
/// gets its own constant so the install path is purely data-
/// driven: pick the profile, build the payload, write the file.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HookProfile {
    /// `(EventName, script_basename)` tuples in the order the
    /// agent surfaces them. Event names are case-sensitive and
    /// agent-specific — Claude Code uses `SessionStart` while
    /// Cursor uses `sessionStart`. The POSIX script filename resolves
    /// against `hooks/<agent-dir>/`; Windows rendering rewrites the
    /// `.sh` suffix to `.ps1`.
    pub events: &'static [(&'static str, &'static str)],
    /// JSON shape the file uses.
    pub shape: HookShape,
}

/// Codex's hook-event vocabulary (per the openai/codex source —
/// see `codex-rs/config/src/hook_config.rs`). Same nested shape as
/// Claude Code. `SessionEnd` shipped in Codex CLI 0.145.0 (openai/codex
/// PR #33895, "Add SessionEnd hooks for thread teardown"); it is a
/// root-session-only event that runs synchronously during thread
/// teardown. Codex still has no `SubagentStart`/`SubagentStop` in a
/// first-party-supported form here, so those stay Claude-Code-only.
pub(crate) const CODEX_EVENTS: [(&str, &str); 7] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
];

/// Command Code's stable shell-hook vocabulary. Mods expose more lifecycle
/// events but remain experimental, so the first-party integration uses only
/// these documented stable boundaries.
pub(crate) const COMMAND_CODE_EVENTS: [(&str, &str); 4] = [
    ("SessionStart", "session-start.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("Stop", "stop.sh"),
];

/// Cursor's hook-event vocabulary (per
/// <https://cursor.com/docs/agent/hooks>). camelCase event names
/// and a FLAT JSON shape (no inner `hooks: [...]` wrapper).
/// `beforeSubmitPrompt` maps to ai-memory's `user-prompt-submit`
/// concept. Cursor has no `userPromptSubmit` event.
pub(crate) const CURSOR_EVENTS: [(&str, &str); 8] = [
    ("sessionStart", "session-start.sh"),
    ("sessionEnd", "session-end.sh"),
    ("beforeSubmitPrompt", "user-prompt-submit.sh"),
    ("preToolUse", "pre-tool-use.sh"),
    ("postToolUse", "post-tool-use.sh"),
    ("postToolUseFailure", "post-tool-use.sh"),
    ("preCompact", "pre-compact.sh"),
    ("stop", "stop.sh"),
];

/// Gemini CLI's hook-event vocabulary (per
/// <https://geminicli.com/docs/hooks/reference>). Event names use
/// PascalCase. The vocab DIFFERS from Claude Code's:
///   - `BeforeTool` / `AfterTool` instead of `PreToolUse` / `PostToolUse`
///   - `PreCompress` instead of `PreCompact`
///   - No `UserPromptSubmit` equivalent (skipped)
///   - No `Stop` event (SessionEnd covers it)
pub(crate) const GEMINI_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start.sh"),
    ("SessionEnd", "session-end.sh"),
    ("BeforeTool", "pre-tool-use.sh"),
    ("AfterTool", "post-tool-use.sh"),
    ("PreCompress", "pre-compact.sh"),
];

/// Per-agent profile constants. Add a new agent by adding one of
/// these + a script-dir name + a config-file path resolver — the
/// payload-build path picks up the rest from `shape`.
pub(crate) const CODEX_PROFILE: HookProfile = HookProfile {
    events: &CODEX_EVENTS,
    shape: HookShape::Nested,
};
pub(crate) const COMMAND_CODE_PROFILE: HookProfile = HookProfile {
    events: &COMMAND_CODE_EVENTS,
    shape: HookShape::NestedWithoutMatcher,
};
/// Copilot keeps its flat entry shape even with PascalCase event names — only
/// the payload sent to the script switches to snake_case — so this is
/// `FlatWithoutMatcher`, not Claude's `Nested` (see the variant's doc).
pub(crate) const COPILOT_CLI_PROFILE: HookProfile = HookProfile {
    events: &COPILOT_CLI_EVENTS,
    shape: HookShape::FlatWithoutMatcher,
};
pub(crate) const CURSOR_PROFILE: HookProfile = HookProfile {
    events: &CURSOR_EVENTS,
    shape: HookShape::Flat,
};
pub(crate) const GEMINI_PROFILE: HookProfile = HookProfile {
    events: &GEMINI_EVENTS,
    shape: HookShape::Nested,
};

/// Antigravity CLI (`agy`) hook-event vocabulary (per
/// <https://antigravity.google/docs/hooks>). Named-groups format
/// at the top level; events inside each group.
/// Tool events use nested shape (matcher + hooks), lifecycle
/// events use flat shape (direct handler list).
pub(crate) const ANTIGRAVITY_TOOL_EVENTS: [(&str, &str); 2] = [
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
];

pub(crate) const ANTIGRAVITY_LIFECYCLE_EVENTS: [(&str, &str); 2] =
    [("PreInvocation", "session-start.sh"), ("Stop", "stop.sh")];

/// Build the Antigravity CLI (`agy`) `hooks.json` payload.
///
/// Antigravity CLI uses a named-groups format where the top level
/// maps hook-group names to their event configurations. Each group
/// can contain any subset of the supported events. Tool events
/// (`PreToolUse`, `PostToolUse`) use the nested shape with matcher;
/// lifecycle events (`PreInvocation`, `Stop`) use a flat handler
/// list where matcher is ignored.
///
/// The output is `{ "ai-memory": { <events> } }`.
pub(crate) fn build_antigravity_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_antigravity_payload_for_platform(
        emit_root,
        server_url,
        auth_token,
        HookCommandPlatform::current(),
        "antigravity-cli",
        data_dir,
        project_strategy,
    )
}

fn build_antigravity_payload_for_platform(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    platform: HookCommandPlatform,
    agent: &str,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    let mut group = serde_json::Map::new();

    // Tool events: nested shape (matcher + inner hooks array)
    for (event, script) in &ANTIGRAVITY_TOOL_EVENTS {
        let s = script_for_platform(script, platform);
        let abs = emit_root.join(s.as_ref());
        let handler = hook_handler_value(hook_handler_spec(
            &abs,
            server_url,
            auth_token,
            HookCommandContext::new(platform, agent, data_dir, project_strategy),
            HookShape::Nested,
        ));
        group.insert(
            (*event).to_string(),
            json!([{
                "matcher": "",
                "hooks": [handler],
            }]),
        );
    }

    // Lifecycle events: flat shape (direct handler list, no matcher)
    for (event, script) in &ANTIGRAVITY_LIFECYCLE_EVENTS {
        let s = script_for_platform(script, platform);
        let abs = emit_root.join(s.as_ref());
        let handler = hook_handler_value(hook_handler_spec(
            &abs,
            server_url,
            auth_token,
            HookCommandContext::new(platform, agent, data_dir, project_strategy),
            HookShape::Flat,
        ));
        group.insert((*event).to_string(), Value::Array(vec![handler]));
    }

    json!({ "ai-memory": group })
}

/// Build a hook payload for `profile`. The output is always
/// `{ "hooks": { "<EventName>": <profile-specific-array> } }`; the
/// caller is responsible for any sibling top-level keys (e.g.
/// Cursor's `"version": 1`).
#[cfg(test)]
pub(crate) fn build_profile_payload(
    profile: &HookProfile,
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_profile_payload_for_agent(
        profile,
        emit_root,
        server_url,
        auth_token,
        "claude-code",
        None,
        None,
        false,
    )
}

// A hook-render builder that threads several independent render inputs (profile,
// paths, agent, scope strategy, capture opt-in); grouping them into a struct
// would not make the one call path clearer than the named parameters do.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_profile_payload_for_agent(
    profile: &HookProfile,
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    agent: &str,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
    capture_assistant: bool,
) -> serde_json::Value {
    build_hook_payload(
        profile.events,
        emit_root,
        server_url,
        auth_token,
        profile.shape,
        HookCommandContext::new(
            HookCommandPlatform::current(),
            agent,
            data_dir,
            project_strategy,
        )
        .with_capture_assistant(capture_assistant),
    )
}

#[cfg(test)]
pub(crate) fn build_profile_script_payload_for_test(
    profile: &HookProfile,
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    agent: &str,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Value {
    build_hook_payload(
        profile.events,
        emit_root,
        server_url,
        auth_token,
        profile.shape,
        HookCommandContext::new(
            HookCommandPlatform::Posix,
            agent,
            data_dir,
            project_strategy,
        ),
    )
}

fn build_hook_payload(
    events: &[(&str, &str)],
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    shape: HookShape,
    context: HookCommandContext<'_>,
) -> serde_json::Value {
    build_hook_payload_for_platform(events, emit_root, server_url, auth_token, shape, context)
}

/// Build the `(event, command)` pairs behind Kimi Code's `[[hooks]]`
/// TOML entries. Kimi Code hook entries accept only `event` / `matcher`
/// / `command` / `timeout` — any other key makes the whole config.toml
/// fail to load — so callers emit exactly `event` + `command` and leave
/// the rest at Kimi Code's defaults (no `matcher` = match everything;
/// no `timeout` = 30s). Commands come from the shared `hook_command`
/// helper for the current platform: native `ai-memory hook --event …`
/// invocations by default, or the staged script bundle (`.sh` on
/// POSIX, `.ps1` on Windows) on the compatibility platforms.
pub(crate) fn kimi_code_hook_commands(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> Vec<(&'static str, String)> {
    kimi_code_hook_commands_for_platform(
        emit_root,
        server_url,
        auth_token,
        HookCommandPlatform::current(),
        data_dir,
        project_strategy,
    )
}

fn kimi_code_hook_commands_for_platform(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    platform: HookCommandPlatform,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> Vec<(&'static str, String)> {
    KIMI_CODE_EVENTS
        .iter()
        .map(|(event, script)| {
            let script = script_for_platform(script, platform);
            let abs = emit_root.join(script.as_ref());
            let command = hook_command(
                &abs,
                server_url,
                auth_token,
                HookCommandContext::new(platform, "kimi-code", data_dir, project_strategy),
            );
            (*event, command)
        })
        .collect()
}

/// Build the `hooks` map for a Kiro CLI v2-engine agent config. Entry
/// shape per the shipping `Hook` struct (aws/amazon-q-developer-cli
/// `crates/chat-cli/src/cli/agent/hook.rs`): a flat
/// `{ "command": … }` object per trigger. Two deliberate omissions:
///
/// - no `matcher` key: in Kiro v2 an *absent* matcher applies the hook
///   to every tool, while an empty-string matcher is a tool-name
///   pattern that matches nothing — the opposite of Claude Code's
///   empty-matcher convention, so `HookShape::Flat` must not be reused;
/// - no `type` key: the v2 `Hook` struct has no such field.
///
/// `agentSpawn` sets `max_output_size` above the 10 KiB default so a
/// fetched handoff + compiled project brief is not truncated mid-JSON
/// before Kiro adds it to the agent context.
pub(crate) fn build_kiro_cli_v2_hooks_value(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Map<String, Value> {
    build_kiro_cli_v2_hooks_value_for_platform(
        emit_root,
        server_url,
        auth_token,
        HookCommandPlatform::current(),
        data_dir,
        project_strategy,
    )
}

/// Session-start context injection ceiling for Kiro v2 (`max_output_size`).
/// Kiro truncates hook stdout beyond this; 64 KiB comfortably covers a
/// handoff plus a `[briefing]`-budgeted brief.
pub(crate) const KIRO_CLI_V2_SESSION_START_MAX_OUTPUT: usize = 64 * 1024;

fn build_kiro_cli_v2_hooks_value_for_platform(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    platform: HookCommandPlatform,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> serde_json::Map<String, Value> {
    let mut hooks = serde_json::Map::new();
    for (event, script) in KIRO_CLI_V2_EVENTS {
        let script = script_for_platform(script, platform);
        let abs = emit_root.join(script.as_ref());
        let command = hook_command(
            &abs,
            server_url,
            auth_token,
            HookCommandContext::new(platform, "kiro-cli", data_dir, project_strategy),
        );
        let mut entry = serde_json::Map::new();
        entry.insert("command".to_string(), Value::String(command));
        if event == "agentSpawn" {
            entry.insert(
                "max_output_size".to_string(),
                Value::from(KIRO_CLI_V2_SESSION_START_MAX_OUTPUT),
            );
        }
        hooks.insert(event.to_string(), json!([Value::Object(entry)]));
    }
    hooks
}

/// Build Kiro CLI v3's standalone `{ "version": "v1", "hooks": [...] }`
/// registration document. Each entry is deliberately named so apply and
/// uninstall can prove ownership using both the name and command signature.
pub(crate) fn build_kiro_cli_v3_hooks_value(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
) -> Value {
    let platform = HookCommandPlatform::current();
    let hooks = KIRO_CLI_V3_EVENTS
        .iter()
        .map(|(trigger, script)| {
            let platform_script = script_for_platform(script, platform);
            let command = hook_command(
                &emit_root.join(platform_script.as_ref()),
                server_url,
                auth_token,
                HookCommandContext::new(platform, "kiro-cli", data_dir, project_strategy),
            );
            let name = script.strip_suffix(".sh").map_or_else(
                || format!("ai-memory-{script}"),
                |stem| format!("ai-memory-{stem}"),
            );
            let timeout = if *trigger == "SessionStart" { 5 } else { 1 };
            json!({
                "name": name,
                "trigger": trigger,
                "action": {
                    "type": "command",
                    "command": command,
                },
                "timeout": timeout,
                "enabled": true,
            })
        })
        .collect::<Vec<_>>();
    json!({"version": "v1", "hooks": hooks})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookCommandPlatform {
    Posix,
    /// Windows script fallback: invoke the staged `.ps1` hook through an
    /// encoded PowerShell program so a host runner cannot expand its env setup.
    Windows,
    /// Claude Code on Windows invokes hooks through bash (Git for
    /// Windows), not PowerShell. Commands use POSIX `.sh` scripts
    /// wrapped in `bash -c '...'` with drive-letter paths converted
    /// to Git Bash format (`C:\x` → `/c/x`).
    WindowsBash,
    /// Windows, native: invoke the `ai-memory` binary directly
    /// (`<exe> hook --event … --agent …`) with no shell or child
    /// processes — ~3.5× faster per hook than `WindowsBash`. Default for
    /// Claude Code on Windows; see
    /// `docs/windows.md#native-hook-command-claude-code-on-windows`.
    WindowsNative,
    /// POSIX (Linux/macOS), native: invoke the `ai-memory` binary directly
    /// (`<exe> hook --event …`) instead of the `.sh` script, so the hook gets
    /// the local spool + OIDC-token fallback. The **default** for native
    /// Linux/macOS Claude Code installs (mirrors `WindowsNative`). The
    /// Linux/macOS Docker wrapper forces `posix` so its host-rendered config
    /// keeps the `.sh` path (the host has no local binary). Override with
    /// `AI_MEMORY_HOOK_PLATFORM=posix` to get the shell scripts.
    PosixNative,
}

#[derive(Clone, Copy)]
struct HookCommandContext<'a> {
    platform: HookCommandPlatform,
    agent: &'a str,
    data_dir: Option<&'a Path>,
    /// Install-time default project strategy baked into the command
    /// (`install-hooks --project-strategy`). `None` bakes nothing.
    project_strategy: Option<&'a str>,
    /// Whether this render path may use Claude Code's exec-form hook handler.
    /// Only `install-hooks --agent claude-code` sets this; setup-agent/docker
    /// snippets keep command-string script fallback even when the platform env
    /// is overridden to `windows-native`.
    claude_windows_exec_allowed: bool,
    /// Bake `--capture-assistant` onto the native `stop` command only (#196).
    /// Set exclusively by `install-hooks --agent claude-code --capture-assistant`.
    capture_assistant: bool,
}

impl<'a> HookCommandContext<'a> {
    const fn new(
        platform: HookCommandPlatform,
        agent: &'a str,
        data_dir: Option<&'a Path>,
        project_strategy: Option<&'a str>,
    ) -> Self {
        Self {
            platform,
            agent,
            data_dir,
            project_strategy,
            claude_windows_exec_allowed: false,
            capture_assistant: false,
        }
    }

    const fn allow_claude_windows_exec(mut self) -> Self {
        self.claude_windows_exec_allowed = true;
        self
    }

    const fn with_capture_assistant(mut self, on: bool) -> Self {
        self.capture_assistant = on;
        self
    }
}

/// Bake `--capture-assistant` onto the native `stop` command only (#196), so the
/// other events stay byte-identical. Empty for every other event, and for a
/// context that did not opt in.
fn native_capture_assistant_arg(context: HookCommandContext<'_>, event: &str) -> &'static str {
    if context.capture_assistant && event == "stop" {
        " --capture-assistant"
    } else {
        ""
    }
}

impl HookCommandPlatform {
    /// Parse an explicit `AI_MEMORY_HOOK_PLATFORM` override. `None` when the
    /// var is unset or names no known platform — callers then apply their own
    /// per-render-path default. One parser so a new platform value can't be
    /// recognised by one render path and silently ignored by another.
    fn from_env_override() -> Option<Self> {
        let v = std::env::var("AI_MEMORY_HOOK_PLATFORM").ok()?;
        match v {
            v if v.eq_ignore_ascii_case("windows") => Some(Self::Windows),
            v if v.eq_ignore_ascii_case("posix") || v.eq_ignore_ascii_case("unix") => {
                Some(Self::Posix)
            }
            v if v.eq_ignore_ascii_case("windows-bash") => Some(Self::WindowsBash),
            v if v.eq_ignore_ascii_case("windows-native") => Some(Self::WindowsNative),
            v if v.eq_ignore_ascii_case("posix-native") => Some(Self::PosixNative),
            _ => None,
        }
    }

    fn current() -> Self {
        Self::from_env_override().unwrap_or(if cfg!(windows) {
            Self::WindowsNative
        } else {
            // Local installs use the native hook command for policy-v1.
            // `posix` remains an explicit legacy script compatibility override.
            Self::PosixNative
        })
    }

    /// Platform for agents known to use bash as their hook runner on
    /// Windows (currently Claude Code). Returns `WindowsNative` on
    /// Windows unless overridden by `AI_MEMORY_HOOK_PLATFORM`.
    fn for_bash_runner() -> Self {
        // Native macOS / Linux defaults to the binary hook command (spool +
        // OIDC), same as Windows. The Linux/macOS Docker wrapper forces
        // `posix` so its host-rendered config keeps using the `.sh` scripts.
        Self::from_env_override().unwrap_or(if cfg!(windows) {
            Self::WindowsNative
        } else {
            Self::PosixNative
        })
    }

    /// The native binary command for this OS, ignoring any override: for an
    /// agent with no script bundle a script platform would name files that
    /// were never staged.
    const fn native() -> Self {
        if cfg!(windows) {
            Self::WindowsNative
        } else {
            Self::PosixNative
        }
    }

    /// Script fallback for setup-agent / docker-host snippets. Respects an
    /// explicit override, but defaults to the shell command because setup-agent
    /// copies scripts, not a host-local native binary.
    fn for_bash_script_runner() -> Self {
        Self::from_env_override().unwrap_or(if cfg!(windows) {
            Self::WindowsBash
        } else {
            Self::Posix
        })
    }
}

fn build_hook_payload_for_platform(
    events: &[(&str, &str)],
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    shape: HookShape,
    context: HookCommandContext<'_>,
) -> serde_json::Value {
    let mut hooks_block = serde_json::Map::new();
    for (event, script) in events {
        let script = script_for_platform(script, context.platform);
        let abs = emit_root.join(script.as_ref());

        // Claude Code's hook schema (per
        // https://code.claude.com/docs/en/hooks):
        //   "<EventName>": [
        //     { "matcher": "<tool-name regex or empty>",
        //       "hooks": [ { "type": "command", "command": "..." } ]
        //     }
        //   ]
        //
        // Shell-form handlers INLINE env vars into the command string itself
        // (`AI_MEMORY_HOOK_URL=... AI_MEMORY_AUTH_TOKEN=... /path`)
        // rather than passing them through an `env` field on the hook entry.
        // Native Claude Code Windows installs instead use official exec form,
        // where server/auth/project options are passed as raw argv tokens.
        // Reasons shell-form handlers keep inline env vars:
        //   1. CC doesn't appear to honour an `env` field at this
        //      level — observed empirically: the hook fires but
        //      the script sees neither var and falls back to the
        //      127.0.0.1 default, so POSTs go nowhere.
        //   2. Inlining the env into the command string is
        //      portable across any shell-style hook runner — POSIX
        //      `VAR=val command` syntax is universally honoured.
        //   3. The hook scripts already read those env vars (see
        //      `hooks/claude-code/session-start.sh` etc.), so no
        //      script changes are required on POSIX. Windows uses an
        //      explicit PowerShell command with equivalent env setup.
        let handler = hook_handler_value(hook_handler_spec(
            &abs, server_url, auth_token, context, shape,
        ));

        // Empty matcher = fire on every event of this kind. Right
        // for ai-memory's capture hooks (every prompt, every tool
        // call, every session boundary).
        let entry = match shape {
            HookShape::Nested => json!([{
                "matcher": "",
                "hooks": [handler],
            }]),
            HookShape::NestedWithoutMatcher => json!([{
                "hooks": [handler],
            }]),
            HookShape::Flat => Value::Array(vec![hook_handler_with_matcher(handler)]),
            HookShape::FlatWithoutMatcher => Value::Array(vec![handler]),
        };
        hooks_block.insert((*event).to_string(), entry);
    }
    json!({ "hooks": hooks_block })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HookHandlerSpec {
    ShellString(String),
    Exec { command: String, args: Vec<String> },
}

fn hook_handler_spec(
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
    _shape: HookShape,
) -> HookHandlerSpec {
    if context.platform == HookCommandPlatform::WindowsNative
        && context.claude_windows_exec_allowed
        && context.agent == "claude-code"
    {
        return windows_native_exec_spec(script, server_url, auth_token, context);
    }
    HookHandlerSpec::ShellString(hook_command(script, server_url, auth_token, context))
}

fn hook_handler_value(spec: HookHandlerSpec) -> Value {
    match spec {
        HookHandlerSpec::ShellString(command) => json!({
            "type": "command",
            "command": command,
        }),
        HookHandlerSpec::Exec { command, args } => json!({
            "type": "command",
            "command": command,
            "args": args,
        }),
    }
}

fn hook_handler_with_matcher(mut handler: Value) -> Value {
    if let Some(obj) = handler.as_object_mut() {
        obj.insert("matcher".to_string(), Value::String(String::new()));
    }
    handler
}

fn script_for_platform(script: &str, platform: HookCommandPlatform) -> Cow<'_, str> {
    match platform {
        HookCommandPlatform::Posix
        | HookCommandPlatform::PosixNative
        | HookCommandPlatform::WindowsBash
        | HookCommandPlatform::WindowsNative => Cow::Borrowed(script),
        HookCommandPlatform::Windows => match script.strip_suffix(".sh") {
            Some(stem) => Cow::Owned(format!("{stem}.ps1")),
            None => Cow::Borrowed(script),
        },
    }
}

pub(crate) fn hook_script_for_current_platform(script: &str) -> Cow<'_, str> {
    script_for_platform(script, HookCommandPlatform::current())
}

pub(crate) fn hook_script_for_claude_code(script: &str) -> Cow<'_, str> {
    script_for_platform(script, HookCommandPlatform::for_bash_runner())
}

#[must_use]
pub(crate) fn local_hook_policy_v1_supported() -> bool {
    matches!(
        HookCommandPlatform::current(),
        HookCommandPlatform::PosixNative | HookCommandPlatform::WindowsNative
    )
}

/// Fallback when no usable executable path can be embedded: the bare command
/// name, resolved through `PATH` when each hook runs.
#[cfg(not(windows))]
const FALLBACK_HOOK_EXE: &str = "ai-memory";
#[cfg(windows)]
const FALLBACK_HOOK_EXE: &str = "ai-memory.exe";

/// Strip the ` (deleted)` marker the Linux kernel appends to the
/// `/proc/<pid>/exe` readlink once the executable's inode has been unlinked or
/// replaced (Rust std passes the marker through verbatim —
/// rust-lang/rust#40284). A real executable may itself use that suffix, so
/// callers must prefer the unmodified path when it still exists.
fn strip_deleted_suffix(exe: PathBuf) -> PathBuf {
    match exe.to_str() {
        Some(text) => text
            .strip_suffix(" (deleted)")
            .map(PathBuf::from)
            .unwrap_or(exe),
        None => exe,
    }
}

/// The executable path to embed in a rendered hook command, given what
/// `current_exe()` produced.
///
/// The classic corrupting sequence is `ai-memory upgrade`: it replaces its own
/// binary and then re-renders the staged hook configs from the same, now
/// exe-deleted process, so `current_exe()` comes back as `<path> (deleted)`.
/// Stripping the marker yields the path that now holds the freshly installed
/// binary. A missing file or a failed resolution falls back to
/// [`FALLBACK_HOOK_EXE`] instead of baking a dead absolute path into every
/// hook command.
///
/// A binary inside the Nix store also falls back to the bare command: the
/// store path names one immutable package version, which `nix-collect-garbage`
/// deletes after the next profile switch, while the profile's `bin/` on PATH
/// keeps pointing at the current build.
fn embedded_exe_from(raw: Option<PathBuf>) -> PathBuf {
    embedded_exe_with(raw, Path::is_file)
}

/// [`embedded_exe_from`] with the existence probe injected, so the store-path
/// fallback is testable on hosts without a `/nix/store`.
fn embedded_exe_with(raw: Option<PathBuf>, is_file: impl Fn(&Path) -> bool) -> PathBuf {
    let Some(exe) = raw else {
        return PathBuf::from(FALLBACK_HOOK_EXE);
    };
    if is_nix_store_path(&exe) {
        return PathBuf::from(FALLBACK_HOOK_EXE);
    }
    if is_file(&exe) {
        return exe;
    }
    let stripped = strip_deleted_suffix(exe);
    if is_file(&stripped) {
        stripped
    } else {
        PathBuf::from(FALLBACK_HOOK_EXE)
    }
}

fn is_nix_store_path(path: &Path) -> bool {
    path.starts_with("/nix/store")
}

/// [`embedded_exe_from`] over this process's own executable.
pub(crate) fn hook_embedded_exe_path() -> PathBuf {
    embedded_exe_from(std::env::current_exe().ok())
}

fn hook_command(
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> String {
    match context.platform {
        HookCommandPlatform::Posix => {
            let mut prefix = format!("AI_MEMORY_HOOK_URL={} ", shell_quote(server_url));
            if let Some(t) = auth_token {
                prefix.push_str(&format!("AI_MEMORY_AUTH_TOKEN={} ", shell_quote(t)));
            }
            if let Some(s) = context.project_strategy {
                prefix.push_str(&format!("AI_MEMORY_PROJECT_STRATEGY={} ", shell_quote(s)));
            }
            format!("{prefix}{}", shell_quote(&script.to_string_lossy()))
        }
        HookCommandPlatform::Windows => {
            // Hook runners launch this child with redirected streams. Force
            // text output and suppress non-interactive progress so Windows
            // PowerShell does not serialize progress records as CLIXML. Leave
            // input formatting at its default: the hook reads raw Console.In.
            let mut setup = format!(
                "$ProgressPreference='SilentlyContinue'; $env:AI_MEMORY_HOOK_URL={}",
                powershell_quote(server_url)
            );
            if let Some(t) = auth_token {
                setup.push_str(&format!(
                    "; $env:AI_MEMORY_AUTH_TOKEN={}",
                    powershell_quote(t)
                ));
            }
            if let Some(s) = context.project_strategy {
                setup.push_str(&format!(
                    "; $env:AI_MEMORY_PROJECT_STRATEGY={}",
                    powershell_quote(s)
                ));
            }
            let program = format!("{setup}; & {}", powershell_quote(&script.to_string_lossy()));
            let encoded = powershell_encoded_command(&program);
            format!(
                "powershell.exe -NoLogo -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand {encoded}"
            )
        }
        HookCommandPlatform::WindowsBash => {
            let bash_path = to_git_bash_path(&script.to_string_lossy());
            let mut inner = format!("AI_MEMORY_HOOK_URL={} ", shell_quote(server_url));
            if let Some(t) = auth_token {
                inner.push_str(&format!("AI_MEMORY_AUTH_TOKEN={} ", shell_quote(t)));
            }
            if let Some(s) = context.project_strategy {
                inner.push_str(&format!("AI_MEMORY_PROJECT_STRATEGY={} ", shell_quote(s)));
            }
            inner.push_str(&shell_quote(&bash_path));
            format!("bash -c {}", shell_quote(&inner))
        }
        HookCommandPlatform::WindowsNative => {
            // Legacy/setup-agent/fallback string form: invoke the binary
            // directly as `"<exe>" hook --event <e> --agent ...`. Primary
            // Claude Code WindowsNative `install-hooks` uses exec form instead
            // (see `windows_native_exec_spec`). The event token is the script
            // stem (`pre-tool-use.sh` → `pre-tool-use`). No shell, no child
            // processes in the intended runner.
            //
            // Quote with DOUBLE quotes, not POSIX single quotes: Claude Code
            // on Windows shell-form runners may treat '…' literally and error
            // out; double quotes + the native Windows path work in cmd.exe and
            // Git Bash. The event name is a fixed slug with no shell
            // metacharacters, so it is left unquoted.
            let exe = hook_embedded_exe_path().to_string_lossy().into_owned();
            let event = script
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            // Antigravity's executor wraps the whole command in `cmd /c "…"`
            // and does not strip inner quotes, so it needs the bare form
            // (#611); every other Windows agent keeps the double-quoted form.
            let bare = windows_hook_command_is_bare(context.agent);
            let quote = if bare {
                NativeQuote::WindowsBare
            } else {
                NativeQuote::Windows
            };
            let quote_field = |s: &str| {
                if bare {
                    s.to_string()
                } else {
                    win_double_quote(s)
                }
            };
            let mut cmd = format!(
                "{}{}{} hook --event {event} --agent {agent} --server-url {}",
                powershell_call_operator(context.agent),
                quote_field(&exe),
                native_data_dir_arg(context.data_dir, quote),
                quote_field(server_url),
                agent = context.agent,
            );
            if let Some(t) = auth_token {
                cmd.push_str(&format!(" --auth-token {}", quote_field(t)));
            }
            cmd.push_str(&native_project_strategy_arg(
                context.project_strategy,
                quote,
            ));
            cmd.push_str(native_capture_assistant_arg(context, event));
            cmd
        }
        HookCommandPlatform::PosixNative => {
            // Native POSIX (opt-in): invoke the binary directly so the hook
            // gets the local spool + OIDC fallback, instead of the `.sh` script
            // that POSTs via curl. Mirrors `WindowsNative` but with POSIX
            // single-quote quoting. The event name is the script stem.
            let exe = hook_embedded_exe_path().to_string_lossy().into_owned();
            let event = script
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let mut cmd = format!(
                "{}{} hook --event {event} --agent {agent} --server-url {}",
                shell_quote(&exe),
                native_data_dir_arg(context.data_dir, NativeQuote::Posix),
                shell_quote(server_url),
                agent = context.agent,
            );
            if let Some(t) = auth_token {
                cmd.push_str(&format!(" --auth-token {}", shell_quote(t)));
            }
            cmd.push_str(&native_project_strategy_arg(
                context.project_strategy,
                NativeQuote::Posix,
            ));
            cmd.push_str(native_capture_assistant_arg(context, event));
            cmd
        }
    }
}

fn powershell_encoded_command(program: &str) -> String {
    let utf16_le = program
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(utf16_le)
}

fn windows_native_exec_spec(
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> HookHandlerSpec {
    let exe = hook_embedded_exe_path();
    windows_native_exec_spec_with_exe(&exe, script, server_url, auth_token, context)
}

fn windows_native_exec_spec_with_exe(
    exe: &Path,
    script: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    context: HookCommandContext<'_>,
) -> HookHandlerSpec {
    let event = script
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let mut args = Vec::new();
    if let Some(data_dir) = context.data_dir {
        args.push("--data-dir".to_string());
        args.push(plain_windows_path_arg(data_dir));
    }
    args.extend([
        "hook".to_string(),
        "--event".to_string(),
        event.to_string(),
        "--agent".to_string(),
        context.agent.to_string(),
        "--server-url".to_string(),
        server_url.to_string(),
    ]);
    if let Some(t) = auth_token {
        args.push("--auth-token".to_string());
        args.push(t.to_string());
    }
    if let Some(strategy) = context.project_strategy {
        args.push("--project-strategy".to_string());
        args.push(strategy.to_string());
    }
    // Native-exec is the PRIMARY Windows Claude Code path, so the flag must be
    // baked here too — not only in the string form above (#196).
    if context.capture_assistant && event == "stop" {
        args.push("--capture-assistant".to_string());
    }
    HookHandlerSpec::Exec {
        command: plain_windows_path_arg(exe),
        args,
    }
}

fn plain_windows_path_arg(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    strip_windows_verbatim_prefix(&lossy).into_owned()
}

#[derive(Clone, Copy)]
enum NativeQuote {
    Posix,
    Windows,
    /// Windows, but with NO surrounding double quotes. Antigravity's hook
    /// executor runs the `command` string through `cmd /c "<string>"`, and
    /// cmd does not strip the inner quotes, so a quoted `"C:\…\ai-memory.exe"`
    /// is reported as "not recognized" and crashes the session (#611). The
    /// whole-string wrapping means a bare path is what actually runs; a path
    /// with a space in it is a known residual limitation (as it was before
    /// the native-hook rendering added quotes).
    WindowsBare,
}

/// Whether an agent's Windows hook executor needs the command rendered
/// WITHOUT surrounding quotes (it wraps the whole string in `cmd /c "…"`
/// and does not strip inner quotes). Antigravity CLI is the confirmed
/// case (#611); other Windows JSON-hook agents run the command in a form
/// where the double quotes are correct, so they keep them.
fn windows_hook_command_is_bare(agent: &str) -> bool {
    agent == "antigravity-cli"
}

fn native_data_dir_arg(data_dir: Option<&Path>, quote: NativeQuote) -> String {
    let Some(data_dir) = data_dir else {
        return String::new();
    };
    // Render safe verbatim Windows data-dir forms as plain paths (#116).
    let lossy = data_dir.to_string_lossy();
    let path = strip_windows_verbatim_prefix(&lossy);
    match quote {
        NativeQuote::Posix => format!(" --data-dir {}", shell_quote(&path)),
        NativeQuote::Windows => format!(" --data-dir {}", win_double_quote(&path)),
        NativeQuote::WindowsBare => format!(" --data-dir {path}"),
    }
}

/// Append ` --project-strategy <value>` to a native hook command when an
/// install-time default was baked in (`install-hooks --project-strategy`).
/// `None` appends nothing, keeping the command byte-identical to before.
fn native_project_strategy_arg(strategy: Option<&str>, quote: NativeQuote) -> String {
    let Some(strategy) = strategy else {
        return String::new();
    };
    match quote {
        NativeQuote::Posix => format!(" --project-strategy {}", shell_quote(strategy)),
        NativeQuote::Windows => format!(" --project-strategy {}", win_double_quote(strategy)),
        NativeQuote::WindowsBare => format!(" --project-strategy {strategy}"),
    }
}

/// Convert a Windows path to Git Bash (MSYS2) format.
/// `C:\Users\alice\hooks\x.sh` → `/c/Users/alice/hooks/x.sh`
fn to_git_bash_path(path: &str) -> String {
    let s = path.replace('\\', "/");
    if s.len() >= 3
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.as_bytes()[1] == b':'
        && s.as_bytes()[2] == b'/'
    {
        let drive = (s.as_bytes()[0] as char).to_ascii_lowercase();
        format!("/{drive}{}", &s[2..])
    } else {
        s
    }
}

/// Minimal shell quoting for embedding values into a `VAR=val cmd` prefix or
/// command path. Leaves only conservative shell-safe characters unquoted;
/// wraps everything else in single quotes and escapes embedded `'` via
/// `'\''`.
pub(crate) fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '%' | '+' | '=' | ',')
    }) {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn powershell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Wrap a value in double quotes for the `WindowsNative` hook command.
/// Claude Code on Windows runs hook commands via cmd.exe, which does not
/// honour POSIX single quotes; double quotes work in both cmd.exe and Git
/// Bash. The quoted values (binary path, URL, hex auth token) never
/// contain a literal `"`; any is stripped defensively rather than risk a
/// broken command line.
/// `& ` when this agent's Windows hook command is evaluated by PowerShell,
/// otherwise empty.
///
/// `win_double_quote` wraps the executable path because cmd.exe needs it —
/// see the note on that function. PowerShell parses a quoted literal in
/// command position as a *string expression* rather than an invocation, so
/// the same quoting that makes the command correct under cmd.exe makes it a
/// no-op that exits 1 with a ParserError under PowerShell. Every Codex hook
/// died that way on Windows (#515).
///
/// Deliberately an allowlist keyed on the agent, not a global change. The
/// call operator is **not** valid under cmd.exe, where `&` separates
/// commands — emitting it for Claude Code would break the integration that
/// works today. An agent joins this list only once its runner has been
/// observed, because both the failure and the false fix are silent: a wrong
/// answer either way produces a hook that installs cleanly and captures
/// nothing.
fn powershell_call_operator(agent: &str) -> &'static str {
    // Codex evaluates `~/.codex/hooks.json` command strings with PowerShell
    // on Windows (#515, reproduced against Codex CLI on a native install).
    // Grok Build CLI does the same for `~/.grok/hooks/*.json` (observed on
    // Grok 1.0.41: every ai-memory hook failed with a ParserError in the
    // session's `hook_execution` updates, and its hooks guide documents the
    // PowerShell `$VAR` rewrite).
    // Copilot CLI evaluates its Windows hook command with PowerShell as well
    // (#1462), causing a ParserError and tool denial when omitting `&`.
    if matches!(agent, "codex" | "grok" | "copilot-cli") {
        "& "
    } else {
        ""
    }
}

fn win_double_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', ""))
}

/// #515: Codex evaluates its Windows hook command with PowerShell, where a
/// quoted path in command position is a string expression rather than an
/// invocation — every hook exited 1 with a ParserError. The call operator
/// fixes that, and must NOT appear for a cmd.exe runner, where `&`
/// separates commands and would break a working integration.
#[test]
fn codex_windows_command_invokes_rather_than_quotes() {
    let cmd = hook_command(
        Path::new("stop.sh"),
        "http://127.0.0.1:49374",
        None,
        HookCommandContext::new(HookCommandPlatform::WindowsNative, "codex", None, None),
    );
    assert!(
        cmd.starts_with("& \""),
        "codex must use the call operator: {cmd}"
    );
}

/// Grok runs its Windows hooks through PowerShell like Codex, so every
/// ai-memory hook failed to parse and captured nothing.
#[test]
fn grok_windows_command_invokes_rather_than_quotes() {
    let cmd = hook_command(
        Path::new("stop.sh"),
        "http://127.0.0.1:49374",
        None,
        HookCommandContext::new(HookCommandPlatform::WindowsNative, "grok", None, None),
    );
    assert!(
        cmd.starts_with("& \""),
        "grok must use the call operator: {cmd}"
    );
}

/// #1462: Copilot CLI evaluates its Windows hooks with PowerShell, where a
/// quoted path in command position causes a ParserError without `&`.
#[test]
fn copilot_cli_windows_command_invokes_rather_than_quotes() {
    let cmd = hook_command(
        Path::new("stop.sh"),
        "http://127.0.0.1:49374",
        None,
        HookCommandContext::new(HookCommandPlatform::WindowsNative, "copilot-cli", None, None),
    );
    assert!(
        cmd.starts_with("& \""),
        "copilot-cli must use the call operator: {cmd}"
    );
}

#[test]
fn claude_code_windows_command_is_unchanged_by_the_codex_fix() {
    let cmd = hook_command(
        Path::new("stop.sh"),
        "http://127.0.0.1:49374",
        None,
        HookCommandContext::new(
            HookCommandPlatform::WindowsNative,
            "claude-code",
            None,
            None,
        ),
    );
    assert!(
        !cmd.starts_with("&"),
        "cmd.exe treats `&` as a separator; claude-code must keep the bare quoted path: {cmd}"
    );
    assert!(cmd.starts_with('"'), "expected a quoted exe path: {cmd}");
}

/// TypeScript runtime shared by every generated integration that must
/// survive an unreachable server (#580): a spool writer emitting the CLI
/// `hook-spool` on-disk contract (same filenames, same `SpoolEntry` JSON,
/// same permissions) plus a self-drain. Callers only need `TOKEN`, `AGENT`,
/// `timeoutSignal`, and the node `join`/`homedir`/fs imports in scope.
/// Runtime credential fallback shared by every generated TypeScript
/// integration. #552 moved the bearer out of generated files into a 0600
/// `<data_dir>/auth-token` file and taught the native hook runtimes to read
/// it back, but the generated TS adapters kept rendering `TOKEN = null`
/// under `--apply` and had no equivalent read-back, so every request went
/// out unauthenticated against a Bearer-enabled server. Generated files
/// call this from `authHeaders()` and the spool writer. Needs `TOKEN`,
/// `join`, `homedir`, `existsSync`, and `readFileSync` (as `readMarkerText`)
/// in scope — the same surface `ts_spool_runtime` already assumes.
pub(crate) fn ts_resolve_token_fn() -> &'static str {
    r#"
// Resolve the server bearer per request. A statically embedded token wins
// (inline installs), then the environment, then the 0600 auth-token file
// that `install-hooks --apply` persists under the data dir (#552).
function resolveToken(): string | null {
  if (TOKEN) return TOKEN;
  const envToken = process.env.AI_MEMORY_AUTH_TOKEN;
  if (envToken && envToken.trim()) return envToken.trim();
  try {
    const dataDir = process.env.AI_MEMORY_DATA_DIR?.trim();
    const base = dataDir
      ? dataDir
      : process.platform === "darwin"
        ? join(homedir(), "Library", "Application Support", "ai-memory")
        : process.platform === "win32"
          ? join(process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local"), "ai-memory")
          : join(process.env.XDG_DATA_HOME?.trim() || join(homedir(), ".local", "share"), "ai-memory");
    const tokenPath = join(base, "auth-token");
    if (existsSync(tokenPath)) {
      const fromFile = readMarkerText(tokenPath, "utf-8").trim();
      if (fromFile) return fromFile;
    }
  } catch {
    // Best effort: an unreadable token file leaves auth absent.
  }
  return null;
}
"#
}

/// `authHeaders()`, shared by every generated TypeScript integration: the
/// headers every request to the server carries. Besides the bearer (resolved
/// per request by `ts_resolve_token_fn`), that is
/// [`ai_memory_hooks::CLIENT_MARKER_HEADER`] naming the integration: an
/// unauthenticated server reached through a non-loopback host name refuses a
/// handoff fetch without it. Needs `AGENT` and `resolveToken` in scope.
pub(crate) fn ts_auth_headers_fn() -> &'static str {
    r#"// Every request names this client: an unauthenticated server reached through
// a non-loopback host name refuses a handoff fetch without X-AI-Memory-Client,
// because a web page cannot add that header to a request it makes.
function authHeaders(): Record<string, string> {
  const headers: Record<string, string> = { "X-AI-Memory-Client": AGENT };
  const token = resolveToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  return headers;
}
"#
}

/// `timeoutSignal`, shared by every generated TypeScript integration: a
/// request deadline composed with `hookAbort`. A host that tears its capture
/// state down aborts `hookAbort` once its final deliveries had their drain
/// budget (OpenCode 2 on each location's unload), so whatever is still in
/// flight fails over to the spool at once instead of each waiting out its own
/// timeout. The module-level integrations never abort it.
pub(crate) fn ts_timeout_signal() -> &'static str {
    r#"const hookAbort = new AbortController();

function timeoutSignal(ms: number): AbortSignal | undefined {
  if (typeof AbortSignal === "undefined") return undefined;
  const factory = (AbortSignal as unknown as { timeout?: (ms: number) => AbortSignal }).timeout;
  const anyFactory = (AbortSignal as unknown as { any?: (signals: AbortSignal[]) => AbortSignal }).any;
  if (!factory) return hookAbort.signal;
  return anyFactory ? anyFactory([hookAbort.signal, factory(ms)]) : factory(ms);
}
"#
}

pub(crate) fn ts_spool_runtime() -> &'static str {
    r#"
// ---- offline spool (#580): the same on-disk contract as `ai-memory hook` ----
// <data_dir>/hook-spool, entries the CLI's hook-drain understands. Written on
// delivery failure, drained here on first use and after each queue flush; the
// ingest_key minted at spool time makes a double drain idempotent server-side.
function hookSpoolDir(): string {
  const env = process.env.AI_MEMORY_DATA_DIR;
  if (env && env.trim()) return join(env, "hook-spool");
  const home = homedir();
  const base =
    process.platform === "darwin"
      ? join(home, "Library", "Application Support", "ai-memory")
      : process.platform === "win32"
        ? join(process.env.LOCALAPPDATA ?? join(home, "AppData", "Local"), "ai-memory")
        : join(process.env.XDG_DATA_HOME?.trim() || join(home, ".local", "share"), "ai-memory");
  return join(base, "hook-spool");
}

// A random prefix per copy of this state keeps two copies in one process
// (OpenCode 2 location instances, OMP and Pi loading each other's extension)
// from renaming onto each other's same-millisecond entry; the counter keeps
// one copy's entries in order.
const spoolSeqPrefix = Math.floor(Math.random() * 0x100000000).toString(16).padStart(8, "0");
let spoolSeq = 0;

function spoolFailedHook(url: URL | string, payload: Record<string, unknown>): void {
  try {
    const dir = hookSpoolDir();
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const createdMs = Date.now();
    // Idempotency key minted ONCE at spool time and baked into the URL,
    // so this plugin's drain and the CLI's drain can both deliver the
    // entry without double-ingesting.
    const key = `ts${createdMs.toString(16)}${Math.floor(Math.random() * 0xffffffff).toString(16)}`;
    const u = new URL(String(url));
    if (!u.searchParams.has("ingest_key")) u.searchParams.set("ingest_key", key);
    const token = resolveToken();
    const entry = {
      url: u.toString(),
      body: JSON.stringify(payload),
      created_ms: createdMs,
      auth_mode: token ? "static" : "none",
      ...(token ? { token } : {}),
      attempts: 0,
    };
    const seq = spoolSeqPrefix + (spoolSeq++ & 0xffffffff).toString(16).padStart(8, "0");
    const name = `${String(createdMs).padStart(13, "0")}-${process.pid}-${seq}.json`;
    const tmp = join(dir, `${name}.tmp`);
    writeFileSync(tmp, JSON.stringify(entry), { mode: 0o600 });
    renameSync(tmp, join(dir, name));
  } catch (_e) {
    // Spooling is best-effort on top of best-effort capture.
  }
}

// A 5xx, or one of the 4xx a saturated or momentarily unwilling server answers
// with (408, 425, 429), asks for a retry: the event is fine and a later attempt
// can land it. Any other 4xx rejects the event for good. The shell, PowerShell
// and native drains classify the same way.
function hookStatusRetryable(status: number): boolean {
  return status >= 500 || status === 408 || status === 425 || status === 429;
}

let spoolDrainPromise: Promise<void> | undefined;

function requestSpoolDrain(): void {
  if (spoolDrainPromise) return;
  spoolDrainPromise = drainHookSpool().finally(() => {
    spoolDrainPromise = undefined;
  });
}

async function drainHookSpool(): Promise<void> {
  let names: string[];
  try {
    names = readdirSync(hookSpoolDir()).filter((n) => n.endsWith(".json"));
  } catch (_e) {
    return; // no spool dir = nothing queued
  }
  names.sort();
  for (const name of names.slice(0, 500)) {
    const file = join(hookSpoolDir(), name);
    let entry: { url?: string; body?: string; token?: string };
    try {
      entry = JSON.parse(readMarkerText(file, "utf8"));
    } catch (_e) {
      continue; // torn or foreign file; leave it for the CLI drain
    }
    if (!entry.url || typeof entry.body !== "string") continue;
    try {
      const headers: Record<string, string> = {
        "Content-Type": "application/json",
        "X-AI-Memory-Client": AGENT,
      };
      if (entry.token) headers["Authorization"] = `Bearer ${entry.token}`;
      const resp = await fetch(entry.url, {
        method: "POST",
        headers,
        body: entry.body,
        signal: timeoutSignal(2000),
      }).catch(() => undefined);
      if (!resp) return; // still unreachable; stop, keep the backlog
      if (resp.ok || (resp.status >= 400 && resp.status < 500 && !hookStatusRetryable(resp.status))) {
        // Delivered, or permanently rejected - either way, done with it.
        try { unlinkSync(file); } catch (_e) {}
      } else {
        return; // 5xx or a transient 4xx: server unhappy; retry a later drain
      }
    } catch (_e) {
      return;
    }
    await new Promise<void>((r) => setTimeout(r, 50));
  }
}

"#
}

/// `Some(())` when this machine can execute the emitted TypeScript.
/// Node is not a build dependency of this project, so a box without it
/// (or on a Node too old for type stripping) skips the runtime evidence
/// instead of failing.
#[cfg(test)]
pub(crate) fn node_strip_types_available() -> Option<()> {
    let probe = std::process::Command::new("node")
        .args(["--experimental-strip-types", "--version"])
        .output();
    match probe {
        Ok(output) if output.status.success() => Some(()),
        _ => {
            eprintln!(
                "skipping Node-required runtime evidence: node lacks --experimental-strip-types"
            );
            None
        }
    }
}

/// Two copies of the spool state in one process (OpenCode 2 location
/// instances, OMP and Pi sharing an extensions dir) must not write the
/// same file name in the same millisecond, and every request deadline
/// must compose with the state's abort signal.
#[cfg(test)]
pub(crate) fn assert_shared_ts_delivery_runtime(name: &str, source: &str) {
    assert!(
        source.contains("const spoolSeqPrefix = Math.floor(Math.random() * 0x100000000)"),
        "{name}: spool names need a per-state random prefix"
    );
    assert!(
        source.contains(
            "const seq = spoolSeqPrefix + (spoolSeq++ & 0xffffffff).toString(16).padStart(8, \"0\");"
        ),
        "{name}: spool seq must stay 16 hex digits"
    );
    assert_eq!(
        source
            .matches("const hookAbort = new AbortController();")
            .count(),
        1,
        "{name}"
    );
    assert_eq!(
        source.matches("function timeoutSignal(").count(),
        1,
        "{name}"
    );
    assert!(
        source.contains("anyFactory([hookAbort.signal, factory(ms)])"),
        "{name}: request deadlines must also honour hookAbort"
    );
    assert!(
        source.contains(
            "return status >= 500 || status === 408 || status === 425 || status === 429;"
        ),
        "{name}: a transient 4xx must stay retryable like a 5xx"
    );
    assert!(
        source.contains("resp.status < 500 && !hookStatusRetryable(resp.status)"),
        "{name}: the drain must keep an entry the server only asked to retry"
    );
    assert_every_ts_request_carries_client_marker(name, source);
}

/// An unauthenticated server reached through a non-loopback host name refuses
/// a handoff fetch without the client marker, so every `fetch` a generated
/// integration makes must send it: through `authHeaders()` (hook POSTs, the
/// handoff GET), `mcpHeaders()` (which spreads `authHeaders()`), or the spool
/// drain's own header table.
#[cfg(test)]
fn assert_every_ts_request_carries_client_marker(name: &str, source: &str) {
    assert_eq!(
        source.matches(ts_auth_headers_fn()).count(),
        1,
        "{name}: exactly one shared authHeaders()"
    );
    assert!(
        source.contains(
            "\"Content-Type\": \"application/json\",\n        \"X-AI-Memory-Client\": AGENT,"
        ),
        "{name}: the spool drain must send the client marker"
    );
    if source.contains("function mcpHeaders(") {
        assert!(
            source.contains("    ...authHeaders(),\n"),
            "{name}: mcpHeaders"
        );
    }
    let mut requests = 0;
    for (at, _) in source.match_indices("fetch(") {
        // Skip identifiers that merely end in `fetch(`, e.g. `prefetch(`.
        if source[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        requests += 1;
        let call: String = source[at..].chars().take(240).collect();
        assert!(
            call.contains("authHeaders()")
                || call.contains("mcpHeaders(")
                || call.contains("        headers,\n"),
            "{name}: a fetch without the client marker:\n{call}"
        );
    }
    assert!(
        requests >= 3,
        "{name}: expected the hook, handoff and drain requests"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(windows)]
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    #[cfg(windows)]
    use std::process::Stdio;

    #[test]
    fn strip_deleted_suffix_removes_only_the_trailing_kernel_marker() {
        assert_eq!(
            strip_deleted_suffix(PathBuf::from("/opt/ai-memory/bin/ai-memory (deleted)")),
            PathBuf::from("/opt/ai-memory/bin/ai-memory")
        );
        // A marker mid-path is a legitimate directory name, not the kernel's
        // artifact; only the trailing suffix reads as one.
        let mid = PathBuf::from("/opt/dir (deleted)/ai-memory");
        assert_eq!(strip_deleted_suffix(mid.clone()), mid);
        let clean = PathBuf::from("/opt/ai-memory/bin/ai-memory");
        assert_eq!(strip_deleted_suffix(clean.clone()), clean);
    }

    #[test]
    fn embedded_exe_strips_the_marker_when_the_stripped_path_exists() {
        // The exact corruption window of `ai-memory upgrade`: the process is
        // still executing the old inode after the rename-over replace, so
        // `current_exe()` carries the marker while the stripped path already
        // holds the freshly installed binary.
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("ai-memory");
        fs::write(&exe, b"binary").expect("write exe");
        let deleted = PathBuf::from(format!("{} (deleted)", exe.display()));
        assert_eq!(embedded_exe_from(Some(deleted)), exe);
    }

    #[test]
    fn embedded_exe_preserves_a_real_file_whose_name_has_the_marker_suffix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let literal = dir.path().join("ai-memory (deleted)");
        let stripped = dir.path().join("ai-memory");
        fs::write(&literal, b"literal binary").expect("write literal exe");
        fs::write(&stripped, b"decoy binary").expect("write stripped decoy");

        assert_eq!(embedded_exe_from(Some(literal.clone())), literal);
    }

    #[test]
    fn embedded_exe_falls_back_when_the_resolved_path_is_missing() {
        // A failed resolution or a dead path must not bake a broken absolute
        // path into hook commands; the bare name resolves through PATH when
        // each hook runs.
        assert_eq!(embedded_exe_from(None), PathBuf::from(FALLBACK_HOOK_EXE));
        assert_eq!(
            embedded_exe_from(Some(PathBuf::from("/nonexistent/ai-memory (deleted)"))),
            PathBuf::from(FALLBACK_HOOK_EXE)
        );
    }

    fn rendered_exec_command(exe: &Path) -> String {
        let spec = windows_native_exec_spec_with_exe(
            exe,
            Path::new("/hooks/post-tool-use.sh"),
            "http://h:49374",
            None,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                None,
                None,
            ),
        );
        let HookHandlerSpec::Exec { command, .. } = spec else {
            panic!("expected exec spec")
        };
        command
    }

    /// The existence probe reports the store binary as present, so only the
    /// store-path guard can produce the bare command: without it the hook
    /// would name a path garbage collection later deletes.
    #[test]
    fn hook_rendered_from_a_nix_store_exe_names_the_bare_command() {
        let store = PathBuf::from("/nix/store/abc123xyz-ai-memory-2.6.3/bin/ai-memory");
        let exe = embedded_exe_with(Some(store.clone()), |_| true);
        assert_eq!(exe, PathBuf::from(FALLBACK_HOOK_EXE));
        assert_eq!(rendered_exec_command(&exe), FALLBACK_HOOK_EXE);
        let deleted = PathBuf::from(format!("{} (deleted)", store.display()));
        assert_eq!(
            embedded_exe_with(Some(deleted), |_| true),
            PathBuf::from(FALLBACK_HOOK_EXE)
        );
    }

    #[test]
    fn hook_rendered_from_a_normal_exe_keeps_the_absolute_path() {
        for normal in [
            "/usr/bin/ai-memory",
            "/home/dev/.nix-profile/bin/ai-memory",
            "/opt/nix/store/bin/ai-memory",
        ] {
            let exe = embedded_exe_with(Some(PathBuf::from(normal)), |_| true);
            assert_eq!(exe, PathBuf::from(normal));
            assert_eq!(rendered_exec_command(&exe), normal);
        }
    }

    #[test]
    fn hook_embedded_exe_path_names_an_existing_file_without_marker() {
        let exe = hook_embedded_exe_path();
        // A test binary built inside a Nix sandbox runs from the store, where
        // the resolver deliberately renders the bare command.
        if std::env::current_exe().is_ok_and(|current| is_nix_store_path(&current)) {
            assert_eq!(exe, PathBuf::from(FALLBACK_HOOK_EXE));
            return;
        }
        assert!(exe.is_file(), "{} should exist", exe.display());
        assert!(!exe.to_string_lossy().ends_with(" (deleted)"));
    }

    fn decode_powershell_encoded_command(command: &str) -> String {
        let (_, encoded) = command
            .split_once(" -EncodedCommand ")
            .expect("missing PowerShell -EncodedCommand payload");
        assert!(
            !encoded.contains(char::is_whitespace),
            "encoded payload must be one command-line token"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("invalid base64 PowerShell program");
        assert_eq!(bytes.len() % 2, 0, "UTF-16LE payload has an odd length");
        let utf16 = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        String::from_utf16(&utf16).expect("invalid UTF-16 PowerShell program")
    }

    #[cfg(windows)]
    fn command_for_available_powershell(command: &str, exe: &str) -> String {
        if exe.eq_ignore_ascii_case("powershell.exe") {
            command.to_owned()
        } else {
            format!(
                "function powershell.exe {{ & {} @args }}; {command}",
                powershell_quote(exe)
            )
        }
    }

    fn build_posix_hook_payload(
        events: &[(&str, &str)],
        root: &Path,
        server_url: &str,
        auth_token: Option<&str>,
        shape: HookShape,
    ) -> serde_json::Value {
        build_hook_payload_for_platform(
            events,
            root,
            server_url,
            auth_token,
            shape,
            HookCommandContext::new(HookCommandPlatform::Posix, "claude-code", None, None),
        )
    }

    #[test]
    fn bearer_header_is_none_when_no_token() {
        assert!(bearer_header_value(None).is_none());
    }

    #[test]
    fn bearer_header_prefixes_with_bearer() {
        let h = bearer_header_value(Some("abc123")).unwrap();
        assert_eq!(h, "Bearer abc123");
    }

    /// Node-required runtime evidence for the exact TypeScript emitted by
    /// `ts_capture_policy_v1`. This deliberately executes the emitted source,
    /// rather than maintaining a JavaScript copy in the test suite. Ignored
    /// because Node is not a build requirement; the Linux CI test job installs
    /// Node and runs it explicitly, so a missing Node fails instead of skipping.
    #[test]
    #[ignore = "needs Node >= 22.6; CI runs it with --ignored"]
    fn generated_capture_policy_v1_node_runtime_evidence() {
        let Some(()) = node_strip_types_available() else {
            return;
        };

        let temp = tempfile::tempdir().unwrap();
        let module = temp.path().join("capture-policy-runtime-evidence.ts");
        let fixture = include_str!("../../../ai-memory-hooks/tests/fixtures/capture-policy.json");
        let fixture = serde_json::to_string(fixture).unwrap();
        let source = format!(
            r#"import {{ closeSync, mkdirSync, opendirSync as realOpendirSync, openSync as realOpenSync, readFileSync as readMarkerText, readSync, readdirSync, statSync, writeFileSync }} from "node:fs";
import {{ basename, dirname, join, resolve }} from "node:path";
import {{ homedir }} from "node:os";

// A marker spelled as a Windows path (`\\?\C:\repo\.ai-memory.toml`) is read from the real file it stands for, so its directory is the Windows one.
const markerAliases = new Map<string, string>();
function openSync(path: string, flags: string) {{ return realOpenSync(markerAliases.get(path) ?? path, flags); }}

// Counts the entries the policy's directory walk reads.
let opendirReads = 0;
function opendirSync(path: string) {{ const dir = realOpendirSync(path); return {{ readSync() {{ opendirReads++; return dir.readSync(); }}, closeSync() {{ dir.closeSync(); }} }}; }}
const fixtureText = {fixture};
const markerRoot = process.argv[2]!;
const markerFixtures = new Map<string, string>();
function findMarker(cwd: string | undefined): string | undefined {{ return cwd ? markerFixtures.get(cwd) : undefined; }}
{policy}

const fixture = JSON.parse(fixtureText);
// Case-sensitive POSIX matching unless a check opts into a macOS-style host.
captureFoldCase = false;
const privatePath = "/PRIVATE_PATH_SENTINEL/item";
const privatePattern = "/PRIVATE_PATTERN_SENTINEL/item";
const privateBody = "PRIVATE_BODY_SENTINEL";
let serial = 0;
function fail(label: string): never {{ throw new Error(`runtime evidence failed: ${{label}}`); }}
function check(ok: unknown, label: string): asserts ok {{ if (!ok) fail(label); }}
function marker(text: string, cwd = join(markerRoot, `fixture-${{serial++}}`)): string {{
  mkdirSync(cwd, {{ recursive: true }});
  const file = join(cwd, ".ai-memory.toml");
  writeFileSync(file, text);
  markerFixtures.set(cwd, file);
  return cwd;
}}
function decision(payload: Record<string, unknown>, cwd: string) {{ return capturePolicy(payload, cwd); }}
function expectDecision(payload: Record<string, unknown>, cwd: string, disposition: string, family: string, extraction: string, count: number, label: string): void {{
  const result = decision(payload, cwd);
  check(result.disposition === disposition, `${{label}} disposition`);
  check(result.protocol?.tool_family === family, `${{label}} family`);
  check(result.protocol?.extraction_state === extraction, `${{label}} extraction`);
  check(result.protocol?.path_count === count, `${{label}} count`);
}}
const active = '[capture]\nignore_paths = ["secret/**"]\n';
for (const vector of fixture.decisions.filter((v: any) => ["open-code", "omp", "pi", "openclaw"].includes(v.agent))) {{
  const cwd = marker(active);
  expectDecision(vector.payload, cwd, vector.disposition, vector.tool_family, vector.extraction_state, vector.path_count, `fixture-${{vector.agent}} ${{JSON.stringify(vector.payload)}}`);
}}
for (const vector of fixture.normalization) {{
  const cwd = marker(`[capture]\nignore_paths = [${{JSON.stringify(vector.pattern)}}]\n`);
  // Every normalization vector here uses an absolute pattern+candidate pair
  // (so neither ever needs the real marker directory as a join base); the
  // fixture's own `cwd` is what decides host flavor. A Windows-shaped `cwd`
  // (e.g. "C:/") can't be a real directory on this (POSIX) test runner —
  // `resolve("C:/")` would mangle it through the real OS path module — so
  // alias the already-written marker file under the fixture's literal `cwd`
  // string and pass that string straight through, exactly like the native
  // test honors `vector["cwd"]` verbatim.
  const hostCwd: string = /^(?:\\\\|\/\/|[A-Za-z]:)/.test(vector.cwd) ? vector.cwd : cwd;
  if (hostCwd !== cwd) markerFixtures.set(hostCwd, markerFixtures.get(cwd)!);
  expectDecision({{ tool: "edit", args: {{ path: vector.candidate }} }}, hostCwd, vector.match ? "drop" : "keep", "file", "extracted", 1, "fixture-normalization");
}}
expectDecision({{ tool: "edit", args: {{ path: "private/item" }} }}, marker('[capture]\nignore_paths = ["private/**"]\n'), "drop", "file", "extracted", 1, "marker-relative");
// Shell parity with the native hook's lexical command matching (#948): the
// same shared vectors `capture_policy.rs` `shell_fixture_vectors` runs.
const shellRoot = marker(`[capture]\nignore_paths = ${{JSON.stringify(fixture.shell.ignore_paths)}}\n`);
for (const vector of fixture.shell.vectors) {{
  const cwd = vector.cwd ? join(shellRoot, vector.cwd) : shellRoot;
  markerFixtures.set(cwd, join(shellRoot, ".ai-memory.toml"));
  const payload = JSON.parse(JSON.stringify(vector.payload ?? {{ tool: "bash", args: {{ command: vector.command }} }}).replaceAll("{{root}}", shellRoot));
  captureFoldCase = vector.fold_case === true;
  expectDecision(payload, cwd, vector.disposition, "non-file", "extracted", 0, `shell-${{vector.disposition}}: ${{JSON.stringify(vector)}}`);
}}
captureFoldCase = false;
// Gemini reads a `read_many_files` directory entry as `entry/**` (shared vectors).
const directoryRoot = marker(`[capture]\nignore_paths = ${{JSON.stringify(fixture.read_many_files_directories.ignore_paths)}}\n`);
for (const vector of fixture.read_many_files_directories.vectors) {{
  expectDecision({{ tool: "read_many_files", args: {{ include: vector.include }} }}, directoryRoot, vector.disposition, "file", "extracted", vector.include.length, `read-many-directory: ${{JSON.stringify(vector)}}`);
}}
// Output readers: stripped under a policy, untouched without one.
const reader = {{ tool: "process", args: {{ action: "log" }}, output: privateBody }};
check(capturePolicy(reader, "/no-marker").disposition === "keep", "reader-inactive-keep");
for (const cwd of [marker(active), marker('[capture')]) {{
  const result = capturePolicy(reader, cwd);
  check(result.disposition === "metadata-only" && result.protocol?.tool_family === "unknown", "reader-metadata-only");
  check(!JSON.stringify(result.payload).includes(privateBody), "reader-redaction");
}}
const bash = (command: string) => ({{ tool: "bash", args: {{ command }} }});
check(capturePolicy(bash("cat docs/adr/x.md"), "/no-marker").disposition === "keep", "shell-inactive-keep");
// A Windows host reads cmd caret and PowerShell backquote escapes out of any
// shell tool; mirrors `windows_escape_characters_are_read_on_a_windows_host`.
const windowsShell = "D:/shell"; markerFixtures.set(windowsShell, markerFixtures.get(marker('[capture]\nignore_paths = ["C:/Secret/**"]\n'))!);
expectDecision(bash("type C:\\Secr^et\\x.md"), windowsShell, "drop", "non-file", "extracted", 0, "windows-host-caret");
expectDecision(bash("Get-Content C:\\Secr`et\\x.md"), windowsShell, "drop", "non-file", "extracted", 0, "windows-host-backquote");
expectDecision(bash("type C:\\Public\\x^.md"), windowsShell, "keep", "non-file", "extracted", 0, "windows-host-public");
// A macOS host folds case for file tools too; mirrors `a_case_folding_host_matches_paths_without_regard_to_case`.
captureFoldCase = true;
expectDecision({{ tool: "edit", args: {{ path: "CASE/item" }} }}, marker('[capture]\nignore_paths = ["case/**"]\n'), "drop", "file", "extracted", 1, "fold-case-file");
expectDecision({{ tool: "edit", args: {{ path: "PUBLIC/item" }} }}, marker('[capture]\nignore_paths = ["case/**"]\n'), "keep", "file", "extracted", 1, "fold-case-file-public");
captureFoldCase = false;
check(capturePolicy(bash("cat docs/{{adr,x}}/1.md"), "/no-marker").disposition === "keep", "shell-expansion-inactive-keep");
const shellExpansion = capturePolicy({{ ...bash("cat docs/{{adr,x}}/1.md"), output: privateBody }}, shellRoot);
check(shellExpansion.disposition === "metadata-only" && !JSON.stringify(shellExpansion.payload).includes(privateBody), "shell-expansion-redaction");
const shellInvalid = capturePolicy({{ ...bash(`cat ${{privatePath}}`), output: privateBody }}, marker('[capture'));
check(shellInvalid.disposition === "metadata-only" && shellInvalid.protocol?.policy_state === "invalid" && shellInvalid.protocol?.tool_family === "non-file", "shell-invalid-metadata-only");
for (const forbidden of [privatePath, privateBody]) check(!JSON.stringify(shellInvalid.payload).includes(forbidden), "shell-invalid-redaction");
expectDecision({{ tool: "web_search", args: {{ query: "docs/adr" }} }}, marker('[capture'), "keep", "non-file", "extracted", 0, "invalid-commandless-non-file-keep");
expectDecision({{ tool: "bash", args: {{ command: 7 }} }}, marker('[capture'), "metadata-only", "non-file", "extracted", 0, "invalid-unparseable-shell-metadata-only");
const argvScript = "echo x; ".repeat(350) + "ls a?.rs";
const manyPatterns = marker(`[capture]\nignore_paths = [${{Array.from({{ length: 40 }}, (_, i) => `"private${{i}}/**"`).join(",")}}]\n`);
expectDecision({{ tool: "shell", args: {{ command: ["bash", "-lc", argvScript] }} }}, manyPatterns, "keep", "non-file", "extracted", 0, "argv-script-not-budget-dropped");
expectDecision(bash(`cat ${{Array(8).fill("docs/x".repeat(400)).join(" ")}}`), marker(`[capture]\nignore_paths = ["${{"docs/**/?".repeat(100)}}"]\n`), "drop", "non-file", "extracted", 0, "shell-budget-fails-closed");
const nestedRoot = join(markerRoot, "nested-repo");
const nestedCwd = join(nestedRoot, "subdir");
mkdirSync(nestedCwd, {{ recursive: true }});
const nestedMarker = join(nestedRoot, ".ai-memory.toml");
writeFileSync(nestedMarker, '[capture]\nignore_paths = ["private/**"]\n');
markerFixtures.set(nestedCwd, nestedMarker);
expectDecision({{ tool: "edit", args: {{ path: "../private/item" }} }}, nestedCwd, "drop", "file", "extracted", 1, "nested-cwd-parent-private");
expectDecision({{ tool: "edit", args: {{ path: "private/item" }} }}, nestedCwd, "keep", "file", "extracted", 1, "nested-cwd-local-public");
// Another checkout's exclusions hold for a call made from elsewhere, file tool or shell; mirrors `another_checkouts_exclusions_hold_for_a_call_made_elsewhere`.
const crossA = join(markerRoot, "cross-a"); const crossB = join(markerRoot, "cross-b"); const crossBare = join(markerRoot, "cross-bare"); const crossBroken = join(markerRoot, "cross-broken");
for (const dir of [crossA, crossB, crossBare, crossBroken]) mkdirSync(dir, {{ recursive: true }});
for (const [dir, text] of [[crossA, '[capture]\nignore_paths = ["secrets.env"]\n'], [crossB, 'project = "b"\n'], [crossBroken, "[capture\n"]]) {{ writeFileSync(join(dir, ".ai-memory.toml"), text); markerFixtures.set(dir, join(dir, ".ai-memory.toml")); }}
expectDecision({{ tool: "read", args: {{ path: "../cross-a/secrets.env" }} }}, crossB, "drop", "file", "extracted", 1, "cross-file-relative");
expectDecision({{ tool: "read", args: {{ path: join(crossA, "secrets.env") }} }}, crossBare, "drop", "file", "extracted", 1, "cross-file-unmarked");
expectDecision(bash("cat ../cross-a/secrets.env"), crossB, "drop", "non-file", "extracted", 0, "cross-shell-relative");
expectDecision(bash(`cat ${{join(crossA, "secrets.env")}}`), crossBare, "drop", "non-file", "extracted", 0, "cross-shell-unmarked");
expectDecision(bash("cat ../cross-a/*.env"), crossB, "drop", "non-file", "extracted", 0, "cross-shell-glob");
expectDecision({{ tool: "bash", args: {{ command: "cat secrets.env", workdir: crossA }} }}, crossB, "drop", "non-file", "extracted", 0, "cross-shell-workdir");
const crossBrokenResult = capturePolicy({{ ...bash("cat ../cross-broken/x.md"), output: privateBody }}, crossB);
check(crossBrokenResult.disposition === "metadata-only" && crossBrokenResult.protocol?.policy_state === "invalid" && !JSON.stringify(crossBrokenResult.payload).includes(privateBody), "cross-broken-metadata-only");
let crossMany = "cat"; for (let i = 0; i <= CAPTURE_MAX_FOREIGN_MARKERS; i++) {{ const dir = join(markerRoot, `cross-m${{i}}`); mkdirSync(dir, {{ recursive: true }}); writeFileSync(join(dir, ".ai-memory.toml"), 'project = "m"\n'); markerFixtures.set(dir, join(dir, ".ai-memory.toml")); crossMany += ` ../cross-m${{i}}/x.md`; }}
check(capturePolicy(bash(crossMany), crossB).disposition === "metadata-only", "cross-too-many-markers");
for (const [payload, cwd, label] of [[bash("cat ../cross-a/public.md README.md"), crossB, "cross-shell-public"], [{{ tool: "read", args: {{ path: "../cross-a/public.md" }} }}, crossB, "cross-file-public"], [bash("cat secrets.env"), crossB, "cross-own-file"], [bash("cat ../cross-m0/x.md ../cross-m1/x.md"), crossB, "cross-few-markers"]] as [Record<string, unknown>, string, string][]) check(capturePolicy(payload, cwd).disposition === "keep", label);
// Globs in directory segments, search/list tools, patch headers and unreadable targets reach another checkout's policy; mirrors `globs_searches_and_unreadable_targets_answer_to_another_checkout`.
const globParent = join(markerRoot, "glob-parent"); const globA = join(globParent, "a"); const globB = join(globParent, "b");
for (const dir of [globA, join(globB, "src")]) mkdirSync(dir, {{ recursive: true }});
writeFileSync(join(globA, ".ai-memory.toml"), '[capture]\nignore_paths = ["secrets.env"]\n'); markerFixtures.set(globA, join(globA, ".ai-memory.toml"));
writeFileSync(join(globB, ".ai-memory.toml"), 'project = "b"\n'); markerFixtures.set(globB, join(globB, ".ai-memory.toml"));
for (const [payload, cwd, label] of [[bash("cat ../*/secrets.env"), globB, "glob-sibling"], [bash("cat */secrets.env"), globParent, "glob-parent"], [bash("cat ../{{a,c}}/secrets.env"), globB, "glob-brace"], [bash("cat ../[a]/secrets.env"), globB, "glob-bracket"], [bash("cat ../**/secrets.env"), globB, "glob-globstar"], [bash("cat ../{{a/x,c}}/secrets.env"), globB, "glob-spanning"], [{{ tool: "grep", args: {{ pattern: "KEY", path: "../a/secrets.env" }} }}, globB, "search-path"], [{{ tool: "glob", args: {{ pattern: "../a/*.env" }} }}, globB, "search-pattern"], [{{ tool: "grep", args: {{ pattern: "KEY", include: "*/secrets.env" }} }}, globParent, "search-filter"], [{{ tool: "apply_patch", args: {{ patchText: "*** Begin Patch\n*** Update File: ../a/secrets.env\n*** End Patch" }} }}, globB, "patch-header"], [{{ tool: "read", args: {{}} }}, globParent, "unreadable-target"]] as [Record<string, unknown>, string, string][]) {{ const result = capturePolicy({{ ...payload, output: privateBody }}, cwd); check(result.disposition === "drop" || (result.disposition === "metadata-only" && !JSON.stringify(result.payload).includes(privateBody)), label); }}
for (const [payload, cwd, label] of [[bash("cat ../*/public.md"), globB, "glob-public"], [bash("ls src/*"), globB, "glob-own"], [{{ tool: "grep", args: {{ pattern: "KEY", path: "src" }} }}, globB, "search-own"], [{{ tool: "glob", args: {{ pattern: "**/*.ts" }} }}, globB, "search-own-globstar"], [{{ tool: "grep", args: {{ pattern: "KEY", path: "b" }} }}, globParent, "search-public"], [{{ tool: "apply_patch", args: {{ patchText: "*** Begin Patch\n*** Add File: src/x.ts\n*** End Patch" }} }}, globB, "patch-own"]] as [Record<string, unknown>, string, string][]) check(capturePolicy(payload, cwd).disposition === "keep", label);
// A glob rooted inside the event's own checkout is not walked, so a monorepo with more matching directories than the walk's bounds keeps its commands; the sibling attack still drops. Mirrors `a_monorepo_glob_inside_the_own_checkout_is_not_walked`.
const mono = join(markerRoot, "mono-work", "mono"); const monoA = join(markerRoot, "mono-work", "a");
for (const dir of [join(mono, ".git"), join(mono, "log"), monoA]) mkdirSync(dir, {{ recursive: true }});
for (let i = 0; i < 300; i++) {{ mkdirSync(join(mono, "packages", `p${{i}}`, "src"), {{ recursive: true }}); writeFileSync(join(mono, "packages", `p${{i}}`, "package.json"), "{{}}"); mkdirSync(join(mono, "node_modules", `m${{i}}`), {{ recursive: true }}); }}
writeFileSync(join(monoA, ".ai-memory.toml"), '[capture]\nignore_paths = ["secrets.env"]\n'); markerFixtures.set(monoA, join(monoA, ".ai-memory.toml"));
for (const policy of [undefined, '[capture]\nignore_paths = ["private/**"]\n']) {{
  if (policy) {{ writeFileSync(join(mono, ".ai-memory.toml"), policy); markerFixtures.set(mono, join(mono, ".ai-memory.toml")); }}
  for (const command of ["cat packages/*/package.json", "ls packages/*/src", "du -sh packages/*", "ls node_modules/*/package.json", "for f in packages/*/package.json; do echo $f; done", "tail -n 50 log/*.log"]) check(capturePolicy(bash(command), mono).disposition === "keep", `monorepo-shell ${{!!policy}}: ${{command}}`);
  for (const payload of [{{ tool: "glob", args: {{ pattern: "packages/*/package.json" }} }}, {{ tool: "grep", args: {{ pattern: "x", path: "packages", glob: "*/package.json" }} }}]) check(capturePolicy(payload, mono).disposition === (policy ? "drop" : "keep"), `monorepo-search ${{!!policy}}: ${{JSON.stringify(payload)}}`);
  for (const command of ["cat ../*/secrets.env", "cat ../a/*.env"]) check(capturePolicy(bash(command), mono).disposition === "drop", `monorepo-sibling ${{!!policy}}: ${{command}}`);
}}
// A walk outside the checkout stops at its directory bound (counted as a "**" lists) and its time budget; mirrors `a_glob_walk_outside_the_checkout_stops_at_its_bounds`.
const budgetRoot = join(markerRoot, "budget"); mkdirSync(join(budgetRoot, "x", "y"), {{ recursive: true }}); writeFileSync(join(budgetRoot, "x", ".ai-memory.toml"), "");
const fastBudget = (): CaptureGlobBudget => ({{ dirs: 0, entries: 0, deadline: 50, now: () => 0 }});
check(captureMarkersBelow(budgetRoot, ["**"], undefined, fastBudget())?.length === 1, "walk-in-time");
check(captureMarkersBelow(budgetRoot, ["**"], budgetRoot, fastBudget())?.length === 0, "walk-inside-checkout");
check(captureMarkersBelow(budgetRoot, ["**"], undefined, {{ dirs: 0, entries: 0, deadline: 50, now: () => 51 }}) === undefined, "walk-past-time");
const deepRoot = join(markerRoot, "deep"); for (let o = 0; o < 64; o++) for (let i = 0; i < 64; i++) mkdirSync(join(deepRoot, `d${{o}}`, `e${{i}}`), {{ recursive: true }});
const deepBudget = fastBudget(); check(captureMarkersBelow(deepRoot, ["**"], undefined, deepBudget) === undefined && deepBudget.entries < 1024, "walk-stops-early");
// Syntax that builds a step out of the checkout reaches the other checkout's policy; mirrors `constructed_parent_steps_answer_to_another_checkout`.
const conWork = join(markerRoot, "con-work"); const conA = join(conWork, "a"); const conB = join(conWork, "b");
for (const dir of [conA, join(conB, ".git"), join(conB, "sub", "x")]) mkdirSync(dir, {{ recursive: true }});
writeFileSync(join(conA, ".ai-memory.toml"), '[capture]\nignore_paths = ["secrets.env"]\n'); markerFixtures.set(conA, join(conA, ".ai-memory.toml"));
for (const [payload, label] of [[bash("cat ./**/../a/secrets.env"), "con-globstar"], [{{ tool: "glob", args: {{ pattern: "./**/../a/secrets.env" }} }}, "con-globstar-tool"], [{{ tool: "grep", args: {{ pattern: "KEY", glob: "**/../a/secrets.env" }} }}, "con-globstar-filter"], [bash("cat .{{.,x}}/a/secrets.env"), "con-brace-dot"], [bash("cat {{.,}}./a/secrets.env"), "con-brace-empty"], [bash("cat .{{.,.}}/a/secrets.env"), "con-brace-twice"], [bash("cat .{{-..0}}/a/secrets.env"), "con-brace-sequence"], [bash("cat ../(a)/secrets.env"), "con-group"], [bash("cat ../(a|c)/secrets.env"), "con-group-alternation"], [bash("cat ..$'\\x2f'a/secrets.env"), "con-ansi-hex"], [bash("cat $'\\056\\056'/a/secrets.env"), "con-ansi-octal"], [bash("cat .*/a/secrets.env"), "con-dot-star"], [bash("cat ./.?/a/secrets.env"), "con-dot-question"], [{{ tool: "apply_patch", args: {{ patchText: "*** Begin Patch\n*** Add File: sub/y.rs\n+y\n  *** Update File: ../a/secrets.env\n+KEY\n*** End Patch" }} }}, "con-indented-patch"], [{{ tool: "grep", args: {{ pattern: "KEY", path: "..", file_glob: "a/*.env" }} }}, "con-file-glob"]] as [Record<string, unknown>, string][]) {{ const result = capturePolicy({{ ...payload, output: privateBody }}, conB); check(result.disposition === "drop" || (result.disposition === "metadata-only" && !JSON.stringify(result.payload).includes(privateBody)), label); }}
for (const [payload, label] of [[bash("cat sub/**/../README.md"), "con-own-globstar"], [bash("cat {{sub,lib}}/README.md"), "con-own-brace"], [bash("cat v{{1..3}}/README.md"), "con-own-sequence"], [bash("(cd sub && ls)"), "con-subshell"], [bash("ls .*"), "con-dot-list"], [bash("cat .config/x"), "con-dot-directory"], [bash("cat .github/*.yml"), "con-dot-glob"], [bash("cat sub$'\\x2f'x/README.md"), "con-own-ansi"], [{{ tool: "apply_patch", args: {{ patchText: "*** Begin Patch\n  *** Add File: sub/x.rs\n+fn x() {{}}\n*** End Patch" }} }}, "con-own-patch"], [{{ tool: "grep", args: {{ pattern: "KEY", path: "sub", file_glob: "x/*.rs" }} }}, "con-own-file-glob"]] as [Record<string, unknown>, string][]) check(capturePolicy(payload, conB).disposition === "keep", label);
// Double-quoted scripts, POSIX classes, extglobs, brace-built dot-globs and oversized expansions that could leave reach checkout A's policy; an oversized expansion that cannot leave is kept (v2.6.3 kept it). Mirrors the additions to `constructed_parent_steps_answer_to_another_checkout`.
for (const [command, label] of [["bash -c \"cat \\$'\\\\x2e\\\\x2e'/a/secrets.env\"", "con-dq-ansi"], ["eval \"cat \\$'\\\\x2e\\\\x2e'/a/secrets.env\"", "con-dq-eval"], ["cat .[[:punct:]]/a/secrets.env", "con-posix-class"], ["cat .@(.)/a/secrets.env", "con-extglob-at"], ["cat .?(.)/a/secrets.env", "con-extglob-optional"], ["cat .*(.)/a/secrets.env", "con-extglob-star"], ["cat .+(.)/a/secrets.env", "con-extglob-plus"], ["cat .{{*,x}}/a/secrets.env", "con-brace-dot-glob"], ["cat .{{-..~}}/a/secrets.env", "con-oversized-leaves"]]) check(capturePolicy(bash(command), conB).disposition !== "keep", label);
for (const [command, label] of [["bash -c \"echo \\$HOME && cat ../a/README.md\"", "con-dq-own"], ["ls .[[:alpha:]]*", "con-posix-own"], ["cat .[[:alpha:]]x/README.md", "con-posix-own-directory"], ["touch test/file{{1..200}}.txt", "con-oversized"], ["mkdir -p build/{{1..100}}/out", "con-oversized-directory"], ["rm -f logs/app.{{1..99}}.log", "con-oversized-dotted"], ["cat data/{{a..h}}{{1..9}}.csv", "con-oversized-product"]]) check(capturePolicy(bash(command), conB).disposition === "keep", label);
// A "**" above the checkout skips the own subtree; mirrors `a_globstar_above_the_checkout_skips_the_own_subtree`.
check(captureMarkersBelow(join(markerRoot, "mono-work"), ["**"], mono, fastBudget())?.includes(join(monoA, ".ai-memory.toml")) === true, "globstar-skips-own-subtree");
check(captureMarkersBelow(join(markerRoot, "mono-work"), ["**"], undefined, fastBudget()) === undefined, "globstar-own-subtree-exceeds-bound");
// Deep trees look each directory up once and share the budget; the other checkout's location comes first. Mirrors `marker_lookups_in_a_deep_tree_cost_one_probe_per_directory`.
const deepLevels = 300; let deepDir = conB; const deepWords: string[] = []; for (let level = 1; level <= deepLevels; level++) {{ deepDir = join(deepDir, "d"); deepWords.push("d/".repeat(level) + "f"); }} mkdirSync(deepDir, {{ recursive: true }});
check(capturePolicy(bash(`cat ${{deepWords.join(" ")}}`), conB).disposition === "keep", "deep-inside-kept");
// A clock that advances one millisecond per reading runs out within the deep list: inside the own checkout that keeps the call, and the other checkout's location is looked up first.
let readings = 0; captureNow = () => {{ readings++; return 0; }};
try {{ check(capturePolicy(bash(`cat ${{deepWords.join(" ")}}`), conB).disposition === "keep" && readings < 5 * deepLevels, `deep-one-probe-per-directory ${{readings}}`); }} finally {{ captureNow = Date.now; }}
let ticks = 0; captureNow = () => ticks++;
try {{ check(capturePolicy(bash(`cat ${{deepWords.join(" ")}}`), conB).disposition === "keep", "deep-out-of-budget-inside-kept"); check(capturePolicy(bash(`cat ${{deepWords.join(" ")}} ../a/secrets.env`), conB).disposition === "drop", "deep-attack-first"); mkdirSync(join(conB, "vendor"), {{ recursive: true }}); writeFileSync(join(conB, "vendor", ".ai-memory.toml"), '[capture]\nignore_paths = ["secrets.env"]\n'); ticks = 0; check(capturePolicy(bash(`cat ${{deepWords.join(" ")}} vendor/secrets.env`), conB).disposition === "drop", "deep-nested-shallow-first"); check(capturePolicy(bash(`cat ${{join(markerRoot, "elsewhere", "d".repeat(1), ...deepWords.slice(0, 120).map(() => "d"))}}/f`), conB).disposition === "metadata-only", "deep-out-of-budget-outside-unprovable"); }} finally {{ captureNow = Date.now; }}
const deepMemo: CaptureMarkerMemo = {{ governing: new Map(), checkouts: new Map() }}; const deepBudget2: CaptureGlobBudget = {{ dirs: 0, entries: 0, deadline: 50, now: () => 0, probes: 0 }}; let lookupDir = deepDir; for (let level = 0; level < deepLevels; level++, lookupDir = dirname(lookupDir)) check(captureMemoMarker(lookupDir, deepMemo, deepBudget2) === null, "deep-memo-answer");
check((deepBudget2.probes ?? 0) < 3 * deepLevels, `deep-memo-probes ${{deepBudget2.probes}}`);
check(captureMemoMarker(deepDir, {{ governing: new Map(), checkouts: new Map() }}, {{ dirs: 0, entries: 0, deadline: 50, now: () => 51, probes: 0 }}) === undefined, "deep-memo-past-budget");
// A `.git` at home does not make every checkout under it the own one; mirrors `a_dotfiles_checkout_at_home_is_not_the_own_checkout`.
const realHome = process.env.HOME; const dotHome = join(markerRoot, "dot-home"); const dotProjects = join(dotHome, "Projects"); for (const dir of [join(dotHome, ".git"), join(dotProjects, "a"), join(dotProjects, "b")]) mkdirSync(dir, {{ recursive: true }}); writeFileSync(join(dotProjects, "a", ".ai-memory.toml"), '[capture]\nignore_paths = ["secrets.env"]\n');
process.env.HOME = dotHome;
try {{ check(capturePolicy(bash("cat */secrets.env"), dotProjects).disposition === "drop", "dotfiles-home-glob"); check(capturePolicy(bash(`cat ${{dotProjects}}/*/secrets.env`), dotProjects).disposition === "drop", "dotfiles-home-absolute"); check(!captureOwnCheckout(dotHome), "dotfiles-home-not-own"); }} finally {{ process.env.HOME = realHome; }}
// A macOS host folds case beyond ASCII and composes Unicode; mirrors `a_case_folding_host_matches_paths_without_regard_to_case`.
captureFoldCase = true;
for (const path of ["\u00e9crits/x.md", "\u00c9CRITS/x.md", "E\u0301crits/x.md"]) expectDecision({{ tool: "edit", args: {{ path }} }}, marker('[capture]\nignore_paths = ["\u00c9crits/**"]\n'), "drop", "file", "extracted", 1, `fold-unicode ${{JSON.stringify(path)}}`);
expectDecision({{ tool: "edit", args: {{ path: "ecrits/x.md" }} }}, marker('[capture]\nignore_paths = ["\u00c9crits/**"]\n'), "keep", "file", "extracted", 1, "fold-unicode-control");
captureFoldCase = false;
expectDecision({{ tool: "edit", args: {{ path: `${{homedir()}}/home-private/item` }} }}, marker('[capture]\nignore_paths = ["~/home-private/**"]\n'), "drop", "file", "extracted", 1, "home-expansion");
expectDecision({{ tool: "edit", args: {{ path: "case/item" }} }}, marker('[capture]\nignore_paths = ["Case/**"]\n'), "keep", "file", "extracted", 1, "posix-case");
expectDecision({{ tool: "edit", args: {{ path: "c:/SECRET/item" }} }}, marker('[capture]\nignore_paths = ["C:/secret/**"]\n'), "drop", "file", "extracted", 1, "windows-drive-case");
const uncCwd = "C:/"; markerFixtures.set(uncCwd, markerFixtures.get(marker(`[capture]\nignore_paths = ['${{String.raw`\\server\share/**`}}']\n`))!);
expectDecision({{ tool: "edit", args: {{ path: "//SERVER/SHARE/item" }} }}, uncCwd, "drop", "file", "extracted", 1, "windows-unc-case");
expectDecision({{ tool: "edit", args: {{ path: "x" }} }}, marker('[capture'), "metadata-only", "file", "extracted", 1, "malformed-table");
const malformedQuoteCwd = marker('[capture]\nignore_paths = ["private/**');
expectDecision({{ tool: "edit", args: {{ path: privatePath }} }}, malformedQuoteCwd, "metadata-only", "file", "extracted", 1, "malformed-unterminated-quote");
const badUtf8Cwd = `/fixture/${{serial++}}`; const badUtf8 = join(markerRoot, "bad-utf8.toml"); writeFileSync(badUtf8, Buffer.from([0xff])); markerFixtures.set(badUtf8Cwd, badUtf8);
expectDecision({{ tool: "edit", args: {{ path: "x" }} }}, badUtf8Cwd, "metadata-only", "file", "extracted", 1, "invalid-utf8");
expectDecision({{ tool: "edit", args: {{ path: "x" }} }}, marker(`[capture]\nignore_paths = ["${{"x".repeat(65537)}}"]`), "metadata-only", "file", "extracted", 1, "large-marker");
expectDecision({{ tool: "edit", args: {{ path: "literal#item" }} }}, marker("[capture]\nignore_paths = [\n  'literal#item',\n  \"basic#item\"\n]\n"), "drop", "file", "extracted", 1, "multiline-and-hash");
expectDecision({{ tool: "edit", args: {{ path: "C:relative" }} }}, marker(active), "metadata-only", "file", "extracted", 1, "drive-relative");
expectDecision({{ tool: "future-tool", args: {{ path: "secret/a" }} }}, marker(active), "keep", "unknown", "extracted", 0, "unknown-tool");
// Gemini `read_many_files` glob syntax the matcher cannot evaluate fails closed;
// an empty glob list proves nothing.
expectDecision({{ tool: "read_many_files", args: {{ include: ["public/a", "secret/[ab]"] }} }}, marker(active), "metadata-only", "file", "extracted", 2, "read-many-unsupported-glob");
expectDecision({{ tool: "read_many_files", args: {{ include: ["src/**/*.rs", "secret/{{a,b}}"] }} }}, marker(active), "metadata-only", "file", "extracted", 2, "read-many-brace-glob");
expectDecision({{ tool: "read_many_files", args: {{ include: [] }} }}, marker(active), "metadata-only", "file", "missing-or-malformed", 0, "read-many-empty");
expectDecision({{ tool: "read_many_files", args: {{ include: ["**/*.md"] }} }}, marker('[capture]\nignore_paths = ["docs/adr/**"]\n'), "drop", "file", "extracted", 1, "read-many-globstar");
const adversarial = "a".repeat(4090);
expectDecision({{ tool: "multi_edit", args: {{ edits: [{{ path: "secret/a" }}, {{ path: adversarial }}] }} }}, marker('[capture]\nignore_paths = ["secret/**", "*'.concat("a".repeat(1022), '"]\n')), "drop", "file", "extracted", 2, "first-drop-beats-budget");

const requests: string[] = []; const queue: Record<string, unknown>[] = [];
function emit(payload: Record<string, unknown>, cwd: string): void {{ const result = capturePolicy(payload, cwd); if (result.disposition === "drop") return; queue.push(result.payload); requests.push(JSON.stringify(result.payload)); }}
const gatedCwd = marker(`[capture]\nignore_paths = [${{JSON.stringify(privatePattern)}}]\n`);
emit({{ tool: "edit", args: {{ path: "/PRIVATE_PATTERN_SENTINEL/item", nested: {{ body: privateBody }} }}, output: privateBody, error: privateBody }}, gatedCwd);
emit({{ tool: "bash", args: {{ command: "cat /PRIVATE_PATTERN_SENTINEL/item" }}, output: privateBody }}, gatedCwd);
check(queue.length === 0 && requests.length === 0, "drop-gates-queue-and-fetch");
emit({{ tool: "edit", args: {{ path: privatePath, nested: {{ body: privateBody }} }}, output: privateBody, error: privateBody }}, malformedQuoteCwd);
const malformedRequest = requests.at(-1) ?? "";
const malformedQueued = JSON.stringify(queue.at(-1));
check(JSON.parse(malformedRequest)._ai_memory_capture?.policy_state === "invalid", "malformed-quote-invalid-protocol");
const invalidSearch = capturePolicy({{ tool: "grep", args: {{ pattern: "x" }} }}, malformedQuoteCwd);
check(invalidSearch.protocol?.policy_state === "invalid" && invalidSearch.protocol.extraction_state === "extracted", "invalid-search-extraction-parity");
for (const forbidden of [privatePath, privateBody]) check(!malformedQueued.includes(forbidden) && !malformedRequest.includes(forbidden), "malformed-quote-redaction");
emit({{ tool: "edit", args: {{ path: "C:relative", nested: {{ path: privatePath, body: privateBody }} }}, output: privateBody, error: privateBody, sessionID: "session-safe", cwd: "/cwd-safe", tool_call_id: "call-safe" }}, gatedCwd);
const metadata = requests.at(-1) ?? "";
const metadataBody = JSON.parse(metadata);
check(metadataBody.session_id === "session-safe", "metadata-request-session");
check(metadataBody.cwd === "/cwd-safe", "metadata-request-cwd");
check(metadataBody.tool_family === "file", "metadata-request-family");
check(metadataBody.tool_name === "file", "metadata-request-name");
check(metadataBody.tool_call_id === "call-safe", "metadata-request-call");
check(metadataBody._ai_memory_capture?.disposition === "metadata-only" && metadataBody._ai_memory_capture?.policy_state === "active", "metadata-request-protocol");
for (const forbidden of [privatePath, privatePattern, privateBody]) check(!metadata.includes(forbidden), "metadata-request-redaction");
const inactivePayload = {{ tool: "edit", args: {{ path: privatePath }}, output: privateBody }};
const inactive = capturePolicy(inactivePayload, "/no-marker");
check(inactive.disposition === "keep" && inactive.payload === inactivePayload && !inactive.protocol, "inactive-preserves-object");
const activeSearch = capturePolicy({{ tool: "grep", args: {{ pattern: "x" }} }}, gatedCwd);
check(activeSearch.protocol?.policy_state === "active" && activeSearch.protocol.extraction_state === "not-applicable", "active-search-not-applicable");
const activeKeep = capturePolicy({{ tool: "bash", args: {{ command: privateBody }} }}, gatedCwd);
check(activeKeep.disposition === "keep" && activeKeep.protocol?.version === 1 && activeKeep.protocol.policy_state === "active", "active-keep-adds-protocol");
// Windows spellings of an ignored file, and the POSIX sessions they must not touch (shared vectors; mirrors `windows_spelling_fixture_vectors`).
for (const section of [fixture.windows, fixture.windows_posix_controls]) {{
  markerFixtures.set(section.cwd, markerFixtures.get(marker(`[capture]\nignore_paths = ${{JSON.stringify(section.ignore_paths)}}\n`))!);
  for (const vector of section.files) {{ const result = capturePolicy({{ tool: "edit", args: {{ path: vector.path }} }}, section.cwd); check(result.disposition === vector.disposition && result.protocol?.tool_family === "file", `windows-file ${{section.cwd}} ${{JSON.stringify(vector)}}: ${{result.disposition}}`); }}
  for (const vector of section.shell ?? []) {{ const result = capturePolicy(vector.payload ?? bash(vector.command), section.cwd); check(result.disposition === vector.disposition, `windows-shell ${{JSON.stringify(vector)}}: ${{result.disposition}}`); }}
}}
// A Windows session whose cwd, marker directory or pattern is spelled with a device prefix, an administrative share or a UNC path; PowerShell's literal backslashes; both readings of `/a/x` (shared vectors; mirrors `windows_spelling_fixture_vectors`). The marker sits in the cwd itself, so relative patterns resolve from its Windows spelling.
for (const section of fixture.windows_scopes) {{
  const markerPath = `${{section.cwd}}\\.ai-memory.toml`;
  markerAliases.set(markerPath, markerFixtures.get(marker(`[capture]\nignore_paths = ${{JSON.stringify(section.ignore_paths)}}\n`))!);
  markerFixtures.set(section.cwd, markerPath);
  for (const vector of section.files) {{ const result = capturePolicy({{ tool: "edit", args: {{ path: vector.path }} }}, section.cwd); check(result.disposition === vector.disposition && result.protocol?.tool_family === "file", `windows-file ${{section.cwd}} ${{JSON.stringify(vector)}}: ${{result.disposition}}`); }}
  for (const vector of section.shell ?? []) {{ const result = capturePolicy(vector.payload ?? bash(vector.command), section.cwd); check(result.disposition === vector.disposition, `windows-shell ${{JSON.stringify(vector)}}: ${{result.disposition}}`); }}
}}
// Every TOML spelling of the capture table the native parser honours, and the ones it rejects (shared vectors; mirrors `capture_section_spellings_match_toml_semantics`).
for (const text of fixture.marker_spellings.honoured) check(capturePolicy({{ tool: "edit", args: {{ path: "secret/x" }} }}, marker(text)).disposition === "drop", `marker-honoured ${{JSON.stringify(text)}}`);
for (const text of fixture.marker_spellings.rejected) {{ const result = capturePolicy({{ tool: "edit", args: {{ path: "public/x" }} }}, marker(text)); check(result.disposition === "metadata-only" && result.protocol?.policy_state === "invalid", `marker-rejected ${{JSON.stringify(text)}}`); }}
for (const text of fixture.marker_spellings.unrelated) check(capturePolicy({{ tool: "edit", args: {{ path: "secret/x" }} }}, marker(text)).disposition === "keep", `marker-unrelated ${{JSON.stringify(text)}}`);
// The tool names each family covers come from the native constants (shared vectors; mirrors `tool_family_fixture_vectors`).
for (const vector of fixture.tool_families) check(captureTool({{ tool: vector.name, args: {{}} }}).family === vector.family, `tool-family ${{vector.name}}`);
// The harness-spool check is linear in the path and charged to the match budget; mirrors `output_spool_work_is_linear_and_budgeted`.
const spoolPath = `/tmp/${{"*/".repeat(2040)}}x`; const spoolWork = {{ work: 0 }};
check(captureOutputSpool(spoolPath, spoolWork) === false && spoolWork.work <= 64 * spoolPath.length, `spool-linear ${{spoolWork.work}}`);
check(captureOutputSpool("/home/u/.claude/projects/p/s/tool-results/a.txt", {{ work: 0 }}) === true, "spool-still-found");
check(captureOutputSpool(spoolPath, {{ work: CAPTURE_MAX_WORK }}) === undefined, "spool-past-budget-unproven");
const spoolRoot = marker('[capture]\nignore_paths = ["/s"]\n');
const spoolPaths = Array.from({{ length: 32 }}, (_, i) => `/tmp/${{i}}/${{"*/".repeat(2000)}}x`);
check(capturePolicy({{ tool: "read", args: {{ paths: spoolPaths }} }}, spoolRoot).disposition === "metadata-only", "spool-budget-fails-closed");
check(capturePolicy({{ tool: "read", args: {{ paths: spoolPaths.slice(0, 1) }} }}, spoolRoot).disposition === "keep", "spool-one-path-keep");
// A command longer than the matcher splits is unprovable; mirrors `an_oversized_shell_command_is_unprovable`.
const atLimit = `cat public.md${{" ".repeat(CAPTURE_MAX_COMMAND_CHARS - 13)}}`;
check(capturePolicy(bash(atLimit), shellRoot).disposition === "keep", "command-at-limit-keep");
check(capturePolicy(bash(`${{atLimit}} `), shellRoot).disposition === "metadata-only", "command-oversized-metadata-only");
check(capturePolicy({{ tool: "shell", args: {{ command: ["bash", "-lc", atLimit] }} }}, shellRoot).disposition === "metadata-only", "argv-oversized-metadata-only");
check(capturePolicy(bash(`${{atLimit}} `), "/no-marker").disposition === "metadata-only", "command-oversized-lookup-unprovable");
// Inline program source (Hermes `execute_code`) is metadata-only under a policy, like `python3 -c`.
const inlineCode = {{ tool: "execute_code", args: {{ code: "print(open('secret/a.txt').read())" }}, output: privateBody }};
check(capturePolicy(inlineCode, "/no-marker").disposition === "keep", "inline-code-inactive-keep");
const inlineResult = capturePolicy(inlineCode, marker(active));
check(inlineResult.disposition === "metadata-only" && inlineResult.protocol?.tool_family === "non-file" && !JSON.stringify(inlineResult.payload).includes(privateBody), "inline-code-metadata-only");
// A directory walk reads one entry at a time and stops at the entry bound instead of listing the whole directory first.
const wide = join(markerRoot, "wide"); for (let i = 0; i < 100; i++) mkdirSync(join(wide, `d${{i}}`), {{ recursive: true }});
opendirReads = 0;
check(captureMarkersBelow(wide, ["*"], undefined, {{ dirs: 0, entries: CAPTURE_MAX_GLOB_ENTRIES - 5, deadline: 50, now: () => 0 }}) === undefined && opendirReads > 0 && opendirReads <= 6, `walk-reads-to-the-entry-bound ${{opendirReads}}`);
// #992: only the native hook routes `server` profiles; these integrations
// must drop rather than deliver a routed repository to the install default,
// including under a nested marker that does not repeat the key.
check(capturePolicy(bash("echo hi"), marker('server = "team-b"\n')).disposition === "drop", "server-routed-drops");
check(capturePolicy(bash("echo hi"), marker("server = team-b\n")).disposition === "drop", "server-routed-bare-drops");
check(capturePolicy(bash("echo hi"), marker('servers = "x"\n')).disposition === "keep", "servers-key-is-not-server");
const routedRoot = join(markerRoot, "routed-repo");
const routedChild = join(routedRoot, "child");
mkdirSync(join(routedRoot, ".git"), {{ recursive: true }});
mkdirSync(routedChild, {{ recursive: true }});
writeFileSync(join(routedRoot, ".ai-memory.toml"), 'server = "team-b"\n');
writeFileSync(join(routedChild, ".ai-memory.toml"), 'workspace = "child"\n');
markerFixtures.set(routedChild, join(routedChild, ".ai-memory.toml"));
check(capturePolicy(bash("echo hi"), routedChild).disposition === "drop", "server-routed-inherited-drops");
"#,
            fixture = fixture,
            policy = ts_capture_policy_v1("denylist"),
        );
        fs::write(&module, source).unwrap();
        let output = Command::new("node")
            .args([
                "--experimental-strip-types",
                module.to_str().unwrap(),
                temp.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Node runtime evidence failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for sentinel in [
            "PRIVATE_PATH_SENTINEL",
            "PRIVATE_PATTERN_SENTINEL",
            "PRIVATE_BODY_SENTINEL",
        ] {
            assert!(
                !diagnostics.contains(sentinel),
                "Node diagnostics leaked a private sentinel"
            );
        }

        // #661: under `--capture-mode allowlist`, a repository with no marker
        // must emit nothing at all — for a prompt/session event as much as a
        // file event, since `inspect` (and its `search-list`/`file` families)
        // is never reached for those event kinds. A repository WITH a marker
        // but an empty `[capture]` section must still be admitted: `state`
        // is "inactive" in both the no-marker and marker-with-no-section
        // cases, so the gate must key on marker *presence*, never on
        // `config.state`, or this second case would be wrongly dropped.
        let allowlist_module = temp.path().join("capture-policy-allowlist-evidence.ts");
        let allowlist_source = format!(
            r#"import {{ closeSync, mkdirSync, opendirSync, openSync, readFileSync as readMarkerText, readSync, readdirSync, statSync, writeFileSync }} from "node:fs";
import {{ basename, dirname, join, resolve }} from "node:path";
import {{ homedir }} from "node:os";

const markerRoot = process.argv[2]!;
const markerFixtures = new Map<string, string>();
function findMarker(cwd: string | undefined): string | undefined {{ return cwd ? markerFixtures.get(cwd) : undefined; }}
{policy}

function fail(label: string): never {{ throw new Error(`allowlist runtime evidence failed: ${{label}}`); }}
function check(ok: unknown, label: string): asserts ok {{ if (!ok) fail(label); }}

const promptDrop = capturePolicy({{ prompt: "hello" }}, "/no-marker-allowlist");
check(promptDrop.disposition === "drop", "allowlist-no-marker-prompt-event-drop");

const fileDrop = capturePolicy({{ tool: "edit", args: {{ path: "x" }} }}, "/no-marker-allowlist");
check(fileDrop.disposition === "drop", "allowlist-no-marker-file-event-drop");

const markedDir = join(markerRoot, "marked-empty");
mkdirSync(markedDir, {{ recursive: true }});
const markerFile = join(markedDir, ".ai-memory.toml");
writeFileSync(markerFile, "[capture]\n");
markerFixtures.set("/marked-empty-cwd", markerFile);
const markedButEmpty = capturePolicy({{ tool: "edit", args: {{ path: "x" }} }}, "/marked-empty-cwd");
check(markedButEmpty.disposition === "keep", "allowlist-marker-present-empty-capture-not-dropped");
"#,
            policy = ts_capture_policy_v1("allowlist"),
        );
        fs::write(&allowlist_module, allowlist_source).unwrap();
        let allowlist_output = Command::new("node")
            .args([
                "--experimental-strip-types",
                allowlist_module.to_str().unwrap(),
                temp.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            allowlist_output.status.success(),
            "allowlist Node runtime evidence failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&allowlist_output.stdout),
            String::from_utf8_lossy(&allowlist_output.stderr),
        );
    }

    /// `AI_MEMORY_CAPTURE_OWNER` hands capture to an external producer: the
    /// generated `capturePolicy` must drop before it even looks for a marker.
    /// Executed against the real emitted TypeScript rather than a hand-kept
    /// copy, and cheap enough (three short Node runs, no fixtures) to stay in
    /// the default tier instead of joining the manual evidence test above.
    #[test]
    fn generated_capture_policy_gates_on_external_capture_owner() {
        let Some(()) = node_strip_types_available() else {
            return;
        };
        let guard = tempfile::tempdir().unwrap();
        let temp = guard.path().to_path_buf();
        const HARNESS: &str = r#"import { closeSync, mkdirSync, opendirSync, openSync, readFileSync as readMarkerText, readSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { homedir } from "node:os";

const markerRoot = process.argv[2]!;
const inheritOnly = process.argv[3] === "inherit";
let markerScans = 0;
const repo = join(markerRoot, "repo");
mkdirSync(repo, { recursive: true });
const markerFile = join(repo, ".ai-memory.toml");
writeFileSync(markerFile, '[capture]\nignore_paths = ["secret/**"]\n');
function findMarker(cwd: string | undefined): string | undefined { markerScans++; return cwd === repo ? markerFile : undefined; }
__AI_MEMORY_POLICY__

function fail(label: string): never { throw new Error(`external-owner gate failed: ${label}`); }
function check(ok: unknown, label: string): asserts ok { if (!ok) fail(label); }

function probe(label: string, owned: boolean): void {
  markerScans = 0;
  const payload = { tool: "edit", args: { path: "public/item" } };
  const result = capturePolicy(payload, repo);
  if (owned) {
    check(result.disposition === "drop", `${label} disposition`);
    // Before the capture-policy marker scan: no disk read by the policy, no
    // disposition work, and the payload comes back untouched so nothing
    // downstream can queue it.
    check(markerScans === 0, `${label} marker scan`);
    check(result.protocol === undefined, `${label} protocol`);
    check(result.payload === payload, `${label} payload identity`);
  } else {
    check(result.disposition === "keep", `${label} disposition`);
    check(markerScans > 0, `${label} marker scan`);
    check(result.protocol?.policy_state === "active", `${label} protocol`);
  }
}

if (inheritOnly) {
  // The value really arrived through the process environment, not a mutation
  // this harness made.
  check((process.env.AI_MEMORY_CAPTURE_OWNER ?? "").trim() !== "", "inherited owner missing");
  probe("inherited-owner", true);
} else {
  for (const [value, owned] of [[undefined, false], ["", false], [" \t\n", false], ["orchestrator-a", true], ["  orchestrator-a  ", true]] as [string | undefined, boolean][]) {
    if (value === undefined) delete process.env.AI_MEMORY_CAPTURE_OWNER;
    else process.env.AI_MEMORY_CAPTURE_OWNER = value;
    probe(`owner=${JSON.stringify(value)}`, owned);
  }
}
"#;

        for (mode, inherit) in [("denylist", false), ("denylist", true), ("allowlist", true)] {
            let module = temp.join(format!(
                "capture-owner-{mode}-{}.ts",
                if inherit { "inherit" } else { "matrix" }
            ));
            fs::write(
                &module,
                HARNESS.replace("__AI_MEMORY_POLICY__", &ts_capture_policy_v1(mode)),
            )
            .unwrap();
            let mut command = Command::new("node");
            command.args([
                "--experimental-strip-types",
                module.to_str().unwrap(),
                temp.to_str().unwrap(),
                if inherit { "inherit" } else { "matrix" },
            ]);
            if inherit {
                command.env("AI_MEMORY_CAPTURE_OWNER", "orchestrator-a");
            } else {
                command.env_remove("AI_MEMORY_CAPTURE_OWNER");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "capture-owner gate evidence failed (mode={mode}, inherit={inherit})\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    #[test]
    fn claude_code_payload_has_all_events() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let hooks = v.get("hooks").and_then(|h| h.as_object()).unwrap();
        assert_eq!(hooks.len(), CLAUDE_CODE_EVENTS.len());
        for (event, _) in CLAUDE_CODE_EVENTS {
            assert!(hooks.contains_key(event), "missing event {event}");
        }
        assert_eq!(
            v.pointer("/hooks/SessionEnd/0/hooks/0/timeout"),
            Some(&Value::from(CLAUDE_SESSION_END_TIMEOUT_SECS))
        );
        assert!(
            v.pointer("/hooks/PostToolUse/0/hooks/0/timeout").is_none(),
            "only the cancellation-prone SessionEnd budget is raised"
        );
    }

    #[test]
    fn grok_native_payload_uses_grok_agent() {
        let root = PathBuf::from("/host/hooks/grok");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::PosixNative, "grok", None, None),
        );
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(command.contains("--agent grok"), "{command}");
        assert!(!command.contains("claude-code"), "{command}");
    }

    #[test]
    fn grok_script_payload_uses_grok_bundle() {
        let root = PathBuf::from("/host/hooks/grok");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::Posix, "grok", None, None),
        );
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        let normalized = command.replace('\\', "/");
        assert!(
            normalized.contains("/host/hooks/grok/session-start.sh"),
            "{command}"
        );
        assert!(!command.contains("claude-code"), "{command}");
    }

    #[test]
    fn devin_payload_has_all_events() {
        let root = PathBuf::from("/host/hooks/devin");
        let v = build_devin_payload(&root, "http://localhost:49374", None);
        let hooks = v.get("hooks").and_then(|h| h.as_object()).unwrap();
        assert_eq!(hooks.len(), DEVIN_EVENTS.len());
        for (event, _) in DEVIN_EVENTS {
            assert!(hooks.contains_key(event), "missing event {event}");
        }
    }

    #[test]
    fn devin_native_payload_uses_devin_agent() {
        let root = PathBuf::from("/host/hooks/devin");
        let v = build_hook_payload_for_platform(
            &DEVIN_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::PosixNative, "devin", None, None),
        );
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(command.contains("--agent devin"), "{command}");
        assert!(!command.contains("grok"), "{command}");
    }

    #[test]
    fn devin_script_payload_uses_devin_bundle() {
        let root = PathBuf::from("/host/hooks/devin");
        let v = build_hook_payload_for_platform(
            &DEVIN_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::Posix, "devin", None, None),
        );
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        let normalized = command.replace('\\', "/");
        assert!(
            normalized.contains("/host/hooks/devin/session-start.sh"),
            "{command}"
        );
        assert!(!command.contains("grok"), "{command}");
    }

    #[test]
    fn claude_code_payload_embeds_auth_token_when_provided() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_posix_hook_payload(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            Some("tok"),
            HookShape::Nested,
        );
        // Env vars are inlined into the command string so CC's
        // hook runner sees them regardless of whether it honours
        // a separate `env` field. Assert the token landed in the
        // command prefix.
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(
            command.contains("AI_MEMORY_AUTH_TOKEN=tok"),
            "command should inline the auth token; got: {command}"
        );
        assert!(
            command.contains("AI_MEMORY_HOOK_URL=http://localhost:49374"),
            "command should inline the hook URL; got: {command}"
        );
    }

    /// Regression guard: Claude Code's hook schema requires the
    /// outer array entries to have `matcher` + a nested `hooks`
    /// array (containing the actual `type: "command"` payload).
    /// We shipped the wrong shape briefly — bare `command` at the
    /// outer level — which made Claude Code refuse to load
    /// settings.json with "hooks: Expected array, but received
    /// undefined" on every event.
    /// Copilot CLI keeps its flat entry shape even when the event names are
    /// PascalCase (only the payload sent to the script changes), and rejects
    /// `matcher` on session/stop/subagent events — so every entry must be a
    /// bare `{type, command}` handler: no nested `hooks`, no `matcher`.
    #[test]
    fn copilot_cli_payload_uses_flat_entries_without_matcher() {
        let root = PathBuf::from("/host/hooks/copilot-cli");
        let v = build_posix_hook_payload(
            COPILOT_CLI_PROFILE.events,
            &root,
            "http://localhost:49374",
            None,
            COPILOT_CLI_PROFILE.shape,
        );
        let hooks = v.pointer("/hooks").and_then(|h| h.as_object()).unwrap();
        assert_eq!(hooks.len(), COPILOT_CLI_EVENTS.len());
        for (event, _) in COPILOT_CLI_EVENTS {
            let entry = v
                .pointer(&format!("/hooks/{event}/0"))
                .and_then(|e| e.as_object())
                .unwrap_or_else(|| panic!("missing /hooks/{event}/0"));
            assert_eq!(
                entry.get("type").and_then(|t| t.as_str()),
                Some("command"),
                "{event}"
            );
            assert!(entry.contains_key("command"), "{event}: {entry:?}");
            assert!(!entry.contains_key("hooks"), "{event}: {entry:?}");
            assert!(!entry.contains_key("matcher"), "{event}: {entry:?}");
        }
    }

    #[test]
    fn cursor_payload_uses_flat_shape() {
        // Flat shape: no inner `hooks: [...]` array; each event
        // maps to an array of {type, command, matcher} entries.
        let root = PathBuf::from("/host/hooks/cursor");
        let v = build_posix_hook_payload(
            CURSOR_PROFILE.events,
            &root,
            "http://localhost:49374",
            Some("tok"),
            CURSOR_PROFILE.shape,
        );
        let session_start = v
            .pointer("/hooks/sessionStart/0")
            .and_then(|e| e.as_object())
            .expect("missing /hooks/sessionStart/0");
        assert_eq!(
            session_start.get("type").and_then(|t| t.as_str()),
            Some("command"),
            "Cursor flat entries put `type` at the outer level"
        );
        assert!(
            session_start.contains_key("command"),
            "Cursor flat entries put `command` at the outer level"
        );
        // No nested hooks array.
        assert!(
            !session_start.contains_key("hooks"),
            "Cursor must NOT use the nested hooks shape — found one: {session_start:?}"
        );
        // Auth token still inlined into command.
        let cmd = session_start
            .get("command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert!(cmd.contains("AI_MEMORY_AUTH_TOKEN=tok"));
        // Events are camelCase, not PascalCase.
        let events: Vec<&str> = v
            .pointer("/hooks")
            .and_then(|h| h.as_object())
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        assert!(events.contains(&"sessionStart"));
        assert!(events.contains(&"preToolUse"));
        assert!(events.contains(&"postToolUseFailure"));
        assert!(
            !events.contains(&"SessionStart"),
            "Cursor uses camelCase, not PascalCase"
        );
    }

    #[test]
    fn gemini_payload_uses_nested_shape_with_gemini_event_names() {
        // Same nested shape as Claude Code, but DIFFERENT event
        // names (BeforeTool / AfterTool / PreCompress; no
        // UserPromptSubmit, no Stop).
        let root = PathBuf::from("/host/hooks/gemini-cli");
        let v = build_profile_payload(
            &GEMINI_PROFILE,
            &root,
            "http://localhost:49374",
            Some("tok"),
        );
        let session_start = v
            .pointer("/hooks/SessionStart/0")
            .and_then(|e| e.as_object())
            .expect("missing /hooks/SessionStart/0");
        // Outer level has matcher + hooks (nested shape).
        assert!(session_start.contains_key("matcher"));
        let inner = session_start
            .get("hooks")
            .and_then(|h| h.as_array())
            .unwrap();
        assert_eq!(inner.len(), 1);
        let entry = inner[0].as_object().unwrap();
        assert_eq!(entry.get("type").and_then(|t| t.as_str()), Some("command"));
        // Event vocab: Gemini-specific names present, Claude Code-
        // only names absent.
        let events: Vec<&str> = v
            .pointer("/hooks")
            .and_then(|h| h.as_object())
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        for expected in [
            "SessionStart",
            "SessionEnd",
            "BeforeTool",
            "AfterTool",
            "PreCompress",
        ] {
            assert!(
                events.contains(&expected),
                "missing Gemini event {expected}"
            );
        }
        for unexpected in [
            "PreToolUse",
            "PostToolUse",
            "UserPromptSubmit",
            "Stop",
            "PreCompact",
        ] {
            assert!(
                !events.contains(&unexpected),
                "Gemini should NOT have CC-only event {unexpected}; got {events:?}"
            );
        }
    }

    #[test]
    fn claude_code_payload_uses_matcher_plus_inner_hooks_shape() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        for (event, _) in CLAUDE_CODE_EVENTS {
            let outer = v
                .pointer(&format!("/hooks/{event}/0"))
                .and_then(|s| s.as_object())
                .unwrap_or_else(|| panic!("missing /hooks/{event}/0"));
            assert!(outer.contains_key("matcher"), "{event}: missing matcher");
            let inner = outer
                .get("hooks")
                .and_then(|h| h.as_array())
                .unwrap_or_else(|| panic!("{event}: missing inner hooks array"));
            assert_eq!(inner.len(), 1);
            let entry = inner[0].as_object().unwrap();
            assert_eq!(
                entry.get("type").and_then(|t| t.as_str()),
                Some("command"),
                "{event}: inner entry must have type: command"
            );
            assert!(
                entry.contains_key("command"),
                "{event}: inner entry missing command"
            );
        }
    }

    #[test]
    fn claude_code_payload_omits_auth_token_when_absent() {
        let root = PathBuf::from("/host/hooks/claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        // Format-agnostic: POSIX/WindowsBash inline `AI_MEMORY_HOOK_URL=…`,
        // WindowsNative passes `--server-url …`. Both carry the host:port,
        // and neither must carry a token when none was supplied.
        assert!(
            command.contains("localhost:49374"),
            "server url expected: {command}"
        );
        assert!(
            !command.contains("AUTH_TOKEN") && !command.contains("--auth-token"),
            "no token expected in command: {command}"
        );
    }

    #[test]
    fn windows_native_emits_binary_command_with_event_token() {
        let root = PathBuf::from(r"C:\hooks");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://h:49374",
            Some("tok"),
            HookShape::Nested,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                None,
                None,
            )
            .allow_claude_windows_exec(),
        );
        // Each native exec-form handler must carry `hook --event <stem>` in
        // argv, where <stem> matches the .sh script the other platforms invoke.
        for (event, script) in CLAUDE_CODE_EVENTS {
            let stem = script.strip_suffix(".sh").unwrap();
            let handler = v.pointer(&format!("/hooks/{event}/0/hooks/0")).unwrap();
            let cmd = handler.get("command").and_then(|s| s.as_str()).unwrap();
            let args: Vec<&str> = handler
                .get("args")
                .and_then(|a| a.as_array())
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            assert!(!cmd.contains("--event"), "{event}: executable only: {cmd}");
            assert!(
                args.windows(2).any(|w| w == ["--event", stem]),
                "{event}: {args:?}"
            );
            assert!(
                args.windows(2).any(|w| w == ["--agent", "claude-code"]),
                "{event}: {args:?}"
            );
            assert!(
                args.windows(2)
                    .any(|w| w == ["--server-url", "http://h:49374"]),
                "{event}: {args:?}"
            );
            assert!(
                args.windows(2).any(|w| w == ["--auth-token", "tok"]),
                "{event}: {args:?}"
            );
        }
    }

    #[test]
    fn windows_native_claude_code_nested_payload_uses_exec_form() {
        let root = PathBuf::from(r"C:\hooks");
        let data_dir = Path::new(r"\\?\C:\Users\me\AppData\Local\ai-memory");
        let v = build_hook_payload_for_platform(
            &[("SessionStart", "session-start.sh")],
            &root,
            "http://h:49374",
            Some("tok"),
            HookShape::Nested,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                Some(data_dir),
                Some("repo-root"),
            )
            .allow_claude_windows_exec(),
        );
        let handler = v.pointer("/hooks/SessionStart/0/hooks/0").unwrap();
        let command = handler.get("command").and_then(|v| v.as_str()).unwrap();
        let args: Vec<&str> = handler
            .get("args")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        assert!(command.contains("ai-memory") || command.contains("ai_memory"));
        assert!(
            !command.contains(" --event "),
            "command is executable only: {command}"
        );
        assert_eq!(
            args,
            vec![
                "--data-dir",
                r"C:\Users\me\AppData\Local\ai-memory",
                "hook",
                "--event",
                "session-start",
                "--agent",
                "claude-code",
                "--server-url",
                "http://h:49374",
                "--auth-token",
                "tok",
                "--project-strategy",
                "repo-root",
            ]
        );
        assert!(
            args.iter().all(|arg| !arg.contains('"')),
            "args are raw argv tokens"
        );
    }

    #[test]
    fn windows_native_exec_spec_strips_verbatim_exe_and_data_dir() {
        let spec = windows_native_exec_spec_with_exe(
            Path::new(r"\\?\C:\Program Files\ai-memory\ai-memory.exe"),
            Path::new(r"C:\hooks\post-tool-use.sh"),
            "http://h:49374",
            None,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                Some(Path::new(r"\\?\C:\Data\ai-memory")),
                None,
            ),
        );
        let HookHandlerSpec::Exec { command, args } = spec else {
            panic!("expected exec spec")
        };
        assert_eq!(command, r"C:\Program Files\ai-memory\ai-memory.exe");
        assert_eq!(args[1], r"C:\Data\ai-memory");
        assert!(args.iter().all(|arg| !arg.contains(r"\\?\")));
    }

    /// The public entry point must emit a complete `hooks:` map with both tool
    /// events, whatever platform it renders for (the suite runs on Windows too).
    #[test]
    fn hermes_hooks_yaml_covers_both_tool_events() {
        let yaml = build_hermes_hooks_yaml("http://127.0.0.1:49374", None, None, None);
        assert!(yaml.starts_with("hooks:\n"));
        for (event, _) in HERMES_EVENTS {
            assert!(yaml.contains(&format!("  {event}:\n")), "missing {event}");
            assert!(
                yaml.contains("    - command: '"),
                "command must be a single-quoted YAML scalar"
            );
        }
        assert_eq!(
            yaml.matches(&format!("timeout: {HERMES_HOOK_TIMEOUT_SECONDS}"))
                .count(),
            HERMES_EVENTS.len()
        );
    }

    /// Hermes splits `command` with `shlex.split` and runs it with no shell, so
    /// the generated line must be bare argv: a `KEY=value` prefix would be
    /// parsed as extra argv entries and the hook would never reach the server.
    #[test]
    fn hermes_hooks_yaml_invokes_the_native_command_without_a_shell() {
        let yaml = build_hermes_hooks_yaml_for_platform(
            "http://127.0.0.1:49374",
            None,
            HookCommandContext::new(
                HookCommandPlatform::PosixNative,
                "hermes",
                Some(Path::new("/data")),
                Some("repo-root"),
            ),
        );
        for (_, our_event) in HERMES_EVENTS {
            assert!(
                yaml.contains(&format!("hook --event {our_event} --agent hermes")),
                "missing the native command for {our_event}"
            );
        }
        assert!(
            !yaml.contains("AI_MEMORY_HOOK_URL="),
            "an env prefix would be parsed as argv, not environment"
        );
        assert!(yaml.contains("--data-dir /data"));
        assert!(yaml.contains("--server-url http://127.0.0.1:49374"));
        assert!(yaml.contains("--project-strategy repo-root"));
    }

    fn exec_args(script: &str, capture_assistant: bool) -> Vec<String> {
        let spec = windows_native_exec_spec_with_exe(
            Path::new(r"C:\ai-memory\ai-memory.exe"),
            Path::new(script),
            "http://h:49374",
            None,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                None,
                None,
            )
            .with_capture_assistant(capture_assistant),
        );
        let HookHandlerSpec::Exec { args, .. } = spec else {
            panic!("expected exec spec")
        };
        args
    }

    #[test]
    fn capture_assistant_exec_flag_only_on_stop_and_only_when_opted_in() {
        // Bare filenames so `file_stem()` yields the event on any host (a
        // backslash path is a single component on Unix).
        // Opted in: only the `stop` command carries the flag (#196, D1).
        assert!(exec_args("stop.sh", true).contains(&"--capture-assistant".to_string()));
        assert!(
            !exec_args("session-start.sh", true).contains(&"--capture-assistant".to_string()),
            "flag must not leak onto non-stop commands"
        );
        // Not opted in: even the stop command stays byte-identical to before.
        assert!(
            !exec_args("stop.sh", false).contains(&"--capture-assistant".to_string()),
            "flag must be absent without opt-in"
        );
    }

    #[test]
    fn capture_assistant_string_form_only_on_stop() {
        // POSIX + Windows string forms: the flag rides only the stop command.
        for platform in [
            HookCommandPlatform::PosixNative,
            HookCommandPlatform::WindowsNative,
        ] {
            let ctx = HookCommandContext::new(platform, "claude-code", None, None)
                .with_capture_assistant(true);
            let stop = hook_command(Path::new("stop.sh"), "http://h", None, ctx);
            let start = hook_command(Path::new("session-start.sh"), "http://h", None, ctx);
            assert!(
                stop.contains("--capture-assistant"),
                "{platform:?} stop missing flag: {stop}"
            );
            assert!(
                !start.contains("--capture-assistant"),
                "{platform:?} session-start must not carry flag: {start}"
            );
        }
    }

    #[test]
    fn windows_native_exec_form_is_claude_only_and_guarded() {
        fn handler_for(
            platform: HookCommandPlatform,
            agent: &str,
            shape: HookShape,
        ) -> serde_json::Value {
            let v = build_hook_payload_for_platform(
                &[("SessionStart", "session-start.sh")],
                Path::new(r"C:\hooks"),
                "http://h:49374",
                Some("tok"),
                shape,
                HookCommandContext::new(platform, agent, None, None).allow_claude_windows_exec(),
            );
            match shape {
                HookShape::Nested | HookShape::NestedWithoutMatcher => {
                    v.pointer("/hooks/SessionStart/0/hooks/0").unwrap().clone()
                }
                HookShape::Flat | HookShape::FlatWithoutMatcher => {
                    v.pointer("/hooks/SessionStart/0").unwrap().clone()
                }
            }
        }

        for (platform, agent, shape) in [
            (
                HookCommandPlatform::WindowsBash,
                "claude-code",
                HookShape::Nested,
            ),
            (
                HookCommandPlatform::Windows,
                "claude-code",
                HookShape::Nested,
            ),
            (HookCommandPlatform::Posix, "claude-code", HookShape::Nested),
            (
                HookCommandPlatform::PosixNative,
                "claude-code",
                HookShape::Nested,
            ),
        ] {
            let handler = handler_for(platform, agent, shape);
            assert!(
                handler.get("args").is_none(),
                "{platform:?}/{agent}/{shape:?} must keep command-string form: {handler}"
            );
        }

        let claude = handler_for(
            HookCommandPlatform::WindowsNative,
            "claude-code",
            HookShape::Nested,
        );
        assert!(
            claude.get("args").is_some(),
            "Claude must use exec form: {claude}"
        );

        for (agent, shape) in [
            ("cursor", HookShape::Flat),
            ("gemini-cli", HookShape::Flat),
            ("codex", HookShape::Nested),
            ("antigravity-cli", HookShape::Flat),
            ("grok", HookShape::Nested),
            ("devin", HookShape::Nested),
            ("copilot-cli", HookShape::FlatWithoutMatcher),
        ] {
            let handler = handler_for(HookCommandPlatform::WindowsNative, agent, shape);
            assert!(
                handler.get("args").is_none(),
                "{agent}/{shape:?} must retain command-string schema: {handler}"
            );
            let command = handler["command"].as_str().unwrap();
            assert!(
                command.contains("hook --event session-start")
                    && command.contains(&format!("--agent {agent}")),
                "{agent}/{shape:?} must invoke the native hook command: {command}"
            );
        }

        let setup_like = build_hook_payload_for_platform(
            &[("SessionStart", "session-start.sh")],
            Path::new(r"C:\hooks"),
            "http://h:49374",
            Some("tok"),
            HookShape::Nested,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                None,
                None,
            ),
        );
        assert!(
            setup_like
                .pointer("/hooks/SessionStart/0/hooks/0/args")
                .is_none(),
            "unapproved Claude render paths must retain command-string schema: {setup_like}"
        );

        let antigravity = build_antigravity_payload_for_platform(
            Path::new(r"C:\hooks"),
            "http://h:49374",
            Some("tok"),
            HookCommandPlatform::WindowsNative,
            "antigravity-cli",
            None,
            None,
        );
        assert!(
            antigravity
                .pointer("/ai-memory/PreInvocation/0/args")
                .is_none(),
            "Antigravity must retain command-string schema: {antigravity}"
        );
    }

    /// #611: Antigravity runs its hook `command` string through
    /// `cmd /c "<string>"` without stripping inner quotes, so a quoted
    /// `"C:\…\ai-memory.exe"` is "not recognized" and crashes the session.
    /// Its Windows command must render UNQUOTED, while other Windows agents
    /// keep the double quotes.
    #[test]
    fn antigravity_windows_command_is_unquoted_but_others_keep_quotes() {
        let ag = build_antigravity_payload_for_platform(
            Path::new(r"C:\Users\me\.local\bin"),
            "https://memory.example.com",
            Some("tok"),
            HookCommandPlatform::WindowsNative,
            "antigravity-cli",
            Some(Path::new(r"C:\Users\me\AppData\Local\ai-memory")),
            None,
        );
        let cmd = ag
            .pointer("/ai-memory/PreInvocation/0/command")
            .and_then(|v| v.as_str())
            .expect("antigravity command string");
        assert!(
            !cmd.contains('"'),
            "antigravity Windows command must carry no double quotes: {cmd}"
        );
        assert!(
            cmd.contains(r"ai-memory hook --event") || cmd.contains(r"ai-memory.exe hook"),
            "unexpected antigravity command shape: {cmd}"
        );
        assert!(
            cmd.contains("--data-dir C:\\Users\\me\\AppData\\Local\\ai-memory")
                && cmd.contains("--server-url https://memory.example.com"),
            "bare (unquoted) args expected: {cmd}"
        );

        // Same renderer, a non-antigravity agent identity: the double quotes
        // stay, so the fix is scoped to antigravity and does not regress the
        // other Windows JSON-hook agents.
        let other = build_antigravity_payload_for_platform(
            Path::new(r"C:\Users\me\.local\bin"),
            "https://memory.example.com",
            Some("tok"),
            HookCommandPlatform::WindowsNative,
            "grok",
            Some(Path::new(r"C:\Users\me\AppData\Local\ai-memory")),
            None,
        );
        let other_cmd = other
            .pointer("/ai-memory/PreInvocation/0/command")
            .and_then(|v| v.as_str())
            .expect("command string");
        assert!(
            other_cmd.contains('"'),
            "non-antigravity Windows agents keep their double quotes: {other_cmd}"
        );
    }

    #[test]
    fn command_code_profile_omits_matchers_and_uses_native_agent_identity() {
        let value = build_hook_payload_for_platform(
            &COMMAND_CODE_EVENTS,
            Path::new("/hooks"),
            "http://memory:49374",
            None,
            HookShape::NestedWithoutMatcher,
            HookCommandContext::new(HookCommandPlatform::PosixNative, "command-code", None, None),
        );

        for event in ["SessionStart", "PreToolUse", "PostToolUse", "Stop"] {
            let definition = &value["hooks"][event][0];
            assert!(definition.get("matcher").is_none(), "event: {event}");
            let command = definition["hooks"][0]["command"].as_str().unwrap();
            assert!(command.contains("--agent command-code"), "{command}");
        }
    }

    #[test]
    fn ibm_bob_profile_uses_common_events_without_matchers() {
        let value = build_ibm_bob_hooks_payload(
            "http://memory:49374",
            None,
            Some(Path::new("/data")),
            Some("repo-root"),
        );

        assert_eq!(value["hooks"].as_object().unwrap().len(), 5);
        for (event, script) in IBM_BOB_EVENTS {
            let definition = &value["hooks"][event][0];
            assert!(definition.get("matcher").is_none(), "event: {event}");
            let command = definition["hooks"][0]["command"].as_str().unwrap();
            let event_name = script.trim_end_matches(".sh");
            assert!(
                command.contains(&format!("hook --event {event_name} --agent ibm-bob")),
                "{event}: {command}"
            );
            let expected_strategy = if cfg!(windows) {
                r#"--project-strategy "repo-root""#
            } else {
                "--project-strategy repo-root"
            };
            assert!(command.contains(expected_strategy), "{command}");
            assert!(!command.contains(".sh"), "{command}");
        }
        assert!(value["hooks"].get("SessionEnd").is_none());
        assert!(value["hooks"].get("PreCompact").is_none());
    }

    #[test]
    fn claude_code_payload_emits_absolute_paths() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("hooks")
            .join("claude-code");
        let v = build_posix_hook_payload(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            None,
            HookShape::Nested,
        );
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        let expected = root.join("session-start.sh").to_string_lossy().to_string();
        assert!(
            cmd.contains(&expected),
            "command should contain the absolute script path: {cmd}"
        );
    }

    #[test]
    fn posix_hook_command_quotes_script_path_and_shell_metachars() {
        let cmd = hook_command(
            &PathBuf::from("/tmp/hooks dir/session-start.sh"),
            "http://localhost:49374/mcp?x=1&y=2",
            Some("tok;rm -rf /"),
            HookCommandContext::new(HookCommandPlatform::Posix, "claude-code", None, None),
        );

        assert!(
            cmd.contains("AI_MEMORY_HOOK_URL='http://localhost:49374/mcp?x=1&y=2'"),
            "URL with query metacharacters must be quoted: {cmd}"
        );
        assert!(
            cmd.contains("AI_MEMORY_AUTH_TOKEN='tok;rm -rf /'"),
            "token with shell metacharacters must be quoted: {cmd}"
        );
        assert!(
            cmd.ends_with("'/tmp/hooks dir/session-start.sh'"),
            "script path with spaces must be quoted: {cmd}"
        );
    }

    #[test]
    fn windows_payload_uses_powershell_and_ps1_hooks() {
        let root = PathBuf::from("C:/Users/alice/.local/share/ai-memory/hooks/claude-code");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "http://localhost:49374",
            Some("tok'en"),
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::Windows, "claude-code", None, None),
        );
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(cmd.starts_with(
            "powershell.exe -NoLogo -NoProfile -NonInteractive -OutputFormat Text -ExecutionPolicy Bypass -EncodedCommand "
        ));
        assert!(
            !cmd.contains("$env:"),
            "outer command must be opaque: {cmd}"
        );
        assert!(
            !cmd.contains("http://localhost:49374"),
            "outer command must not expose values to its caller: {cmd}"
        );
        let program = decode_powershell_encoded_command(cmd);
        assert!(program.starts_with("$ProgressPreference='SilentlyContinue';"));
        assert!(program.contains("$env:AI_MEMORY_HOOK_URL='http://localhost:49374'"));
        assert!(program.contains("$env:AI_MEMORY_AUTH_TOKEN='tok''en'"));
        assert!(
            program.contains("session-start.ps1"),
            "expected ps1 script path: {program}"
        );
        assert!(
            !program.contains("session-start.sh"),
            "Windows command must not use sh: {program}"
        );
    }

    #[test]
    fn antigravity_windows_commands_survive_an_outer_powershell_runner() {
        let root = PathBuf::from("C:/Users/alice/.local/share/ai-memory/hooks/antigravity-cli");
        let v = build_antigravity_payload_for_platform(
            &root,
            "http://localhost:49374",
            Some("tok'en"),
            HookCommandPlatform::Windows,
            "antigravity-cli",
            None,
            Some("repo-root"),
        );

        for pointer in [
            "/ai-memory/PreInvocation/0/command",
            "/ai-memory/PreToolUse/0/hooks/0/command",
        ] {
            let command = v.pointer(pointer).and_then(Value::as_str).unwrap();
            assert!(
                command.contains(" -EncodedCommand "),
                "{pointer}: {command}"
            );
            assert!(
                command.contains(" -OutputFormat Text "),
                "{pointer}: nested PowerShell output must stay textual: {command}"
            );
            assert!(
                !command.contains("$env:")
                    && !command.contains("localhost:49374")
                    && !command.contains(".ps1"),
                "{pointer}: outer runner must see only an opaque program: {command}"
            );

            let program = decode_powershell_encoded_command(command);
            assert!(
                program.starts_with("$ProgressPreference='SilentlyContinue';"),
                "{pointer}: {program}"
            );
            assert!(
                program.contains("$env:AI_MEMORY_HOOK_URL='http://localhost:49374'"),
                "{pointer}: {program}"
            );
            assert!(
                program.contains("$env:AI_MEMORY_AUTH_TOKEN='tok''en'"),
                "{pointer}: {program}"
            );
            assert!(
                program.contains("$env:AI_MEMORY_PROJECT_STRATEGY='repo-root'"),
                "{pointer}: {program}"
            );
            assert!(program.contains(".ps1"), "{pointer}: {program}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_encoded_hook_executes_through_an_outer_powershell() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("hook with spaces.ps1");
        fs::write(
            &script,
            r#"if ($env:AI_MEMORY_HOOK_URL -ne "http://localhost:49374") { exit 9 }
if ($env:AI_MEMORY_AUTH_TOKEN -ne "tok'en") { exit 10 }
Write-Progress -Activity "hook progress" -Status "must stay silent"
$payload = [Console]::In.ReadToEnd()
[Console]::Out.Write($payload)
"#,
        )
        .unwrap();
        let command = hook_command(
            &script,
            "http://localhost:49374",
            Some("tok'en"),
            HookCommandContext::new(HookCommandPlatform::Windows, "antigravity-cli", None, None),
        );

        let powershell = ai_memory_test_support::powershell_exe();
        let outer_command = command_for_available_powershell(&command, powershell);
        let mut child = Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &outer_command,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"hook":"ok"}"#)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "nested PowerShell failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "non-interactive hook wrote stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, br#"{"hook":"ok"}"#);
    }

    #[test]
    fn antigravity_payload_uses_named_groups_with_mixed_shape() {
        let root = PathBuf::from("/host/hooks/antigravity-cli");
        let v = build_antigravity_payload_for_platform(
            &root,
            "http://localhost:49374",
            Some("tok"),
            HookCommandPlatform::Posix,
            "antigravity-cli",
            None,
            None,
        );

        // Top-level key is the named group "ai-memory", not "hooks"
        let group = v
            .get("ai-memory")
            .and_then(|g| g.as_object())
            .expect("missing top-level 'ai-memory' named group");
        assert!(
            !v.as_object().unwrap().contains_key("hooks"),
            "Antigravity uses named groups, not a 'hooks' wrapper"
        );

        // Tool events: nested shape (matcher + hooks array)
        let pre_tool = group
            .get("PreToolUse")
            .and_then(|e| e.as_array())
            .expect("missing PreToolUse");
        let outer = pre_tool[0].as_object().unwrap();
        assert!(outer.contains_key("matcher"));
        let inner = outer.get("hooks").and_then(|h| h.as_array()).unwrap();
        assert_eq!(inner.len(), 1);
        let entry = inner[0].as_object().unwrap();
        assert_eq!(entry.get("type").and_then(|t| t.as_str()), Some("command"));

        // Lifecycle events: flat shape (no matcher, direct handler list)
        let pre_invocation = group
            .get("PreInvocation")
            .and_then(|e| e.as_array())
            .expect("missing PreInvocation");
        let handler = pre_invocation[0].as_object().unwrap();
        assert!(
            !handler.contains_key("matcher"),
            "PreInvocation should not have matcher (flat shape)"
        );
        assert!(
            !handler.contains_key("hooks"),
            "PreInvocation should not have inner hooks array (flat shape)"
        );
        assert_eq!(
            handler.get("type").and_then(|t| t.as_str()),
            Some("command")
        );

        // Auth token inlined into commands
        let cmd = handler.get("command").and_then(|c| c.as_str()).unwrap();
        assert!(cmd.contains("AI_MEMORY_AUTH_TOKEN=tok"));

        let stop = group
            .get("Stop")
            .and_then(|e| e.as_array())
            .expect("missing Stop");
        let stop_cmd = stop[0]
            .get("command")
            .and_then(|c| c.as_str())
            .expect("Stop command missing");
        assert!(
            stop_cmd.contains("stop.sh"),
            "Stop must record a stop observation, not synthesize session-end handoffs: {stop_cmd}"
        );

        // All expected events present
        for expected in ["PreToolUse", "PostToolUse", "PreInvocation", "Stop"] {
            assert!(
                group.contains_key(expected),
                "missing Antigravity event {expected}"
            );
        }
    }

    #[test]
    fn kimi_code_commands_cover_all_events_with_script_paths() {
        let root = PathBuf::from("/host/hooks/kimi-code");
        let commands = kimi_code_hook_commands_for_platform(
            &root,
            "http://localhost:49374",
            Some("tok"),
            HookCommandPlatform::Posix,
            None,
            None,
        );
        assert_eq!(commands.len(), KIMI_CODE_EVENTS.len());
        let events: Vec<&str> = commands.iter().map(|(event, _)| *event).collect();
        for (event, script) in KIMI_CODE_EVENTS {
            assert!(events.contains(&event), "missing Kimi Code event {event}");
            let (_, cmd) = commands.iter().find(|(e, _)| *e == event).unwrap();
            let expected = root.join(script);
            assert!(
                cmd.contains(expected.to_string_lossy().as_ref()),
                "{event}: command must point at the staged script: {cmd}"
            );
        }
        let (_, session_start) = &commands[0];
        assert!(
            session_start.contains("AI_MEMORY_HOOK_URL=http://localhost:49374"),
            "{session_start}"
        );
        assert!(
            session_start.contains("AI_MEMORY_AUTH_TOKEN=tok"),
            "{session_start}"
        );
    }

    #[test]
    fn kimi_code_commands_windows_use_ps1_scripts() {
        let root = PathBuf::from(r"C:\hooks\kimi-code");
        let commands = kimi_code_hook_commands_for_platform(
            &root,
            "http://h:49374",
            None,
            HookCommandPlatform::Windows,
            None,
            None,
        );
        let (_, cmd) = &commands[0];
        let program = decode_powershell_encoded_command(cmd);
        assert!(program.contains("session-start.ps1"), "{program}");
        assert!(!program.contains("session-start.sh"), "{program}");
    }

    #[test]
    fn to_git_bash_path_converts_drive_letter_and_backslashes() {
        assert_eq!(
            to_git_bash_path(r"C:\Users\alice\hooks\x.sh"),
            "/c/Users/alice/hooks/x.sh"
        );
        assert_eq!(to_git_bash_path(r"D:\Projects\repo"), "/d/Projects/repo");
    }

    #[test]
    fn to_git_bash_path_preserves_posix_paths() {
        assert_eq!(
            to_git_bash_path("/already/posix/path"),
            "/already/posix/path"
        );
    }

    #[test]
    fn to_git_bash_path_handles_forward_slash_windows_paths() {
        assert_eq!(
            to_git_bash_path("C:/Users/alice/hooks/x.sh"),
            "/c/Users/alice/hooks/x.sh"
        );
    }

    #[test]
    fn windows_bash_hook_command_wraps_in_bash_c_with_git_bash_paths() {
        let cmd = hook_command(
            &PathBuf::from(
                r"C:\Users\alice\.local\share\ai-memory\hooks\claude-code\session-start.sh",
            ),
            "https://my-server.example.com",
            Some("tok123"),
            HookCommandContext::new(HookCommandPlatform::WindowsBash, "claude-code", None, None),
        );
        assert!(
            cmd.starts_with("bash -c "),
            "command must be bash-wrapped: {cmd}"
        );
        assert!(
            cmd.contains("/c/Users/alice/"),
            "Windows path must be converted to Git Bash format: {cmd}"
        );
        assert!(
            cmd.contains("session-start.sh"),
            "must use .sh script: {cmd}"
        );
        assert!(
            cmd.contains("AI_MEMORY_HOOK_URL=https://my-server.example.com"),
            "must inline hook URL: {cmd}"
        );
        assert!(
            cmd.contains("AI_MEMORY_AUTH_TOKEN=tok123"),
            "must inline auth token: {cmd}"
        );
    }

    #[test]
    fn windows_bash_hook_command_omits_token_when_absent() {
        let cmd = hook_command(
            &PathBuf::from(r"C:\Users\alice\hooks\session-start.sh"),
            "http://localhost:49374",
            None,
            HookCommandContext::new(HookCommandPlatform::WindowsBash, "claude-code", None, None),
        );
        assert!(cmd.starts_with("bash -c "));
        assert!(
            !cmd.contains("AI_MEMORY_AUTH_TOKEN"),
            "no token expected: {cmd}"
        );
    }

    #[test]
    fn windows_bash_script_for_platform_keeps_sh_extension() {
        let s = script_for_platform("session-start.sh", HookCommandPlatform::WindowsBash);
        assert_eq!(s, "session-start.sh");
    }

    // ── install-time --project-strategy baking (#128) ────────────────
    // A baked `Some("repo-root")` must surface in every command arm; the
    // default `None` must leave every arm byte-identical (no strategy).

    fn strategy_cmd(platform: HookCommandPlatform, strategy: Option<&str>) -> String {
        hook_command(
            &PathBuf::from("/tmp/hooks/claude-code/session-start.sh"),
            "http://localhost:49374",
            None,
            HookCommandContext::new(platform, "claude-code", None, strategy),
        )
    }

    #[test]
    fn posix_hook_command_bakes_project_strategy_env() {
        let cmd = strategy_cmd(HookCommandPlatform::Posix, Some("repo-root"));
        assert!(
            cmd.contains("AI_MEMORY_PROJECT_STRATEGY=repo-root"),
            "posix must bake the strategy env: {cmd}"
        );
    }

    #[test]
    fn windows_ps_hook_command_bakes_project_strategy_env() {
        let cmd = strategy_cmd(HookCommandPlatform::Windows, Some("repo-root"));
        let program = decode_powershell_encoded_command(&cmd);
        assert!(
            program.contains("$env:AI_MEMORY_PROJECT_STRATEGY='repo-root'"),
            "powershell must bake the strategy env: {program}"
        );
    }

    #[test]
    fn windows_bash_hook_command_bakes_project_strategy_env() {
        let cmd = strategy_cmd(HookCommandPlatform::WindowsBash, Some("repo-root"));
        assert!(cmd.starts_with("bash -c "), "{cmd}");
        assert!(
            cmd.contains("AI_MEMORY_PROJECT_STRATEGY=repo-root"),
            "windows-bash must bake the strategy env inside bash -c: {cmd}"
        );
    }

    #[test]
    fn posix_native_hook_command_passes_project_strategy_flag() {
        let cmd = strategy_cmd(HookCommandPlatform::PosixNative, Some("repo-root"));
        assert!(
            cmd.contains("--project-strategy repo-root"),
            "posix-native must pass the strategy flag: {cmd}"
        );
    }

    #[test]
    fn windows_native_hook_command_passes_project_strategy_flag() {
        let cmd = strategy_cmd(HookCommandPlatform::WindowsNative, Some("repo-root"));
        assert!(
            cmd.contains(r#"--project-strategy "repo-root""#),
            "windows-native must pass the strategy flag (double-quoted): {cmd}"
        );
    }

    #[test]
    fn hook_command_omits_project_strategy_when_none() {
        for platform in [
            HookCommandPlatform::Posix,
            HookCommandPlatform::Windows,
            HookCommandPlatform::WindowsBash,
            HookCommandPlatform::PosixNative,
            HookCommandPlatform::WindowsNative,
        ] {
            let cmd = strategy_cmd(platform, None);
            let inspect = if platform == HookCommandPlatform::Windows {
                decode_powershell_encoded_command(&cmd)
            } else {
                cmd
            };
            assert!(
                !inspect.contains("AI_MEMORY_PROJECT_STRATEGY"),
                "{platform:?}: no strategy env when None: {inspect}"
            );
            assert!(
                !inspect.contains("--project-strategy"),
                "{platform:?}: no strategy flag when None: {inspect}"
            );
        }
    }

    #[test]
    fn posix_native_hook_command_invokes_binary_directly() {
        let cmd = hook_command(
            &PathBuf::from("/home/alice/.local/share/ai-memory/hooks/claude-code/session-start.sh"),
            "https://my-server.example.com",
            Some("tok123"),
            HookCommandContext::new(HookCommandPlatform::PosixNative, "claude-code", None, None),
        );
        assert!(
            cmd.contains("hook --event session-start"),
            "invokes the binary subcommand with the event stem: {cmd}"
        );
        assert!(cmd.contains("--agent claude-code"), "{cmd}");
        assert!(cmd.contains("https://my-server.example.com"), "{cmd}");
        assert!(
            cmd.contains("--auth-token") && cmd.contains("tok123"),
            "{cmd}"
        );
        assert!(
            !cmd.contains("session-start.sh"),
            "must NOT reference the .sh script: {cmd}"
        );
        assert!(!cmd.starts_with("bash -c"), "no shell wrapper: {cmd}");
    }

    #[test]
    fn posix_native_hook_command_omits_token_when_absent() {
        let cmd = hook_command(
            &PathBuf::from("/home/alice/hooks/pre-tool-use.sh"),
            "http://localhost:49374",
            None,
            HookCommandContext::new(
                HookCommandPlatform::PosixNative,
                "codex",
                Some(Path::new("/home/alice/.local/share/custom memory")),
                None,
            ),
        );
        assert!(cmd.contains("hook --event pre-tool-use"), "{cmd}");
        assert!(cmd.contains("--agent codex"), "{cmd}");
        assert!(
            cmd.contains("--data-dir '/home/alice/.local/share/custom memory'"),
            "{cmd}"
        );
        assert!(!cmd.contains("--auth-token"), "no token expected: {cmd}");
    }

    #[test]
    fn windows_native_command_strips_verbatim_data_dir() {
        // Regression for #116: native hook commands must render a plain data dir.
        let cmd = hook_command(
            &PathBuf::from(
                r"C:/Users/me/AppData/Local/ai-memory/hooks/claude-code/post-tool-use.sh",
            ),
            "https://srv.example.com",
            None,
            HookCommandContext::new(
                HookCommandPlatform::WindowsNative,
                "claude-code",
                Some(Path::new(r"\\?\C:\Users\me\AppData\Local\ai-memory")),
                None,
            ),
        );
        assert!(
            cmd.contains(r#"--data-dir "C:\Users\me\AppData\Local\ai-memory""#),
            "plain data dir expected: {cmd}"
        );
        assert!(cmd.contains("hook --event post-tool-use"), "{cmd}");
        assert!(
            !cmd.contains(r"\\?\"),
            "verbatim prefix must not leak into the hook command: {cmd}"
        );
    }

    #[test]
    fn windows_bash_payload_uses_bash_c_and_sh_hooks() {
        let root = PathBuf::from(r"C:\Users\alice\.local\share\ai-memory\hooks\claude-code");
        let v = build_hook_payload_for_platform(
            &CLAUDE_CODE_EVENTS,
            &root,
            "https://my-server.example.com",
            Some("tok123"),
            HookShape::Nested,
            HookCommandContext::new(HookCommandPlatform::WindowsBash, "claude-code", None, None),
        );
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(
            cmd.starts_with("bash -c "),
            "command must be bash-wrapped: {cmd}"
        );
        assert!(
            cmd.contains("/c/Users/alice/"),
            "path must be in Git Bash format: {cmd}"
        );
        assert!(
            cmd.contains("session-start.sh"),
            "must use .sh script: {cmd}"
        );
        assert!(
            !cmd.contains("session-start.ps1"),
            "must not use .ps1: {cmd}"
        );
        assert!(
            cmd.contains("AI_MEMORY_HOOK_URL="),
            "must inline URL: {cmd}"
        );
        assert!(
            cmd.contains("AI_MEMORY_AUTH_TOKEN=tok123"),
            "must inline token: {cmd}"
        );
    }
}
