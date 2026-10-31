//! Server-side ingest drop list (`[ingest]`, #1241).
//!
//! An operator can name what the server should never store, whatever the
//! client sends: tool calls by tool-use id prefix (Claude Code marks plugin
//! tool calls with their own id prefix) and tool names exactly, plus whole
//! hook event kinds (`drop_events`). A matching event is acknowledged like
//! any other policy drop. All lists run right after the subagent exclusion in
//! both the single and the batch ingress, so a marked event always seeds tail
//! tracking before a policy matcher can drop it, and before it reaches the rate
//! limiter, the ingest semaphore, the sanitizer or storage. The lists are read
//! once at startup and bounded, so the checks stay cheap on the hook path.

use crate::payload::{HookEnvelope, HookEvent};

/// Most entries either list may hold.
pub const MAX_DROP_ENTRIES: usize = 64;
/// Longest entry, in characters.
pub const MAX_DROP_ENTRY_CHARS: usize = 128;

/// Tool events the server drops by tool-use id prefix or exact tool name,
/// plus whole hook event kinds dropped outright (`drop_events`). A matching
/// event is acknowledged like any other policy drop, before it reaches the
/// sanitizer, the rate limiter, the ingest semaphore or storage. The lists
/// are read once at startup and bounded, so the checks stay cheap on the hook
/// path.
///
/// Both checks run after the subagent exclusion, so a marked subagent event
/// still seeds the tracking set its unmarked tail is dropped against even when
/// its tool id, tool name, or event kind is listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestDropList {
    tool_use_id_prefixes: Vec<String>,
    tool_names: Vec<String>,
    drop_events: IngestDropEvents,
}

impl IngestDropList {
    /// Validate and build the list.
    ///
    /// # Errors
    /// Returns a message naming the offending list when either holds more than
    /// [`MAX_DROP_ENTRIES`] entries, or an entry that is blank, longer than
    /// [`MAX_DROP_ENTRY_CHARS`] characters, or contains a control character.
    /// A `drop_events` entry that is not a droppable hook event kind is
    /// rejected with a message naming it.
    pub fn new(
        tool_use_id_prefixes: Vec<String>,
        tool_names: Vec<String>,
        drop_events: &[String],
    ) -> Result<Self, String> {
        validate("drop_tool_use_id_prefixes", &tool_use_id_prefixes)?;
        validate("drop_tool_names", &tool_names)?;
        let drop_events = IngestDropEvents::new(drop_events)?;
        Ok(Self {
            tool_use_id_prefixes,
            tool_names,
            drop_events,
        })
    }

    /// Whether this event's tool id or name is on the tool drop lists.
    ///
    /// Matches only tool events (`pre-tool-use` / `post-tool-use`); whole-kind
    /// drops (`drop_events`) are checked by [`Self::matches_event_kind`].
    #[must_use]
    pub fn matches(&self, env: &HookEnvelope) -> bool {
        if self.tool_use_id_prefixes.is_empty() && self.tool_names.is_empty() {
            return false;
        }
        if !matches!(env.event, HookEvent::PreToolUse | HookEvent::PostToolUse) {
            return false;
        }
        // The per-agent mapping the observation metadata uses knows where
        // each harness puts the call id (`callID`, `extra.tool_call_id`, ...);
        // an agent without one falls back to the generic Claude-style fields.
        let (name, id) = match crate::capture_policy::tool_payload_fields(env.agent, &env.raw) {
            Some((name, id)) => (Some(name.to_owned()), id.map(str::to_owned)),
            None => (
                crate::payload::event_tool_name(&env.raw),
                crate::payload::event_tool_use_id(&env.raw),
            ),
        };
        if let Some(id) = id.as_deref()
            && self
                .tool_use_id_prefixes
                .iter()
                .any(|prefix| id.starts_with(prefix.as_str()))
        {
            return true;
        }
        name.is_some_and(|name| self.tool_names.contains(&name))
    }

    /// Whether this event's whole kind is dropped by `[ingest] drop_events`.
    ///
    /// Unlike [`Self::matches`] this ignores the tool id/name and looks only
    /// at the event kind, so it applies to every tool, agent and payload
    /// shape. The router runs it right after the subagent exclusion (so a
    /// marked subagent event still seeds the tail tracking) and before the
    /// rate limiter, the ingest semaphore, the sanitizer and storage.
    #[must_use]
    pub fn matches_event_kind(&self, env: &HookEnvelope) -> bool {
        self.drop_events.matches(env)
    }
}

/// Hook event kinds the server drops outright (`[ingest] drop_events`).
///
/// Unlike [`IngestDropList`] (which matches on a tool's id or name within a
/// tool event), this drops a whole event *kind* whatever the client sends. The
/// motivating case is `pre-tool-use`: on claude-code its body is at most
/// `tool_family`/`tool_call_id` and is empty when the call carries no
/// `tool_input`, while its 1:1 `post-tool-use` pair carries the content. An
/// operator who wants to stop storing the redundant half can name the kind
/// here. A matching event is acknowledged like any other policy drop, before
/// it reaches the sanitizer, the rate limiter, the ingest semaphore or
/// storage. The set is read once at startup and bounded, so the check stays
/// cheap on the hook path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestDropEvents {
    kinds: Vec<HookEvent>,
}

/// The droppable kinds, as their canonical kebab-case wire names, for error
/// and config docs.
const DROPPABLE_KINDS: &str = "user-prompt, pre-tool-use, post-tool-use, \
     pre-compact, post-compaction, notification, stop, subagent-start, \
     subagent-stop";

/// Whether an event kind may be named in `[ingest] drop_events`.
///
/// `session-start` and `session-end` are excluded: the router uses them to
/// open and close a session, so dropping them would strand the session state
/// machine (a session never opened, or never ended). The unrecognized-event
/// bucket (`other`) is excluded too: naming it would drop every event without
/// a typed kind — a much broader drop than an operator asking for one kind
/// intends. A kind not listed here is intentionally non-droppable: when a new
/// variant is added to [`HookEvent`], the reviewer must consciously decide
/// whether it may be dropped rather than have it silently join the set.
const fn is_droppable_event(event: HookEvent) -> bool {
    matches!(
        event,
        HookEvent::UserPrompt
            | HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::PreCompact
            | HookEvent::PostCompaction
            | HookEvent::Notification
            | HookEvent::Stop
            | HookEvent::SubagentStart
            | HookEvent::SubagentStop
    )
}

impl IngestDropEvents {
    /// Validate and build the drop set.
    ///
    /// Each value is parsed with the typed [`HookEvent::parse`] (never a free
    /// string compare). Duplicates collapse to one kind. The set is bounded by
    /// the number of droppable kinds, so no separate size limit is needed.
    ///
    /// # Errors
    /// Returns a message naming the offending value when it is blank, not a
    /// known hook event kind, or a kind that drives session state.
    pub fn new(raw: &[String]) -> Result<Self, String> {
        let mut kinds: Vec<HookEvent> = Vec::with_capacity(raw.len());
        for value in raw {
            if value.trim().is_empty() {
                return Err("[ingest].drop_events has a blank entry".to_string());
            }
            let event = HookEvent::parse(value);
            if !is_droppable_event(event) {
                let reason = match event {
                    HookEvent::SessionStart | HookEvent::SessionEnd => {
                        "drives session state (the router opens and closes sessions with it) \
                         and cannot be dropped"
                    }
                    HookEvent::Other => "is not a known hook event kind",
                    // A kind outside the droppable set that is neither
                    // session-state nor the unrecognized bucket: currently
                    // none, but a newly added enum variant lands here so the
                    // failure is explicit rather than silently droppable.
                    _ => "cannot be dropped",
                };
                return Err(format!(
                    "[ingest].drop_events: {value} {reason}. Droppable kinds: \
                     {DROPPABLE_KINDS}"
                ));
            }
            if !kinds.contains(&event) {
                kinds.push(event);
            }
        }
        Ok(Self { kinds })
    }

    /// Whether this event's kind is dropped. Unlike [`IngestDropList::matches`]
    /// this ignores the tool id/name and looks only at the event kind, so it
    /// applies to every tool, agent and payload shape.
    #[must_use]
    pub fn matches(&self, env: &HookEnvelope) -> bool {
        !self.kinds.is_empty() && self.kinds.contains(&env.event)
    }
}

fn validate(list: &str, entries: &[String]) -> Result<(), String> {
    if entries.len() > MAX_DROP_ENTRIES {
        return Err(format!(
            "[ingest].{list} holds {} entries; the limit is {MAX_DROP_ENTRIES}",
            entries.len()
        ));
    }
    for entry in entries {
        if entry.trim().is_empty() {
            return Err(format!("[ingest].{list} has a blank entry"));
        }
        if entry.chars().count() > MAX_DROP_ENTRY_CHARS {
            return Err(format!(
                "[ingest].{list} has an entry longer than {MAX_DROP_ENTRY_CHARS} characters"
            ));
        }
        if entry.chars().any(char::is_control) {
            return Err(format!(
                "[ingest].{list} has an entry with a control character"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::HookQuery;

    fn event(name: &str, body: serde_json::Value) -> HookEnvelope {
        agent_event("claude-code", name, body)
    }

    fn agent_event(agent: &str, name: &str, body: serde_json::Value) -> HookEnvelope {
        HookEnvelope::from_query_and_body(
            HookQuery {
                event: name.into(),
                agent: Some(agent.into()),
                ..Default::default()
            },
            body,
        )
    }

    fn list() -> IngestDropList {
        IngestDropList::new(
            vec!["toolu_plugin_".into()],
            vec!["mcp__noisy__poll".into()],
            &[],
        )
        .unwrap()
    }

    #[test]
    fn a_tool_use_id_prefix_or_exact_tool_name_matches() {
        let by_id = event(
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "Read", "tool_use_id": "toolu_plugin_42" }),
        );
        assert!(list().matches(&by_id));
        let by_name = event(
            "pre-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "mcp__noisy__poll", "tool_use_id": "toolu_01" }),
        );
        assert!(list().matches(&by_name));
    }

    #[test]
    fn other_tools_prefixes_inside_ids_and_non_tool_events_do_not_match() {
        let ordinary = event(
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "Read", "tool_use_id": "toolu_01abc" }),
        );
        assert!(!list().matches(&ordinary));
        // A prefix matches at the start only, and a name only exactly.
        let embedded = event(
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "mcp__noisy__poll_more", "tool_use_id": "x_toolu_plugin_1" }),
        );
        assert!(!list().matches(&embedded));
        let prompt = event(
            "user-prompt-submit",
            serde_json::json!({ "session_id": "s", "tool_name": "mcp__noisy__poll", "prompt": "hi" }),
        );
        assert!(!list().matches(&prompt));
        assert!(!IngestDropList::default().matches(&ordinary));
    }

    #[test]
    fn each_agent_is_matched_on_its_own_tool_fields() {
        let list =
            IngestDropList::new(vec!["plugin-".into()], vec!["noisy_poll".into()], &[]).unwrap();
        // OpenCode, Pi and OMP carry the call id as `callID` and the name as `tool`.
        let opencode = agent_event(
            "opencode",
            "post-tool-use",
            serde_json::json!({ "sessionID": "s", "tool": "read", "callID": "plugin-7" }),
        );
        assert!(list.matches(&opencode));
        // Hermes nests the call id under `extra`.
        let hermes = agent_event(
            "hermes",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "read", "extra": { "tool_call_id": "plugin-9" } }),
        );
        assert!(list.matches(&hermes));
        // Antigravity nests the tool name under `toolCall`.
        let antigravity = agent_event(
            "antigravity-cli",
            "pre-tool-use",
            serde_json::json!({ "session_id": "s", "toolCall": { "name": "noisy_poll", "args": {} } }),
        );
        assert!(list.matches(&antigravity));
        // Meta Muse Code carries the provider call id in `tool_use_id`, and a
        // `PostToolUseFailure` is a tool event too.
        let muse = agent_event(
            "muse",
            "PostToolUseFailure",
            serde_json::json!({ "session_id": "s", "tool_name": "read_file", "tool_input": { "path": "a" }, "tool_use_id": "plugin-3", "error": "x" }),
        );
        assert!(list.matches(&muse));
        let muse_kept = agent_event(
            "muse",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "read_file", "tool_use_id": "call_3" }),
        );
        assert!(!list.matches(&muse_kept));
        // CodeBuddy Code sends Claude Code's `tool_name` / `tool_use_id`.
        let codebuddy = agent_event(
            "codebuddy",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "Read", "tool_input": { "file_path": "a" }, "tool_use_id": "plugin-4" }),
        );
        assert!(list.matches(&codebuddy));
        let codebuddy_by_name = agent_event(
            "codebuddy",
            "pre-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "noisy_poll", "tool_use_id": "toolu_4" }),
        );
        assert!(list.matches(&codebuddy_by_name));
        let codebuddy_kept = agent_event(
            "codebuddy",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "Read", "tool_use_id": "toolu_4" }),
        );
        assert!(!list.matches(&codebuddy_kept));
        // Qwen Code sends the same Claude-shaped `tool_name` / `tool_use_id`.
        let qwen = agent_event(
            "qwen-code",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "write_file", "tool_input": { "file_path": "a" }, "tool_use_id": "plugin-5" }),
        );
        assert!(list.matches(&qwen));
        let qwen_kept = agent_event(
            "qwen-code",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "write_file", "tool_use_id": "toolu_5" }),
        );
        assert!(!list.matches(&qwen_kept));
        // Gemini CLI sends `tool_name` but no tool-call id, so only its name
        // matches; a `tool_use_id` in its payload is not read as one.
        let gemini = agent_event(
            "gemini-cli",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "hook_event_name": "AfterTool", "tool_name": "noisy_poll", "tool_input": {}, "tool_response": { "llmContent": "x" } }),
        );
        assert!(list.matches(&gemini));
        let gemini_kept = agent_event(
            "gemini-cli",
            "pre-tool-use",
            serde_json::json!({ "session_id": "s", "hook_event_name": "BeforeTool", "tool_name": "read_file", "tool_input": { "file_path": "a" }, "tool_use_id": "plugin-6" }),
        );
        assert!(!list.matches(&gemini_kept));
        // Controls: the same agents with ordinary ids and names stay.
        let kept = agent_event(
            "opencode",
            "post-tool-use",
            serde_json::json!({ "sessionID": "s", "tool": "read", "callID": "call-7" }),
        );
        assert!(!list.matches(&kept));
        let kept = agent_event(
            "antigravity-cli",
            "pre-tool-use",
            serde_json::json!({ "session_id": "s", "toolCall": { "name": "read", "args": {} } }),
        );
        assert!(!list.matches(&kept));
        // An agent with no closed mapping still matches on the generic fields.
        let generic = agent_event(
            "some-new-agent",
            "post-tool-use",
            serde_json::json!({ "session_id": "s", "tool_name": "noisy_poll" }),
        );
        assert!(list.matches(&generic));
    }

    #[test]
    fn entries_are_bounded_and_must_be_printable() {
        assert!(IngestDropList::new(vec![" ".into()], Vec::new(), &[]).is_err());
        assert!(IngestDropList::new(Vec::new(), vec!["a\nb".into()], &[]).is_err());
        assert!(
            IngestDropList::new(vec!["x".repeat(MAX_DROP_ENTRY_CHARS + 1)], Vec::new(), &[])
                .is_err()
        );
        let too_many = (0..=MAX_DROP_ENTRIES).map(|i| format!("t{i}")).collect();
        assert!(IngestDropList::new(Vec::new(), too_many, &[]).is_err());
        assert!(
            IngestDropList::new(vec!["x".repeat(MAX_DROP_ENTRY_CHARS)], Vec::new(), &[]).is_ok()
        );
    }

    #[test]
    fn drop_events_default_drops_nothing() {
        let list = IngestDropEvents::default();
        assert!(list.kinds.is_empty());
        // Nothing is dropped by default: every kind, incl. pre-tool-use, matches no.
        assert!(!list.matches(&event(
            "pre-tool-use",
            serde_json::json!({ "session_id": "s" })
        )));
        assert!(!list.matches(&event(
            "post-tool-use",
            serde_json::json!({ "session_id": "s" })
        )));
    }

    #[test]
    fn drop_events_named_kind_matches_its_events_only() {
        let list = IngestDropEvents::new(&["pre-tool-use".into()]).unwrap();
        // The named kind is dropped, on any agent and any payload shape.
        assert!(list.matches(&event(
            "pre-tool-use",
            serde_json::json!({ "session_id": "s" })
        )));
        assert!(list.matches(&agent_event(
            "opencode",
            "pre-tool-use",
            serde_json::json!({ "sessionID": "s" })
        )));
        // Its 1:1 pair and every other kind are untouched.
        assert!(!list.matches(&event(
            "post-tool-use",
            serde_json::json!({ "session_id": "s" })
        )));
        assert!(!list.matches(&event(
            "user-prompt",
            serde_json::json!({ "session_id": "s", "prompt": "hi" })
        )));
        assert!(!list.matches(&event("session-start", serde_json::json!({}))));
        assert!(!list.matches(&event(
            "session-end",
            serde_json::json!({ "session_id": "s" })
        )));
        assert!(!list.matches(&event("stop", serde_json::json!({ "session_id": "s" }))));
    }

    #[test]
    fn drop_events_accepts_case_and_underscore_wire_spellings() {
        // The typed parser, not a string compare: every accepted spelling of
        // the same kind collapses to one entry and matches.
        let list = IngestDropEvents::new(&[
            "PreToolUse".into(),
            "pre_tool_use".into(),
            "pre-tool-use".into(),
        ])
        .unwrap();
        assert_eq!(list.kinds.len(), 1, "duplicate kinds collapse");
        assert!(list.matches(&event(
            "preToolUse",
            serde_json::json!({ "session_id": "s" })
        )));
    }

    #[test]
    fn drop_events_rejects_session_state_kinds() {
        for value in [
            "session-start",
            "session-end",
            "SessionStart",
            "session_end",
        ] {
            let error = IngestDropEvents::new(&[value.into()]).unwrap_err();
            assert!(
                error.contains(value) && error.contains("session state"),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn drop_events_rejects_unknown_and_unrecognized_bucket() {
        for value in ["bogus", "", "other", "notification-2"] {
            let error = IngestDropEvents::new(&[value.into()]).unwrap_err();
            assert!(error.contains("drop_events"), "{value}: {error}");
        }
        // The error names the offending value for an operator to fix.
        let error = IngestDropEvents::new(&["pre-tool-us".into()]).unwrap_err();
        assert!(
            error.contains("pre-tool-us"),
            "unknown kinds name the value: {error}"
        );
        assert!(
            error.contains("Droppable kinds"),
            "and list the valid ones: {error}"
        );
    }

    #[test]
    fn drop_events_multiple_kinds() {
        let list = IngestDropEvents::new(&["pre-tool-use".into(), "stop".into()]).unwrap();
        assert!(list.matches(&event(
            "pre-tool-use",
            serde_json::json!({ "session_id": "s" })
        )));
        assert!(list.matches(&event("stop", serde_json::json!({ "session_id": "s" }))));
        assert!(!list.matches(&event(
            "post-tool-use",
            serde_json::json!({ "session_id": "s" })
        )));
    }
}
