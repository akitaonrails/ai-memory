//! Shared rendering helpers for the install-* / setup-agent commands.
//!
//! These three subcommands (`install-hooks`, `install-mcp`,
//! `setup-agent`) all emit configuration snippets that share two
//! pieces of state:
//!
//! 1. The seven Claude Code lifecycle-hook events ai-memory wires
//!    up — kept in sync between hook-bundle generation (setup-agent)
//!    and JSON-config rendering (install-hooks).
//! 2. The optional `Authorization: Bearer <token>` header used by
//!    both MCP client configs (install-mcp) and hook env blocks
//!    (install-hooks / setup-agent).
//!
//! Each subcommand still owns its per-client output formatting (the
//! commentary that frames the JSON snippet differs from client to
//! client and is the part that makes the printout readable). What
//! lives here is only the *data* both consume.

use std::path::Path;

use serde_json::json;

/// Claude Code lifecycle events ai-memory hooks. Each pair is
/// `(event-name-in-Claude-Code-settings, hook-script-filename)`.
///
/// Adding a hook event means updating this list AND adding the
/// matching `hooks/{claude-code,codex,cursor,gemini-cli,opencode}/<filename>` script —
/// the e2e test + the generator in `bin/regen-hooks` (if added) both
/// key off this constant.
pub(crate) const CLAUDE_CODE_EVENTS: [(&str, &str); 7] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PreToolUse", "pre-tool-use.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("PreCompact", "pre-compact.sh"),
    ("Stop", "stop.sh"),
    ("SessionEnd", "session-end.sh"),
];

/// Return the platform-appropriate script filename. On Windows the
/// hook runner invokes PowerShell `.ps1` scripts; everywhere else
/// POSIX `.sh` scripts.
pub(crate) fn platform_script_name(sh_name: &str) -> String {
    if cfg!(windows) {
        sh_name.replace(".sh", ".ps1")
    } else {
        sh_name.to_string()
    }
}

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

/// Build the Claude Code `settings.json` fragment that wires the
/// seven hooks. Used by both:
/// - `install-hooks --agent claude-code` (script paths are
///   wherever the user told us via `--hooks-dir`)
/// - `setup-agent --agent claude-code` (script paths are where
///   `--host-prefix` says they'll live on the host)
///
/// `emit_root` is the directory that will contain `*.sh`; it is
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
    build_hook_payload(
        &CLAUDE_CODE_EVENTS,
        emit_root,
        server_url,
        auth_token,
        HookShape::Nested,
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
    /// Cursor uses `sessionStart`. The script basename resolves
    /// against `hooks/<agent-dir>/`.
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
pub(crate) const CURSOR_EVENTS: [(&str, &str); 7] = [
    ("sessionStart", "session-start.sh"),
    ("sessionEnd", "session-end.sh"),
    ("beforeSubmitPrompt", "user-prompt-submit.sh"),
    ("preToolUse", "pre-tool-use.sh"),
    ("postToolUse", "post-tool-use.sh"),
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

/// Build a Codex-flavoured hook payload. Thin alias for back-compat;
/// new code should call `build_profile_payload(&CODEX_PROFILE, …)`.
pub(crate) fn build_codex_payload(
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_profile_payload(&CODEX_PROFILE, emit_root, server_url, auth_token)
}

/// Build a hook payload for `profile`. The output is always
/// `{ "hooks": { "<EventName>": <profile-specific-array> } }`; the
/// caller is responsible for any sibling top-level keys (e.g.
/// Cursor's `"version": 1`).
pub(crate) fn build_profile_payload(
    profile: &HookProfile,
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> serde_json::Value {
    build_hook_payload(
        profile.events,
        emit_root,
        server_url,
        auth_token,
        profile.shape,
    )
}

fn build_hook_payload(
    events: &[(&str, &str)],
    emit_root: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    shape: HookShape,
) -> serde_json::Value {
    let mut hooks_block = serde_json::Map::new();
    for (event, script) in events {
        let actual_script = platform_script_name(script);
        let abs = emit_root.join(&actual_script);

        // Claude Code's hook schema (per
        // https://code.claude.com/docs/en/hooks):
        //   "<EventName>": [
        //     { "matcher": "<tool-name regex or empty>",
        //       "hooks": [ { "type": "command", "command": "..." } ]
        //     }
        //   ]
        //
        // We INLINE env vars into the command string itself
        // rather than passing them through an `env` field on the
        // hook entry. Reasons:
        //   1. CC doesn't appear to honour an `env` field at this
        //      level — observed empirically: the hook fires but
        //      the script sees neither var and falls back to the
        //      127.0.0.1 default, so POSTs go nowhere.
        //   2. Inlining the env into the command string is
        //      portable across any shell-style hook runner — POSIX
        //      `VAR=val command` syntax is universally honoured;
        //      on Windows we use `$env:VAR='val'; & powershell …`.
        //   3. The hook scripts already read those env vars (see
        //      `hooks/claude-code/session-start.sh` etc.), so no
        //      script changes are required.
        let command = if cfg!(windows) {
            let mut parts = Vec::new();
            parts.push(format!(
                "$env:AI_MEMORY_HOOK_URL='{}'",
                server_url.replace('\'', "''")
            ));
            if let Some(t) = auth_token {
                parts.push(format!(
                    "$env:AI_MEMORY_AUTH_TOKEN='{}'",
                    t.replace('\'', "''")
                ));
            }
            parts.push(format!(
                "& powershell -NoProfile -File '{}'",
                abs.to_string_lossy().replace('\'', "''")
            ));
            parts.join("; ")
        } else {
            let mut prefix = format!("AI_MEMORY_HOOK_URL={} ", shell_quote(server_url));
            if let Some(t) = auth_token {
                prefix.push_str(&format!("AI_MEMORY_AUTH_TOKEN={} ", shell_quote(t)));
            }
            format!("{prefix}{}", abs.to_string_lossy())
        };

        // Empty matcher = fire on every event of this kind. Right
        // for ai-memory's capture hooks (every prompt, every tool
        // call, every session boundary).
        let entry = match shape {
            HookShape::Nested => json!([{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": command,
                }],
            }]),
            HookShape::Flat => json!([{
                "type": "command",
                "command": command,
                "matcher": "",
            }]),
        };
        hooks_block.insert((*event).to_string(), entry);
    }
    json!({ "hooks": hooks_block })
}

/// Minimal shell quoting for embedding values into a `VAR=val cmd`
/// prefix. Wraps in single quotes; embedded `'` is escaped via
/// `'\''`. Safe for the URLs and bearer tokens we embed (no
/// realistic value contains anything else weird).
fn shell_quote(s: &str) -> String {
    if !s.contains(['\'', ' ', '"', '$', '`', '\\']) {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bearer_header_is_none_when_no_token() {
        assert!(bearer_header_value(None).is_none());
    }

    #[test]
    fn bearer_header_prefixes_with_bearer() {
        let h = bearer_header_value(Some("abc123")).unwrap();
        assert_eq!(h, "Bearer abc123");
    }

    #[test]
    fn claude_code_payload_has_seven_events() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\host\hooks\claude-code")
        } else {
            PathBuf::from("/host/hooks/claude-code")
        };
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let hooks = v.get("hooks").and_then(|h| h.as_object()).unwrap();
        assert_eq!(hooks.len(), 7);
        for (event, _) in CLAUDE_CODE_EVENTS {
            assert!(hooks.contains_key(event), "missing event {event}");
        }
    }

    #[test]
    fn claude_code_payload_embeds_auth_token_when_provided() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\host\hooks\claude-code")
        } else {
            PathBuf::from("/host/hooks/claude-code")
        };
        let v = build_claude_code_payload(&root, "http://localhost:49374", Some("tok"));
        // Env vars are inlined into the command string so CC's
        // hook runner sees them regardless of whether it honours
        // a separate `env` field. Assert the token landed in the
        // command prefix.
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(
            command.contains("AI_MEMORY_AUTH_TOKEN") && command.contains("tok"),
            "command should inline the auth token; got: {command}"
        );
        assert!(
            command.contains("AI_MEMORY_HOOK_URL") && command.contains("http://localhost:49374"),
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
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\host\hooks\cursor")
        } else {
            PathBuf::from("/host/hooks/cursor")
        };
        let v = build_profile_payload(
            &CURSOR_PROFILE,
            &root,
            "http://localhost:49374",
            Some("tok"),
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
        assert!(
            cmd.contains("AI_MEMORY_AUTH_TOKEN") && cmd.contains("tok"),
            "command should inline the auth token; got: {cmd}"
        );
        // Events are camelCase, not PascalCase.
        let events: Vec<&str> = v
            .pointer("/hooks")
            .and_then(|h| h.as_object())
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        assert!(events.contains(&"sessionStart"));
        assert!(events.contains(&"preToolUse"));
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
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\host\hooks\gemini-cli")
        } else {
            PathBuf::from("/host/hooks/gemini-cli")
        };
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
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\host\hooks\claude-code")
        } else {
            PathBuf::from("/host/hooks/claude-code")
        };
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
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\host\hooks\claude-code")
        } else {
            PathBuf::from("/host/hooks/claude-code")
        };
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let command = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(command.contains("AI_MEMORY_HOOK_URL"));
        assert!(
            !command.contains("AI_MEMORY_AUTH_TOKEN"),
            "no token expected in command: {command}"
        );
    }

    #[test]
    fn claude_code_payload_emits_absolute_paths() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\Users\user\.ai-memory\hooks\claude-code")
        } else {
            PathBuf::from("/home/user/.ai-memory/hooks/claude-code")
        };
        let v = build_claude_code_payload(&root, "http://localhost:49374", None);
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        let script_name = platform_script_name("session-start.sh");
        let expected_path = root.join(&script_name).to_string_lossy().to_string();
        assert!(
            cmd.contains(&expected_path),
            "command should contain the absolute script path: {cmd}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn claude_code_payload_emits_powershell_commands_on_windows() {
        let root = PathBuf::from(r"C:\Users\user\.ai-memory\hooks\claude-code");
        let v = build_claude_code_payload(&root, "http://localhost:49374", Some("tok"));
        let cmd = v
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(|s| s.as_str())
            .unwrap();
        assert!(
            cmd.contains("$env:AI_MEMORY_HOOK_URL="),
            "Windows command should use $env: syntax; got: {cmd}"
        );
        assert!(
            cmd.contains("powershell"),
            "Windows command should invoke powershell; got: {cmd}"
        );
        assert!(
            cmd.contains(".ps1"),
            "Windows command should reference .ps1 script; got: {cmd}"
        );
    }
}
