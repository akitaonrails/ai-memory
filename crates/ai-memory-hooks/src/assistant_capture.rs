//! Assistant-message capture plumbing (issue #196).
//!
//! Some agent harnesses attach the assistant's final turn to their `Stop`
//! lifecycle event — Claude Code sends it as a top-level `last_assistant_message`
//! string. That text is high-value for recall but privacy-sensitive: it can
//! quote code, secrets, or content from paths ai-memory never sees. Capturing
//! it is therefore an explicit, double opt-in feature (server config +
//! `install-hooks --capture-assistant`).
//!
//! This module owns the single source of truth for WHICH raw field carries the
//! assistant message per agent/event, the unconditional strip that keeps that
//! raw field off the local spool, the wire, tracing, and storage, and the
//! opt-in path that re-introduces a sanitized, capped excerpt: the client-side
//! [`transform_for_client`] and the server-side [`apply_assistant_backstop`].

use ai_memory_core::{AgentKind, Sanitizer};
use serde::{Deserialize, Serialize};

use crate::payload::{HookEnvelope, HookEvent, truncate_utf8_bytes};

/// Synthetic body key carrying the opt-in, sanitized assistant excerpt from the
/// client to the server. Distinct from the raw `last_assistant_message` field,
/// which is always stripped: this one is the deliberate, capped protocol.
pub const ASSISTANT_MARKER_KEY: &str = "_ai_memory_assistant";

/// Protocol version for the opt-in `_ai_memory_assistant` body marker the client
/// attaches when capture is enabled. Bumping it invalidates markers a stale
/// server would otherwise trust.
pub const ASSISTANT_PROTOCOL_VERSION: u8 = 1;

/// Hard ceiling on the raw assistant-message string the client reads before
/// sanitizing/truncating. Oversized input is treated as absent.
pub const ASSISTANT_MESSAGE_MAX_INPUT_BYTES: usize = 64 * 1024;

/// Byte cap on the sanitized excerpt the opt-in path persists. Kept equal to
/// `truncate_excerpt`'s existing 2 KB excerpt contract so Stop bodies do not
/// become a second, larger excerpt norm.
pub const ASSISTANT_EXCERPT_MAX_BYTES: usize = 2_000;

/// Every raw top-level field name known to carry an assistant message across
/// supported agents. The unconditional strip removes each of these; the closed
/// per-agent table below decides which is a *candidate* for opt-in capture. The
/// union is kept tiny on purpose — one entry per distinct wire spelling.
const ASSISTANT_MESSAGE_FIELDS: &[&str] = &["last_assistant_message"];

/// The raw field that carries the assistant's final message for `(agent, event)`,
/// or `None` when the pair has no verified assistant-message field.
///
/// Closed table: only `ClaudeCode + Stop` is supported today. Extend
/// deliberately — a new entry opts an agent/event into capture and MUST have its
/// field name present in [`ASSISTANT_MESSAGE_FIELDS`] so the strip covers it
/// (enforced by `closed_table_fields_are_all_stripped`).
#[must_use]
pub fn assistant_message_field(agent: AgentKind, event: HookEvent) -> Option<&'static str> {
    match (agent, event) {
        (AgentKind::ClaudeCode, HookEvent::Stop) => Some("last_assistant_message"),
        _ => None,
    }
}

/// The opt-in assistant-capture wire protocol carried in the body under
/// [`ASSISTANT_MARKER_KEY`]. `deny_unknown_fields` + the explicit version gate
/// means a malformed or future-versioned marker is rejected (dropped, never
/// persisted) rather than partially trusted.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantCaptureProtocol {
    /// Wire version. Must equal [`ASSISTANT_PROTOCOL_VERSION`].
    pub version: u8,
    /// Client-sanitized, byte-capped assistant excerpt. The server re-scrubs it
    /// with its configured `Sanitizer` at the persistence boundary.
    pub excerpt: String,
}

impl AssistantCaptureProtocol {
    /// Parse a marker value, accepting only the current protocol version. Any
    /// unknown field, wrong type, or version mismatch yields `None`.
    #[must_use]
    fn parse(value: &serde_json::Value) -> Option<Self> {
        let parsed: Self = serde_json::from_value(value.clone()).ok()?;
        (parsed.version == ASSISTANT_PROTOCOL_VERSION).then_some(parsed)
    }
}

/// Outcome of the client-side assistant transform.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ClientAssistantTransform {
    /// The raw JSON was mutated (raw field removed and/or protocol spliced in),
    /// so the caller must reserialize the spool/wire payload.
    pub changed: bool,
    /// A valid protocol was spliced in, so the caller appends
    /// `&capture_assistant=1` to the event URL.
    pub captured: bool,
}

/// Client-side transform for an install that opted into assistant capture.
///
/// Reads the candidate assistant message for `(agent, event)`, unconditionally
/// strips the raw field, and — when the value is a non-empty, in-bounds string —
/// sanitizes it with the built-in `Sanitizer`, truncates it to
/// [`ASSISTANT_EXCERPT_MAX_BYTES`] on a UTF-8 boundary, and splices the versioned
/// [`AssistantCaptureProtocol`] into the body under [`ASSISTANT_MARKER_KEY`].
///
/// Scrub happens BEFORE truncation (a secret straddling the cap must be redacted
/// before it can be cut). Non-string, empty, or oversized values yield no
/// protocol — the raw field is still stripped, so the event degrades to an empty
/// Stop rather than leaking anything.
pub fn transform_for_client(
    raw: &mut serde_json::Value,
    agent: AgentKind,
    event: HookEvent,
) -> ClientAssistantTransform {
    // Read the eligible value BEFORE stripping removes the field.
    let candidate = assistant_message_field(agent, event).and_then(|field| {
        raw.as_object()
            .and_then(|object| object.get(field))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= ASSISTANT_MESSAGE_MAX_INPUT_BYTES)
            .map(str::to_string)
    });

    let mut changed = strip_assistant_message_raw(raw);

    let Some(value) = candidate else {
        return ClientAssistantTransform {
            changed,
            captured: false,
        };
    };
    let scrubbed = Sanitizer::builtin().scrub(&value);
    let excerpt = truncate_utf8_bytes(&scrubbed, ASSISTANT_EXCERPT_MAX_BYTES);
    if excerpt.is_empty() {
        return ClientAssistantTransform {
            changed,
            captured: false,
        };
    }
    let protocol = AssistantCaptureProtocol {
        version: ASSISTANT_PROTOCOL_VERSION,
        excerpt,
    };
    let mut captured = false;
    if let Some(object) = raw.as_object_mut()
        && let Ok(marker) = serde_json::to_value(&protocol)
    {
        object.insert(ASSISTANT_MARKER_KEY.to_string(), marker);
        changed = true;
        captured = true;
    }
    ClientAssistantTransform { changed, captured }
}

/// Server-side backstop for the opt-in assistant excerpt (#196).
///
/// Always consumes the [`ASSISTANT_MARKER_KEY`] from `env.raw` so it can never
/// persist. Populates `env.body_excerpt` with the excerpt ONLY when every gate
/// holds: the server enabled capture, the client requested it (`capture_assistant`
/// query flag), the agent/event is a supported candidate, and the marker parses
/// as a current-version protocol with a non-empty excerpt. Any failure leaves
/// `body_excerpt` as `None` — an empty Stop — so a forged or stale marker cannot
/// inject content. Infallible: never returns an error, so a capture decision can
/// never turn into a batch fail-fast.
pub fn apply_assistant_backstop(env: &mut HookEnvelope, server_enabled: bool) {
    // Consume the marker unconditionally: it must never survive into the stored
    // raw, whether or not the gates below accept it.
    let marker = env
        .raw
        .as_object_mut()
        .and_then(|object| object.remove(ASSISTANT_MARKER_KEY));

    let eligible = server_enabled
        && env.capture_assistant_requested
        && assistant_message_field(env.agent, env.event).is_some();
    if !eligible {
        return;
    }
    if let Some(marker) = marker
        && let Some(protocol) = AssistantCaptureProtocol::parse(&marker)
        && !protocol.excerpt.is_empty()
    {
        // Re-enforce the excerpt cap at the persistence boundary — never trust
        // the client's length. Symmetric to the server re-scrub in `process()`:
        // a forged or buggy client that satisfies the gates cannot inject an
        // oversized Stop body (the request body limit alone is 10 MiB). Almost
        // always a no-op, since a well-behaved client already truncated.
        env.body_excerpt = Some(truncate_utf8_bytes(
            &protocol.excerpt,
            ASSISTANT_EXCERPT_MAX_BYTES,
        ));
    }
}

/// Unconditionally remove every known assistant-message field from a raw hook
/// payload's top-level object, returning whether anything was removed.
///
/// This is a defense applied on BOTH sides of the wire (client pre-spool and
/// server pre-envelope) and for EVERY agent/event, not just the supported pair:
/// a raw assistant-message field must never reach the spool, the wire, tracing,
/// or storage unless the explicit opt-in path re-introduces it as a sanitized,
/// capped excerpt. Only top-level keys are inspected — the same scope
/// as `body_is_subagent`, and where every supported harness places the field.
pub fn strip_assistant_message_raw(raw: &mut serde_json::Value) -> bool {
    let Some(object) = raw.as_object_mut() else {
        return false;
    };
    let mut removed = false;
    for field in ASSISTANT_MESSAGE_FIELDS {
        if object.remove(*field).is_some() {
            removed = true;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `ClaudeCode + Stop` is a capture candidate; every other agent/event
    /// pair across the full agent surface must return `None`.
    #[test]
    fn only_claude_stop_is_a_capture_candidate() {
        let events = [
            HookEvent::SessionStart,
            HookEvent::UserPrompt,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::PreCompact,
            HookEvent::PostCompaction,
            HookEvent::Notification,
            HookEvent::Stop,
            HookEvent::SessionEnd,
            HookEvent::SubagentStart,
            HookEvent::SubagentStop,
            HookEvent::Other,
        ];
        for agent in AgentKind::ALL {
            for event in events {
                let expected = agent == AgentKind::ClaudeCode && event == HookEvent::Stop;
                assert_eq!(
                    assistant_message_field(agent, event).is_some(),
                    expected,
                    "agent={agent:?} event={event:?}"
                );
            }
        }
    }

    /// The strip must cover every field the closed table can name, or an opted-in
    /// agent/event could carry a raw field the strip misses.
    #[test]
    fn closed_table_fields_are_all_stripped() {
        let events = [
            HookEvent::SessionStart,
            HookEvent::UserPrompt,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::PreCompact,
            HookEvent::PostCompaction,
            HookEvent::Notification,
            HookEvent::Stop,
            HookEvent::SessionEnd,
            HookEvent::SubagentStart,
            HookEvent::SubagentStop,
            HookEvent::Other,
        ];
        for agent in AgentKind::ALL {
            for event in events {
                if let Some(field) = assistant_message_field(agent, event) {
                    assert!(
                        ASSISTANT_MESSAGE_FIELDS.contains(&field),
                        "closed-table field {field:?} is not in the strip set"
                    );
                }
            }
        }
    }

    /// The strip is unconditional: it removes the raw field regardless of agent
    /// and reports the removal, so a client that never verified the agent still
    /// cannot leak it.
    #[test]
    fn strip_removes_field_and_reports_change() {
        let mut raw = serde_json::json!({
            "session_id": "s1",
            "last_assistant_message": "SENTINEL_ASSISTANT_MESSAGE",
        });
        assert!(strip_assistant_message_raw(&mut raw));
        assert!(raw.get("last_assistant_message").is_none());
        assert_eq!(raw.get("session_id").and_then(|v| v.as_str()), Some("s1"));
    }

    #[test]
    fn strip_is_noop_when_absent_or_not_object() {
        let mut without = serde_json::json!({"session_id": "s1"});
        assert!(!strip_assistant_message_raw(&mut without));
        assert_eq!(without, serde_json::json!({"session_id": "s1"}));

        let mut array = serde_json::json!(["last_assistant_message"]);
        assert!(!strip_assistant_message_raw(&mut array));
        assert_eq!(array, serde_json::json!(["last_assistant_message"]));
    }

    fn stop_env(raw: serde_json::Value, requested: bool) -> HookEnvelope {
        HookEnvelope::from_query_and_body(
            crate::payload::HookQuery {
                event: "stop".into(),
                agent: Some("claude-code".into()),
                capture_assistant: requested.then(|| "1".to_string()),
                ..Default::default()
            },
            raw,
        )
    }

    #[test]
    fn client_transform_splices_sanitized_capped_excerpt() {
        let mut raw = serde_json::json!({
            "session_id": "s1",
            "last_assistant_message": "fixed the bug"
        });
        let out = transform_for_client(&mut raw, AgentKind::ClaudeCode, HookEvent::Stop);
        assert!(out.changed && out.captured);
        assert!(raw.get("last_assistant_message").is_none());
        let marker = raw.get(ASSISTANT_MARKER_KEY).expect("protocol spliced");
        let protocol = AssistantCaptureProtocol::parse(marker).expect("valid v1 protocol");
        assert_eq!(protocol.version, ASSISTANT_PROTOCOL_VERSION);
        assert_eq!(protocol.excerpt, "fixed the bug");
    }

    #[test]
    fn client_transform_scrubs_before_truncating() {
        // A built-in secret pattern must be redacted in the excerpt.
        let secret = "AKIA".to_string() + &"A".repeat(16); // AWS access key id shape
        let mut raw = serde_json::json!({ "last_assistant_message": format!("key {secret}") });
        let out = transform_for_client(&mut raw, AgentKind::ClaudeCode, HookEvent::Stop);
        assert!(out.captured);
        let excerpt = raw[ASSISTANT_MARKER_KEY]["excerpt"].as_str().unwrap();
        assert!(!excerpt.contains(&secret), "secret survived: {excerpt}");
        assert!(excerpt.contains("[REDACTED]"), "not redacted: {excerpt}");
    }

    #[test]
    fn client_transform_truncates_multibyte_within_cap() {
        let big = "é".repeat(ASSISTANT_EXCERPT_MAX_BYTES); // 2 bytes each → 2x the cap
        let mut raw = serde_json::json!({ "last_assistant_message": big });
        let out = transform_for_client(&mut raw, AgentKind::ClaudeCode, HookEvent::Stop);
        assert!(out.captured);
        let excerpt = raw[ASSISTANT_MARKER_KEY]["excerpt"].as_str().unwrap();
        assert!(excerpt.len() <= ASSISTANT_EXCERPT_MAX_BYTES, "over cap");
        assert!(excerpt.ends_with('…'), "truncation marker missing");
        // Valid UTF-8 (no split codepoint): re-parsing as str succeeds by construction.
        assert!(std::str::from_utf8(excerpt.as_bytes()).is_ok());
    }

    #[test]
    fn client_transform_strips_but_omits_protocol_for_empty_or_oversized() {
        for value in ["", &"x".repeat(ASSISTANT_MESSAGE_MAX_INPUT_BYTES + 1)] {
            let mut raw = serde_json::json!({ "last_assistant_message": value });
            let out = transform_for_client(&mut raw, AgentKind::ClaudeCode, HookEvent::Stop);
            assert!(!out.captured, "value {:?} must not capture", value.len());
            assert!(
                raw.get("last_assistant_message").is_none(),
                "raw not stripped"
            );
            assert!(
                raw.get(ASSISTANT_MARKER_KEY).is_none(),
                "protocol spliced anyway"
            );
        }
    }

    #[test]
    fn client_transform_ignores_non_candidate_agent_event() {
        // Non-Claude agent: raw field still stripped defensively, no protocol.
        let mut raw = serde_json::json!({ "last_assistant_message": "hi" });
        let out = transform_for_client(&mut raw, AgentKind::Codex, HookEvent::Stop);
        assert!(!out.captured);
        assert!(raw.get("last_assistant_message").is_none());
        assert!(raw.get(ASSISTANT_MARKER_KEY).is_none());
    }

    #[test]
    fn backstop_populates_body_when_all_gates_pass() {
        let mut raw = serde_json::json!({ "last_assistant_message": "done" });
        transform_for_client(&mut raw, AgentKind::ClaudeCode, HookEvent::Stop);
        let mut env = stop_env(raw, true);
        apply_assistant_backstop(&mut env, true);
        assert_eq!(env.body_excerpt.as_deref(), Some("done"));
        assert!(
            env.raw.get(ASSISTANT_MARKER_KEY).is_none(),
            "marker persisted"
        );
    }

    #[test]
    fn backstop_drops_when_server_off_or_flag_absent() {
        let make = || {
            let mut raw = serde_json::json!({ "last_assistant_message": "done" });
            transform_for_client(&mut raw, AgentKind::ClaudeCode, HookEvent::Stop);
            raw
        };
        // Server disabled.
        let mut env = stop_env(make(), true);
        apply_assistant_backstop(&mut env, false);
        assert!(env.body_excerpt.is_none());
        assert!(
            env.raw.get(ASSISTANT_MARKER_KEY).is_none(),
            "marker still consumed"
        );
        // Query flag absent (client did not request).
        let mut env = stop_env(make(), false);
        apply_assistant_backstop(&mut env, true);
        assert!(env.body_excerpt.is_none());
    }

    #[test]
    fn backstop_caps_oversized_excerpt_server_side() {
        // A forged/buggy client can satisfy the gates with an excerpt far above
        // the 2 KB cap (the client-side truncation is not trustworthy). The
        // server must re-enforce the cap at the persistence boundary.
        let huge = "a".repeat(ASSISTANT_EXCERPT_MAX_BYTES * 500);
        let raw = serde_json::json!({
            ASSISTANT_MARKER_KEY: { "version": 1, "excerpt": huge }
        });
        let mut env = stop_env(raw, true);
        apply_assistant_backstop(&mut env, true);
        let body = env.body_excerpt.expect("gates pass, body set");
        assert!(
            body.len() <= ASSISTANT_EXCERPT_MAX_BYTES,
            "server did not cap oversized excerpt: {} bytes",
            body.len()
        );
    }

    #[test]
    fn backstop_rejects_forged_empty_and_bad_version() {
        for marker in [
            serde_json::json!({ "version": 1, "excerpt": "" }),
            serde_json::json!({ "version": 2, "excerpt": "future" }),
            serde_json::json!({ "version": 1, "excerpt": "x", "extra": true }),
            serde_json::json!({ "excerpt": "no version" }),
        ] {
            let raw = serde_json::json!({ ASSISTANT_MARKER_KEY: marker });
            let mut env = stop_env(raw, true);
            apply_assistant_backstop(&mut env, true);
            assert!(
                env.body_excerpt.is_none(),
                "forged marker accepted: {:?}",
                env.body_excerpt
            );
        }
    }

    #[test]
    fn backstop_ignores_non_stop_event() {
        let raw = serde_json::json!({
            ASSISTANT_MARKER_KEY: { "version": 1, "excerpt": "x" }
        });
        let mut env = HookEnvelope::from_query_and_body(
            crate::payload::HookQuery {
                event: "user-prompt-submit".into(),
                agent: Some("claude-code".into()),
                capture_assistant: Some("1".into()),
                ..Default::default()
            },
            raw,
        );
        apply_assistant_backstop(&mut env, true);
        assert!(env.body_excerpt.is_none());
    }
}
