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
use std::path::Path;

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
#[must_use]
pub(crate) fn ts_capture_policy_v1() -> &'static str {
    r##"// capture-policy-v1 (generated; do not fork between adapters)
const CAPTURE_POLICY_V1 = 1;
const CAPTURE_MARKER_MAX_BYTES = 64 * 1024;
const CAPTURE_MAX_PATTERNS = 128;
const CAPTURE_MAX_PATTERN_CHARS = 1024;
const CAPTURE_MAX_PATH_CHARS = 4096;
const CAPTURE_MAX_CANDIDATES = 32;
const CAPTURE_MAX_WORK = 1000000;
const CAPTURE_MAX_CALL_ID_CHARS = 128;

type CaptureDisposition = "keep" | "drop" | "metadata-only";
type CaptureProtocol = { version: 1; disposition: CaptureDisposition; policy_state: "inactive" | "active" | "invalid"; tool_family: "file" | "search-list" | "non-file" | "unknown"; path_count: number; extraction_state: "not-applicable" | "extracted" | "missing-or-malformed" | "unsupported-schema" };

type CaptureConfig = { state: "inactive" | "active" | "invalid"; patterns: { path: string; windows: boolean; directory?: string }[]; base: string };
function readFileSync(path: string, encoding?: "utf8"): any { if (encoding) return readMarkerText(path, encoding); const fd = openSync(path, "r"); try { const bytes = Buffer.allocUnsafe(CAPTURE_MARKER_MAX_BYTES + 1); const count = readSync(fd, bytes, 0, bytes.length, 0); if (count > CAPTURE_MARKER_MAX_BYTES) throw new Error("marker too large"); const result = bytes.subarray(0, count); new TextDecoder("utf-8", { fatal: true }).decode(result); return result; } finally { closeSync(fd); } }
function captureTrimComment(line: string): string { let quote = ""; let escaped = false; for (let i = 0; i < line.length; i++) { const c = line[i]; if (escaped) { escaped = false; continue; } if (c === "\\" && quote === '"') { escaped = true; continue; } if ((c === '"' || c === "'") && (!quote || quote === c)) quote = quote ? "" : c; else if (c === "#" && !quote) { line = line.slice(0, i); break; } } if (line.trimStart().startsWith("[") && !/^\s*\[[^\]]+\]\s*$/.test(line)) throw new Error("invalid table header"); if (quote) throw new Error("unterminated string"); return line; }
function captureNormalize(path: string): { path: string; windows: boolean } | undefined { const p = path.replace(/\\/g, "/"); let root: string; let tail: string[]; if (p.startsWith("//")) { const x = p.slice(2).split("/").filter(Boolean); if (x.length < 2) return undefined; root = `//${x.shift()}/${x.shift()}`; tail = x; } else if (/^[A-Za-z]:\//.test(p)) { root = `${p[0].toUpperCase()}:/`; tail = p.slice(3).split("/"); } else if (p.startsWith("/")) { root = "/"; tail = p.slice(1).split("/"); } else return undefined; const out: string[] = []; for (const x of tail) { if (!x || x === ".") continue; if (x === "..") out.pop(); else out.push(x); } return { path: root + (out.length ? (root.endsWith("/") ? "" : "/") + out.join("/") : ""), windows: root !== "/" }; }
function captureJoin(base: string, child: string): string { if (/^[^A-Za-z]?:|^[A-Za-z]:[^/\\]/.test(child)) return child; return `${base.replace(/[\\/]+$/, "")}/${child}`; }
function captureValidGlob(p: string): boolean { return !!p && [...p].length <= CAPTURE_MAX_PATTERN_CHARS && !/[!{}\[\]()|^$%]/.test(p) && !p.includes("${") && !p.includes("***") && !p.replace(/\\/g, "/").split("/").includes("..") && (!p.startsWith("~") || p.startsWith("~/")) && !/^[^A-Za-z]?:/.test(p) && !/^[A-Za-z]:[^/\\]/.test(p); }
function captureParseArray(value: string): string[] | undefined { let i = 0; const out: string[] = []; const ws = () => { while (/\s/.test(value[i] ?? "")) i++; }; const basic = { b: "\b", t: "\t", n: "\n", f: "\f", r: "\r", '"': '"', "\\": "\\" } as Record<string, string>; ws(); if (value[i++] !== "[") return undefined; for (;;) { ws(); if (value[i] === "]") { i++; ws(); return i === value.length ? out : undefined; } const quote = value[i++]; if (quote !== '"' && quote !== "'") return undefined; let s = ""; for (;;) { if (i >= value.length) return undefined; const c = value[i++]; if (c === quote) break; if (c === "\\" && quote === '"') { const e = value[i++]; if (e in basic) s += basic[e]; else if (e === "u" || e === "U") { const count = e === "u" ? 4 : 8; const hex = value.slice(i, i + count); if (!new RegExp(`^[0-9A-Fa-f]{${count}}$`).test(hex)) return undefined; const n = Number.parseInt(hex, 16); if (n > 0x10ffff || (n >= 0xd800 && n <= 0xdfff)) return undefined; s += String.fromCodePoint(n); i += count; } else return undefined; } else if (c === "\n" || c === "\r") return undefined; else s += c; } out.push(s); ws(); if (value[i] === ",") { i++; continue; } if (value[i] === "]") continue; return undefined; } }
function captureConfig(cwd: string | undefined): CaptureConfig {
  const marker = findMarker(cwd);
  const candidateBase = captureNormalize(cwd ? resolve(cwd) : "")?.path ?? "";
  if (!marker) return { state: "inactive", patterns: [], base: candidateBase };
  try {
    const bytes = readFileSync(marker);
    const markerBase = captureNormalize(dirname(marker))?.path ?? candidateBase;
    if (bytes.byteLength > CAPTURE_MARKER_MAX_BYTES) return { state: "invalid", patterns: [], base: candidateBase };
    let section = "";
    let value = "";
    let collecting = false;
    let seen = false;
    for (const raw of bytes.toString("utf8").split(/\r?\n/)) {
      const line = captureTrimComment(raw).trim();
      if (!line) continue;
      const table = /^\[([^\]]+)\]$/.exec(line);
      if (table) {
        if (collecting) return { state: "invalid", patterns: [], base: candidateBase };
        section = table[1];
        continue;
      }
      if (section !== "capture") continue;
      if (!seen) {
        const kv = /^([A-Za-z0-9_-]+)\s*=\s*(.*)$/.exec(line);
        if (!kv || kv[1] !== "ignore_paths") return { state: "invalid", patterns: [], base: candidateBase };
        seen = true;
        value = kv[2];
        collecting = !value.includes("]");
      } else if (collecting) {
        value += ` ${line}`;
        collecting = !value.includes("]");
      } else return { state: "invalid", patterns: [], base: candidateBase };
    }
    if (collecting) return { state: "invalid", patterns: [], base: candidateBase };
    if (!seen) return { state: "inactive", patterns: [], base: candidateBase };
    const strings = captureParseArray(value);
    if (!strings || strings.length > CAPTURE_MAX_PATTERNS) return { state: "invalid", patterns: [], base: candidateBase };
    const home = homedir();
    const patterns = strings.map((source) => {
      if (!captureValidGlob(source)) return undefined;
      const expanded = source.startsWith("~/")
        ? captureJoin(home, source.slice(2))
        : /^(?:\/|\\\\|[A-Za-z]:[\\/])/.test(source)
          ? source
          : captureJoin(markerBase, source);
      const normalized = captureNormalize(expanded);
      if (!normalized) return undefined;
      return { path: normalized.path, windows: normalized.windows, directory: normalized.path.endsWith("/**") ? (normalized.path.slice(0, -3) || "/") : undefined };
    });
    if (patterns.some((p) => !p)) return { state: "invalid", patterns: [], base: candidateBase };
    return patterns.length
      ? { state: "active", patterns: patterns as CaptureConfig["patterns"], base: candidateBase }
      : { state: "inactive", patterns: [], base: candidateBase };
  } catch (_e) {
    return { state: "invalid", patterns: [], base: candidateBase };
  }
}
function captureGlob(pattern: string, candidate: string, insensitive: boolean, budget: { work: number }): boolean | undefined { const p = [...pattern]; const c = [...candidate]; const eq = (a: string, b: string) => insensitive && a.charCodeAt(0) < 128 && b.charCodeAt(0) < 128 ? a.toLowerCase() === b.toLowerCase() : a === b; const previous = new Array<boolean>(p.length + 1).fill(false); previous[0] = true; for (let j = 1; j <= p.length; j++) previous[j] = p[j - 1] === "*" && p[j] !== "*" && previous[j - 1]; for (const ch of c) { const current = new Array<boolean>(p.length + 1).fill(false); for (let j = 1; j <= p.length; j++) { if (++budget.work > CAPTURE_MAX_WORK) return undefined; const x = p[j - 1]; current[j] = x === "*" && p[j] === "*" ? false : x === "*" && j >= 2 && p[j - 2] === "*" ? current[j - 2] || previous[j] : x === "*" ? current[j - 1] || (ch !== "/" && previous[j]) : x === "?" ? ch !== "/" && previous[j - 1] : eq(x, ch) && previous[j - 1]; } for (let j = 0; j <= p.length; j++) previous[j] = current[j]; } return previous[p.length]; }
function captureTool(payload: Record<string, unknown>): { family: CaptureProtocol["tool_family"]; paths?: string[]; extraction: CaptureProtocol["extraction_state"]; callID?: string } { const name = typeof payload.tool === "string" ? payload.tool.toLowerCase() : ""; const args = payload.args as Record<string, unknown> | undefined; const call = ["tool_use_id","toolUseId","tool_call_id","toolCallId","call_id","callId","callID"].map((k) => payload[k]).find((v): v is string => typeof v === "string" && /^[A-Za-z0-9_.-]{1,128}$/.test(v)); if (["search","grep","glob","find","list","ls","list_files","read_dir"].includes(name)) return { family: "search-list", extraction: "not-applicable", callID: call }; if (["bash","shell","execute","run_command","web_search"].includes(name)) return { family: "non-file", extraction: "extracted", callID: call }; if (!["read","write","edit","apply_patch","notebookedit","notebook_edit","create_file","delete_file","rename_file","move_file","multi_edit","multiedit","replace","replace_all"].includes(name)) return { family: "unknown", extraction: "extracted", callID: call }; const direct = (o: any): string[] | undefined => { if (!o || typeof o !== "object") return undefined; const r: string[] = []; for (const k of ["file_path","filePath","path","absolute_path","AbsolutePath","notebook_path"]) if (k in o) { if (typeof o[k] !== "string") return undefined; r.push(o[k]); } if ("paths" in o) { if (!Array.isArray(o.paths) || o.paths.some((x: unknown) => typeof x !== "string")) return undefined; r.push(...o.paths); } return r.length && r.length <= CAPTURE_MAX_CANDIDATES ? r : undefined; }; let paths = direct(args); if (["multi_edit","multiedit","replace_all"].includes(name)) { const entries = args?.edits ?? args?.replacements; if (!Array.isArray(entries) || !entries.length || entries.length > CAPTURE_MAX_CANDIDATES) paths = undefined; else { paths = paths ?? []; for (const entry of entries) { const more = direct(entry); if (!more || paths.length + more.length > CAPTURE_MAX_CANDIDATES) { paths = undefined; break; } paths.push(...more); } } } if (!paths || paths.some((p) => !p.trim() || [...p].length > CAPTURE_MAX_PATH_CHARS)) return { family: "file", extraction: "missing-or-malformed", callID: call }; return { family: "file", paths, extraction: "extracted", callID: call }; }
function capturePolicy(payload: Record<string, unknown>, cwd: string | undefined): { disposition: CaptureDisposition; protocol?: CaptureProtocol; payload: Record<string, unknown> } { const config = captureConfig(cwd); const tool = captureTool(payload); let disposition: CaptureDisposition = "keep"; if (config.state === "invalid" && tool.family === "file") disposition = "metadata-only"; else if (config.state === "active" && tool.family === "search-list") disposition = "drop"; else if (config.state === "active" && tool.family === "file") { if (!tool.paths) disposition = "metadata-only"; else { const candidates = tool.paths.map((p) => captureNormalize(/^(?:\/|\\\\|[A-Za-z]:[\\/])/.test(p) ? p : captureJoin(config.base, p))); if (candidates.some((p) => !p)) disposition = "metadata-only"; else { const budget = { work: 0 }; captureMatch: for (const candidate of candidates as { path: string; windows: boolean }[]) for (const pattern of config.patterns) { if (candidate.windows !== pattern.windows) continue; if (pattern.directory && captureGlob(pattern.directory, candidate.path, pattern.windows, budget)) { disposition = "drop"; break captureMatch; } const match = captureGlob(pattern.path, candidate.path, pattern.windows, budget); if (match === undefined) { disposition = "metadata-only"; break; } if (match) { disposition = "drop"; break captureMatch; } } } } } if (config.state === "inactive") return { disposition, payload }; const protocol: CaptureProtocol = { version: CAPTURE_POLICY_V1, disposition, policy_state: config.state, tool_family: tool.family, path_count: tool.paths?.length ?? 0, extraction_state: tool.extraction }; if (disposition === "metadata-only") { const session = payload.sessionID ?? payload.sessionId ?? payload.session_id; const routing = typeof payload.cwd === "string" ? payload.cwd : cwd; return { disposition, protocol, payload: { ...(typeof session === "string" ? { session_id: session } : {}), ...(typeof routing === "string" ? { cwd: routing } : {}), tool_family: tool.family, tool_name: tool.family, ...(tool.callID ? { tool_call_id: tool.callID } : {}), _ai_memory_capture: protocol } }; } if (disposition === "keep") return { disposition, protocol, payload: { ...payload, _ai_memory_capture: protocol } }; return { disposition, protocol, payload }; }
"##
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
    build_hook_payload_for_platform(
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
    )
}

pub(crate) fn build_claude_code_payload_with_data_dir(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: Option<&Path>,
    project_strategy: Option<&str>,
    capture_assistant: bool,
) -> serde_json::Value {
    build_hook_payload_for_platform(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
        HookCommandContext::new(
            HookCommandPlatform::for_bash_runner(),
            "claude-code",
            data_dir,
            project_strategy,
        )
        .allow_claude_windows_exec()
        .with_capture_assistant(capture_assistant),
    )
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
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "ai-memory".to_string());
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
    /// Cursor: `"e": [ { "type":"command", "command":"...",
    /// "matcher":"" } ]` (no inner `hooks` array). Cursor's
    /// `hooks.json` also requires a sibling `version: 1` key at
    /// the top level — handled by the caller's apply path.
    Flat,
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
/// see `codex-rs/config/src/hooks_tests.rs`). Same shape as Claude
/// Code's six common events, EXCEPT: Codex has no `SessionEnd` (it
/// uses `Stop` for both turn-end and session-end signalling).
pub(crate) const CODEX_EVENTS: [(&str, &str); 6] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
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
    )
}

pub(crate) fn build_profile_payload_for_agent(
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
            HookCommandPlatform::current(),
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
            HookShape::Flat => Value::Array(vec![hook_handler_with_matcher(handler)]),
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
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "ai-memory".to_string());
            let event = script
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            let mut cmd = format!(
                "{}{} hook --event {event} --agent {agent} --server-url {}",
                win_double_quote(&exe),
                native_data_dir_arg(context.data_dir, NativeQuote::Windows),
                win_double_quote(server_url),
                agent = context.agent,
            );
            if let Some(t) = auth_token {
                cmd.push_str(&format!(" --auth-token {}", win_double_quote(t)));
            }
            cmd.push_str(&native_project_strategy_arg(
                context.project_strategy,
                NativeQuote::Windows,
            ));
            cmd.push_str(native_capture_assistant_arg(context, event));
            cmd
        }
        HookCommandPlatform::PosixNative => {
            // Native POSIX (opt-in): invoke the binary directly so the hook
            // gets the local spool + OIDC fallback, instead of the `.sh` script
            // that POSTs via curl. Mirrors `WindowsNative` but with POSIX
            // single-quote quoting. The event name is the script stem.
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "ai-memory".to_string());
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
    let exe = std::env::current_exe().unwrap_or_else(|_| Path::new("ai-memory.exe").to_path_buf());
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
fn shell_quote(s: &str) -> String {
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
fn win_double_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', ""))
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

    /// Manual Node-required runtime evidence for the exact TypeScript emitted by
    /// `ts_capture_policy_v1`. This deliberately executes the emitted source,
    /// rather than maintaining a JavaScript copy in the test suite.
    #[test]
    #[ignore = "manual Node-required generated TypeScript runtime evidence"]
    fn generated_capture_policy_v1_node_runtime_evidence() {
        let strip_types = Command::new("node")
            .args(["--experimental-strip-types", "--version"])
            .output();
        let Ok(strip_types) = strip_types else {
            eprintln!(
                "skipping Node-required runtime evidence: node lacks --experimental-strip-types"
            );
            return;
        };
        if !strip_types.status.success() {
            eprintln!(
                "skipping Node-required runtime evidence: node lacks --experimental-strip-types"
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let module = temp.path().join("capture-policy-runtime-evidence.ts");
        let fixture = include_str!("../../../ai-memory-hooks/tests/fixtures/capture-policy.json");
        let fixture = serde_json::to_string(fixture).unwrap();
        let source = format!(
            r#"import {{ closeSync, mkdirSync, openSync, readFileSync as readMarkerText, readSync, writeFileSync }} from "node:fs";
import {{ dirname, join, resolve }} from "node:path";
import {{ homedir }} from "node:os";

const fixtureText = {fixture};
const markerRoot = process.argv[2]!;
const markerFixtures = new Map<string, string>();
function findMarker(cwd: string | undefined): string | undefined {{ return cwd ? markerFixtures.get(cwd) : undefined; }}
{policy}

const fixture = JSON.parse(fixtureText);
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
  expectDecision(vector.payload, cwd, vector.disposition, vector.tool_family, vector.extraction_state, vector.path_count, `fixture-${{vector.agent}}`);
}}
for (const vector of fixture.normalization) {{
  const cwd = marker(`[capture]\nignore_paths = [${{JSON.stringify(vector.pattern)}}]\n`);
  expectDecision({{ tool: "edit", args: {{ path: vector.candidate }} }}, cwd, vector.match ? "drop" : "keep", "file", "extracted", 1, "fixture-normalization");
}}
expectDecision({{ tool: "edit", args: {{ path: "private/item" }} }}, marker('[capture]\nignore_paths = ["private/**"]\n'), "drop", "file", "extracted", 1, "marker-relative");
const nestedRoot = join(markerRoot, "nested-repo");
const nestedCwd = join(nestedRoot, "subdir");
mkdirSync(nestedCwd, {{ recursive: true }});
const nestedMarker = join(nestedRoot, ".ai-memory.toml");
writeFileSync(nestedMarker, '[capture]\nignore_paths = ["private/**"]\n');
markerFixtures.set(nestedCwd, nestedMarker);
expectDecision({{ tool: "edit", args: {{ path: "../private/item" }} }}, nestedCwd, "drop", "file", "extracted", 1, "nested-cwd-parent-private");
expectDecision({{ tool: "edit", args: {{ path: "private/item" }} }}, nestedCwd, "keep", "file", "extracted", 1, "nested-cwd-local-public");
expectDecision({{ tool: "edit", args: {{ path: `${{homedir()}}/home-private/item` }} }}, marker('[capture]\nignore_paths = ["~/home-private/**"]\n'), "drop", "file", "extracted", 1, "home-expansion");
expectDecision({{ tool: "edit", args: {{ path: "case/item" }} }}, marker('[capture]\nignore_paths = ["Case/**"]\n'), "keep", "file", "extracted", 1, "posix-case");
expectDecision({{ tool: "edit", args: {{ path: "c:/SECRET/item" }} }}, marker('[capture]\nignore_paths = ["C:/secret/**"]\n'), "drop", "file", "extracted", 1, "windows-drive-case");
expectDecision({{ tool: "edit", args: {{ path: "//SERVER/SHARE/item" }} }}, marker(`[capture]\nignore_paths = ['${{String.raw`\\server\share/**`}}']\n`), "drop", "file", "extracted", 1, "windows-unc-case");
expectDecision({{ tool: "edit", args: {{ path: "x" }} }}, marker('[capture'), "metadata-only", "file", "extracted", 1, "malformed-table");
const malformedQuoteCwd = marker('[capture]\nignore_paths = ["private/**');
expectDecision({{ tool: "edit", args: {{ path: privatePath }} }}, malformedQuoteCwd, "metadata-only", "file", "extracted", 1, "malformed-unterminated-quote");
const badUtf8Cwd = `/fixture/${{serial++}}`; const badUtf8 = join(markerRoot, "bad-utf8.toml"); writeFileSync(badUtf8, Buffer.from([0xff])); markerFixtures.set(badUtf8Cwd, badUtf8);
expectDecision({{ tool: "edit", args: {{ path: "x" }} }}, badUtf8Cwd, "metadata-only", "file", "extracted", 1, "invalid-utf8");
expectDecision({{ tool: "edit", args: {{ path: "x" }} }}, marker(`[capture]\nignore_paths = ["${{"x".repeat(65537)}}"]`), "metadata-only", "file", "extracted", 1, "large-marker");
expectDecision({{ tool: "edit", args: {{ path: "literal#item" }} }}, marker("[capture]\nignore_paths = [\n  'literal#item',\n  \"basic#item\"\n]\n"), "drop", "file", "extracted", 1, "multiline-and-hash");
expectDecision({{ tool: "edit", args: {{ path: "C:relative" }} }}, marker(active), "metadata-only", "file", "extracted", 1, "drive-relative");
expectDecision({{ tool: "future-tool", args: {{ path: "secret/a" }} }}, marker(active), "keep", "unknown", "extracted", 0, "unknown-tool");
const adversarial = "a".repeat(4090);
expectDecision({{ tool: "multi_edit", args: {{ edits: [{{ path: "secret/a" }}, {{ path: adversarial }}] }} }}, marker('[capture]\nignore_paths = ["secret/**", "*'.concat("a".repeat(1022), '"]\n')), "drop", "file", "extracted", 2, "first-drop-beats-budget");

const requests: string[] = []; const queue: Record<string, unknown>[] = [];
function emit(payload: Record<string, unknown>, cwd: string): void {{ const result = capturePolicy(payload, cwd); if (result.disposition === "drop") return; queue.push(result.payload); requests.push(JSON.stringify(result.payload)); }}
const gatedCwd = marker(`[capture]\nignore_paths = [${{JSON.stringify(privatePattern)}}]\n`);
emit({{ tool: "edit", args: {{ path: "/PRIVATE_PATTERN_SENTINEL/item", nested: {{ body: privateBody }} }}, output: privateBody, error: privateBody }}, gatedCwd);
check(queue.length === 0 && requests.length === 0, "drop-gates-queue-and-fetch");
emit({{ tool: "edit", args: {{ path: privatePath, nested: {{ body: privateBody }} }}, output: privateBody, error: privateBody }}, malformedQuoteCwd);
const malformedRequest = requests.at(-1) ?? "";
const malformedQueued = JSON.stringify(queue.at(-1));
check(JSON.parse(malformedRequest)._ai_memory_capture?.policy_state === "invalid", "malformed-quote-invalid-protocol");
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
const activeKeep = capturePolicy({{ tool: "bash", args: {{ command: privateBody }} }}, gatedCwd);
check(activeKeep.disposition === "keep" && activeKeep.protocol?.version === 1 && activeKeep.protocol.policy_state === "active", "active-keep-adds-protocol");
"#,
            fixture = fixture,
            policy = ts_capture_policy_v1(),
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
                HookShape::Nested => v.pointer("/hooks/SessionStart/0/hooks/0").unwrap().clone(),
                HookShape::Flat => v.pointer("/hooks/SessionStart/0").unwrap().clone(),
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

        let mut child = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &command,
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
