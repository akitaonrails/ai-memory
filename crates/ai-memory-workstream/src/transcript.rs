//! Incremental, read-only extraction from native harness session stores.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ai_memory_core::{
    AgentKind, MANAGED_WORKSTREAM_PACKET_MARKER, NewWorkstreamEvent, WorkstreamEventKind,
};
use anyhow::{Context as _, Result, anyhow};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::ManagedHarness;

const MAX_SCAN_FILES: usize = 50_000;
const MAX_EVENT_BYTES: usize = 128 * 1024;
const MAX_NATIVE_SESSION_ID_BYTES: usize = 512;
const LEGACY_MANAGED_WORKSTREAM_PACKET_PREFIX: &str = "> **ai-memory managed workstream:";

/// Checkout-local native session that can seed an otherwise-empty workstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSessionCandidate {
    /// Harness-native session identifier.
    pub native_session_id: String,
    /// Last observed native-store update time.
    pub updated_at: SystemTime,
}

/// Incremental transcript export produced after a managed child exits.
#[derive(Debug, Clone, Default)]
pub struct ExportedTranscript {
    /// Native session that was read.
    pub native_session_id: String,
    /// Opaque adapter cursor persisted only for the next local read.
    pub source_cursor: Option<String>,
    /// Portable visible events after the incoming cursor.
    pub events: Vec<NewWorkstreamEvent>,
    /// Explicit records of private, malformed, or unsupported source data.
    pub losses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileCursor {
    path: String,
    offset: u64,
    /// Identifies incompatible native stores that share one wire-level agent.
    /// Older cursors omit this field and remain readable after exact path and
    /// metadata validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    flavor: Option<FileFlavor>,
    /// Hash of every committed byte through `offset`. Kimi Code and Grok can
    /// rewrite their journals in place (Kimi on fork/compaction/resume, Grok
    /// on rewind); Kiro's append-only behavior is not documented. Those
    /// adapters validate this prefix before trusting the byte offset. Other
    /// JSONL adapters remain offset-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefix_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FileFlavor {
    KiroV2,
    KiroV3,
}

/// Recover Kiro's incompatible engine flavor from an opaque transcript cursor.
/// The result is advisory only: callers must still validate the exact native
/// session against that engine's store before injecting a resume selector.
#[must_use]
pub fn kiro_harness_from_source_cursor(raw: &str) -> Option<ManagedHarness> {
    match serde_json::from_str::<FileCursor>(raw).ok()?.flavor? {
        FileFlavor::KiroV2 => Some(ManagedHarness::Kiro),
        FileFlavor::KiroV3 => Some(ManagedHarness::KiroV3),
    }
}

const fn file_flavor(harness: ManagedHarness) -> Option<FileFlavor> {
    match harness {
        ManagedHarness::Kiro => Some(FileFlavor::KiroV2),
        ManagedHarness::KiroV3 => Some(FileFlavor::KiroV3),
        _ => None,
    }
}

/// Harnesses whose JSONL journal can be rewritten in place, requiring
/// prefix-validated cursors and content-hash record ids.
const fn journal_rewrites_in_place(harness: ManagedHarness) -> bool {
    matches!(
        harness,
        ManagedHarness::Kimi | ManagedHarness::Kiro | ManagedHarness::KiroV3 | ManagedHarness::Grok
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SqlCursor {
    updated: i64,
    id: String,
}

/// Export unseen visible transcript records for one native session.
pub async fn export_transcript(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    native_session_id: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    if harness == ManagedHarness::OpenCode {
        return export_opencode(home, session_dir, native_session_id, source_cursor);
    }
    if harness == ManagedHarness::OpenCode2 {
        return export_opencode2(home, session_dir, native_session_id, source_cursor);
    }
    if harness == ManagedHarness::Crush {
        return export_crush(cwd, session_dir, native_session_id, source_cursor);
    }
    if harness == ManagedHarness::Antigravity {
        // The conversation store keeps every step as an undocumented protobuf
        // blob whose step-type enum is unversioned, so message text cannot be
        // decoded without guessing at a schema that changes between `agy`
        // releases. Conversation identity and workspace are read (they are
        // stable fields observed in current metadata); the visible-event
        // ledger for this harness comes from lifecycle-hook capture instead.
        return Err(anyhow!(
            "antigravity conversations expose no decodable transcript; this session's events come from hook capture"
        ));
    }
    let path = locate_session_file(harness, home, cwd, session_dir, native_session_id)?
        .ok_or_else(|| anyhow!("native transcript for {native_session_id} was not found"))?;
    export_jsonl(harness, &path, native_session_id, source_cursor)
}

/// Discover a session created after `started_at` when the harness could not be
/// assigned an id before launch and SessionStart did not link one.
pub async fn discover_native_session(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    started_at: SystemTime,
) -> Result<Option<String>> {
    if harness == ManagedHarness::OpenCode {
        return discover_opencode(home, session_dir, cwd, started_at);
    }
    if harness == ManagedHarness::OpenCode2 {
        return discover_opencode2(home, session_dir, cwd, started_at);
    }
    if harness == ManagedHarness::Crush {
        return discover_crush(cwd, session_dir, started_at);
    }
    let mut candidates = collect_session_files(harness, home, session_dir)?;
    candidates.sort_by(|left, right| {
        modified(right)
            .cmp(&modified(left))
            .then_with(|| left.cmp(right))
    });
    for path in candidates.into_iter().take(512) {
        if modified(&path).is_some_and(|time| time + Duration::from_secs(2) < started_at) {
            break;
        }
        if let Some((id, record_cwd)) = session_header_for_cwd(harness, &path, cwd)?
            && same_path(&record_cwd, cwd)
        {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// List newest native sessions whose recorded working directory matches the
/// current checkout. Native stores are opened read-only and unrelated paths are
/// excluded before candidates reach the launcher prompt.
pub async fn list_native_sessions(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    limit: usize,
) -> Result<Vec<NativeSessionCandidate>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    if harness == ManagedHarness::OpenCode {
        return list_opencode_sessions(home, session_dir, cwd, limit);
    }
    if harness == ManagedHarness::OpenCode2 {
        return list_opencode2_sessions(home, session_dir, cwd, limit);
    }
    if harness == ManagedHarness::Crush {
        return list_crush_sessions(cwd, session_dir, limit);
    }

    let mut files = collect_session_files(harness, home, session_dir)?;
    files.sort_by(|left, right| {
        modified(right)
            .cmp(&modified(left))
            .then_with(|| left.cmp(right))
    });
    let mut seen = HashSet::new();
    let mut sessions = Vec::new();
    for path in files.into_iter().take(2_000) {
        let Some(updated_at) = modified(&path) else {
            continue;
        };
        let Ok(Some((native_session_id, recorded_cwd))) =
            session_header_for_cwd(harness, &path, cwd)
        else {
            continue;
        };
        if !same_path(&recorded_cwd, cwd)
            || !valid_native_session_id(&native_session_id)
            || !seen.insert(native_session_id.clone())
        {
            continue;
        }
        sessions.push(NativeSessionCandidate {
            native_session_id,
            updated_at,
        });
        if sessions.len() >= limit {
            break;
        }
    }
    Ok(sessions)
}

/// Check whether one exact native session still exists in the harness's
/// read-only transcript store. `Ok(false)` means the resume target is
/// definitely absent; store access or schema failures remain errors so callers
/// do not mistake an unreadable store for a deleted session.
pub fn native_session_exists(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    native_session_id: &str,
) -> Result<bool> {
    if harness == ManagedHarness::OpenCode {
        return Ok(opencode_updated(home, session_dir, native_session_id)?.is_some());
    }
    if harness == ManagedHarness::OpenCode2 {
        return Ok(opencode2_updated(home, session_dir, native_session_id)?.is_some());
    }
    if harness == ManagedHarness::Crush {
        return Ok(crush_updated(cwd, session_dir, native_session_id)?.is_some());
    }
    Ok(locate_session_file(harness, home, cwd, session_dir, native_session_id)?.is_some())
}

/// Whether a linked Kiro v3 session was found only in the default home rather
/// than the configured `KIRO_HOME` session root.
///
/// Kiro CLI 2.16.2 writes v3 sessions to the default root while a custom
/// `KIRO_HOME` is active, then searches only the custom root on resume. The
/// launcher uses this proof to remove `KIRO_HOME` for that one native process.
pub fn kiro_v3_resume_uses_default_store(
    home: &Path,
    cwd: &Path,
    configured_root: Option<&Path>,
    native_session_id: &str,
) -> Result<bool> {
    let Some(configured_root) = configured_root else {
        return Ok(false);
    };
    let default_root = home.join(".kiro/sessions");
    if configured_root == default_root {
        return Ok(false);
    }
    let found = locate_session_file(
        ManagedHarness::KiroV3,
        home,
        cwd,
        Some(configured_root),
        native_session_id,
    )?;
    Ok(found
        .is_some_and(|path| path.starts_with(&default_root) && !path.starts_with(configured_root)))
}

/// Wait briefly for buffered transcript writers to settle before importing.
pub async fn wait_for_transcript_flush(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    native_session_id: &str,
) -> Result<()> {
    let mut previous = None;
    for _ in 0..10 {
        let current = if harness == ManagedHarness::OpenCode {
            opencode_updated(home, session_dir, native_session_id)?.map(|value| value.to_string())
        } else if harness == ManagedHarness::OpenCode2 {
            opencode2_updated(home, session_dir, native_session_id)?.map(|value| value.to_string())
        } else if harness == ManagedHarness::Crush {
            crush_updated(cwd, session_dir, native_session_id)?.map(|value| value.to_string())
        } else {
            locate_session_file(harness, home, cwd, session_dir, native_session_id)?.and_then(
                |path| {
                    fs::metadata(&path)
                        .ok()
                        .map(|metadata| format!("{}:{}", path.display(), metadata.len()))
                },
            )
        };
        if current.is_some() && current == previous {
            return Ok(());
        }
        previous = current;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

fn export_jsonl(
    harness: ManagedHarness,
    path: &Path,
    native_session_id: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let flavor = file_flavor(harness);
    let cursor = source_cursor
        .and_then(|raw| serde_json::from_str::<FileCursor>(raw).ok())
        .filter(|cursor| {
            Path::new(&cursor.path) == path && (cursor.flavor.is_none() || cursor.flavor == flavor)
        });
    let mut file = File::open(path)
        .with_context(|| format!("opening native transcript {}", path.display()))?;
    let len = file.metadata()?.len();
    let (start, mut prefix_hasher) = if journal_rewrites_in_place(harness) {
        let validated = if let Some(cursor) = cursor.as_ref().filter(|cursor| cursor.offset <= len)
            && let Some(expected) = cursor.prefix_sha256.as_deref()
            && let Some(hasher) = hash_file_prefix(&mut file, cursor.offset)?
            && format!("{:x}", hasher.clone().finalize()) == expected
        {
            Some((cursor.offset, hasher))
        } else {
            None
        };
        validated.unwrap_or_else(|| (0, Sha256::new()))
    } else {
        (
            cursor.map_or(0, |cursor| cursor.offset.min(len)),
            Sha256::new(),
        )
    };
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut offset = start;
    let mut committed_offset = start;
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut losses = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        offset += read as u64;
        if !line.ends_with(b"\n") {
            break;
        }
        if journal_rewrites_in_place(harness) {
            prefix_hasher.update(&line);
        }
        committed_offset = offset;
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                losses.push(format!(
                    "malformed JSONL record at byte {}",
                    offset - read as u64
                ));
                continue;
            }
        };
        let record_id = if journal_rewrites_in_place(harness) {
            // Kimi wire records and Grok chat-history records carry no
            // envelope id, and both journals can be rewritten wholesale (Kimi
            // on fork/compaction/resume, Grok on rewind) — a byte-offset id
            // would silently change meaning. Hashing the raw line keeps
            // record ids (and therefore server-side event dedup) stable
            // across rewrites. Kiro records reuse one message_id across an
            // exchange's Prompt/AssistantMessage/ToolResults records, so
            // the line hash is its unique record id as well.
            let raw = line.strip_suffix(b"\n").unwrap_or(&line);
            format!("{:x}", Sha256::digest(raw))
        } else {
            source_id(&value).unwrap_or_else(|| format!("byte-{}", offset - read as u64))
        };
        match harness {
            ManagedHarness::Claude => parse_claude(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::Codex => parse_codex(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::Pi | ManagedHarness::Omp => parse_pi_family(
                harness.agent_kind(),
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::Kimi => parse_kimi(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::CommandCode => parse_command_code(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::Kiro => parse_kiro(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::KiroV3 => parse_kiro_v3(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::Grok => parse_grok(
                &value,
                native_session_id,
                &record_id,
                &mut events,
                &mut losses,
            ),
            ManagedHarness::OpenCode
            | ManagedHarness::OpenCode2
            | ManagedHarness::Crush
            | ManagedHarness::Antigravity => {
                return Err(anyhow!(
                    "{} transcripts must use their SQLite adapter",
                    harness.as_str()
                ));
            }
        }
    }
    if harness == ManagedHarness::Kimi {
        annotate_kimi_subagents(path, &mut losses);
    }
    Ok(ExportedTranscript {
        native_session_id: native_session_id.to_string(),
        source_cursor: Some(serde_json::to_string(&FileCursor {
            path: path.to_string_lossy().into_owned(),
            offset: committed_offset,
            flavor,
            prefix_sha256: journal_rewrites_in_place(harness)
                .then(|| format!("{:x}", prefix_hasher.finalize())),
        })?),
        events,
        losses: deduplicate_losses(losses),
    })
}

fn hash_file_prefix(file: &mut File, len: u64) -> Result<Option<Sha256>> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = [0_u8; 16 * 1024];
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..limit])?;
        if read == 0 {
            return Ok(None);
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(Some(hasher))
}

fn parse_claude(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    if value.get("isMeta").and_then(Value::as_bool) == Some(true) {
        losses.push("Claude synthetic/meta records were intentionally excluded".into());
        return;
    }
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let compact_boundary = record_type == "system"
        && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary");
    if compact_boundary || matches!(record_type, "summary" | "compact" | "compaction") {
        if let Some(text) = first_string(value, &["summary", "content", "text"]) {
            push_event(
                events,
                AgentKind::ClaudeCode,
                session,
                record_id,
                0,
                WorkstreamEventKind::Compaction,
                Some("assistant"),
                text,
                timestamp(value),
                json!({}),
            );
        }
        return;
    }
    if !matches!(record_type, "user" | "assistant") {
        return;
    }
    let message = value.get("message").unwrap_or(value);
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or(record_type);
    if !matches!(role, "user" | "assistant") {
        losses.push("Claude non-conversation message records were intentionally excluded".into());
        return;
    }
    parse_content_blocks(
        AgentKind::ClaudeCode,
        session,
        record_id,
        role,
        message.get("content"),
        timestamp(value),
        events,
        losses,
    );
}

fn parse_codex(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let record_type = value.get("type").and_then(Value::as_str);
    let payload = value.get("payload").unwrap_or(&Value::Null);
    if record_type == Some("compacted") {
        if let Some(summary) = first_string(payload, &["message", "summary", "content", "text"]) {
            push_event(
                events,
                AgentKind::Codex,
                session,
                record_id,
                0,
                WorkstreamEventKind::Compaction,
                Some("assistant"),
                summary,
                timestamp(value),
                json!({}),
            );
        }
        return;
    }
    if record_type != Some("response_item") {
        return;
    }
    let item_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match item_type {
        "message" => {
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(role, "user" | "assistant") {
                return;
            }
            parse_content_blocks(
                AgentKind::Codex,
                session,
                record_id,
                role,
                payload.get("content"),
                timestamp(value),
                events,
                losses,
            );
        }
        "function_call" | "custom_tool_call" | "tool_call" => {
            let name = first_string(payload, &["name", "tool"]).unwrap_or("tool");
            let body = first_string(payload, &["arguments", "input", "text"]).unwrap_or("");
            push_event(
                events,
                AgentKind::Codex,
                session,
                record_id,
                0,
                WorkstreamEventKind::ToolCall,
                Some("assistant"),
                &format!("{name}: {body}"),
                timestamp(value),
                json!({"tool": name}),
            );
        }
        "function_call_output" | "custom_tool_call_output" | "tool_result" => {
            let body = first_string(payload, &["output", "content", "text"]).unwrap_or("");
            push_event(
                events,
                AgentKind::Codex,
                session,
                record_id,
                0,
                WorkstreamEventKind::ToolResult,
                Some("tool"),
                body,
                timestamp(value),
                json!({}),
            );
        }
        "web_search_call" => {
            let action = payload.get("action").map(compact_json).unwrap_or_default();
            push_event(
                events,
                AgentKind::Codex,
                session,
                record_id,
                0,
                WorkstreamEventKind::ToolCall,
                Some("assistant"),
                &format!("web_search: {action}"),
                timestamp(value),
                json!({"tool": "web_search", "status": payload.get("status").and_then(Value::as_str)}),
            );
        }
        "compacted" | "compaction" => {
            let body = first_string(payload, &["summary", "content", "text"]).unwrap_or("");
            push_event(
                events,
                AgentKind::Codex,
                session,
                record_id,
                0,
                WorkstreamEventKind::Compaction,
                Some("assistant"),
                body,
                timestamp(value),
                json!({}),
            );
        }
        "reasoning" => losses.push("Codex hidden reasoning was intentionally excluded".into()),
        _ => {}
    }
}

fn parse_pi_family(
    agent: AgentKind,
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match record_type {
        "message" => {
            let message = value.get("message").unwrap_or(value);
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("assistant");
            match role {
                "user" | "assistant" => parse_content_blocks(
                    agent,
                    session,
                    record_id,
                    role,
                    message.get("content"),
                    timestamp(value),
                    events,
                    losses,
                ),
                "tool" | "toolResult" | "tool_result" => {
                    let body = message.get("content").map(value_text).unwrap_or_default();
                    let tool = first_string(message, &["toolName", "tool", "name"]);
                    push_event(
                        events,
                        agent,
                        session,
                        record_id,
                        0,
                        WorkstreamEventKind::ToolResult,
                        Some("tool"),
                        &body,
                        timestamp(value),
                        json!({
                            "tool": tool,
                            "is_error": message.get("isError").or_else(|| message.get("is_error")).and_then(Value::as_bool).unwrap_or(false)
                        }),
                    );
                }
                _ => losses.push(format!(
                    "{} non-conversation message records were intentionally excluded",
                    agent.as_str()
                )),
            }
        }
        "compaction" | "compact" | "summary" => {
            let body = first_string(value, &["summary", "content", "text"]).unwrap_or("");
            push_event(
                events,
                agent,
                session,
                record_id,
                0,
                WorkstreamEventKind::Compaction,
                Some("assistant"),
                body,
                timestamp(value),
                json!({}),
            );
        }
        _ => {}
    }
}

/// Kimi Code wire journal (`agents/main/wire.jsonl`): flat records
/// `{type, time?, ...payload}`. `context.append_message` stores user messages
/// and legacy/imported conversation records; native assistant output and tool
/// exchanges are recorded as `context.append_loop_event`. Records like
/// `config.update`/`llm.request` carry private harness data (system prompts,
/// request bodies) that must never reach the ledger. Unknown record types are
/// ignored so newer Kimi versions stay forward-compatible.
fn parse_kimi(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let occurred_at = kimi_timestamp(value);
    match record_type {
        "context.append_message" => {
            let message = value.get("message").unwrap_or(&Value::Null);
            // A `partial` message is re-appended complete once the stream
            // finishes; importing the fragment would duplicate it.
            if message.get("partial").and_then(Value::as_bool) == Some(true) {
                return;
            }
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match role {
                "system" => {
                    losses.push("Kimi system messages were intentionally excluded".into());
                }
                "user" => {
                    // Only genuine user input is imported. Origin-tagged
                    // messages are harness-injected context — including our
                    // own handoff delta (`hook_result`), which would feed the
                    // ledger back into itself.
                    let injected = message
                        .get("origin")
                        .and_then(|origin| origin.get("kind"))
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != "user");
                    if injected {
                        losses.push(
                            "Kimi harness-injected messages were intentionally excluded".into(),
                        );
                        return;
                    }
                    parse_kimi_parts(
                        message,
                        WorkstreamEventKind::Message,
                        "user",
                        session,
                        record_id,
                        occurred_at,
                        events,
                        losses,
                    );
                }
                "assistant" => {
                    let block_count = parse_kimi_parts(
                        message,
                        WorkstreamEventKind::Message,
                        "assistant",
                        session,
                        record_id,
                        occurred_at.clone(),
                        events,
                        losses,
                    );
                    let tool_calls = message
                        .get("toolCalls")
                        .and_then(Value::as_array)
                        .map_or(&[][..], Vec::as_slice);
                    for (index, call) in tool_calls.iter().enumerate() {
                        let function = call.get("function").unwrap_or(call);
                        let name = first_string(function, &["name"]).unwrap_or("tool");
                        // `arguments` is a JSON string; re-serialize it
                        // compact when it parses, mirroring parse_codex's
                        // `"{name}: {body}"` tool-call shape.
                        let arguments = call
                            .get("arguments")
                            .or_else(|| function.get("arguments"))
                            .and_then(Value::as_str)
                            .map(|raw| match serde_json::from_str::<Value>(raw) {
                                Ok(parsed) => compact_json(&parsed),
                                Err(_) => raw.to_string(),
                            })
                            .unwrap_or_default();
                        push_event(
                            events,
                            AgentKind::KimiCode,
                            session,
                            record_id,
                            block_count + index,
                            WorkstreamEventKind::ToolCall,
                            Some("assistant"),
                            &format!("{name}: {arguments}"),
                            occurred_at.clone(),
                            json!({"tool": name}),
                        );
                    }
                }
                "tool" => {
                    let texts = kimi_text_parts(message, losses);
                    let body = texts.parts.join("\n");
                    push_event(
                        events,
                        AgentKind::KimiCode,
                        session,
                        record_id,
                        0,
                        WorkstreamEventKind::ToolResult,
                        Some("tool"),
                        &body,
                        occurred_at,
                        json!({}),
                    );
                }
                _ => {}
            }
        }
        "context.append_loop_event" => {
            let event = value.get("event").unwrap_or(&Value::Null);
            match event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "content.part" => {
                    let part = event.get("part").unwrap_or(&Value::Null);
                    match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "text" => {
                            if let Some(text) = first_string(part, &["text", "content"]) {
                                push_event(
                                    events,
                                    AgentKind::KimiCode,
                                    session,
                                    record_id,
                                    0,
                                    WorkstreamEventKind::Message,
                                    Some("assistant"),
                                    text,
                                    occurred_at,
                                    json!({}),
                                );
                            }
                        }
                        "think" | "thinking" => {
                            losses.push("Kimi hidden reasoning was intentionally excluded".into());
                        }
                        _ => {
                            losses.push(
                                "Kimi non-text content parts were intentionally excluded".into(),
                            );
                        }
                    }
                }
                "tool.call" => {
                    let name = first_string(event, &["name"]).unwrap_or("tool");
                    let arguments = event.get("args").map(compact_json).unwrap_or_default();
                    push_event(
                        events,
                        AgentKind::KimiCode,
                        session,
                        record_id,
                        0,
                        WorkstreamEventKind::ToolCall,
                        Some("assistant"),
                        &format!("{name}: {arguments}"),
                        occurred_at,
                        json!({
                            "tool": name,
                            "tool_call_id": event.get("toolCallId").and_then(Value::as_str)
                        }),
                    );
                }
                "tool.result" => {
                    let result = event.get("result").unwrap_or(&Value::Null);
                    let texts =
                        kimi_content_parts(result.get("output").unwrap_or(&Value::Null), losses);
                    push_event(
                        events,
                        AgentKind::KimiCode,
                        session,
                        record_id,
                        0,
                        WorkstreamEventKind::ToolResult,
                        Some("tool"),
                        &texts.parts.join("\n"),
                        occurred_at,
                        json!({
                            "tool_call_id": event.get("toolCallId").and_then(Value::as_str),
                            "is_error": result.get("isError").and_then(Value::as_bool)
                        }),
                    );
                }
                _ => {}
            }
        }
        "context.apply_compaction" => {
            if let Some(summary) = value.get("summary").and_then(Value::as_str) {
                push_event(
                    events,
                    AgentKind::KimiCode,
                    session,
                    record_id,
                    0,
                    WorkstreamEventKind::Compaction,
                    Some("assistant"),
                    summary,
                    occurred_at,
                    json!({}),
                );
            }
        }
        _ => {}
    }
}

/// Command Code v3 session record. The stable documentation guarantees an
/// append-only tree; the record-level allowlist was checked against the
/// integrity-matched published 1.14.1 bundle and a sanitized live fixture.
/// Parent ids are retained so a consumer can distinguish branch changes
/// without importing hidden reasoning or provider metadata.
fn parse_command_code(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let record_type = value.get("type").and_then(Value::as_str);
    let parent_id = match value.get("parentId") {
        Some(Value::String(parent)) => Some(parent.as_str()),
        Some(Value::Null) | None => None,
        Some(_) => {
            losses.push("Command Code malformed parent ids were intentionally excluded".into());
            return;
        }
    };
    if matches!(record_type, Some("compaction") | Some("branch_summary")) {
        let Some(summary) = value.get("summary").and_then(Value::as_str) else {
            losses.push("Command Code malformed summaries were intentionally excluded".into());
            return;
        };
        let (kind, summary_type) = if record_type == Some("compaction") {
            (WorkstreamEventKind::Compaction, "compaction")
        } else {
            (WorkstreamEventKind::Message, "branch-summary")
        };
        push_event(
            events,
            AgentKind::CommandCode,
            session,
            record_id,
            0,
            kind,
            Some("assistant"),
            summary,
            timestamp(value),
            json!({"parent_id": parent_id, "summary_type": summary_type}),
        );
        return;
    }
    if record_type != Some("message") {
        return;
    }
    let Some(message) = value.get("message").and_then(Value::as_object) else {
        losses.push("Command Code malformed message records were intentionally excluded".into());
        return;
    };
    let Some(role) = message.get("role").and_then(Value::as_str) else {
        losses.push("Command Code messages without a role were intentionally excluded".into());
        return;
    };
    let source = message
        .get("meta")
        .and_then(|meta| meta.get("source"))
        .and_then(Value::as_str);
    if message
        .get("meta")
        .and_then(|meta| meta.get("isMeta"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        losses.push("Command Code synthetic/meta messages were intentionally excluded".into());
        return;
    }
    if !matches!(
        (role, source),
        ("user", Some("user")) | ("assistant", Some("model"))
    ) {
        losses
            .push("Command Code non-user/model message records were intentionally excluded".into());
        return;
    }
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        losses.push(
            "Command Code messages without content blocks were intentionally excluded".into(),
        );
        return;
    };
    let occurred_at = timestamp(value);
    for (index, part) in parts.iter().enumerate() {
        match (role, part.get("type").and_then(Value::as_str)) {
            ("user", Some("text")) => {
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    losses.push(
                        "Command Code malformed text blocks were intentionally excluded".into(),
                    );
                    continue;
                };
                push_event(
                    events,
                    AgentKind::CommandCode,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::Message,
                    Some("user"),
                    text,
                    occurred_at.clone(),
                    json!({"parent_id": parent_id}),
                );
            }
            ("assistant", Some("text")) => {
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    losses.push(
                        "Command Code malformed text blocks were intentionally excluded".into(),
                    );
                    continue;
                };
                push_event(
                    events,
                    AgentKind::CommandCode,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::Message,
                    Some("assistant"),
                    text,
                    occurred_at.clone(),
                    json!({"parent_id": parent_id}),
                );
            }
            ("assistant", Some("thinking")) => {
                losses.push("Command Code hidden reasoning was intentionally excluded".into());
            }
            ("assistant", Some("tool_use")) => {
                let (Some(tool_id), Some(name), Some(input)) = (
                    part.get("id").and_then(Value::as_str),
                    part.get("name").and_then(Value::as_str),
                    part.get("input").filter(|value| value.is_object()),
                ) else {
                    losses.push(
                        "Command Code malformed tool calls were intentionally excluded".into(),
                    );
                    continue;
                };
                push_event(
                    events,
                    AgentKind::CommandCode,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::ToolCall,
                    Some("assistant"),
                    &format!("{name}: {}", compact_json(input)),
                    occurred_at.clone(),
                    json!({"tool": name, "tool_use_id": tool_id, "parent_id": parent_id}),
                );
            }
            ("user", Some("tool_result")) => {
                parse_command_code_tool_result(
                    part,
                    session,
                    record_id,
                    index,
                    parent_id,
                    occurred_at.clone(),
                    events,
                    losses,
                );
            }
            (_, Some(_)) | (_, None) => losses
                .push("Command Code unsupported content blocks were intentionally excluded".into()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_command_code_tool_result(
    part: &Value,
    session: &str,
    record_id: &str,
    block: usize,
    parent_id: Option<&str>,
    occurred_at: Option<String>,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let Some(tool_use_id) = part.get("tool_use_id").and_then(Value::as_str) else {
        losses.push("Command Code malformed tool results were intentionally excluded".into());
        return;
    };
    let Some(parts) = part.get("content").and_then(Value::as_array) else {
        losses.push("Command Code malformed tool results were intentionally excluded".into());
        return;
    };
    let mut texts = Vec::new();
    for item in parts {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    texts.push(text);
                } else {
                    losses.push(
                        "Command Code malformed tool-result text was intentionally excluded".into(),
                    );
                }
            }
            Some("image") => {
                losses.push("Command Code image tool results were intentionally excluded".into());
            }
            _ => losses.push(
                "Command Code unsupported tool-result content was intentionally excluded".into(),
            ),
        }
    }
    push_event(
        events,
        AgentKind::CommandCode,
        session,
        record_id,
        block,
        WorkstreamEventKind::ToolResult,
        Some("tool"),
        &texts.join("\n"),
        occurred_at,
        json!({
            "tool_use_id": tool_use_id,
            "parent_id": parent_id,
            "is_error": part.get("is_error").and_then(Value::as_bool).unwrap_or(false),
        }),
    );
}

/// One event per text part of a kimi message; `think` reasoning and media
/// parts become loss annotations. Returns the number of content parts seen so
/// callers can index sibling events (tool calls) without collisions.
#[allow(clippy::too_many_arguments)]
fn parse_kimi_parts(
    message: &Value,
    kind: WorkstreamEventKind,
    role: &str,
    session: &str,
    record_id: &str,
    occurred_at: Option<String>,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) -> usize {
    let texts = kimi_text_parts(message, losses);
    let part_count = texts.part_count;
    for (index, text) in texts.parts.iter().enumerate() {
        push_event(
            events,
            AgentKind::KimiCode,
            session,
            record_id,
            index,
            kind,
            Some(role),
            text,
            occurred_at.clone(),
            json!({}),
        );
    }
    part_count
}

struct KimiTextParts {
    parts: Vec<String>,
    part_count: usize,
}

/// Collect the visible text of a kimi message's content parts in order,
/// annotating parts that cannot be imported (hidden reasoning, media).
fn kimi_text_parts(message: &Value, losses: &mut Vec<String>) -> KimiTextParts {
    kimi_content_parts(message.get("content").unwrap_or(&Value::Null), losses)
}

fn kimi_content_parts(content: &Value, losses: &mut Vec<String>) -> KimiTextParts {
    let parts: Vec<&Value> = content
        .as_array()
        .map_or_else(|| vec![content], |items| items.iter().collect());
    let mut texts = Vec::with_capacity(parts.len());
    for part in &parts {
        if let Some(text) = part.as_str() {
            texts.push(text.to_string());
            continue;
        }
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = first_string(part, &["text", "content"]) {
                    texts.push(text.to_string());
                }
            }
            // kosong calls the reasoning part `think`; tolerate `thinking`
            // for forward compatibility.
            Some("think" | "thinking") => {
                losses.push("Kimi hidden reasoning was intentionally excluded".into());
            }
            Some(_) => {
                losses.push("Kimi non-text content parts were intentionally excluded".into());
            }
            None => {}
        }
    }
    KimiTextParts {
        parts: texts,
        part_count: parts.len(),
    }
}

/// Kimi wire envelopes carry `time` as an optional ms epoch; other adapters
/// keep the harness's native ISO string, so render this one the same way.
fn kimi_timestamp(value: &Value) -> Option<String> {
    let millis = value.get("time").and_then(Value::as_i64)?;
    jiff::Timestamp::from_millisecond(millis)
        .ok()
        .map(|timestamp| timestamp.to_string())
}

/// Import one Kiro v2-engine session event: a versioned envelope
/// `{"version":"v1","kind":…,"data":{"message_id":…,"content":[…],"meta":…}}`
/// whose `content` parts carry their own `kind`/`data`. Visible kinds:
/// `Prompt` (user), `AssistantMessage` (assistant text + `toolUse` parts),
/// `ToolResults` (`toolResult` parts). Unknown record kinds within the
/// `v1` envelope are ignored so newer Kiro versions stay
/// forward-compatible; a non-`v1` envelope version is annotated instead —
/// that axis signals a schema break, not an additive record type.
fn parse_kiro(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    match value.get("version").and_then(Value::as_str) {
        Some("v1") => {}
        Some(other) => {
            losses.push(format!(
                "Kiro records with unsupported envelope version {other} were skipped"
            ));
            return;
        }
        None => {
            losses.push("Kiro records without an envelope version were skipped".into());
            return;
        }
    }
    let data = value.get("data").unwrap_or(&Value::Null);
    let occurred_at = kiro_timestamp(data);
    let parts = data
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    match value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "Prompt" => {
            for (index, part) in parts.iter().enumerate() {
                match part.get("kind").and_then(Value::as_str).unwrap_or_default() {
                    "text" => {
                        if let Some(text) = part.get("data").and_then(Value::as_str) {
                            push_event(
                                events,
                                AgentKind::KiroCli,
                                session,
                                record_id,
                                index,
                                WorkstreamEventKind::Message,
                                Some("user"),
                                text,
                                occurred_at.clone(),
                                json!({}),
                            );
                        }
                    }
                    _ => {
                        losses
                            .push("Kiro non-text content parts were intentionally excluded".into());
                    }
                }
            }
        }
        "AssistantMessage" => {
            for (index, part) in parts.iter().enumerate() {
                match part.get("kind").and_then(Value::as_str).unwrap_or_default() {
                    "text" => {
                        if let Some(text) = part.get("data").and_then(Value::as_str) {
                            push_event(
                                events,
                                AgentKind::KiroCli,
                                session,
                                record_id,
                                index,
                                WorkstreamEventKind::Message,
                                Some("assistant"),
                                text,
                                occurred_at.clone(),
                                json!({}),
                            );
                        }
                    }
                    "toolUse" => {
                        let tool = part.get("data").unwrap_or(&Value::Null);
                        let name = first_string(tool, &["name", "toolName", "tool_name"])
                            .unwrap_or("tool");
                        let arguments = tool
                            .get("input")
                            .or_else(|| tool.get("args"))
                            .or_else(|| tool.get("arguments"))
                            .map(compact_json)
                            .unwrap_or_else(|| compact_json(tool));
                        push_event(
                            events,
                            AgentKind::KiroCli,
                            session,
                            record_id,
                            index,
                            WorkstreamEventKind::ToolCall,
                            Some("assistant"),
                            &format!("{name}: {arguments}"),
                            occurred_at.clone(),
                            json!({
                                "tool": name,
                                "tool_call_id": first_string(tool, &["tool_use_id", "toolUseId", "id"])
                            }),
                        );
                    }
                    _ => {
                        losses
                            .push("Kiro non-text content parts were intentionally excluded".into());
                    }
                }
            }
        }
        "ToolResults" => {
            for (index, part) in parts.iter().enumerate() {
                if part.get("kind").and_then(Value::as_str) != Some("toolResult") {
                    continue;
                }
                let result = part.get("data").unwrap_or(&Value::Null);
                let body = kiro_result_text(result)
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| compact_json(result));
                push_event(
                    events,
                    AgentKind::KiroCli,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::ToolResult,
                    Some("tool"),
                    &body,
                    occurred_at.clone(),
                    json!({
                        "tool_call_id": first_string(result, &["tool_use_id", "toolUseId", "id"]),
                        "is_error": result
                            .get("is_error")
                            .or_else(|| result.get("isError"))
                            .and_then(Value::as_bool)
                    }),
                );
            }
        }
        _ => {}
    }
}

/// Import the visible allowlist from Kiro v3's `messages.jsonl` journal.
/// Session bookkeeping, lifecycle-hook records, usage summaries, turn
/// boundaries, and assistant operations other than visible `Say` output are
/// deliberately excluded.
fn parse_kiro_v3(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let payload = value.get("payload").unwrap_or(&Value::Null);
    let occurred_at = timestamp(value);
    match payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "user" => {
            if let Some(content) = payload.get("content").and_then(Value::as_str) {
                push_event(
                    events,
                    AgentKind::KiroCli,
                    session,
                    record_id,
                    0,
                    WorkstreamEventKind::Message,
                    Some("user"),
                    content,
                    occurred_at,
                    json!({}),
                );
            }
        }
        "assistant" => {
            if payload.get("operationType").and_then(Value::as_str) != Some("Say") {
                losses.push(
                    "Kiro v3 non-visible assistant operations were intentionally excluded".into(),
                );
                return;
            }
            if let Some(content) = payload.get("content").and_then(Value::as_str) {
                push_event(
                    events,
                    AgentKind::KiroCli,
                    session,
                    record_id,
                    0,
                    WorkstreamEventKind::Message,
                    Some("assistant"),
                    content,
                    occurred_at,
                    json!({}),
                );
            }
        }
        "tool_call" => {
            let name = payload
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let arguments = payload.get("args").map(compact_json).unwrap_or_default();
            push_event(
                events,
                AgentKind::KiroCli,
                session,
                record_id,
                0,
                WorkstreamEventKind::ToolCall,
                Some("assistant"),
                &format!("{name}: {arguments}"),
                occurred_at,
                json!({
                    "tool": name,
                    "tool_call_id": payload.get("toolCallId").and_then(Value::as_str)
                }),
            );
        }
        "tool_result" => {
            let Some(content) = payload.get("content").and_then(Value::as_str) else {
                return;
            };
            push_event(
                events,
                AgentKind::KiroCli,
                session,
                record_id,
                0,
                WorkstreamEventKind::ToolResult,
                Some("tool"),
                content,
                occurred_at,
                json!({
                    "tool_call_id": payload.get("toolCallId").and_then(Value::as_str),
                    "is_error": payload.get("success").and_then(Value::as_bool).map(|success| !success)
                }),
            );
        }
        "ContextualHookInvoked"
        | "session_metadata"
        | "usage_summary"
        | "turn_start"
        | "turn_end"
        | "session_start"
        | "session_event" => {
            losses.push("Kiro v3 private session records were intentionally excluded".into());
        }
        _ => {}
    }
}

/// Kiro event timestamps ride `data.meta.timestamp` as a unix epoch in
/// milliseconds.
fn kiro_timestamp(data: &Value) -> Option<String> {
    let millis = data.get("meta")?.get("timestamp").and_then(Value::as_i64)?;
    jiff::Timestamp::from_millisecond(millis)
        .ok()
        .map(|timestamp| timestamp.to_string())
}

/// Text of a Kiro tool result: its `content` is an array of the same
/// `{kind, data}` parts the message records use (text parts join), with
/// plain-string `content`/`output`/`text` fields tolerated as fallbacks.
fn kiro_result_text(result: &Value) -> Option<String> {
    if let Some(parts) = result.get("content").and_then(Value::as_array) {
        let texts: Vec<&str> = parts
            .iter()
            .filter(|part| part.get("kind").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("data").and_then(Value::as_str))
            .collect();
        if !texts.is_empty() {
            return Some(texts.join("\n"));
        }
    }
    ["content", "output", "text"]
        .iter()
        .find_map(|key| result.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

/// Subagent journals (`agents/<id != main>/wire.jsonl`) are not imported in
/// v1; annotate the gap once so the omission is visible in the ledger.
fn annotate_kimi_subagents(path: &Path, losses: &mut Vec<String>) {
    let Some(agents_dir) = path.ancestors().nth(2) else {
        return;
    };
    let Ok(entries) = fs::read_dir(agents_dir) else {
        return;
    };
    let has_subagent = entries
        .flatten()
        .any(|entry| entry.file_name() != "main" && entry.path().join("wire.jsonl").is_file());
    if has_subagent {
        losses.push("Kimi subagent transcripts were not imported".into());
    }
}

/// Grok chat history (`<session-dir>/chat_history.jsonl`): flat records
/// `{type, ...}`. `system` carries the private system prompt and `reasoning`
/// carries encrypted hidden reasoning — neither may reach the ledger. Tool
/// calls ride on the `assistant` record's `tool_calls` array and pair with
/// separate `tool_result` records; `backend_tool_call` records server-side
/// tools such as web search. Unknown record types are ignored so newer Grok
/// versions stay forward-compatible.
fn parse_grok(
    value: &Value,
    session: &str,
    record_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let record_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match record_type {
        "system" => {
            losses.push("Grok system prompt records were intentionally excluded".into());
        }
        "reasoning" => {
            losses.push("Grok hidden reasoning was intentionally excluded".into());
        }
        "user" => {
            parse_content_blocks(
                AgentKind::Grok,
                session,
                record_id,
                "user",
                value.get("content"),
                timestamp(value),
                events,
                losses,
            );
        }
        "assistant" => {
            let block_count = if let Some(text) = value.get("content").and_then(Value::as_str) {
                push_event(
                    events,
                    AgentKind::Grok,
                    session,
                    record_id,
                    0,
                    WorkstreamEventKind::Message,
                    Some("assistant"),
                    text,
                    timestamp(value),
                    json!({}),
                );
                1
            } else {
                0
            };
            let tool_calls = value
                .get("tool_calls")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            for (index, call) in tool_calls.iter().enumerate() {
                let name = first_string(call, &["name"]).unwrap_or("tool");
                // `arguments` is a JSON string; re-serialize it compact when
                // it parses, mirroring parse_codex's tool-call shape.
                let arguments = call
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(|raw| match serde_json::from_str::<Value>(raw) {
                        Ok(parsed) => compact_json(&parsed),
                        Err(_) => raw.to_string(),
                    })
                    .unwrap_or_default();
                push_event(
                    events,
                    AgentKind::Grok,
                    session,
                    record_id,
                    block_count + index,
                    WorkstreamEventKind::ToolCall,
                    Some("assistant"),
                    &format!("{name}: {arguments}"),
                    timestamp(value),
                    json!({"tool": name}),
                );
            }
        }
        "tool_result" => {
            let body = value.get("content").map(value_text).unwrap_or_default();
            push_event(
                events,
                AgentKind::Grok,
                session,
                record_id,
                0,
                WorkstreamEventKind::ToolResult,
                Some("tool"),
                &body,
                timestamp(value),
                json!({}),
            );
        }
        "backend_tool_call" => {
            let kind = value.get("kind").unwrap_or(&Value::Null);
            let tool = first_string(kind, &["tool_type"]).unwrap_or("backend_tool");
            let action = kind.get("action").map(compact_json).unwrap_or_default();
            push_event(
                events,
                AgentKind::Grok,
                session,
                record_id,
                0,
                WorkstreamEventKind::ToolCall,
                Some("assistant"),
                &format!("{tool}: {action}"),
                timestamp(value),
                json!({"tool": tool}),
            );
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_content_blocks(
    agent: AgentKind,
    session: &str,
    record_id: &str,
    role: &str,
    content: Option<&Value>,
    occurred_at: Option<String>,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let Some(content) = content else { return };
    let blocks: Vec<&Value> = content
        .as_array()
        .map_or_else(|| vec![content], |items| items.iter().collect());
    for (index, block) in blocks.into_iter().enumerate() {
        if let Some(text) = block.as_str() {
            if codex_synthetic_context(agent, role, text) {
                continue;
            }
            push_event(
                events,
                agent,
                session,
                record_id,
                index,
                WorkstreamEventKind::Message,
                Some(role),
                text,
                occurred_at.clone(),
                json!({}),
            );
            continue;
        }
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match block_type {
            "text" | "input_text" | "output_text" => {
                if let Some(text) = first_string(block, &["text", "content"]) {
                    if codex_synthetic_context(agent, role, text) {
                        continue;
                    }
                    push_event(
                        events,
                        agent,
                        session,
                        record_id,
                        index,
                        WorkstreamEventKind::Message,
                        Some(role),
                        text,
                        occurred_at.clone(),
                        json!({}),
                    );
                }
            }
            "tool_use" | "toolCall" | "tool_call" => {
                let name = first_string(block, &["name", "toolName", "tool"]).unwrap_or("tool");
                let input = block
                    .get("input")
                    .or_else(|| block.get("arguments"))
                    .map(compact_json)
                    .unwrap_or_default();
                push_event(
                    events,
                    agent,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::ToolCall,
                    Some("assistant"),
                    &format!("{name}: {input}"),
                    occurred_at.clone(),
                    json!({"tool": name}),
                );
            }
            "tool_result" | "toolResult" => {
                let body = block.get("content").map(value_text).unwrap_or_default();
                if claude_managed_packet_echo(agent, &body) {
                    losses.push(
                        "Claude managed workstream delivery packets were intentionally excluded"
                            .into(),
                    );
                    continue;
                }
                push_event(
                    events,
                    agent,
                    session,
                    record_id,
                    index,
                    WorkstreamEventKind::ToolResult,
                    Some("tool"),
                    &body,
                    occurred_at.clone(),
                    json!({"is_error": block.get("is_error").and_then(Value::as_bool).unwrap_or(false)}),
                );
            }
            "thinking" | "reasoning" | "redacted_thinking" => {
                losses.push(format!(
                    "{} hidden reasoning was intentionally excluded",
                    agent.as_str()
                ));
            }
            _ => {}
        }
    }
}

fn claude_managed_packet_echo(agent: AgentKind, body: &str) -> bool {
    if agent != AgentKind::ClaudeCode {
        return false;
    }
    let body = body.trim_start();
    body.starts_with(MANAGED_WORKSTREAM_PACKET_MARKER)
        || body.starts_with(LEGACY_MANAGED_WORKSTREAM_PACKET_PREFIX)
}

fn codex_synthetic_context(agent: AgentKind, role: &str, text: &str) -> bool {
    if role != "user" {
        return false;
    }
    let trimmed = text.trim_start();
    match agent {
        AgentKind::Codex => {
            trimmed.starts_with("# AGENTS.md instructions for ")
                || trimmed.starts_with("<environment_context>")
                || trimmed.starts_with("<permissions instructions>")
                || trimmed.starts_with("<INSTRUCTIONS>")
        }
        AgentKind::ClaudeCode => trimmed.starts_with("<system-reminder>"),
        // Grok stores harness scaffolding inside `user` records rather than a
        // separate record type: an environment block, then Claude-style
        // reminders carrying project instructions, the skills catalogue, and
        // the connected MCP servers. A single session's reminders measured
        // 42KB against 270 bytes of real input, so importing them both leaks
        // harness internals into the portable ledger and evicts real
        // conversation from the startup packet budget. Genuine input arrives
        // wrapped in `<user_query>`.
        AgentKind::Grok => {
            trimmed.starts_with("<user_info>") || trimmed.starts_with("<system-reminder>")
        }
        _ => false,
    }
}

fn export_opencode(
    home: &Path,
    session_dir: Option<&Path>,
    session: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let db = opencode_db(home, session_dir);
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "opening OpenCode session database {} read-only",
            db.display()
        )
    })?;
    let cursor = source_cursor
        .and_then(|raw| serde_json::from_str::<SqlCursor>(raw).ok())
        .unwrap_or_default();
    let mut statement = connection.prepare(
        "SELECT p.id, p.time_updated, m.data, p.data
         FROM part p JOIN message m ON m.id = p.message_id
         WHERE p.session_id = ?1 AND (p.time_updated > ?2 OR (p.time_updated = ?2 AND p.id > ?3))
         ORDER BY p.time_updated, p.id",
    )?;
    let rows = statement.query_map(params![session, cursor.updated, cursor.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut losses = Vec::new();
    let mut next_cursor = cursor;
    for row in rows {
        let (id, updated, message_raw, part_raw) = row?;
        next_cursor = SqlCursor {
            updated,
            id: id.clone(),
        };
        let Ok(message) = serde_json::from_str::<Value>(&message_raw) else {
            losses.push(format!("malformed OpenCode message for part {id}"));
            continue;
        };
        let Ok(part) = serde_json::from_str::<Value>(&part_raw) else {
            losses.push(format!("malformed OpenCode part {id}"));
            continue;
        };
        parse_opencode(&message, &part, session, &id, &mut events, &mut losses);
    }
    Ok(ExportedTranscript {
        native_session_id: session.to_string(),
        source_cursor: Some(serde_json::to_string(&next_cursor)?),
        events,
        losses: deduplicate_losses(losses),
    })
}

fn export_crush(
    cwd: &Path,
    session_dir: Option<&Path>,
    session: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let db = crush_db(cwd, session_dir);
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening Crush session database {} read-only", db.display()))?;
    let cursor = source_cursor
        .and_then(|raw| serde_json::from_str::<SqlCursor>(raw).ok())
        .unwrap_or_default();
    let mut statement = connection.prepare(
        "SELECT id, role, parts, updated_at, is_summary_message \
         FROM messages \
         WHERE session_id = ?1 \
           AND (updated_at > ?2 OR (updated_at = ?2 AND id > ?3)) \
         ORDER BY updated_at, id",
    )?;
    let rows = statement.query_map(params![session, cursor.updated, cursor.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut losses = Vec::new();
    let mut next_cursor = cursor;
    for row in rows {
        let (id, role, parts_raw, updated, is_summary) = row?;
        next_cursor = SqlCursor {
            updated,
            id: id.clone(),
        };
        let Ok(parts) = serde_json::from_str::<Value>(&parts_raw) else {
            losses.push(format!("malformed Crush message parts for {id}"));
            continue;
        };
        let Some(parts) = parts.as_array() else {
            losses.push(format!("malformed Crush message parts for {id}"));
            continue;
        };
        parse_crush_parts(
            &role,
            parts,
            is_summary != 0,
            session,
            &id,
            &mut events,
            &mut losses,
        );
    }
    Ok(ExportedTranscript {
        native_session_id: session.to_string(),
        source_cursor: Some(serde_json::to_string(&next_cursor)?),
        events,
        losses: deduplicate_losses(losses),
    })
}

fn parse_crush_parts(
    role: &str,
    parts: &[Value],
    is_summary: bool,
    session: &str,
    message_id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    for (index, part) in parts.iter().enumerate() {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
        let data = part.get("data").unwrap_or(&Value::Null);
        match kind {
            "text" if matches!(role, "user" | "assistant") => {
                if let Some(content) = first_string(data, &["text", "content"]) {
                    push_event(
                        events,
                        AgentKind::Crush,
                        session,
                        message_id,
                        index,
                        if is_summary {
                            WorkstreamEventKind::Compaction
                        } else {
                            WorkstreamEventKind::Message
                        },
                        Some(role),
                        content,
                        None,
                        json!({}),
                    );
                }
            }
            "tool_call" => {
                if data.get("finished").and_then(Value::as_bool) == Some(false) {
                    losses.push("unfinished Crush tool calls were intentionally excluded".into());
                    continue;
                }
                let name = data.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = data.get("input").map(value_text).unwrap_or_default();
                push_event(
                    events,
                    AgentKind::Crush,
                    session,
                    message_id,
                    index,
                    WorkstreamEventKind::ToolCall,
                    Some("assistant"),
                    &format!("{name}: {input}"),
                    None,
                    json!({"tool": name}),
                );
            }
            "tool_result" => {
                let name = data.get("name").and_then(Value::as_str).unwrap_or("tool");
                let content = data
                    .get("content")
                    .or_else(|| data.get("data"))
                    .map(value_text)
                    .unwrap_or_default();
                push_event(
                    events,
                    AgentKind::Crush,
                    session,
                    message_id,
                    index,
                    WorkstreamEventKind::ToolResult,
                    Some("tool"),
                    &content,
                    None,
                    json!({
                        "tool": name,
                        "is_error": data.get("is_error").and_then(Value::as_bool)
                    }),
                );
            }
            "reasoning" => losses.push("Crush hidden reasoning was intentionally excluded".into()),
            "binary" => losses.push("Crush binary attachment was intentionally excluded".into()),
            _ => {}
        }
    }
}

fn parse_opencode(
    message: &Value,
    part: &Value,
    session: &str,
    id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "text" => {
            if !matches!(role, "user" | "assistant") {
                losses.push(
                    "OpenCode non-conversation message records were intentionally excluded".into(),
                );
                return;
            }
            if let Some(text) = first_string(part, &["text", "content"]) {
                push_event(
                    events,
                    AgentKind::OpenCode,
                    session,
                    id,
                    0,
                    WorkstreamEventKind::Message,
                    Some(role),
                    text,
                    None,
                    json!({}),
                );
            }
        }
        "tool" => {
            let name = first_string(part, &["tool", "name"]).unwrap_or("tool");
            let state = part.get("state").unwrap_or(&Value::Null);
            let input = state.get("input").map(compact_json).unwrap_or_default();
            push_event(
                events,
                AgentKind::OpenCode,
                session,
                id,
                0,
                WorkstreamEventKind::ToolCall,
                Some("assistant"),
                &format!("{name}: {input}"),
                None,
                json!({"tool": name}),
            );
            if let Some(output) = state
                .get("output")
                .map(value_text)
                .filter(|value| !value.is_empty())
            {
                push_event(
                    events,
                    AgentKind::OpenCode,
                    session,
                    id,
                    1,
                    WorkstreamEventKind::ToolResult,
                    Some("tool"),
                    &output,
                    None,
                    json!({"status": state.get("status").and_then(Value::as_str)}),
                );
            }
        }
        "compaction" => {
            let body = first_string(part, &["summary", "text", "content"]).unwrap_or("");
            push_event(
                events,
                AgentKind::OpenCode,
                session,
                id,
                0,
                WorkstreamEventKind::Compaction,
                Some("assistant"),
                body,
                None,
                json!({}),
            );
        }
        "reasoning" => losses.push("OpenCode hidden reasoning was intentionally excluded".into()),
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn push_event(
    events: &mut Vec<NewWorkstreamEvent>,
    agent: AgentKind,
    session: &str,
    record_id: &str,
    block: usize,
    kind: WorkstreamEventKind,
    role: Option<&str>,
    content: &str,
    occurred_at: Option<String>,
    metadata: Value,
) {
    if content.trim().is_empty() {
        return;
    }
    let content = truncate_utf8(content, MAX_EVENT_BYTES);
    let seed = format!(
        "{}\0{session}\0{record_id}\0{block}\0{}\0{content}",
        agent.as_str(),
        kind.as_str()
    );
    events.push(NewWorkstreamEvent {
        event_id: format!("native:{:x}", Sha256::digest(seed.as_bytes())),
        agent,
        native_session_id: session.to_string(),
        source_record_id: Some(record_id.to_string()),
        kind,
        role: role.map(str::to_string),
        content: content.to_string(),
        occurred_at,
        metadata,
    });
}

fn locate_session_file(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    id: &str,
) -> Result<Option<PathBuf>> {
    let roots = session_roots(harness, home, session_dir);
    let root = &roots[0];
    if !valid_native_session_id(id) {
        return Ok(None);
    }
    if harness == ManagedHarness::Antigravity {
        // The conversation id is the file name, so no scan is ever needed.
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let exact = root.join(format!("{id}.db"));
        return Ok(session_path_matches(harness, &exact, id, cwd)?.then_some(exact));
    }
    if harness == ManagedHarness::Claude {
        let encoded = cwd.to_string_lossy().replace('/', "-");
        let exact = root.join(encoded).join(format!("{id}.jsonl"));
        if exact.is_file() {
            return Ok(Some(exact));
        }
    }
    if harness == ManagedHarness::Kimi {
        // Bucket names are one-way cwd hashes, so only the bucket level can
        // be enumerated — but the session id below it is a plain directory
        // name, giving an exact fast path per bucket.
        if let Ok(buckets) = fs::read_dir(root) {
            for bucket in buckets.flatten() {
                let candidate = bucket.path().join(id).join("agents/main/wire.jsonl");
                if session_path_matches(harness, &candidate, id, cwd)? {
                    return Ok(Some(candidate));
                }
            }
        }
    }
    if harness == ManagedHarness::CommandCode {
        // The project bucket is a one-way cwd slug. Enumerate only that level,
        // then probe the exact UUID filename and validate its self-describing
        // header before returning it.
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        if let Ok(buckets) = fs::read_dir(root) {
            for bucket in buckets.take(MAX_SCAN_FILES) {
                let bucket = bucket?;
                if !bucket.file_type()?.is_dir() {
                    continue;
                }
                let candidate = bucket.path().join(format!("{id}.jsonl"));
                if session_path_matches(harness, &candidate, id, cwd)? {
                    return Ok(Some(candidate));
                }
            }
        }
        return Ok(None);
    }
    if harness == ManagedHarness::Kiro {
        // The store is flat and shared by every checkout. Require both the
        // UUID file name and its sibling metadata to identify this checkout;
        // a server-linked id is untrusted input and must not select another
        // project's transcript.
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let exact = root.join(format!("{id}.jsonl"));
        if session_path_matches(harness, &exact, id, cwd)? {
            return Ok(Some(exact));
        }
        return Ok(None);
    }
    if harness == ManagedHarness::KiroV3 {
        let Some(uuid) = id.strip_prefix("sess_") else {
            return Ok(None);
        };
        if Uuid::parse_str(uuid).is_err() {
            return Ok(None);
        }
        for root in roots {
            let Ok(buckets) = fs::read_dir(&root) else {
                continue;
            };
            for bucket in buckets.take(MAX_SCAN_FILES) {
                let bucket = bucket?;
                if !bucket.file_type()?.is_dir() {
                    continue;
                }
                let exact = bucket.path().join(id).join("messages.jsonl");
                if session_path_matches(harness, &exact, id, cwd)? {
                    return Ok(Some(exact));
                }
            }
        }
        return Ok(None);
    }
    let mut files = collect_session_files(harness, home, session_dir)?;
    files.sort_by_key(|path| temporary_transcript(path));
    for path in files.into_iter().take(2_000) {
        if session_path_matches(harness, &path, id, cwd)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn session_path_matches(
    harness: ManagedHarness,
    path: &Path,
    id: &str,
    cwd: &Path,
) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    Ok(session_header_for_cwd(harness, path, cwd)?
        .is_some_and(|(found, recorded_cwd)| found == id && same_path(&recorded_cwd, cwd)))
}

fn transcript_file(harness: ManagedHarness, path: &Path) -> bool {
    if harness == ManagedHarness::Grok {
        // Only the conversation journal. `events.jsonl`, `updates.jsonl`, and
        // `rewind_points.jsonl` in the same session directory carry harness
        // internals and are excluded.
        return path.file_name().and_then(|name| name.to_str()) == Some("chat_history.jsonl");
    }
    if harness == ManagedHarness::Kimi {
        // Only the main agent's journal: `agents/main/wire.jsonl`. Subagent
        // wire journals and any other *.jsonl in the store are excluded.
        return path.file_name().and_then(|name| name.to_str()) == Some("wire.jsonl")
            && path
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                == Some("main");
    }
    if harness == ManagedHarness::Kiro {
        return path.extension().is_some_and(|ext| ext == "jsonl")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| Uuid::parse_str(stem).is_ok());
    }
    if harness == ManagedHarness::CommandCode {
        // Sidecars such as `<id>.checkpoints.jsonl` do not have a UUID file
        // stem and are excluded from discovery and import.
        return path.extension().is_some_and(|ext| ext == "jsonl")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| Uuid::parse_str(stem).is_ok());
    }
    if harness == ManagedHarness::KiroV3 {
        return path.file_name().and_then(|name| name.to_str()) == Some("messages.jsonl")
            && path
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("sess_"))
                .is_some_and(|uuid| Uuid::parse_str(uuid).is_ok());
    }
    if harness == ManagedHarness::Antigravity {
        // One SQLite database per conversation, named by its id.
        return path.extension().is_some_and(|ext| ext == "db");
    }
    path.extension().is_some_and(|ext| ext == "jsonl")
        || matches!(harness, ManagedHarness::Pi | ManagedHarness::Omp) && temporary_transcript(path)
}

fn temporary_transcript(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "tmp")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(".jsonl."))
}

fn session_header(harness: ManagedHarness, path: &Path) -> Result<Option<(String, PathBuf)>> {
    if harness == ManagedHarness::Kimi {
        return kimi_session_header(path);
    }
    if harness == ManagedHarness::CommandCode {
        return command_code_session_header(path);
    }
    if harness == ManagedHarness::Kiro {
        return kiro_session_header(path);
    }
    if harness == ManagedHarness::Grok {
        return grok_session_header(path);
    }
    if harness == ManagedHarness::Antigravity {
        return antigravity_session_header(path);
    }
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (id, cwd) = match harness {
            ManagedHarness::Claude => (
                value.get("sessionId").and_then(Value::as_str),
                value.get("cwd").and_then(Value::as_str),
            ),
            ManagedHarness::Codex => {
                let payload = value.get("payload").unwrap_or(&Value::Null);
                (
                    payload.get("id").and_then(Value::as_str),
                    payload.get("cwd").and_then(Value::as_str),
                )
            }
            ManagedHarness::Pi | ManagedHarness::Omp => (
                value.get("id").and_then(Value::as_str),
                value.get("cwd").and_then(Value::as_str),
            ),
            ManagedHarness::OpenCode
            | ManagedHarness::OpenCode2
            | ManagedHarness::Crush
            | ManagedHarness::Kimi
            | ManagedHarness::CommandCode
            | ManagedHarness::Kiro
            | ManagedHarness::KiroV3
            | ManagedHarness::Grok
            | ManagedHarness::Antigravity => (None, None),
        };
        if let (Some(id), Some(cwd)) = (id, cwd) {
            return Ok(Some((id.to_string(), PathBuf::from(cwd))));
        }
    }
    Ok(None)
}

/// Command Code v3 transcripts are self-describing on their first line. Fail
/// closed on an unknown version, malformed UUID/timestamp/path, or a header id
/// that disagrees with the transcript filename.
fn command_code_session_header(path: &Path) -> Result<Option<(String, PathBuf)>> {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return Ok(None);
    };
    if Uuid::parse_str(stem).is_err() {
        return Ok(None);
    }
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = String::new();
    let read =
        std::io::Read::take(&mut reader, (MAX_EVENT_BYTES + 1) as u64).read_line(&mut line)?;
    if read == 0 || read > MAX_EVENT_BYTES {
        return Ok(None);
    }
    let Ok(header) = serde_json::from_str::<Value>(&line) else {
        return Ok(None);
    };
    if header.get("type").and_then(Value::as_str) != Some("session")
        || header.get("version").and_then(Value::as_u64) != Some(3)
    {
        return Ok(None);
    }
    let (Some(id), Some(timestamp), Some(cwd)) = (
        header.get("id").and_then(Value::as_str),
        header.get("timestamp").and_then(Value::as_str),
        header.get("cwd").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    let cwd = PathBuf::from(cwd);
    if id != stem
        || Uuid::parse_str(id).is_err()
        || timestamp.parse::<jiff::Timestamp>().is_err()
        || !cwd.is_absolute()
    {
        return Ok(None);
    }
    Ok(Some((id.to_string(), cwd)))
}

fn session_header_for_cwd(
    harness: ManagedHarness,
    path: &Path,
    cwd: &Path,
) -> Result<Option<(String, PathBuf)>> {
    if harness == ManagedHarness::KiroV3 {
        kiro_v3_session_header(path, cwd)
    } else {
        session_header(harness, path)
    }
}

/// Kimi sessions are self-describing in `<session-dir>/state.json` — the wire
/// journal itself carries no session id or cwd, and the bucket directory name
/// is a one-way hash of the cwd, so neither can be inferred from the layout.
/// Kimi 0.29 used `workDir`; 0.34 uses `cwd`. Conflicting aliases and an
/// optional id that disagrees with the directory fail closed. The journal path
/// is `<session-dir>/agents/main/wire.jsonl`, making the session directory the
/// third ancestor. Missing/invalid state means the session is unusable for
/// checkout matching, not an error.
fn kimi_session_header(path: &Path) -> Result<Option<(String, PathBuf)>> {
    let Some(session_dir) = path.ancestors().nth(3) else {
        return Ok(None);
    };
    let Ok(raw) = fs::read_to_string(session_dir.join("state.json")) else {
        return Ok(None);
    };
    let Ok(state) = serde_json::from_str::<Value>(&raw) else {
        return Ok(None);
    };
    let Some(id) = session_dir.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    match state.get("id") {
        Some(Value::String(recorded)) if recorded == id => {}
        Some(_) => return Ok(None),
        None => {}
    }
    let legacy = match state.get("workDir") {
        Some(Value::String(cwd)) => Some(cwd.as_str()),
        Some(_) => return Ok(None),
        None => None,
    };
    let current = match state.get("cwd") {
        Some(Value::String(cwd)) => Some(cwd.as_str()),
        Some(_) => return Ok(None),
        None => None,
    };
    let cwd = match (legacy, current) {
        (Some(legacy), Some(current))
            if legacy == current || same_path(Path::new(legacy), Path::new(current)) =>
        {
            current
        }
        (Some(_), Some(_)) | (None, None) => return Ok(None),
        (Some(cwd), None) | (None, Some(cwd)) => cwd,
    };
    Ok(Some((id.to_string(), PathBuf::from(cwd))))
}

/// Kiro v2 sessions pair a flat `<uuid>.jsonl` event stream with
/// `<uuid>.json` metadata. Missing, malformed, or mismatched metadata makes
/// the transcript ineligible for discovery or resume.
fn kiro_session_header(path: &Path) -> Result<Option<(String, PathBuf)>> {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return Ok(None);
    };
    if Uuid::parse_str(stem).is_err() {
        return Ok(None);
    }
    let Ok(raw) = fs::read_to_string(path.with_extension("json")) else {
        return Ok(None);
    };
    let Ok(metadata) = serde_json::from_str::<Value>(&raw) else {
        return Ok(None);
    };
    let Some(id) = metadata.get("session_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if id != stem {
        return Ok(None);
    }
    let Some(cwd) = metadata.get("cwd").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(Some((id.to_string(), PathBuf::from(cwd))))
}

/// Kiro v3 stores one self-describing directory per `sess_<uuid>` session.
/// Only the observed schema/data-model pair is accepted, and at least one
/// recorded workspace must resolve to the current checkout.
fn kiro_v3_session_header(path: &Path, cwd: &Path) -> Result<Option<(String, PathBuf)>> {
    if path.file_name().and_then(|name| name.to_str()) != Some("messages.jsonl") {
        return Ok(None);
    }
    let Some(session_dir) = path.parent() else {
        return Ok(None);
    };
    let Some(id) = session_dir.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let Some(uuid) = id.strip_prefix("sess_") else {
        return Ok(None);
    };
    if Uuid::parse_str(uuid).is_err() {
        return Ok(None);
    }
    let Ok(raw) = fs::read_to_string(session_dir.join("session.json")) else {
        return Ok(None);
    };
    let Ok(metadata) = serde_json::from_str::<Value>(&raw) else {
        return Ok(None);
    };
    if metadata.get("schemaVersion").and_then(Value::as_str) != Some("1.0.0")
        || metadata.get("dataModelVersion").and_then(Value::as_u64) != Some(1)
        || metadata.get("id").and_then(Value::as_str) != Some(id)
    {
        return Ok(None);
    }
    let Some(workspaces) = metadata.get("workspacePaths").and_then(Value::as_array) else {
        return Ok(None);
    };
    let matching = workspaces
        .iter()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .find(|workspace| same_path(workspace, cwd));
    Ok(matching.map(|workspace| (id.to_string(), workspace)))
}

/// Grok sessions are self-describing in `<session-dir>/summary.json`
/// (`info.id` + `info.cwd`). The chat-history journal carries no session id or
/// cwd, and the bucket directory name is a URL-encoded cwd that is never
/// parsed — recorded metadata is the only accepted checkout identifier.
/// Missing/invalid metadata means the session is unusable for checkout
/// matching, not an error.
fn grok_session_header(path: &Path) -> Result<Option<(String, PathBuf)>> {
    let Some(session_dir) = path.parent() else {
        return Ok(None);
    };
    let Ok(raw) = fs::read_to_string(session_dir.join("summary.json")) else {
        return Ok(None);
    };
    let Ok(summary) = serde_json::from_str::<Value>(&raw) else {
        return Ok(None);
    };
    let info = summary.get("info").unwrap_or(&Value::Null);
    let (Some(id), Some(cwd)) = (
        info.get("id").and_then(Value::as_str),
        info.get("cwd").and_then(Value::as_str),
    ) else {
        return Ok(None);
    };
    Ok(Some((id.to_string(), PathBuf::from(cwd))))
}

/// Antigravity keeps one SQLite database per conversation, named
/// `<conversation-id>.db`, so the id comes from the file name. The workspace it
/// was opened on lives in `trajectory_metadata_blob`, a protobuf message whose
/// first field holds a nested message whose first field is the workspace
/// `file://` URI. Only those two fields are read: every other field is a step
/// payload with an undocumented, unversioned schema.
///
/// A database that does not carry both is unusable for checkout matching, not
/// an error — the conversations directory may hold databases from an `agy`
/// version whose metadata is shaped differently.
fn antigravity_session_header(path: &Path) -> Result<Option<(String, PathBuf)>> {
    let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return Ok(None);
    };
    if !valid_native_session_id(id) || Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Ok(None);
    };
    let blob = connection.query_row(
        "SELECT data FROM trajectory_metadata_blob WHERE id = 'main' LIMIT 1",
        [],
        |row| row.get::<_, Vec<u8>>(0),
    );
    let Ok(blob) = blob else {
        return Ok(None);
    };
    let Some(workspace) = protobuf_field(&blob, 1)
        .and_then(|nested| protobuf_field(nested, 1))
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(path_from_file_uri)
    else {
        return Ok(None);
    };
    Ok(Some((id.to_string(), workspace)))
}

/// Bytes of the first length-delimited field numbered `field`, or `None` when
/// the message does not contain one. Non-matching fields are skipped by wire
/// type; an unknown wire type ends the walk rather than guessing a length.
fn protobuf_field(message: &[u8], field: u64) -> Option<&[u8]> {
    let mut cursor = 0usize;
    while cursor < message.len() {
        let (key, used) = protobuf_varint(&message[cursor..])?;
        cursor += used;
        match key & 0b111 {
            0 => cursor += protobuf_varint(&message[cursor..])?.1,
            1 => cursor = cursor.checked_add(8)?,
            2 => {
                let (length, used) = protobuf_varint(&message[cursor..])?;
                cursor += used;
                let end = cursor.checked_add(usize::try_from(length).ok()?)?;
                let bytes = message.get(cursor..end)?;
                if key >> 3 == field {
                    return Some(bytes);
                }
                cursor = end;
            }
            5 => cursor = cursor.checked_add(4)?,
            _ => return None,
        }
    }
    None
}

/// Decode one base-128 varint, returning its value and the bytes consumed.
fn protobuf_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (index, byte) in bytes.iter().take(10).enumerate() {
        if index == 9 && *byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

/// Local path behind a `file://` URI, or `None` for any other scheme.
///
/// Windows records `file:///C:/dir`, whose leading slash is part of the URI
/// grammar and not of the path; POSIX records `file:///home/dir`, where it is.
/// The drive-letter shape tells them apart without a `#[cfg]` split, so a
/// database copied between platforms still parses.
fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let rest = percent_decode(uri.strip_prefix("file://")?);
    let bytes = rest.as_bytes();
    let drive_prefixed = bytes.first() == Some(&b'/')
        && bytes.get(2) == Some(&b':')
        && bytes.get(1).is_some_and(u8::is_ascii_alphabetic);
    let path = if drive_prefixed { &rest[1..] } else { &rest };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Decode `%XX` escapes; invalid escapes are kept verbatim so a path that was
/// never encoded survives unchanged.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = bytes.get(index + 1..index + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn discover_opencode(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    started_at: SystemTime,
) -> Result<Option<String>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let since = started_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut statement = connection.prepare(
        "SELECT id FROM session WHERE directory = ?1 AND time_updated >= ?2 ORDER BY time_updated DESC LIMIT 1",
    )?;
    match statement.query_row(params![cwd.to_string_lossy(), since], |row| row.get(0)) {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn list_opencode_sessions(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    limit: usize,
) -> Result<Vec<NativeSessionCandidate>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = connection.prepare(
        "SELECT id, time_updated FROM session \
         WHERE directory = ?1 ORDER BY time_updated DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![cwd.to_string_lossy(), limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (native_session_id, updated_millis) = row?;
        let Ok(updated_millis) = u64::try_from(updated_millis) else {
            continue;
        };
        if !valid_native_session_id(&native_session_id) {
            continue;
        }
        let Some(updated_at) = UNIX_EPOCH.checked_add(Duration::from_millis(updated_millis)) else {
            continue;
        };
        sessions.push(NativeSessionCandidate {
            native_session_id,
            updated_at,
        });
    }
    Ok(sessions)
}

fn discover_crush(
    cwd: &Path,
    session_dir: Option<&Path>,
    started_at: SystemTime,
) -> Result<Option<String>> {
    Ok(list_crush_sessions(cwd, session_dir, 1)?
        .into_iter()
        .find(|candidate| candidate.updated_at + Duration::from_secs(2) >= started_at)
        .map(|candidate| candidate.native_session_id))
}

fn list_crush_sessions(
    cwd: &Path,
    session_dir: Option<&Path>,
    limit: usize,
) -> Result<Vec<NativeSessionCandidate>> {
    let db = crush_db(cwd, session_dir);
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = connection
        .prepare("SELECT id, updated_at FROM sessions ORDER BY updated_at DESC LIMIT ?1")?;
    let rows = statement.query_map([limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (native_session_id, updated) = row?;
        if !valid_native_session_id(&native_session_id) {
            continue;
        }
        let Some(updated_at) = native_timestamp(updated) else {
            continue;
        };
        sessions.push(NativeSessionCandidate {
            native_session_id,
            updated_at,
        });
    }
    Ok(sessions)
}

fn crush_updated(cwd: &Path, session_dir: Option<&Path>, session: &str) -> Result<Option<i64>> {
    let db = crush_db(cwd, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    match connection.query_row(
        "SELECT updated_at FROM sessions WHERE id = ?1",
        [session],
        |row| row.get(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn crush_db(cwd: &Path, session_dir: Option<&Path>) -> PathBuf {
    session_dir.unwrap_or(&cwd.join(".crush")).join("crush.db")
}

fn native_timestamp(value: i64) -> Option<SystemTime> {
    let value = u64::try_from(value).ok()?;
    if value < 100_000_000_000 {
        UNIX_EPOCH.checked_add(Duration::from_secs(value))
    } else {
        UNIX_EPOCH.checked_add(Duration::from_millis(value))
    }
}

fn opencode_updated(home: &Path, session_dir: Option<&Path>, session: &str) -> Result<Option<i64>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    match connection.query_row(
        "SELECT time_updated FROM session WHERE id = ?1",
        [session],
        |row| row.get(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn opencode_db(home: &Path, session_dir: Option<&Path>) -> PathBuf {
    session_dir.map_or_else(
        || home.join(".local/share/opencode/opencode.db"),
        |dir| dir.join("opencode.db"),
    )
}

/// OpenCode 2.0 beta transcript adapter.
///
/// The beta keeps v1's database file but replaced its tables: `session_v2`
/// carries sessions, `session_message` carries typed messages. Shapes below
/// were verified against beta-18999: user `{text, files}`, assistant
/// `content[]` with `text` / `tool` / `reasoning` parts. Reasoning content
/// is encrypted and excluded like v1's hidden reasoning; anything else
/// unrecognized becomes a bounded loss, never a guess — the beta schema is
/// still changing.
fn export_opencode2(
    home: &Path,
    session_dir: Option<&Path>,
    session: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    let db = opencode_db(home, session_dir);
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| {
        format!(
            "opening OpenCode session database {} read-only",
            db.display()
        )
    })?;
    let cursor = source_cursor
        .and_then(|raw| serde_json::from_str::<SqlCursor>(raw).ok())
        .unwrap_or_default();
    let mut statement = connection.prepare(
        "SELECT id, time_updated, type, data FROM session_message \
         WHERE session_id = ?1 AND (time_updated > ?2 OR (time_updated = ?2 AND id > ?3)) \
         ORDER BY time_updated, id",
    )?;
    let rows = statement.query_map(params![session, cursor.updated, cursor.id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut losses = Vec::new();
    let mut next_cursor = cursor;
    for row in rows {
        let (id, updated, message_type, data_raw) = row?;
        next_cursor = SqlCursor {
            updated,
            id: id.clone(),
        };
        let Ok(data) = serde_json::from_str::<Value>(&data_raw) else {
            losses.push(format!("malformed OpenCode 2 message {id}"));
            continue;
        };
        parse_opencode2(&message_type, &data, session, &id, &mut events, &mut losses);
    }
    Ok(ExportedTranscript {
        native_session_id: session.to_string(),
        source_cursor: Some(serde_json::to_string(&next_cursor)?),
        events,
        losses: deduplicate_losses(losses),
    })
}

fn parse_opencode2(
    message_type: &str,
    data: &Value,
    session: &str,
    id: &str,
    events: &mut Vec<NewWorkstreamEvent>,
    losses: &mut Vec<String>,
) {
    match message_type {
        "user" => {
            if let Some(text) = data.get("text").and_then(Value::as_str) {
                push_event(
                    events,
                    AgentKind::OpenCode,
                    session,
                    id,
                    0,
                    WorkstreamEventKind::Message,
                    Some("user"),
                    text,
                    None,
                    json!({}),
                );
            }
        }
        "assistant" => {
            let Some(parts) = data.get("content").and_then(Value::as_array) else {
                losses.push(format!(
                    "OpenCode 2 assistant message {id} has no content parts"
                ));
                return;
            };
            for (index, part) in parts.iter().enumerate() {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
                // One message holds many parts; the block namespaces each
                // part's events deterministically (call before its result).
                let block = index * 2;
                match kind {
                    "text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            push_event(
                                events,
                                AgentKind::OpenCode,
                                session,
                                id,
                                block,
                                WorkstreamEventKind::Message,
                                Some("assistant"),
                                text,
                                None,
                                json!({}),
                            );
                        }
                    }
                    "tool" => {
                        let name = part.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let state = part.get("state").unwrap_or(&Value::Null);
                        let input = state.get("input").map(compact_json).unwrap_or_default();
                        push_event(
                            events,
                            AgentKind::OpenCode,
                            session,
                            id,
                            block,
                            WorkstreamEventKind::ToolCall,
                            Some("assistant"),
                            &format!("{name}: {input}"),
                            None,
                            json!({"tool": name}),
                        );
                        let status = state
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("completed");
                        if status == "completed" {
                            let output = state
                                .get("content")
                                .and_then(Value::as_array)
                                .map(|contents| {
                                    contents
                                        .iter()
                                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                })
                                .unwrap_or_default();
                            if !output.trim().is_empty() {
                                push_event(
                                    events,
                                    AgentKind::OpenCode,
                                    session,
                                    id,
                                    block + 1,
                                    WorkstreamEventKind::ToolResult,
                                    Some("tool"),
                                    &output,
                                    None,
                                    json!({"status": status}),
                                );
                            }
                        } else {
                            losses
                                .push(format!("OpenCode 2 tool {name} ended with status {status}"));
                        }
                    }
                    "reasoning" => {
                        losses.push("OpenCode 2 hidden reasoning was intentionally excluded".into())
                    }
                    other => losses.push(format!(
                        "OpenCode 2 {other} part was intentionally excluded"
                    )),
                }
            }
        }
        // System catalog notices and similar non-conversation records.
        _ => {}
    }
}

fn discover_opencode2(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    started_at: SystemTime,
) -> Result<Option<String>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let since = started_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut statement = connection.prepare(
        "SELECT id FROM session_v2 WHERE directory = ?1 AND time_updated >= ?2 \
         ORDER BY time_updated DESC LIMIT 1",
    )?;
    match statement.query_row(params![cwd.to_string_lossy(), since], |row| row.get(0)) {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn list_opencode2_sessions(
    home: &Path,
    session_dir: Option<&Path>,
    cwd: &Path,
    limit: usize,
) -> Result<Vec<NativeSessionCandidate>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(Vec::new());
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = connection.prepare(
        "SELECT id, time_updated FROM session_v2 \
         WHERE directory = ?1 ORDER BY time_updated DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![cwd.to_string_lossy(), limit as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (native_session_id, updated_millis) = row?;
        let Ok(updated_millis) = u64::try_from(updated_millis) else {
            continue;
        };
        if !valid_native_session_id(&native_session_id) {
            continue;
        }
        let Some(updated_at) = UNIX_EPOCH.checked_add(Duration::from_millis(updated_millis)) else {
            continue;
        };
        sessions.push(NativeSessionCandidate {
            native_session_id,
            updated_at,
        });
    }
    Ok(sessions)
}

fn opencode2_updated(
    home: &Path,
    session_dir: Option<&Path>,
    session: &str,
) -> Result<Option<i64>> {
    let db = opencode_db(home, session_dir);
    if !db.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    match connection.query_row(
        "SELECT time_updated FROM session_v2 WHERE id = ?1",
        [session],
        |row| row.get(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn session_root(harness: ManagedHarness, home: &Path, override_dir: Option<&Path>) -> PathBuf {
    if let Some(override_dir) = override_dir {
        return override_dir.to_path_buf();
    }
    match harness {
        ManagedHarness::Claude => home.join(".claude/projects"),
        ManagedHarness::Codex => home.join(".codex/sessions"),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => home.join(".local/share/opencode"),
        ManagedHarness::Pi => home.join(".pi/agent/sessions"),
        ManagedHarness::Crush => home.join(".crush"),
        ManagedHarness::Omp => home.join(".omp/agent/sessions"),
        ManagedHarness::Kimi => home.join(".kimi-code/sessions"),
        ManagedHarness::CommandCode => home.join(".commandcode/projects"),
        ManagedHarness::Kiro => home.join(".kiro/sessions/cli"),
        ManagedHarness::KiroV3 => home.join(".kiro/sessions"),
        ManagedHarness::Grok => home.join(".grok/sessions"),
        ManagedHarness::Antigravity => home.join(".gemini/antigravity-cli/conversations"),
    }
}

/// Kiro CLI 2.16.2 honored `KIRO_HOME` for its v2 store but wrote v3 sessions
/// to the default home during acceptance. Scan the configured root first and
/// the default root as a compatibility fallback; every result still passes
/// strict metadata and checkout validation.
fn session_roots(
    harness: ManagedHarness,
    home: &Path,
    override_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let primary = session_root(harness, home, override_dir);
    let mut roots = vec![primary.clone()];
    if harness == ManagedHarness::KiroV3 {
        let fallback = home.join(".kiro/sessions");
        if fallback != primary {
            roots.push(fallback);
        }
    }
    roots
}

fn collect_session_files(
    harness: ManagedHarness,
    home: &Path,
    override_dir: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for root in session_roots(harness, home, override_dir) {
        for path in collect_files(&root, |path| transcript_file(harness, path))? {
            if seen.insert(path.clone()) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn collect_files(root: &Path, predicate: impl Fn(&Path) -> bool + Copy) -> Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in
            fs::read_dir(&directory).with_context(|| format!("reading {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path)
            } else if file_type.is_file() && predicate(&path) {
                files.push(path);
                if files.len() >= MAX_SCAN_FILES {
                    return Ok(files);
                }
            }
        }
    }
    Ok(files)
}

fn source_id(value: &Value) -> Option<String> {
    for key in ["uuid", "id", "messageId", "call_id", "callId"] {
        if let Some(id) = value.get(key).and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    value.get("payload").and_then(|payload| {
        ["id", "call_id", "callId"]
            .into_iter()
            .find_map(|key| payload.get(key).and_then(Value::as_str).map(str::to_string))
    })
}

fn timestamp(value: &Value) -> Option<String> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn first_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(value_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => first_string(value, &["text", "content", "output"])
            .map_or_else(|| compact_json(value), str::to_string),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn truncate_utf8(value: &str, max: usize) -> &str {
    if value.len() <= max {
        return value;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1
    }
    &value[..end]
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn valid_native_session_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_NATIVE_SESSION_ID_BYTES
        && !value.starts_with('-')
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn deduplicate_losses(losses: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    for loss in losses {
        if !output.contains(&loss) {
            output.push(loss)
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_path_comparison_fails_closed_when_canonicalization_fails() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("existing");
        fs::create_dir(&existing).unwrap();

        assert!(same_path(&existing, &existing));
        assert!(!same_path(&existing, &temp.path().join("missing")));
        assert!(!same_path(
            &temp.path().join("missing-left"),
            &temp.path().join("missing-right")
        ));
    }

    #[tokio::test]
    async fn jsonl_candidate_discovery_covers_every_file_adapter_and_checkout_scope() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();

        for (harness, header) in [
            (
                ManagedHarness::Claude,
                json!({"sessionId":"claude-id","cwd":cwd}),
            ),
            (
                ManagedHarness::Codex,
                json!({"type":"session_meta","payload":{"id":"codex-id","cwd":cwd}}),
            ),
            (
                ManagedHarness::Pi,
                json!({"type":"session","id":"pi-id","cwd":cwd}),
            ),
            (
                ManagedHarness::Omp,
                json!({"type":"session","id":"omp-id","cwd":cwd}),
            ),
        ] {
            let root = temp.path().join(harness.as_str());
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("matching.jsonl"), format!("{header}\n")).unwrap();
            fs::write(
                root.join("other.jsonl"),
                match harness {
                    ManagedHarness::Claude => {
                        format!("{}\n", json!({"sessionId":"other-id","cwd":other}))
                    }
                    ManagedHarness::Codex => format!(
                        "{}\n",
                        json!({"type":"session_meta","payload":{"id":"other-id","cwd":other}})
                    ),
                    ManagedHarness::Pi | ManagedHarness::Omp => format!(
                        "{}\n",
                        json!({"type":"session","id":"other-id","cwd":other})
                    ),
                    // Kimi's header lives in state.json and Grok's in
                    // summary.json, not the journal; covered by their own
                    // discovery tests.
                    ManagedHarness::OpenCode
                    | ManagedHarness::OpenCode2
                    | ManagedHarness::Crush
                    | ManagedHarness::Kimi
                    | ManagedHarness::CommandCode
                    | ManagedHarness::Kiro
                    | ManagedHarness::KiroV3
                    | ManagedHarness::Grok
                    | ManagedHarness::Antigravity => {
                        unreachable!()
                    }
                },
            )
            .unwrap();

            let sessions = list_native_sessions(harness, temp.path(), &cwd, Some(&root), 8)
                .await
                .unwrap();
            let expected_id = format!("{}-id", harness.as_str());
            assert_eq!(sessions.len(), 1, "{} candidates", harness.as_str());
            assert_eq!(sessions[0].native_session_id, expected_id);
            assert!(
                native_session_exists(harness, temp.path(), &cwd, Some(&root), &expected_id)
                    .unwrap(),
                "{} existing session",
                harness.as_str()
            );
            assert!(
                !native_session_exists(harness, temp.path(), &cwd, Some(&root), "missing").unwrap(),
                "{} missing session",
                harness.as_str()
            );
        }
    }

    #[tokio::test]
    async fn command_code_v3_discovery_requires_exact_uuid_header_and_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        let root = temp.path().join("command-code-projects");
        let bucket = root.join("opaque-project-slug");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(&bucket).unwrap();

        let matching = "7c1d5698-204a-4c0f-ae9c-43db7fc4e41d";
        let wrong_checkout = "2cce5126-f57d-4ddd-8f66-e5bb409f60db";
        let future_schema = "bb40bd9b-d60a-4a87-91c6-57d12c3d3002";
        fs::write(
            bucket.join(format!("{matching}.jsonl")),
            format!(
                "{}\n",
                json!({"type":"session","version":3,"id":matching,"timestamp":"2026-08-07T17:00:00Z","cwd":cwd})
            ),
        )
        .unwrap();
        fs::write(
            bucket.join(format!("{wrong_checkout}.jsonl")),
            format!(
                "{}\n",
                json!({"type":"session","version":3,"id":wrong_checkout,"timestamp":"2026-08-07T17:00:00Z","cwd":other})
            ),
        )
        .unwrap();
        fs::write(
            bucket.join(format!("{future_schema}.jsonl")),
            format!(
                "{}\n",
                json!({"type":"session","version":4,"id":future_schema,"timestamp":"2026-08-07T17:00:00Z","cwd":cwd})
            ),
        )
        .unwrap();
        fs::write(
            bucket.join(format!("{matching}.checkpoints.jsonl")),
            "{\"prompt\":\"private checkpoint\"}\n",
        )
        .unwrap();

        let sessions = list_native_sessions(
            ManagedHarness::CommandCode,
            temp.path(),
            &cwd,
            Some(&root),
            8,
        )
        .await
        .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_session_id, matching);
        assert!(
            native_session_exists(
                ManagedHarness::CommandCode,
                temp.path(),
                &cwd,
                Some(&root),
                matching,
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::CommandCode,
                temp.path(),
                &cwd,
                Some(&root),
                wrong_checkout,
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::CommandCode,
                temp.path(),
                &cwd,
                Some(&root),
                future_schema,
            )
            .unwrap()
        );
    }

    #[test]
    fn command_code_v3_exports_only_visible_allowlisted_blocks_incrementally() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let session = "7c1d5698-204a-4c0f-ae9c-43db7fc4e41d";
        let path = temp.path().join(format!("{session}.jsonl"));
        let records = [
            json!({"type":"session","version":3,"id":session,"timestamp":"2026-08-07T17:00:00Z","cwd":cwd}),
            json!({"type":"message","id":"u1","parentId":null,"timestamp":"2026-08-07T17:00:01Z","message":{"role":"user","content":[{"type":"text","text":"visible user"}],"meta":{"source":"user"}}}),
            json!({"type":"message","id":"a1","parentId":"u1","timestamp":"2026-08-07T17:00:02Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private reasoning","signature":"private signature"},{"type":"text","text":"visible assistant"},{"type":"tool_use","id":"tool-1","name":"read_directory","input":{"path":"src"}}],"meta":{"source":"model"}},"model":"private model","usage":{"costUsd":99}}),
            json!({"type":"message","id":"t1","parentId":"a1","timestamp":"2026-08-07T17:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":true,"content":[{"type":"text","text":"visible result"},{"type":"image","source":{"type":"base64","data":"private image"}}]}],"meta":{"source":"user"}}}),
            json!({"type":"compaction","id":"c1","parentId":"t1","timestamp":"2026-08-07T17:00:04Z","summary":"visible compacted context","firstKeptEntryId":"u1","tokensBefore":1000,"details":"private details"}),
            json!({"type":"branch_summary","id":"b1","parentId":"u1","timestamp":"2026-08-07T17:00:05Z","fromId":"c1","summary":"visible abandoned-branch summary","details":"private details"}),
            json!({"type":"message","id":"injected","parentId":"b1","timestamp":"2026-08-07T17:00:06Z","message":{"role":"user","content":[{"type":"text","text":"harness context"}],"meta":{"source":"hook"}}}),
        ];
        fs::write(
            &path,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();

        let first = export_jsonl(ManagedHarness::CommandCode, &path, session, None).unwrap();
        assert_eq!(first.events.len(), 6);
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| (event.kind, event.role.as_deref(), event.content.as_str()))
                .collect::<Vec<_>>(),
            [
                (WorkstreamEventKind::Message, Some("user"), "visible user"),
                (
                    WorkstreamEventKind::Message,
                    Some("assistant"),
                    "visible assistant",
                ),
                (
                    WorkstreamEventKind::ToolCall,
                    Some("assistant"),
                    "read_directory: {\"path\":\"src\"}",
                ),
                (
                    WorkstreamEventKind::ToolResult,
                    Some("tool"),
                    "visible result",
                ),
                (
                    WorkstreamEventKind::Compaction,
                    Some("assistant"),
                    "visible compacted context",
                ),
                (
                    WorkstreamEventKind::Message,
                    Some("assistant"),
                    "visible abandoned-branch summary",
                ),
            ]
        );
        assert_eq!(first.events[1].metadata["parent_id"], "u1");
        assert_eq!(first.events[2].metadata["tool_use_id"], "tool-1");
        assert_eq!(first.events[3].metadata["parent_id"], "a1");
        assert_eq!(first.events[3].metadata["is_error"], true);
        assert_eq!(first.events[4].metadata["summary_type"], "compaction");
        assert_eq!(first.events[5].metadata["summary_type"], "branch-summary");
        assert!(
            first
                .losses
                .iter()
                .any(|loss| loss.contains("hidden reasoning"))
        );
        assert!(
            first
                .losses
                .iter()
                .any(|loss| loss.contains("image tool results"))
        );
        assert!(
            first
                .losses
                .iter()
                .any(|loss| loss.contains("non-user/model"))
        );
        assert!(first.events.iter().all(|event| {
            !event.content.contains("private") && !event.content.contains("harness context")
        }));

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write as _;
        writeln!(
            file,
            "{}",
            json!({"type":"message","id":"a2","parentId":"u1","timestamp":"2026-08-07T17:00:07Z","message":{"role":"assistant","content":[{"type":"text","text":"visible alternate branch"}],"meta":{"source":"model"}}})
        )
        .unwrap();
        let second = export_jsonl(
            ManagedHarness::CommandCode,
            &path,
            session,
            first.source_cursor.as_deref(),
        )
        .unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].content, "visible alternate branch");
        assert_eq!(second.events[0].metadata["parent_id"], "u1");
    }

    #[tokio::test]
    async fn opencode_candidate_discovery_is_newest_first_and_checkout_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        let db_root = temp.path().join("opencode");
        fs::create_dir_all(&db_root).unwrap();
        let connection = Connection::open(db_root.join("opencode.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session( \
                     id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_updated INTEGER NOT NULL);",
            )
            .unwrap();
        for (id, directory, updated) in [
            ("older", &cwd, 100_i64),
            ("newer", &cwd, 200_i64),
            ("unrelated", &other, 300_i64),
        ] {
            connection
                .execute(
                    "INSERT INTO session VALUES (?1, ?2, ?3)",
                    params![id, directory.to_string_lossy(), updated],
                )
                .unwrap();
        }

        let sessions = list_native_sessions(
            ManagedHarness::OpenCode,
            temp.path(),
            &cwd,
            Some(&db_root),
            8,
        )
        .await
        .unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|candidate| candidate.native_session_id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert!(
            native_session_exists(
                ManagedHarness::OpenCode,
                temp.path(),
                &cwd,
                Some(&db_root),
                "newer"
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::OpenCode,
                temp.path(),
                &cwd,
                Some(&db_root),
                "missing"
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn crush_candidate_discovery_and_incremental_export_are_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let data = cwd.join(".crush");
        fs::create_dir_all(&data).unwrap();
        let db = data.join("crush.db");
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions(id TEXT PRIMARY KEY, updated_at INTEGER NOT NULL);\n\
                 CREATE TABLE messages(\
                    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL,\
                    parts TEXT NOT NULL, updated_at INTEGER NOT NULL,\
                    is_summary_message INTEGER NOT NULL DEFAULT 0);",
            )
            .unwrap();
        for (id, updated) in [("older", 1_700_000_000_i64), ("newer", 1_800_000_000)] {
            connection
                .execute("INSERT INTO sessions VALUES (?1, ?2)", params![id, updated])
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO messages VALUES ('m1', 'newer', 'assistant', ?1, 1, 0)",
                [json!([
                    {"type":"reasoning","data":{"text":"private"}},
                    {"type":"tool_call","data":{"name":"bash","input":{"cmd":"date"},"finished":false}},
                    {"type":"text","data":{"text":"visible"}}
                ])
                .to_string()],
            )
            .unwrap();

        let candidates = list_native_sessions(ManagedHarness::Crush, temp.path(), &cwd, None, 8)
            .await
            .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.native_session_id.as_str())
                .collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert!(
            native_session_exists(ManagedHarness::Crush, temp.path(), &cwd, None, "newer").unwrap()
        );
        assert!(
            !native_session_exists(ManagedHarness::Crush, temp.path(), &cwd, None, "missing")
                .unwrap()
        );

        let first = export_crush(&cwd, None, "newer", None).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].content, "visible");
        assert!(first.losses.iter().any(|loss| loss.contains("reasoning")));
        assert!(first.losses.iter().any(|loss| loss.contains("unfinished")));
        connection
            .execute(
                "INSERT INTO messages VALUES ('m2', 'newer', 'tool', ?1, 2, 0)",
                [json!([{"type":"tool_result","data":{"name":"bash","content":"ok","is_error":false}}]).to_string()],
            )
            .unwrap();
        let second = export_crush(&cwd, None, "newer", first.source_cursor.as_deref()).unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].kind, WorkstreamEventKind::ToolResult);
        assert_eq!(second.events[0].content, "ok");
    }

    #[test]
    fn incomplete_final_jsonl_record_does_not_advance_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session.jsonl");
        fs::write(&path, b"{\"type\":\"message\",\"id\":\"one\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n{\"type\":").unwrap();
        let export = export_jsonl(ManagedHarness::Pi, &path, "session", None).unwrap();
        let cursor: FileCursor =
            serde_json::from_str(export.source_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(cursor.offset, 74);
        assert_eq!(export.events.len(), 1);
    }

    #[test]
    fn omp_adapter_reads_complete_atomic_write_temp_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        fs::create_dir(&cwd).unwrap();
        let session = "019f80c5-0148-7000-82d5-9a3c4c9b9be3";
        let path = temp
            .path()
            .join(format!(".session_{session}.jsonl.nonce.tmp"));
        std::fs::write(
            &path,
            format!("{}\n", json!({"type":"session","id":session,"cwd":cwd})),
        )
        .unwrap();

        let found = locate_session_file(
            ManagedHarness::Omp,
            temp.path(),
            &cwd,
            Some(temp.path()),
            session,
        )
        .unwrap();

        assert_eq!(found.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn claude_adapter_excludes_thinking_and_keeps_tools() {
        let value = json!({"type":"assistant","uuid":"record","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"private"},
            {"type":"tool_use","name":"Read","input":{"file":"README.md"}},
            {"type":"text","text":"Done"}
        ]}});
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_claude(&value, "session", "record", &mut events, &mut losses);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, WorkstreamEventKind::ToolCall);
        assert_eq!(events[1].content, "Done");
        assert_eq!(losses.len(), 1);
    }

    #[test]
    fn claude_adapter_excludes_read_back_managed_packets_only_at_the_origin() {
        let value = json!({
            "type":"user",
            "uuid":"record",
            "message":{"role":"user","content":[
                {
                    "type":"tool_result",
                    "content":format!(
                        "\n  {MANAGED_WORKSTREAM_PACKET_MARKER}\n> **ai-memory managed workstream: default**\nprivate packet"
                    )
                },
                {
                    "type":"tool_result",
                    "content":"  > **ai-memory managed workstream: default**\nlegacy packet"
                },
                {
                    "type":"tool_result",
                    "content":format!(
                        "ordinary file output\nmentions {MANAGED_WORKSTREAM_PACKET_MARKER} later"
                    )
                }
            ]}
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_claude(&value, "session", "record", &mut events, &mut losses);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WorkstreamEventKind::ToolResult);
        assert!(events[0].content.starts_with("ordinary file output"));
        assert_eq!(
            losses,
            [
                "Claude managed workstream delivery packets were intentionally excluded",
                "Claude managed workstream delivery packets were intentionally excluded"
            ]
        );
    }

    #[test]
    fn claude_adapter_keeps_compaction_and_excludes_meta_records() {
        let compact = json!({
            "type":"system",
            "subtype":"compact_boundary",
            "uuid":"compact",
            "content":"portable compact summary",
            "isMeta":false
        });
        let meta = json!({
            "type":"user",
            "uuid":"meta",
            "isMeta":true,
            "message":{"role":"user","content":"private harness metadata"}
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_claude(&compact, "session", "compact", &mut events, &mut losses);
        parse_claude(&meta, "session", "meta", &mut events, &mut losses);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WorkstreamEventKind::Compaction);
        assert_eq!(events[0].content, "portable compact summary");
        assert_eq!(
            losses,
            ["Claude synthetic/meta records were intentionally excluded"]
        );
    }

    #[test]
    fn event_ids_are_stable() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        push_event(
            &mut first,
            AgentKind::Codex,
            "s",
            "r",
            0,
            WorkstreamEventKind::Message,
            Some("user"),
            "hello",
            None,
            json!({}),
        );
        push_event(
            &mut second,
            AgentKind::Codex,
            "s",
            "r",
            0,
            WorkstreamEventKind::Message,
            Some("user"),
            "hello",
            None,
            json!({}),
        );
        assert_eq!(first[0].event_id, second[0].event_id);
    }

    #[test]
    fn codex_adapter_excludes_reloaded_harness_context() {
        let value = json!({"type":"response_item","payload":{"type":"message","role":"user","content":[
            {"type":"input_text","text":"# AGENTS.md instructions for /repo\n<INSTRUCTIONS>private</INSTRUCTIONS>"},
            {"type":"input_text","text":"actual request"}
        ]}});
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_codex(&value, "session", "record", &mut events, &mut losses);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "actual request");
    }

    #[test]
    fn codex_adapter_keeps_current_top_level_compaction_shape() {
        let value = json!({
            "type":"compacted",
            "timestamp":"2026-01-01T00:00:00Z",
            "payload":{"message":"portable compact summary","replacement_history":[]}
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_codex(&value, "session", "record", &mut events, &mut losses);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WorkstreamEventKind::Compaction);
        assert_eq!(events[0].content, "portable compact summary");
    }

    #[test]
    fn message_adapters_exclude_non_conversation_roles() {
        let claude = json!({
            "type":"user",
            "message":{"role":"system","content":"private Claude instructions"}
        });
        let opencode_message = json!({"role":"system"});
        let opencode_part = json!({"type":"text","text":"private OpenCode instructions"});
        let pi = json!({
            "type":"message",
            "message":{"role":"system","content":"private Pi instructions"}
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();

        parse_claude(&claude, "session", "claude", &mut events, &mut losses);
        parse_opencode(
            &opencode_message,
            &opencode_part,
            "session",
            "opencode",
            &mut events,
            &mut losses,
        );
        parse_pi_family(
            AgentKind::Pi,
            &pi,
            "session",
            "pi",
            &mut events,
            &mut losses,
        );

        assert!(events.is_empty());
        assert_eq!(losses.len(), 3);
    }

    #[test]
    fn pi_family_adapter_normalizes_tool_result_messages() {
        let value = json!({
            "type":"message",
            "timestamp":"2026-01-01T00:00:00Z",
            "message":{
                "role":"toolResult",
                "toolName":"read",
                "isError":false,
                "content":[{"type":"text","text":"file contents"}]
            }
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_pi_family(
            AgentKind::Omp,
            &value,
            "session",
            "record",
            &mut events,
            &mut losses,
        );

        assert!(losses.is_empty());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WorkstreamEventKind::ToolResult);
        assert_eq!(events[0].role.as_deref(), Some("tool"));
        assert!(events[0].content.contains("file contents"));
        assert_eq!(events[0].metadata["tool"], "read");
    }

    #[test]
    fn opencode_adapter_reads_sqlite_incrementally_without_writing_it() {
        let home = tempfile::tempdir().unwrap();
        let db = opencode_db(home.path(), None);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT);\n\
                 CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, \
                                   time_updated INTEGER, data TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('m1', 's1', ?1)",
                [json!({"role":"user"}).to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO part VALUES ('p1', 'm1', 's1', 1, ?1)",
                [json!({"type":"text","text":"first"}).to_string()],
            )
            .unwrap();

        let first = export_opencode(home.path(), None, "s1", None).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].content, "first");
        connection
            .execute(
                "INSERT INTO part VALUES ('p2', 'm1', 's1', 2, ?1)",
                [json!({"type":"text","text":"second"}).to_string()],
            )
            .unwrap();
        let second =
            export_opencode(home.path(), None, "s1", first.source_cursor.as_deref()).unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].content, "second");
    }

    /// Beta store fixture: `session_v2` + `session_message` with the shapes
    /// observed on beta-18999 (user text, assistant text/tool/reasoning).
    fn opencode2_fixture(home: &Path) {
        let db = opencode_db(home, None);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let connection = Connection::open(&db).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_v2(id TEXT PRIMARY KEY, directory TEXT NOT NULL, \
                                   time_updated INTEGER NOT NULL);\
                 CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, \
                                   time_updated INTEGER, data TEXT);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_v2 VALUES ('s2', ?1, 10)",
                [home.join("repo").to_string_lossy().to_string()],
            )
            .unwrap();
        for (id, updated, kind, data) in [
            (
                "m1",
                1,
                "user",
                json!({"text": "do the thing", "files": []}).to_string(),
            ),
            (
                "m2",
                2,
                "assistant",
                json!({"content": [
                    {"type": "reasoning", "text": "", "state": {}},
                    {"type": "text", "text": "on it"},
                    {"type": "tool", "name": "read", "state": {
                        "status": "completed",
                        "input": {"path": "a.txt"},
                        "content": [{"type": "text", "text": "file bytes"}],
                    }},
                ]})
                .to_string(),
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO session_message VALUES (?1, 's2', ?2, ?3, ?4)",
                    params![id, kind, updated, data],
                )
                .unwrap();
        }
    }

    #[test]
    fn opencode2_adapter_reads_v2_tables_incrementally() {
        let home = tempfile::tempdir().unwrap();
        opencode2_fixture(home.path());

        let first = export_opencode2(home.path(), None, "s2", None).unwrap();
        let contents: Vec<_> = first
            .events
            .iter()
            .map(|event| (event.kind, event.content.as_str()))
            .collect();
        assert!(contents.contains(&(WorkstreamEventKind::Message, "do the thing")));
        assert!(contents.contains(&(WorkstreamEventKind::Message, "on it")));
        assert!(
            contents
                .iter()
                .any(|(kind, content)| *kind == WorkstreamEventKind::ToolCall
                    && content.starts_with("read: "))
        );
        assert!(contents.contains(&(WorkstreamEventKind::ToolResult, "file bytes")));
        assert!(
            first.losses.iter().any(|loss| loss.contains("reasoning")),
            "encrypted reasoning must stay a named loss: {:?}",
            first.losses
        );

        // Incremental cursor: nothing new since the first export.
        let second =
            export_opencode2(home.path(), None, "s2", first.source_cursor.as_deref()).unwrap();
        assert!(second.events.is_empty());
        assert!(second.losses.is_empty());
    }

    #[test]
    fn opencode2_updated_tracks_session_v2_rows() {
        let home = tempfile::tempdir().unwrap();
        opencode2_fixture(home.path());
        assert_eq!(
            opencode2_updated(home.path(), None, "s2").unwrap(),
            Some(10)
        );
        assert_eq!(
            opencode2_updated(home.path(), None, "missing").unwrap(),
            None
        );
    }

    #[test]
    fn opencode2_discovery_ignores_other_checkouts() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).unwrap();
        let db_root = temp.path().join("opencode");
        fs::create_dir_all(&db_root).unwrap();
        let connection = Connection::open(db_root.join("opencode.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_v2( \
                     id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_updated INTEGER NOT NULL);",
            )
            .unwrap();
        for (id, directory, updated) in [
            ("older", &cwd, 100_i64),
            ("newer", &cwd, 200_i64),
            ("unrelated", &other, 300_i64),
        ] {
            connection
                .execute(
                    "INSERT INTO session_v2 VALUES (?1, ?2, ?3)",
                    params![id, directory.to_string_lossy(), updated],
                )
                .unwrap();
        }
        let found = discover_opencode2(
            temp.path(),
            Some(&db_root),
            &cwd,
            UNIX_EPOCH + Duration::from_millis(150),
        )
        .unwrap();
        assert_eq!(found.as_deref(), Some("newer"));
    }

    /// Build a two-bucket kimi store: `session_a` checked out at `cwd`,
    /// `session_b` at `other`. Returns `(root, wire_a)`.
    fn kimi_store_fixture(cwd: &Path, other: &Path) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for (bucket, id, locator_key, work_dir) in [
            ("wd_repo_a1b2c3d4e5f6", "session_aaa", "workDir", cwd),
            ("wd_other_f6e5d4c3b2a1", "session_bbb", "cwd", other),
        ] {
            let session_dir = root.path().join(bucket).join(id);
            fs::create_dir_all(session_dir.join("agents/main")).unwrap();
            let mut state = serde_json::Map::new();
            state.insert("id".into(), Value::String(id.into()));
            state.insert(locator_key.into(), json!(work_dir));
            fs::write(
                session_dir.join("state.json"),
                Value::Object(state).to_string(),
            )
            .unwrap();
        }
        let wire_a = root
            .path()
            .join("wd_repo_a1b2c3d4e5f6/session_aaa/agents/main/wire.jsonl");
        (root, wire_a)
    }

    #[tokio::test]
    async fn kimi_discovery_matches_checkout_via_state_json_not_bucket_name() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        let (root, wire_a) = kimi_store_fixture(&cwd, &other);
        fs::write(
            &wire_a,
            "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
        )
        .unwrap();
        let wire_b = root
            .path()
            .join("wd_other_f6e5d4c3b2a1/session_bbb/agents/main/wire.jsonl");
        fs::write(&wire_b, "").unwrap();
        // The "other" bucket is alphabetically first and its session newer,
        // so only an exact state locator match can pick the right session.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&wire_b, "{\"type\":\"metadata\"}\n").unwrap();

        let sessions = list_native_sessions(
            ManagedHarness::Kimi,
            temp.path(),
            &cwd,
            Some(root.path()),
            8,
        )
        .await
        .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_session_id, "session_aaa");

        let other_sessions = list_native_sessions(
            ManagedHarness::Kimi,
            temp.path(),
            &other,
            Some(root.path()),
            8,
        )
        .await
        .unwrap();
        assert_eq!(other_sessions.len(), 1);
        assert_eq!(other_sessions[0].native_session_id, "session_bbb");

        let found = locate_session_file(
            ManagedHarness::Kimi,
            temp.path(),
            &cwd,
            Some(root.path()),
            "session_bbb",
        )
        .unwrap();
        assert!(found.is_none());
        let found_for_other = locate_session_file(
            ManagedHarness::Kimi,
            temp.path(),
            &other,
            Some(root.path()),
            "session_bbb",
        )
        .unwrap();
        assert_eq!(found_for_other.as_deref(), Some(wire_b.as_path()));
        assert!(
            native_session_exists(
                ManagedHarness::Kimi,
                temp.path(),
                &cwd,
                Some(root.path()),
                "session_aaa"
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::Kimi,
                temp.path(),
                &cwd,
                Some(root.path()),
                "missing"
            )
            .unwrap()
        );
    }

    #[test]
    fn kimi_state_rejects_conflicting_locators_and_mismatched_ids() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        let session_dir = temp.path().join("session_expected");
        let wire = session_dir.join("agents/main/wire.jsonl");
        fs::create_dir_all(wire.parent().unwrap()).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(&wire, "").unwrap();

        fs::write(
            session_dir.join("state.json"),
            json!({"id":"session_expected","workDir":cwd,"cwd":other}).to_string(),
        )
        .unwrap();
        assert!(kimi_session_header(&wire).unwrap().is_none());

        fs::write(
            session_dir.join("state.json"),
            json!({"id":"session_other","cwd":cwd}).to_string(),
        )
        .unwrap();
        assert!(kimi_session_header(&wire).unwrap().is_none());

        fs::write(
            session_dir.join("state.json"),
            json!({"id":"session_expected","cwd":[cwd]}).to_string(),
        )
        .unwrap();
        assert!(kimi_session_header(&wire).unwrap().is_none());

        fs::write(
            session_dir.join("state.json"),
            json!({"id":"session_expected","workDir":cwd,"cwd":cwd}).to_string(),
        )
        .unwrap();
        assert_eq!(
            kimi_session_header(&wire).unwrap(),
            Some(("session_expected".into(), cwd))
        );
    }

    #[test]
    fn kimi_export_maps_visible_records_and_excludes_private_ones() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let session_dir = temp.path().join("store/wd_repo_a1b2c3d4e5f6/session_aaa");
        fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        fs::write(
            session_dir.join("state.json"),
            json!({"workDir": cwd}).to_string(),
        )
        .unwrap();
        let wire = session_dir.join("agents/main/wire.jsonl");
        let lines = [
            json!({"type":"context.append_message","time":1_700_000_000_000_i64,"message":{"role":"user","content":[{"type":"text","text":"hello kimi"}]}}),
            json!({"type":"context.append_message","message":{"role":"user","origin":{"kind":"hook_result","event":"UserPromptSubmit"},"content":[{"type":"text","text":"injected handoff delta"}]}}),
            json!({"type":"context.append_message","message":{"role":"user","origin":{"kind":"injection","variant":"todo"},"content":[{"type":"text","text":"injected todo"}]}}),
            json!({"type":"context.append_message","message":{"role":"system","content":[{"type":"text","text":"private system prompt"}]}}),
            json!({"type":"context.append_message","message":{"role":"assistant","content":[{"type":"think","think":"private reasoning"},{"type":"text","text":"visible answer"}],"toolCalls":[{"type":"function","id":"call_1","function":{"name":"bash","arguments":"{\"cmd\": \"ls\"}"}}]}}),
            json!({"type":"context.append_message","message":{"role":"tool","toolCallId":"call_1","content":[{"type":"text","text":"result ok"}]}}),
            json!({"type":"context.append_message","message":{"role":"assistant","partial":true,"content":[{"type":"text","text":"stream fragment"}]}}),
            json!({"type":"context.apply_compaction","summary":"compact summary","compactedCount":4}),
            json!({"type":"context.append_loop_event","event":{"type":"content.part","uuid":"part-think","stepUuid":"step-1","part":{"type":"think","think":"private loop reasoning"}}}),
            json!({"type":"context.append_loop_event","event":{"type":"content.part","uuid":"part-text","stepUuid":"step-1","part":{"type":"text","text":"loop visible answer"}}}),
            json!({"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"call-2","stepUuid":"step-1","toolCallId":"call_2","name":"Read","args":{"path":"README.md"}}}),
            json!({"type":"context.append_loop_event","event":{"type":"tool.result","parentUuid":"call-2","toolCallId":"call_2","result":{"output":[{"type":"text","text":"loop result ok"}],"isError":false}}}),
            json!({"type":"config.update","systemPrompt":"never copied"}),
            json!({"type":"turn.prompt","text":"duplicate projection"}),
        ];
        let raw: String = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<Vec<_>>()
            .concat();
        fs::write(&wire, &raw).unwrap();

        let export = export_jsonl(ManagedHarness::Kimi, &wire, "session_aaa", None).unwrap();
        let kinds: Vec<_> = export.events.iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            [
                WorkstreamEventKind::Message,
                WorkstreamEventKind::Message,
                WorkstreamEventKind::ToolCall,
                WorkstreamEventKind::ToolResult,
                WorkstreamEventKind::Compaction,
                WorkstreamEventKind::Message,
                WorkstreamEventKind::ToolCall,
                WorkstreamEventKind::ToolResult,
            ]
        );
        assert_eq!(export.events[0].content, "hello kimi");
        assert_eq!(export.events[0].role.as_deref(), Some("user"));
        assert_eq!(
            export.events[0].occurred_at.as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(export.events[1].content, "visible answer");
        assert_eq!(export.events[2].content, "bash: {\"cmd\":\"ls\"}");
        assert_eq!(export.events[2].metadata["tool"], "bash");
        assert_eq!(export.events[3].role.as_deref(), Some("tool"));
        assert_eq!(export.events[4].content, "compact summary");
        assert_eq!(export.events[5].content, "loop visible answer");
        assert_eq!(export.events[6].content, "Read: {\"path\":\"README.md\"}");
        assert_eq!(export.events[6].metadata["tool"], "Read");
        assert_eq!(export.events[6].metadata["tool_call_id"], "call_2");
        assert_eq!(export.events[7].content, "loop result ok");
        assert_eq!(export.events[7].metadata["is_error"], false);
        assert!(
            export
                .events
                .iter()
                .all(|event| !event.content.contains("injected")
                    && !event.content.contains("private")
                    && !event.content.contains("stream fragment")
                    && !event.content.contains("duplicate")),
            "{:?}",
            export.events.iter().map(|e| &e.content).collect::<Vec<_>>()
        );
        for expected in ["harness-injected", "system messages", "hidden reasoning"] {
            assert!(
                export.losses.iter().any(|loss| loss.contains(expected)),
                "missing loss {expected}: {:?}",
                export.losses
            );
        }

        // Record ids are the sha256 of the raw journal line, stable across
        // whole-file rewrites (fork/compaction/resume rewrites the journal).
        let first_line = raw.lines().next().unwrap();
        assert_eq!(
            export.events[0].source_record_id.as_deref(),
            Some(format!("{:x}", Sha256::digest(first_line.as_bytes())).as_str())
        );
        fs::write(&wire, &raw).unwrap();
        let reimport = export_jsonl(ManagedHarness::Kimi, &wire, "session_aaa", None).unwrap();
        let ids: Vec<_> = export.events.iter().map(|event| &event.event_id).collect();
        let reids: Vec<_> = reimport
            .events
            .iter()
            .map(|event| &event.event_id)
            .collect();
        assert_eq!(ids, reids, "event ids must survive journal rewrites");
    }

    #[test]
    fn kimi_export_is_incremental_and_tolerates_an_unfinished_tail() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp
            .path()
            .join("store/bucket/session_x/agents/main/wire.jsonl");
        fs::create_dir_all(wire.parent().unwrap()).unwrap();
        let first = "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"one\"}]}}";
        let second = "{\"type\":\"context.append_message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"two\"}]}}";
        fs::write(&wire, format!("{first}\n{{\"type\":")).unwrap();

        let initial = export_jsonl(ManagedHarness::Kimi, &wire, "session_x", None).unwrap();
        assert_eq!(initial.events.len(), 1);
        let cursor: FileCursor =
            serde_json::from_str(initial.source_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(cursor.offset, first.len() as u64 + 1);
        assert!(cursor.prefix_sha256.is_some());

        fs::write(&wire, format!("{first}\n{second}\n")).unwrap();
        let incremental = export_jsonl(
            ManagedHarness::Kimi,
            &wire,
            "session_x",
            Some(&serde_json::to_string(&cursor).unwrap()),
        )
        .unwrap();
        assert_eq!(incremental.events.len(), 1);
        assert_eq!(incremental.events[0].content, "two");
    }

    #[test]
    fn kimi_export_resets_cursor_after_an_in_place_journal_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let wire = temp
            .path()
            .join("store/bucket/session_x/agents/main/wire.jsonl");
        fs::create_dir_all(wire.parent().unwrap()).unwrap();
        let message = |role: &str, text: &str| {
            json!({
                "type": "context.append_message",
                "message": {
                    "role": role,
                    "content": [{"type": "text", "text": text}],
                },
            })
            .to_string()
        };
        let first = message("user", "one");
        let second = message("assistant", "two");
        fs::write(&wire, format!("{first}\n{second}\n")).unwrap();

        let initial = export_jsonl(ManagedHarness::Kimi, &wire, "session_x", None).unwrap();
        let initial_ids: Vec<_> = initial
            .events
            .iter()
            .map(|event| event.event_id.clone())
            .collect();
        let inserted = message(
            "user",
            "a rewritten prefix whose different byte length invalidates the old offset",
        );
        let third = message("assistant", "three");
        fs::write(&wire, format!("{inserted}\n{first}\n{second}\n{third}\n")).unwrap();

        let rewritten = export_jsonl(
            ManagedHarness::Kimi,
            &wire,
            "session_x",
            initial.source_cursor.as_deref(),
        )
        .unwrap();
        assert_eq!(
            rewritten
                .events
                .iter()
                .map(|event| event.content.as_str())
                .collect::<Vec<_>>(),
            [
                "a rewritten prefix whose different byte length invalidates the old offset",
                "one",
                "two",
                "three",
            ]
        );
        assert_eq!(rewritten.events[1].event_id, initial_ids[0]);
        assert_eq!(rewritten.events[2].event_id, initial_ids[1]);
    }

    #[test]
    fn kimi_export_annotates_unimported_subagent_journals() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = temp.path().join("store/bucket/session_x");
        let main = session_dir.join("agents/main/wire.jsonl");
        fs::create_dir_all(main.parent().unwrap()).unwrap();
        fs::write(
            &main,
            "{\"type\":\"context.append_message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
        )
        .unwrap();

        let without = export_jsonl(ManagedHarness::Kimi, &main, "session_x", None).unwrap();
        assert!(!without.losses.iter().any(|loss| loss.contains("subagent")));

        let sub = session_dir.join("agents/sub-1/wire.jsonl");
        fs::create_dir_all(sub.parent().unwrap()).unwrap();
        fs::write(&sub, "{\"type\":\"context.append_message\"}\n").unwrap();
        let with = export_jsonl(ManagedHarness::Kimi, &main, "session_x", None).unwrap();
        assert!(
            with.losses
                .iter()
                .any(|loss| loss.contains("subagent transcripts were not imported")),
            "{:?}",
            with.losses
        );
        // The subagent journal itself is never picked up as a transcript.
        assert!(!transcript_file(ManagedHarness::Kimi, &sub));
        assert!(transcript_file(ManagedHarness::Kimi, &main));
    }

    #[test]
    fn kimi_adapter_excludes_non_conversation_roles_like_other_adapters() {
        let value = json!({
            "type":"context.append_message",
            "message":{"role":"system","content":[{"type":"text","text":"private Kimi instructions"}]}
        });
        let mut events = Vec::new();
        let mut losses = Vec::new();
        parse_kimi(&value, "session", "record", &mut events, &mut losses);
        assert!(events.is_empty());
        assert_eq!(losses, ["Kimi system messages were intentionally excluded"]);
    }

    /// Build a two-bucket grok store: `session_a` checked out at `cwd`,
    /// `session_b` at `other`. Returns `(root, chat_a)`.
    fn grok_store_fixture(cwd: &Path, other: &Path) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for (bucket, id, work_dir) in [
            ("%2Fother%2Fencoded", "019f-session-aaa", cwd),
            ("%2Frepo%2Fencoded", "019f-session-bbb", other),
        ] {
            let session_dir = root.path().join(bucket).join(id);
            fs::create_dir_all(&session_dir).unwrap();
            fs::write(
                session_dir.join("summary.json"),
                json!({"info": {"id": id, "cwd": work_dir}}).to_string(),
            )
            .unwrap();
        }
        let chat_a = root
            .path()
            .join("%2Fother%2Fencoded/019f-session-aaa/chat_history.jsonl");
        (root, chat_a)
    }

    #[tokio::test]
    async fn grok_discovery_matches_checkout_via_summary_json_not_bucket_name() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        // Bucket names deliberately contradict the recorded cwd, so only
        // summary.json metadata can produce a correct match.
        let (root, chat_a) = grok_store_fixture(&cwd, &other);
        fs::write(&chat_a, "{\"type\":\"user\",\"content\":\"hi\"}\n").unwrap();
        let chat_b = root
            .path()
            .join("%2Frepo%2Fencoded/019f-session-bbb/chat_history.jsonl");
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&chat_b, "{\"type\":\"user\",\"content\":\"hello\"}\n").unwrap();
        // Sibling harness internals must never count as transcripts.
        fs::write(chat_a.parent().unwrap().join("events.jsonl"), "{}\n").unwrap();

        let sessions = list_native_sessions(
            ManagedHarness::Grok,
            temp.path(),
            &cwd,
            Some(root.path()),
            8,
        )
        .await
        .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_session_id, "019f-session-aaa");

        let found = locate_session_file(
            ManagedHarness::Grok,
            temp.path(),
            &cwd,
            Some(root.path()),
            "019f-session-aaa",
        )
        .unwrap();
        assert_eq!(found.as_deref(), Some(chat_a.as_path()));
        assert!(
            native_session_exists(
                ManagedHarness::Grok,
                temp.path(),
                &cwd,
                Some(root.path()),
                "019f-session-aaa"
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::Grok,
                temp.path(),
                &cwd,
                Some(root.path()),
                "missing"
            )
            .unwrap()
        );
        assert!(!transcript_file(
            ManagedHarness::Grok,
            &chat_a.parent().unwrap().join("events.jsonl")
        ));
    }

    #[test]
    fn grok_adapter_excludes_private_records_and_keeps_visible_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chat_history.jsonl");
        let records = [
            json!({"type":"system","content":"private system prompt"}),
            json!({"type":"user","content":[{"type":"text","text":"<user_info>\nOS: linux\n</user_info>"}]}),
            json!({"type":"user","content":[{"type":"text","text":"fix the bug"}]}),
            json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque","status":"done"}),
            json!({"type":"assistant","content":"reading the file","model_id":"grok-4.5",
                   "tool_calls":[{"id":"call-1","name":"read_file","arguments":"{\"target_file\":\"a.rs\"}"}]}),
            json!({"type":"tool_result","tool_call_id":"call-1","content":"fn main() {}"}),
            json!({"type":"backend_tool_call","kind":{"tool_type":"web_search","action":{"type":"search","query":"rust"}}}),
        ];
        let body = records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        fs::write(&path, body).unwrap();

        let export = export_jsonl(ManagedHarness::Grok, &path, "019f-session", None).unwrap();
        let contents: Vec<_> = export
            .events
            .iter()
            .map(|event| (event.kind, event.content.as_str()))
            .collect();
        assert_eq!(
            contents,
            [
                (WorkstreamEventKind::Message, "fix the bug"),
                (WorkstreamEventKind::Message, "reading the file"),
                (
                    WorkstreamEventKind::ToolCall,
                    "read_file: {\"target_file\":\"a.rs\"}"
                ),
                (WorkstreamEventKind::ToolResult, "fn main() {}"),
                (
                    WorkstreamEventKind::ToolCall,
                    "web_search: {\"type\":\"search\",\"query\":\"rust\"}"
                ),
            ]
        );
        assert!(
            export
                .losses
                .iter()
                .any(|loss| loss.contains("system prompt records"))
        );
        assert!(
            export
                .losses
                .iter()
                .any(|loss| loss.contains("hidden reasoning"))
        );
        let cursor: FileCursor = serde_json::from_str(&export.source_cursor.unwrap()).unwrap();
        assert!(cursor.prefix_sha256.is_some());
    }

    /// Grok keeps harness scaffolding in `user` records, unlike Kimi, which
    /// isolates it in `config.update` records the adapter never reads. A real
    /// session measured 42KB of reminders (skills catalogue plus connected MCP
    /// servers) against 270 bytes of genuine input, which both leaked harness
    /// internals into the ledger and evicted real conversation from the
    /// startup packet budget.
    #[test]
    fn grok_adapter_excludes_injected_system_reminders_from_user_records() {
        let records = [
            json!({"type":"user","content":[{"type":"text","text":"<user_info>\nOS Version: linux\n</user_info>"}]}),
            json!({"type":"user","content":[{"type":"text","text":"<system-reminder>\nAs you answer the user's questions, you can use the following context\n</system-reminder>"}]}),
            // Indented in real sessions, so the check must tolerate leading space.
            json!({"type":"user","content":[{"type":"text","text":"  <system-reminder> The following skills are available for use: ego-browser…</system-reminder>"}]}),
            json!({"type":"user","content":[{"type":"text","text":"<system-reminder>\nMCP servers connected: - ai-memory (16 tools)\n</system-reminder>"}]}),
            json!({"type":"user","content":[{"type":"text","text":"<user_query>\nfix the bug\n</user_query>"}]}),
        ];
        let mut events = Vec::new();
        let mut losses = Vec::new();
        for (index, record) in records.iter().enumerate() {
            parse_grok(
                record,
                "019f-session",
                &format!("record-{index}"),
                &mut events,
                &mut losses,
            );
        }

        let contents: Vec<_> = events
            .iter()
            .map(|event| event.content.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            contents,
            ["<user_query>\nfix the bug\n</user_query>"],
            "only genuine user input may reach the ledger"
        );
    }

    #[test]
    fn grok_export_resets_cursor_after_an_in_place_journal_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("chat_history.jsonl");
        fs::write(&path, "{\"type\":\"user\",\"content\":\"one\"}\n").unwrap();
        let first = export_jsonl(ManagedHarness::Grok, &path, "019f-session", None).unwrap();
        assert_eq!(first.events.len(), 1);

        // Appending keeps the validated prefix and resumes incrementally.
        let mut appended = fs::read(&path).unwrap();
        appended.extend(b"{\"type\":\"user\",\"content\":\"two\"}\n");
        fs::write(&path, appended).unwrap();
        let second = export_jsonl(
            ManagedHarness::Grok,
            &path,
            "019f-session",
            first.source_cursor.as_deref(),
        )
        .unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].content, "two");

        // A rewind rewrites the journal in place: the stored prefix no longer
        // matches, so the export replays from the start with stable event ids.
        fs::write(
            &path,
            "{\"type\":\"user\",\"content\":\"one\"}\n{\"type\":\"assistant\",\"content\":\"rewound\"}\n",
        )
        .unwrap();
        let third = export_jsonl(
            ManagedHarness::Grok,
            &path,
            "019f-session",
            second.source_cursor.as_deref(),
        )
        .unwrap();
        assert_eq!(third.events.len(), 2);
        assert_eq!(
            third.events[0].event_id, first.events[0].event_id,
            "identical lines must keep identical event ids across rewrites"
        );
    }

    fn push_protobuf_varint(output: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            output.push(if value == 0 { byte } else { byte | 0x80 });
            if value == 0 {
                break;
            }
        }
    }

    fn push_protobuf_bytes(output: &mut Vec<u8>, field: u64, value: &[u8]) {
        push_protobuf_varint(output, (field << 3) | 2);
        push_protobuf_varint(output, u64::try_from(value.len()).unwrap());
        output.extend_from_slice(value);
    }

    /// Build the two protobuf layers observed in `agy` v1.1.7 metadata. The
    /// unrelated fields ensure discovery searches by field number instead of
    /// assuming the workspace URI is the entire message.
    fn antigravity_metadata(uri: &str) -> Vec<u8> {
        let mut nested = Vec::new();
        push_protobuf_varint(&mut nested, 2 << 3);
        push_protobuf_varint(&mut nested, 7);
        push_protobuf_bytes(&mut nested, 1, uri.as_bytes());
        push_protobuf_bytes(&mut nested, 4, b"fixture-metadata");

        let mut outer = Vec::new();
        push_protobuf_bytes(&mut outer, 3, b"unrelated");
        push_protobuf_bytes(&mut outer, 1, &nested);
        push_protobuf_varint(&mut outer, 5 << 3);
        push_protobuf_varint(&mut outer, 1);
        outer
    }

    /// `file://` URI for a local path, the way `agy` records its workspace.
    /// Windows paths carry a drive letter and need the extra leading slash.
    fn file_uri(path: &Path) -> String {
        let text = path.to_string_lossy().replace('\\', "/");
        if text.starts_with('/') {
            format!("file://{text}")
        } else {
            format!("file:///{text}")
        }
    }

    fn write_antigravity_conversation(root: &Path, id: &str, uri: &str) -> PathBuf {
        let path = root.join(format!("{id}.db"));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "CREATE TABLE trajectory_metadata_blob (id text DEFAULT \"main\", data blob, PRIMARY KEY (id))",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                params![antigravity_metadata(uri)],
            )
            .unwrap();
        path
    }

    /// The conversation id is the file name and the workspace comes from the
    /// metadata blob, so a checkout only sees its own conversations. The long
    /// component forces both protobuf layers to use multi-byte varint lengths.
    #[tokio::test]
    async fn antigravity_lists_only_conversations_from_this_workspace() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.parent().unwrap().join("outside.db"), b"outside").unwrap();
        let cwd = temp.path().join(format!("checkout-{}", "x".repeat(140)));
        fs::create_dir_all(&cwd).unwrap();
        let mine = "a0d5ac62-2501-4780-b783-76d159c56cb3";
        let theirs = "9576275f-7c4e-4709-b372-22d1ad2a0af8";
        write_antigravity_conversation(&root, mine, &file_uri(&cwd));
        write_antigravity_conversation(&root, theirs, "file:///somewhere/else");

        let sessions =
            list_native_sessions(ManagedHarness::Antigravity, temp.path(), &cwd, None, 8)
                .await
                .unwrap();

        assert_eq!(
            sessions
                .iter()
                .map(|s| s.native_session_id.as_str())
                .collect::<Vec<_>>(),
            [mine]
        );
        assert!(
            native_session_exists(ManagedHarness::Antigravity, temp.path(), &cwd, None, mine)
                .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::Antigravity,
                temp.path(),
                &cwd,
                None,
                "11111111-1111-1111-1111-111111111111"
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::Antigravity,
                temp.path(),
                &cwd,
                None,
                "../outside"
            )
            .unwrap()
        );
    }

    /// A database whose metadata is shaped differently — an older or newer
    /// `agy` — is skipped, never an error: it would otherwise take the whole
    /// listing down with it.
    #[tokio::test]
    async fn antigravity_skips_conversations_without_readable_metadata() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&root).unwrap();
        let cwd = temp.path().join("checkout");
        fs::create_dir_all(&cwd).unwrap();
        // Right name, no metadata table at all.
        Connection::open(root.join("53fb8b64-76c5-4fd8-91d2-dfabe2be4188.db")).unwrap();
        fs::write(root.join("not-a-database.db"), b"garbage").unwrap();

        let sessions =
            list_native_sessions(ManagedHarness::Antigravity, temp.path(), &cwd, None, 8)
                .await
                .unwrap();

        assert!(sessions.is_empty());
    }

    /// Every step payload is an undocumented protobuf blob, so the ledger for
    /// this harness comes from hook capture. The failure has to say so.
    #[tokio::test]
    async fn antigravity_transcript_export_explains_why_it_is_unavailable() {
        let temp = tempfile::TempDir::new().unwrap();
        let error = export_transcript(
            ManagedHarness::Antigravity,
            temp.path(),
            temp.path(),
            None,
            "a0d5ac62-2501-4780-b783-76d159c56cb3",
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("hook capture"), "{error}");
    }

    const KIRO_SESSION_A: &str = "3f6d1c2a-0000-4000-8000-000000000aaa";
    const KIRO_SESSION_B: &str = "3f6d1c2a-0000-4000-8000-000000000bbb";
    const KIRO_V3_SESSION: &str = "sess_c3774f9d-269e-40d1-aa02-2bb0c0817b4e";

    fn write_kiro_session(root: &Path, id: &str, cwd: &Path, lines: &[Value]) {
        fs::write(
            root.join(format!("{id}.json")),
            json!({
                "session_id": id,
                "cwd": cwd,
                "created_at": "2026-08-01T10:00:00Z",
                "updated_at": "2026-08-01T10:05:00Z",
                "title": "sanitized structural fixture",
                "session_state": {"version": "v1", "conversation_metadata": {}}
            })
            .to_string(),
        )
        .unwrap();
        let transcript = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(root.join(format!("{id}.jsonl")), transcript).unwrap();
    }

    fn write_kiro_v3_session(root: &Path, id: &str, workspaces: &[&Path], messages: &str) {
        let session_dir = root.join("checkout-fixture").join(id);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            json!({
                "schemaVersion": "1.0.0",
                "dataModelVersion": 1,
                "id": id,
                "workspacePaths": workspaces,
                "createdAt": "2026-08-06T10:00:00Z",
                "lastModifiedAt": "2026-08-06T10:05:00Z",
                "agentMode": "vibe",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        fs::write(session_dir.join("messages.jsonl"), messages).unwrap();
    }

    #[tokio::test]
    async fn kiro_discovery_and_exact_lookup_are_checkout_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        let root = temp.path().join("kiro-sessions");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(&root).unwrap();
        write_kiro_session(&root, KIRO_SESSION_A, &cwd, &[]);
        write_kiro_session(&root, KIRO_SESSION_B, &other, &[]);

        let sessions =
            list_native_sessions(ManagedHarness::Kiro, temp.path(), &cwd, Some(&root), 8)
                .await
                .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_session_id, KIRO_SESSION_A);

        assert_eq!(
            locate_session_file(
                ManagedHarness::Kiro,
                temp.path(),
                &cwd,
                Some(&root),
                KIRO_SESSION_A,
            )
            .unwrap()
            .as_deref(),
            Some(root.join(format!("{KIRO_SESSION_A}.jsonl")).as_path())
        );
        assert!(
            locate_session_file(
                ManagedHarness::Kiro,
                temp.path(),
                &cwd,
                Some(&root),
                KIRO_SESSION_B,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            locate_session_file(
                ManagedHarness::Kiro,
                temp.path(),
                &cwd,
                Some(&root),
                "../../outside",
            )
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn kiro_mismatched_metadata_never_becomes_resumable() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let root = temp.path().join("kiro-sessions");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(format!("{KIRO_SESSION_A}.json")),
            json!({"session_id": KIRO_SESSION_B, "cwd": cwd}).to_string(),
        )
        .unwrap();
        fs::write(root.join(format!("{KIRO_SESSION_A}.jsonl")), "{}\n").unwrap();

        assert!(
            list_native_sessions(ManagedHarness::Kiro, temp.path(), &cwd, Some(&root), 8,)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::Kiro,
                temp.path(),
                &cwd,
                Some(&root),
                KIRO_SESSION_A,
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn kiro_v3_discovery_is_checkout_scoped_and_never_cross_resumes_v2() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let other = temp.path().join("other");
        let v2_root = temp.path().join("v2");
        let v3_root = temp.path().join("v3");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(&v2_root).unwrap();
        write_kiro_session(&v2_root, KIRO_SESSION_A, &cwd, &[]);
        write_kiro_v3_session(
            &v3_root,
            KIRO_V3_SESSION,
            &[&other, &cwd],
            include_str!("../tests/fixtures/kiro-v3-messages.jsonl"),
        );

        let sessions =
            list_native_sessions(ManagedHarness::KiroV3, temp.path(), &cwd, Some(&v3_root), 8)
                .await
                .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].native_session_id, KIRO_V3_SESSION);
        assert!(
            native_session_exists(
                ManagedHarness::KiroV3,
                temp.path(),
                &cwd,
                Some(&v3_root),
                KIRO_V3_SESSION,
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::Kiro,
                temp.path(),
                &cwd,
                Some(&v2_root),
                KIRO_V3_SESSION,
            )
            .unwrap()
        );
        assert!(
            !native_session_exists(
                ManagedHarness::KiroV3,
                temp.path(),
                &cwd,
                Some(&v3_root),
                KIRO_SESSION_A,
            )
            .unwrap()
        );
    }

    #[tokio::test]
    async fn kiro_v3_rejects_unknown_schema_and_history_only_mirrors() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(root.join("cli")).unwrap();
        fs::write(root.join("cli/fixture.history"), "sanitized prompt\n").unwrap();
        write_kiro_v3_session(&root, KIRO_V3_SESSION, &[&cwd], "{}\n");
        let metadata = root
            .join("checkout-fixture")
            .join(KIRO_V3_SESSION)
            .join("session.json");
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(&metadata).unwrap()).unwrap();
        value["schemaVersion"] = Value::String("2.0.0".into());
        fs::write(&metadata, value.to_string()).unwrap();

        assert!(
            list_native_sessions(ManagedHarness::KiroV3, temp.path(), &cwd, Some(&root), 8,)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            list_native_sessions(
                ManagedHarness::Kiro,
                temp.path(),
                &cwd,
                Some(&root.join("cli")),
                8,
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn kiro_v3_detects_the_custom_home_resume_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let configured = temp.path().join("custom-kiro/sessions");
        let default = temp.path().join(".kiro/sessions");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&configured).unwrap();
        write_kiro_v3_session(&default, KIRO_V3_SESSION, &[&cwd], "{}\n");

        assert!(
            kiro_v3_resume_uses_default_store(
                temp.path(),
                &cwd,
                Some(&configured),
                KIRO_V3_SESSION,
            )
            .unwrap()
        );
        assert!(
            !kiro_v3_resume_uses_default_store(temp.path(), &cwd, Some(&default), KIRO_V3_SESSION,)
                .unwrap()
        );
    }

    #[test]
    fn kiro_export_maps_only_visible_v1_records() {
        let temp = tempfile::tempdir().unwrap();
        let stream = temp.path().join(format!("{KIRO_SESSION_A}.jsonl"));
        fs::write(
            &stream,
            format!(
                "{}{}\n{}\n",
                include_str!("../tests/fixtures/kiro-v2-messages.jsonl"),
                json!({"version":"v1","kind":"Prompt","data":{"message_id":"m4","content":[{"kind":"image","data":{"format":"png"}}]}}),
                json!({"version":"v2","kind":"Prompt","data":{"message_id":"m5","content":[{"kind":"text","data":"future"}]}}),
            ),
        )
        .unwrap();

        let export = export_jsonl(ManagedHarness::Kiro, &stream, KIRO_SESSION_A, None).unwrap();
        assert_eq!(export.events.len(), 4);
        assert_eq!(export.events[0].content, "sanitized v2 prompt");
        assert_eq!(export.events[0].role.as_deref(), Some("user"));
        assert_eq!(
            export.events[0].occurred_at.as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(export.events[1].content, "sanitized v2 reply");
        assert_eq!(export.events[2].kind, WorkstreamEventKind::ToolCall);
        assert_eq!(export.events[3].content, "sanitized v2 result");
        assert!(
            export
                .losses
                .iter()
                .any(|loss| loss.contains("non-text content"))
        );
        assert!(
            export
                .losses
                .iter()
                .any(|loss| loss.contains("unsupported envelope version v2"))
        );
    }

    #[test]
    fn kiro_v3_export_maps_only_visible_records_and_persists_flavor() {
        let temp = tempfile::tempdir().unwrap();
        let stream = temp.path().join("messages.jsonl");
        fs::write(
            &stream,
            include_str!("../tests/fixtures/kiro-v3-messages.jsonl"),
        )
        .unwrap();

        let export = export_jsonl(ManagedHarness::KiroV3, &stream, KIRO_V3_SESSION, None).unwrap();
        assert_eq!(export.events.len(), 4);
        assert_eq!(export.events[0].content, "sanitized v3 prompt");
        assert_eq!(export.events[0].role.as_deref(), Some("user"));
        assert_eq!(export.events[1].content, "sanitized v3 reply");
        assert_eq!(export.events[2].kind, WorkstreamEventKind::ToolCall);
        assert_eq!(export.events[3].content, "sanitized v3 result");
        assert!(
            export
                .losses
                .iter()
                .any(|loss| loss.contains("private session"))
        );
        assert!(
            export
                .losses
                .iter()
                .any(|loss| loss.contains("non-visible assistant"))
        );
        let cursor: Value = serde_json::from_str(export.source_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(
            cursor.get("flavor").and_then(Value::as_str),
            Some("kiro-v3")
        );
    }

    #[test]
    fn kiro_cursor_restarts_after_an_in_place_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let stream = temp.path().join(format!("{KIRO_SESSION_A}.jsonl"));
        fs::write(
            &stream,
            format!(
                "{}\n",
                json!({"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"first"}]}})
            ),
        )
        .unwrap();
        let first = export_jsonl(ManagedHarness::Kiro, &stream, KIRO_SESSION_A, None).unwrap();

        fs::write(
            &stream,
            format!(
                "{}\n",
                json!({"version":"v1","kind":"Prompt","data":{"message_id":"m2","content":[{"kind":"text","data":"rewritten"}]}})
            ),
        )
        .unwrap();
        let rewritten = export_jsonl(
            ManagedHarness::Kiro,
            &stream,
            KIRO_SESSION_A,
            first.source_cursor.as_deref(),
        )
        .unwrap();
        assert_eq!(rewritten.events.len(), 1);
        assert_eq!(rewritten.events[0].content, "rewritten");
    }

    #[test]
    fn file_uris_decode_on_both_platform_shapes() {
        assert_eq!(
            path_from_file_uri("file:///C:/Users/me/Projetos"),
            Some(PathBuf::from("C:/Users/me/Projetos"))
        );
        assert_eq!(
            path_from_file_uri("file:///home/me/projects"),
            Some(PathBuf::from("/home/me/projects"))
        );
        assert_eq!(
            path_from_file_uri("file:///C:/Users/me/My%20Projects"),
            Some(PathBuf::from("C:/Users/me/My Projects"))
        );
        // A stray percent that is not an escape must not eat the next bytes.
        assert_eq!(
            path_from_file_uri("file:///tmp/100%done"),
            Some(PathBuf::from("/tmp/100%done"))
        );
        assert_eq!(path_from_file_uri("https://example.com/x"), None);
        assert_eq!(path_from_file_uri("file://"), None);
    }

    #[test]
    fn protobuf_walk_skips_other_fields_and_wire_types() {
        // field 1 varint, field 2 length-delimited, field 3 fixed64,
        // then field 4 length-delimited — only field 4 must come back.
        let mut message = vec![0x08, 0x96, 0x01];
        message.extend_from_slice(&[0x12, 0x02, b'h', b'i']);
        message.extend_from_slice(&[0x19, 0, 0, 0, 0, 0, 0, 0, 0]);
        message.extend_from_slice(&[0x22, 0x03, b'y', b'e', b's']);

        assert_eq!(protobuf_field(&message, 4), Some(&b"yes"[..]));
        assert_eq!(protobuf_field(&message, 2), Some(&b"hi"[..]));
        // A varint field is not length-delimited, so it is never returned.
        assert_eq!(protobuf_field(&message, 1), None);
        assert_eq!(protobuf_field(&message, 9), None);
        // Truncated input ends the walk instead of panicking on a slice.
        assert_eq!(protobuf_field(&[0x22, 0x10, b'x'], 4), None);
        // A ten-byte varint may carry only one payload bit in its final byte.
        assert_eq!(protobuf_varint(&[0xff; 10]), None);
    }
}
