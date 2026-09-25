//! Envelope validation and the stable retry identity.
//!
//! Validate input before enqueueing so malformed events cannot block a batch.
//! Body values, including `_ai_memory_capture`, are preserved for the server.
//! These checks do not sanitize locally queued content.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// `extension` accepts up to 64 ASCII token characters (docs/external-lifecycle.md).
pub const MAX_PRODUCER_LEN: usize = 64;
/// A stable adapter-side operator namespace. Never a bearer token.
pub const MAX_ACTOR_LEN: usize = 64;
/// `source_event` accepts 128 (docs/external-lifecycle.md).
pub const MAX_EVENT_LEN: usize = 128;
/// Producer-assigned event id. It only feeds the ingest-key hash, so it is
/// bounded free-form text rather than a
/// token: an orchestrator numbering events `run/123/event/2` is normal.
pub const MAX_EVENT_ID_LEN: usize = 128;
/// Wire `agent` such as `claude-code` or `codex`.
pub const MAX_AGENT_LEN: usize = 64;
/// Workspace / project names, URL-encoded into the item query.
pub const MAX_SCOPE_LEN: usize = 128;
/// Native session id, preserved exactly as the harness minted it.
pub const MAX_SESSION_ID_LEN: usize = 256;
/// Native cwd, preserved exactly as the harness reported it.
pub const MAX_CWD_LEN: usize = 4096;
/// Canonicalized body bytes accepted for one event.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Items in one `POST /hook/batch` (server: `MAX_HOOK_BATCH_ITEMS`).
pub const MAX_BATCH_ITEMS: usize = 256;
/// Server ingest keys expire after 30 days; nothing older may be auto-resent.
pub const RETRY_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Lifecycle events that end an execution. A terminal event may only ship once
/// it is its session's oldest pending item, so it can never pass an earlier one.
pub const TERMINAL_EVENTS: &[&str] = &["session-end"];

/// One item of the `enqueue --file` input array.
///
/// `deny_unknown_fields` applies to the envelope only. `body` is an opaque
/// object: unknown keys inside it (a harness payload field, the
/// `_ai_memory_capture` protocol block) are preserved untouched.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputEvent {
    /// Producer-assigned id, unique within the producer/actor/agent/session tuple.
    pub event_id: String,
    /// Wire agent of the live harness (`claude-code`, `codex`, ...).
    pub agent: String,
    /// Canonical lifecycle event name, used for both `event` and `source_event`.
    pub event: String,
    /// Harness payload, delivered verbatim.
    pub body: serde_json::Value,
}

/// A validated event, ready to be persisted and later replayed unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidEvent {
    pub event_id: String,
    pub agent: String,
    pub event: String,
    pub session_id: String,
    pub cwd: String,
    /// Canonical JSON of the body exactly as supplied (sorted keys, no reformatting
    /// of values). The bytes delivered later are these bytes.
    pub body_json: String,
    pub body_sha256: String,
    pub ingest_key: String,
}

impl ValidEvent {
    /// True when this event ends the execution for its session.
    pub fn is_terminal(&self) -> bool {
        TERMINAL_EVENTS.contains(&self.event.as_str())
    }
}

/// `extension`, `source_event` and `ingest_key` share the server's token
/// alphabet: letters, digits, `.`, `_`, `-` and `:`.
fn is_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

/// Free-form identity text (session id, cwd, scope names): bounded, non-blank,
/// and free of control characters that would corrupt a URL or a log line.
fn is_plain(value: &str, max: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= max
        && !value.chars().any(|c| c.is_control())
}

/// Validate the fields that identify a producer namespace.
pub fn check_producer(producer: &str) -> Result<(), String> {
    is_token(producer, MAX_PRODUCER_LEN)
        .then_some(())
        .ok_or_else(|| format!("--producer must be 1..={MAX_PRODUCER_LEN} token characters"))
}

/// Validate the adapter-side operator namespace.
pub fn check_actor(actor: &str) -> Result<(), String> {
    is_token(actor, MAX_ACTOR_LEN)
        .then_some(())
        .ok_or_else(|| format!("--actor must be 1..={MAX_ACTOR_LEN} token characters"))
}

/// Validate a workspace or project name.
pub fn check_scope(label: &str, value: &str) -> Result<(), String> {
    is_plain(value, MAX_SCOPE_LEN)
        .then_some(())
        .ok_or_else(|| format!("--{label} must be 1..={MAX_SCOPE_LEN} printable characters"))
}

/// The retry identity from `docs/external-lifecycle.md`, byte-for-byte.
///
/// The tuple defines the namespace of `event_id`. Payload content does not
/// contribute to the key, so a restarted producer can derive the same identity.
pub fn ingest_key(
    producer: &str,
    actor: &str,
    agent: &str,
    session_id: &str,
    source_event: &str,
    event_id: &str,
) -> String {
    let identity = serde_json::to_string(&[
        "external-capture-v1",
        producer,
        actor,
        agent,
        session_id,
        source_event,
        event_id,
    ])
    .expect("a fixed-size array of strings always serializes");
    let digest = Sha256::digest(identity.as_bytes());
    let mut key = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// Validate one input item against every bound above.
///
/// Errors name the array index and the producer's own `event_id`. They never
/// quote body content: an invalid payload must not become a log line.
pub fn validate(
    index: usize,
    input: InputEvent,
    producer: &str,
    actor: &str,
) -> Result<ValidEvent, String> {
    let at = |detail: &str| format!("item {index}: {detail}");
    if !is_plain(&input.event_id, MAX_EVENT_ID_LEN) {
        return Err(at(&format!(
            "event_id must be 1..={MAX_EVENT_ID_LEN} printable characters"
        )));
    }
    let tag = format!("item {index} (event_id {}): ", input.event_id);
    let at = |detail: &str| format!("{tag}{detail}");
    if !is_token(&input.agent, MAX_AGENT_LEN) {
        return Err(at(&format!(
            "agent must be 1..={MAX_AGENT_LEN} token characters"
        )));
    }
    if !is_token(&input.event, MAX_EVENT_LEN) {
        return Err(at(&format!(
            "event must be 1..={MAX_EVENT_LEN} token characters"
        )));
    }
    let serde_json::Value::Object(body) = &input.body else {
        return Err(at("body must be a JSON object"));
    };
    let Some(serde_json::Value::String(session_id)) = body.get("session_id") else {
        return Err(at("body.session_id must be an explicit string"));
    };
    if !is_plain(session_id, MAX_SESSION_ID_LEN) {
        return Err(at(&format!(
            "body.session_id must be 1..={MAX_SESSION_ID_LEN} printable characters"
        )));
    }
    let Some(serde_json::Value::String(cwd)) = body.get("cwd") else {
        return Err(at("body.cwd must be an explicit string"));
    };
    if !is_plain(cwd, MAX_CWD_LEN) {
        return Err(at(&format!(
            "body.cwd must be 1..={MAX_CWD_LEN} printable characters"
        )));
    }
    let (session_id, cwd) = (session_id.clone(), cwd.clone());
    let body_json = serde_json::to_string(&input.body).map_err(|_| at("body is not encodable"))?;
    if body_json.len() > MAX_BODY_BYTES {
        return Err(at(&format!(
            "body is {} bytes, over the {MAX_BODY_BYTES}-byte limit",
            body_json.len()
        )));
    }
    let digest = Sha256::digest(body_json.as_bytes());
    let body_sha256 = digest.iter().map(|b| format!("{b:02x}")).collect();
    let ingest_key = ingest_key(
        producer,
        actor,
        &input.agent,
        &session_id,
        &input.event,
        &input.event_id,
    );
    Ok(ValidEvent {
        event_id: input.event_id,
        agent: input.agent,
        event: input.event,
        session_id,
        cwd,
        body_json,
        body_sha256,
        ingest_key,
    })
}
