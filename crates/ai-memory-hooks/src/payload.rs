//! Wire envelope received on `POST /hook`.

use ai_memory_core::{AgentKind, ObservationKind};
use serde::{Deserialize, Serialize};

/// Query-string parameters on `POST /hook`.
#[derive(Debug, Clone, Deserialize)]
pub struct HookQuery {
    /// Lifecycle event identifier (kebab-case or snake_case).
    pub event: String,
    /// Agent CLI identifier (`claude-code`, `codex`, `open-code`, `omp`).
    pub agent: Option<String>,
}

/// Coalesced view of an incoming hook event after light parsing of the
/// body. We keep the original raw JSON around so consumers can extract
/// agent-specific fields they care about.
#[derive(Debug, Clone, Serialize)]
pub struct HookEnvelope {
    /// Mapped lifecycle event.
    pub event: HookEvent,
    /// Agent CLI identifier.
    pub agent: AgentKind,
    /// Session identifier, if found in the body. Required for everything
    /// except the initial `SessionStart`.
    pub session_id: Option<String>,
    /// Current working directory at the time of the event.
    pub cwd: Option<String>,
    /// Optional title hint extracted from the body.
    pub title_hint: Option<String>,
    /// Optional body excerpt extracted from the agent's raw payload.
    pub body_excerpt: Option<String>,
    /// The agent's raw JSON, kept for forensics.
    pub raw: serde_json::Value,
}

/// Discriminator for the lifecycle event that triggered the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    /// New session started (capture cwd + model).
    SessionStart,
    /// User submitted a prompt.
    UserPrompt,
    /// Agent is about to call a tool.
    PreToolUse,
    /// Agent finished a tool call.
    PostToolUse,
    /// Compaction event (context window pressure).
    PreCompact,
    /// Agent emitted a notification.
    Notification,
    /// Agent finished its turn (interactive `/stop` or natural end).
    Stop,
    /// Session ended (final).
    SessionEnd,
    /// Anything else.
    Other,
}

impl HookEvent {
    /// Parse a kebab- or snake-case event identifier into [`HookEvent`].
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "session-start" | "session_start" | "SessionStart" => Self::SessionStart,
            "user-prompt" | "user_prompt" | "UserPromptSubmit" => Self::UserPrompt,
            "pre-tool-use" | "pre_tool_use" | "PreToolUse" => Self::PreToolUse,
            "post-tool-use" | "post_tool_use" | "PostToolUse" => Self::PostToolUse,
            "pre-compact" | "pre_compact" | "PreCompact" => Self::PreCompact,
            "notification" | "Notification" => Self::Notification,
            "stop" | "Stop" => Self::Stop,
            "session-end" | "session_end" | "SessionEnd" => Self::SessionEnd,
            _ => Self::Other,
        }
    }

    /// Map to the storage-level [`ObservationKind`].
    #[must_use]
    pub const fn to_observation_kind(self) -> ObservationKind {
        match self {
            Self::SessionStart => ObservationKind::SessionStart,
            Self::UserPrompt => ObservationKind::UserPrompt,
            Self::PreToolUse => ObservationKind::PreToolUse,
            Self::PostToolUse => ObservationKind::PostToolUse,
            Self::PreCompact => ObservationKind::PreCompact,
            Self::Notification => ObservationKind::Notification,
            Self::Stop => ObservationKind::Stop,
            Self::SessionEnd => ObservationKind::SessionEnd,
            Self::Other => ObservationKind::Other,
        }
    }
}

/// Parse an agent identifier into [`AgentKind`]. Unknown values map to
/// [`AgentKind::Other`].
#[must_use]
pub fn parse_agent(s: &str) -> AgentKind {
    match s {
        "claude-code" | "claude_code" | "claude" => AgentKind::ClaudeCode,
        "codex" => AgentKind::Codex,
        "open-code" | "opencode" => AgentKind::OpenCode,
        "omp" | "pi" | "oh-my-pi" => AgentKind::Omp,
        _ => AgentKind::Other,
    }
}

impl HookEnvelope {
    /// Build an envelope from the parsed query + the body JSON. Performs
    /// best-effort extraction of `session_id` / `cwd` / a body excerpt
    /// from common shapes used by Claude Code, Codex, and OpenCode hook
    /// payloads.
    #[must_use]
    pub fn from_query_and_body(query: HookQuery, raw: serde_json::Value) -> Self {
        let event = HookEvent::parse(&query.event);
        let agent = query.agent.as_deref().map_or(AgentKind::Other, parse_agent);
        // OpenCode's plugin SDK sends `sessionID` (capital `ID`) on the
        // tool.execute.*/session.* events; Claude Code uses `session_id`,
        // Codex `sessionId`. JSON keys are case-sensitive, so all three
        // spellings must be listed or OpenCode tool events fail the
        // "missing session_id" check in the router (issue #1).
        let session_id = extract_string(&raw, &["session_id", "sessionId", "sessionID", "session"])
            .or_else(|| {
                extract_string_path(
                    &raw,
                    &[
                        &["info", "id"],
                        &["properties", "sessionID"],
                        &["properties", "info", "id"],
                        &["event", "properties", "sessionID"],
                        &["event", "properties", "info", "id"],
                        &["payload", "info", "id"],
                        &["payload", "properties", "sessionID"],
                        &["payload", "properties", "info", "id"],
                    ],
                )
            });
        let cwd = extract_string(&raw, &["cwd", "current_dir", "working_dir", "directory"])
            .or_else(|| {
                extract_string_path(
                    &raw,
                    &[
                        &["path", "cwd"],
                        &["info", "directory"],
                        &["properties", "info", "directory"],
                        &["event", "properties", "info", "directory"],
                        &["payload", "path", "cwd"],
                        &["payload", "info", "directory"],
                        &["payload", "properties", "info", "directory"],
                    ],
                )
            });
        let title_hint = best_title_hint(event, &raw);
        let body_excerpt = best_body_excerpt(event, &raw);
        Self {
            event,
            agent,
            session_id,
            cwd,
            title_hint,
            body_excerpt,
            raw,
        }
    }
}

fn extract_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        for candidate in extraction_candidates(value) {
            if let Some(s) = candidate.get(*key).and_then(serde_json::Value::as_str)
                && !s.is_empty()
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn extract_string_path(value: &serde_json::Value, paths: &[&[&str]]) -> Option<String> {
    for path in paths {
        if let Some(s) = value_at_path(value, path).and_then(serde_json::Value::as_str)
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    None
}

fn value_at_path<'a>(
    mut value: &'a serde_json::Value,
    path: &[&str],
) -> Option<&'a serde_json::Value> {
    for segment in path {
        value = value.get(*segment)?;
    }
    Some(value)
}

fn extraction_candidates(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    let mut out = Vec::new();
    push_candidates(&mut out, value);
    if let Some(payload) = value.get("payload") {
        push_candidates(&mut out, payload);
    }
    if let Some(event) = value.get("event") {
        push_candidates(&mut out, event);
    }
    out
}

fn push_candidates<'a>(out: &mut Vec<&'a serde_json::Value>, value: &'a serde_json::Value) {
    out.push(value);
    if let Some(properties) = value.get("properties") {
        out.push(properties);
        if let Some(info) = properties.get("info") {
            out.push(info);
        }
    }
    if let Some(info) = value.get("info") {
        out.push(info);
    }
    if let Some(path) = value.get("path") {
        out.push(path);
    }
}

fn best_title_hint(event: HookEvent, raw: &serde_json::Value) -> Option<String> {
    match event {
        HookEvent::SessionStart => extract_string(raw, &["model", "title"]),
        HookEvent::UserPrompt => {
            extract_string(raw, &["prompt", "message", "text"]).map(|s| truncate_for_title(&s))
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => {
            extract_string(raw, &["tool", "tool_name", "name"])
        }
        HookEvent::Notification => extract_string(raw, &["message", "text"]),
        _ => None,
    }
}

fn best_body_excerpt(event: HookEvent, raw: &serde_json::Value) -> Option<String> {
    match event {
        HookEvent::UserPrompt => extract_string(raw, &["prompt", "message", "text"]),
        HookEvent::PostToolUse => {
            let tool = extract_string(raw, &["tool", "tool_name", "name"])?;
            let result = extract_string(raw, &["tool_response", "tool_output", "output", "result"])
                .unwrap_or_else(|| "(no output captured)".into());
            Some(format!("tool: {tool}\n---\n{}", truncate_excerpt(&result)))
        }
        HookEvent::Notification => extract_string(raw, &["message", "text"]),
        _ => None,
    }
}

fn truncate_for_title(s: &str) -> String {
    const MAX: usize = 80;
    let one_line: String = s.chars().take_while(|c| *c != '\n').collect();
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        let mut buf: String = one_line.chars().take(MAX - 1).collect();
        buf.push('…');
        buf
    }
}

fn truncate_excerpt(s: &str) -> String {
    const MAX: usize = 2_000;
    if s.len() <= MAX {
        s.to_string()
    } else {
        let mut buf = String::with_capacity(MAX + '…'.len_utf8());
        let mut end = 0;
        for (idx, ch) in s.char_indices() {
            let next = idx + ch.len_utf8();
            if next > MAX {
                break;
            }
            end = next;
        }
        buf.push_str(&s[..end]);
        buf.push('…');
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_events() {
        assert_eq!(HookEvent::parse("session-start"), HookEvent::SessionStart);
        assert_eq!(HookEvent::parse("PreToolUse"), HookEvent::PreToolUse);
        assert_eq!(HookEvent::parse("user_prompt"), HookEvent::UserPrompt);
        assert_eq!(HookEvent::parse("bogus"), HookEvent::Other);
    }

    #[test]
    fn maps_to_observation_kind() {
        assert_eq!(
            HookEvent::SessionEnd.to_observation_kind(),
            ObservationKind::SessionEnd
        );
    }

    #[test]
    fn envelope_extracts_session_and_cwd() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("claude-code".into()),
        };
        let cwd_val = if cfg!(windows) { r"C:\tmp\x" } else { "/tmp/x" };
        let raw = serde_json::json!({
            "session_id": "abc-123",
            "cwd": cwd_val,
            "model": "claude-sonnet-4-6"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.event, HookEvent::SessionStart);
        assert_eq!(env.session_id.as_deref(), Some("abc-123"));
        assert_eq!(env.cwd.as_deref(), Some(cwd_val));
        assert_eq!(env.title_hint.as_deref(), Some("claude-sonnet-4-6"));
    }

    /// OpenCode's plugin SDK sends `sessionID` (capital `ID`) on the
    /// tool.execute.* / session.* events. Regression for issue #1: this
    /// spelling must be extracted, otherwise non-session-start events
    /// fail the router's "missing session_id" check.
    #[test]
    fn envelope_extracts_opencode_camelcase_session_id() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("open-code".into()),
        };
        let raw = serde_json::json!({
            "sessionID": "ses_abc123",
            "tool": "bash",
            "callID": "call_1"
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_abc123"));
    }

    /// Earlier OpenCode plugin generation wrapped the actual SDK hook
    /// input under `payload`. Keep accepting that shape so users with
    /// an old plugin don't silently lose project routing until they
    /// restart with the fixed plugin.
    #[test]
    fn envelope_extracts_legacy_opencode_nested_payload() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("open-code".into()),
        };
        let raw = serde_json::json!({
            "hook_event_name": "post-tool-use",
            "agent": "open-code",
            "payload": {
                "sessionID": "ses_nested",
                "cwd": "/home/user/ai-memory",
                "tool": "bash",
                "output": "tests passed"
            }
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_nested"));
        assert_eq!(env.cwd.as_deref(), Some("/home/user/ai-memory"));
        assert_eq!(env.title_hint.as_deref(), Some("bash"));
        assert!(
            env.body_excerpt
                .as_deref()
                .is_some_and(|body| body.contains("tests passed")),
            "post-tool body should include nested output: {:?}",
            env.body_excerpt
        );
    }

    /// OpenCode's plugin `event` hook receives bus events shaped like
    /// `{ event: { type, properties } }`; session creation carries the
    /// cwd as `properties.info.directory`.
    #[test]
    fn envelope_extracts_opencode_bus_event_session_info() {
        let q = HookQuery {
            event: "session-start".into(),
            agent: Some("open-code".into()),
        };
        let raw = serde_json::json!({
            "event": {
                "type": "session.created",
                "properties": {
                    "sessionID": "ses_bus",
                    "info": {
                        "id": "ses_bus",
                        "directory": "/home/user/ai-memory",
                        "title": "New session"
                    }
                }
            }
        });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.session_id.as_deref(), Some("ses_bus"));
        assert_eq!(env.cwd.as_deref(), Some("/home/user/ai-memory"));
        assert_eq!(env.title_hint.as_deref(), Some("New session"));
    }

    /// Alternative agent-name spellings all map to the same canonical
    /// AgentKind. The hook scripts and the test e2e shim send slightly
    /// different strings for historical reasons; this asserts we
    /// remain forgiving.
    #[test]
    fn agent_name_aliases_all_map_correctly() {
        assert_eq!(parse_agent("claude-code"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("claude_code"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("claude"), AgentKind::ClaudeCode);
        assert_eq!(parse_agent("codex"), AgentKind::Codex);
        assert_eq!(parse_agent("opencode"), AgentKind::OpenCode);
        assert_eq!(parse_agent("open-code"), AgentKind::OpenCode);
        assert_eq!(parse_agent("omp"), AgentKind::Omp);
        assert_eq!(parse_agent("pi"), AgentKind::Omp);
        assert_eq!(parse_agent("oh-my-pi"), AgentKind::Omp);
        // Anything else is `Other`. Critical for the hook router:
        // a typo in the query string must not crash, it just gets
        // attributed to the catch-all bucket.
        assert_eq!(parse_agent(""), AgentKind::Other);
        assert_eq!(parse_agent("CLAUDE-CODE"), AgentKind::Other); // case-sensitive on purpose
        assert_eq!(parse_agent("gemini-cli"), AgentKind::Other);
        assert_eq!(parse_agent("../../etc/passwd"), AgentKind::Other);
    }

    /// An empty body is legitimate (some hook events carry no
    /// payload). Envelope extraction must produce sane defaults
    /// rather than panicking.
    #[test]
    fn envelope_tolerates_empty_body() {
        let q = HookQuery {
            event: "stop".into(),
            agent: Some("claude-code".into()),
        };
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({}));
        assert_eq!(env.event, HookEvent::Stop);
        assert!(env.session_id.is_none());
        assert!(env.cwd.is_none());
        assert!(env.title_hint.is_none());
        assert!(env.body_excerpt.is_none());
    }

    /// Body is well-formed JSON but the expected `session_id` /
    /// `cwd` keys are missing — extraction returns None per key.
    #[test]
    fn envelope_missing_expected_fields() {
        let q = HookQuery {
            event: "user-prompt".into(),
            agent: Some("claude-code".into()),
        };
        let raw = serde_json::json!({ "garbage": 42 });
        let env = HookEnvelope::from_query_and_body(q, raw);
        assert_eq!(env.event, HookEvent::UserPrompt);
        assert!(env.session_id.is_none());
        assert!(env.cwd.is_none());
    }

    /// Body is a JSON primitive (string / null / number) rather
    /// than an object. The extractors must short-circuit cleanly.
    /// This guards against an upstream that POSTs a stringified
    /// payload by mistake.
    #[test]
    fn envelope_accepts_non_object_body() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
        };
        for raw in [
            serde_json::json!(null),
            serde_json::json!("a stringy payload"),
            serde_json::json!(42),
            serde_json::json!([1, 2, 3]),
        ] {
            let env = HookEnvelope::from_query_and_body(q.clone(), raw);
            assert!(
                env.session_id.is_none(),
                "no session_id from non-object body"
            );
            assert!(env.cwd.is_none(), "no cwd from non-object body");
        }
    }

    /// Empty `agent` query param maps to Other (rather than panic
    /// or default to ClaudeCode). The hook router uses this for the
    /// attribution column, so we want it consistent.
    #[test]
    fn missing_agent_query_param_maps_to_other() {
        let q = HookQuery {
            event: "session-end".into(),
            agent: None,
        };
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({}));
        assert_eq!(env.agent, AgentKind::Other);
    }

    /// Title-hint extraction must truncate at the first newline (the
    /// "first line" rule used everywhere in the wiki log + handoff
    /// surfaces) and cap at 80 chars to keep observation titles
    /// scannable in the log.md heading.
    #[test]
    fn user_prompt_title_truncates_at_newline_and_at_max_chars() {
        let q = HookQuery {
            event: "user-prompt".into(),
            agent: Some("claude-code".into()),
        };
        // Multi-line prompt → title is the first line only.
        let env = HookEnvelope::from_query_and_body(
            q.clone(),
            serde_json::json!({ "prompt": "first line\nsecond line should be lost" }),
        );
        assert_eq!(env.title_hint.as_deref(), Some("first line"));

        // Very long single line → truncated with ellipsis.
        let long = "x".repeat(200);
        let env = HookEnvelope::from_query_and_body(q, serde_json::json!({ "prompt": long }));
        let title = env.title_hint.unwrap();
        assert!(title.chars().count() <= 80);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn post_tool_excerpt_truncates_without_splitting_utf8() {
        let q = HookQuery {
            event: "post-tool-use".into(),
            agent: Some("claude-code".into()),
        };
        let output = format!("{}é", "x".repeat(1_999));
        let env = HookEnvelope::from_query_and_body(
            q,
            serde_json::json!({
                "tool": "bash",
                "result": output,
            }),
        );
        let excerpt = env.body_excerpt.unwrap();
        assert!(excerpt.ends_with('…'));
        assert!(excerpt.starts_with("tool: bash\n---\n"));
    }
}
