//! Antigravity lifecycle hook output extraction, enrichment and prompt
//! recovery.
//!
//! Antigravity CLI's native `PostToolUse` event payload carries invocation
//! metadata (`toolCall`, `conversationId`, `stepIdx`, `artifactDirectoryPath`)
//! but omits command/tool stdout and stderr from the event JSON itself.
//! Instead, the CLI writes step execution logs to:
//! `<artifactDirectoryPath>/.system_generated/steps/<stepIdx>/output.txt`.
//!
//! This module resolves and reads that output file for output-eligible tools
//! (e.g. `run_command`, `view_file`, `list_dir`, etc.) while strictly
//! preserving the code extraction behavior for file-editing tools (`write_to_file`,
//! `replace_file_content`, etc.), which extract actual code changes from `args`.
//!
//! No Antigravity hook event carries the user's prompt, but every event names
//! the conversation's `transcriptPath`. [`antigravity_transcript_path`] and
//! [`read_transcript_prompts`] recover the `USER_INPUT` steps from it, with
//! the same file safety as `output.txt` plus containment under the
//! conversation's own `brain/<conversationId>/` directory. The transcript
//! format is undocumented, so parsing is defensive.
//! [`stop_is_turn_checkpoint`] reads the `Stop` payload the server uses to
//! publish a turn checkpoint. Hook reference:
//! <https://antigravity.google/docs/hooks/>.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::payload::HookEvent;

/// Maximum number of bytes read from Antigravity's `output.txt`.
/// Matches the standard tool excerpt upper bound (2 KB) so spooled events
/// remain compact and within bounded limits.
pub const MAX_ANTIGRAVITY_OUTPUT_BYTES: usize = 2048;

/// Check if a tool is eligible for output extraction from `output.txt`.
///
/// Whitelist rationale:
/// - Tools like `run_command`, `view_file`, and search tools produce valuable
///   execution logs or inspection output in `output.txt`.
/// - Mutation tools (`write_to_file`, `replace_file_content`) must NEVER be
///   enriched here because their `output.txt` only contains generic confirmation
///   strings ("Created file..."), whereas `ai-memory` extracts the real code
///   from `args.CodeContent` / `args.ReplacementContent`. Overwriting with
///   `tool_response` would destroy that code capture.
/// - Unproven generic tools (`read_url_content`, `read_resource`, `call_mcp_tool`)
///   fail closed to `ToolFamily::Unknown`.
#[must_use]
pub fn is_output_eligible(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "run_command"
            | "view_file"
            | "list_dir"
            | "find_by_name"
            | "grep_search"
            | "search_web"
            | "manage_task"
            | "manage_subagents"
    )
}

/// Resolves an artifact directory path, expanding leading `~` or `~/`
/// if `home_dir` is provided.
#[must_use]
pub fn resolve_artifact_dir(raw_path: &str, home_dir: Option<&Path>) -> Option<PathBuf> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(stripped) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        home_dir.map(|h| h.join(stripped))
    } else if trimmed == "~" {
        home_dir.map(Path::to_path_buf)
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Builds the absolute path to a step's `output.txt` file given the artifact directory
/// and the step index.
#[must_use]
pub fn step_output_file_path(artifact_dir: &Path, step_idx: u64) -> PathBuf {
    artifact_dir
        .join(".system_generated")
        .join("steps")
        .join(step_idx.to_string())
        .join("output.txt")
}

/// Reads up to `max_bytes` from `output_path`, ensuring it is a regular file
/// (preventing symlink traversal and blocking on FIFOs/special devices). Slices
/// cleanly on multi-byte UTF-8 code point boundaries and trims trailing whitespace.
/// Returns `None` if the file cannot be read, is not a regular file, or contains
/// only whitespace.
#[must_use]
pub fn read_step_output(output_path: &Path, max_bytes: usize) -> Option<String> {
    let metadata = std::fs::symlink_metadata(output_path).ok()?;
    if !metadata.is_file() {
        return None;
    }

    let file = File::open(output_path).ok()?;
    let mut reader = Read::take(file, max_bytes as u64);
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer).ok()?;
    if buffer.is_empty() {
        return None;
    }

    let text = decode_step_output(&buffer);
    let trimmed = text.trim_end();
    if trimmed.trim().is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Decode step output read under a byte cap. A code point the cap cut in
/// half at the end is dropped; any other invalid byte (a legacy code page
/// such as cp1252, or binary output) becomes U+FFFD, so the text after it is
/// kept instead of being discarded with it.
fn decode_step_output(buffer: &[u8]) -> String {
    let mut out = String::with_capacity(buffer.len());
    let mut rest = buffer;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                out.push_str(valid);
                return out;
            }
            Err(err) => {
                let (valid, after) = rest.split_at(err.valid_up_to());
                out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                let Some(invalid_len) = err.error_len() else {
                    return out;
                };
                out.push(char::REPLACEMENT_CHARACTER);
                rest = &after[invalid_len..];
            }
        }
    }
}

fn extract_step_idx(raw: &serde_json::Value) -> Option<u64> {
    if let Some(idx) = raw.get("stepIdx").and_then(serde_json::Value::as_u64) {
        return Some(idx);
    }
    raw.get("stepIdx")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<u64>().ok())
}

fn extract_artifact_dir_str(raw: &serde_json::Value) -> Option<&str> {
    let path = raw
        .get("artifactDirectoryPath")
        .and_then(serde_json::Value::as_str)?;
    let trimmed = path.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn artifact_dir_from_transcript(
    raw: &serde_json::Value,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    let transcript = raw
        .get("transcriptPath")
        .and_then(serde_json::Value::as_str)?;
    let resolved = resolve_artifact_dir(transcript, home_dir)?;
    let logs_dir = resolved.parent()?;
    let sys_dir = logs_dir.parent()?;
    if sys_dir.file_name()? != ".system_generated" {
        return None;
    }
    sys_dir.parent().map(Path::to_path_buf)
}

fn extract_tool_name(raw: &serde_json::Value) -> Option<&str> {
    raw.get("toolCall")
        .and_then(|tc| tc.get("name"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| raw.get("tool").and_then(serde_json::Value::as_str))
}

/// Enriches an Antigravity hook payload by reading output from the step's `output.txt`
/// if eligible and unpopulated. Returns `true` if `tool_response` was populated.
pub fn enrich_antigravity_step_output(
    raw: &mut serde_json::Value,
    event: HookEvent,
    home_dir: Option<&Path>,
) -> bool {
    if event != HookEvent::PostToolUse {
        return false;
    }
    if raw
        .get("tool_response")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        return false;
    }
    let Some(tool_name) = extract_tool_name(raw) else {
        return false;
    };
    if !is_output_eligible(tool_name) {
        return false;
    }
    let Some(step_idx) = extract_step_idx(raw) else {
        return false;
    };
    let artifact_dir = extract_artifact_dir_str(raw)
        .and_then(|p| resolve_artifact_dir(p, home_dir))
        .or_else(|| artifact_dir_from_transcript(raw, home_dir));
    let Some(artifact_dir) = artifact_dir else {
        return false;
    };
    let output_file = step_output_file_path(&artifact_dir, step_idx);
    let Some(output) = read_step_output(&output_file, MAX_ANTIGRAVITY_OUTPUT_BYTES) else {
        return false;
    };
    if let Some(obj) = raw.as_object_mut() {
        obj.insert("tool_response".into(), serde_json::Value::String(output));
        true
    } else {
        false
    }
}

/// Whether an Antigravity `Stop` closes a finished turn: the model ended it
/// on its own and nothing is still running in the background
/// (`fullyIdle`). The bundled hook reference spells the normal reason
/// `NO_TOOL_CALL` and the web page `model_stop`; both are accepted, in any
/// case. A cancelled, failed or budget-limited loop is not a checkpoint: the
/// next turn or an explicit `finalize-session` covers it.
#[must_use]
pub fn stop_is_turn_checkpoint(raw: &serde_json::Value) -> bool {
    let idle = raw.get("fullyIdle").and_then(serde_json::Value::as_bool) == Some(true);
    let failed = raw
        .get("error")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|error| !error.trim().is_empty());
    idle && !failed
        && raw
            .get("terminationReason")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_normal_termination)
}

fn is_normal_termination(reason: &str) -> bool {
    let reason = reason.trim();
    let reason = strip_prefix_ignore_ascii_case(reason, "TERMINATION_REASON_").unwrap_or(reason);
    reason.eq_ignore_ascii_case("NO_TOOL_CALL") || reason.eq_ignore_ascii_case("MODEL_STOP")
}

fn strip_prefix_ignore_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Antigravity's app data directories under `~/.gemini`, one per surface:
/// the desktop App, the CLI (`agy`) and the IDE.
pub const ANTIGRAVITY_APP_DATA_DIRS: [&str; 3] =
    ["antigravity", "antigravity-cli", "antigravity-ide"];

/// Transcript file names a `transcriptPath` may name. The docs name
/// `transcript.jsonl`; the CLI's payload points at `transcript_full.jsonl`
/// beside it (`docs/desktop/verification-2026-10.md`).
pub const ANTIGRAVITY_TRANSCRIPT_FILES: [&str; 2] = ["transcript.jsonl", "transcript_full.jsonl"];

/// Bytes of transcript one hook reads at most. Tool output makes most of the
/// file, so a turn's new prompt sits near where the previous read stopped;
/// a backlog is picked up by the following hooks.
pub const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 8 * 1024 * 1024;

/// Longest transcript line that is parsed. Longer lines (large tool output)
/// are skipped without being held in memory.
pub const MAX_TRANSCRIPT_LINE_BYTES: usize = 1024 * 1024;

/// Prompts one read returns at most; the cursor stops after the last one.
pub const MAX_TRANSCRIPT_PROMPTS: usize = 64;

/// Where the transcript reader resumes for one conversation. Persisted by
/// the caller between hook processes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptCursor {
    /// File name of the transcript the offset belongs to.
    #[serde(default)]
    pub file: Option<String>,
    /// Fingerprint of the transcript's first complete line. Antigravity only
    /// appends, so a different first line means the file was rewritten (a
    /// rewind), even when it did not get shorter.
    #[serde(default)]
    pub head: Option<String>,
    /// Byte offset just past the last line consumed.
    #[serde(default)]
    pub offset: u64,
    /// Highest `step_index` of a prompt already returned in the current
    /// file. It is cleared when the file is rewritten or truncated, because a
    /// rewind can reuse step numbers for new prompts; the prompts that file
    /// still holds come back once more and the server drops them by their
    /// content-derived ingest keys.
    #[serde(default)]
    pub last_step: Option<u64>,
    /// The previous read stopped inside a line longer than the scan budget;
    /// the next one discards the rest of it first.
    #[serde(default)]
    pub resync: bool,
    /// Prompts returned for this conversation so far.
    #[serde(default)]
    pub prompts: u64,
}

/// One user prompt recovered from a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPrompt {
    /// The step's `step_index`, when it has one.
    pub step_index: Option<u64>,
    /// Byte offset of the step's line, the identity of a step without an index.
    pub line_offset: u64,
    /// Prompt text, outside any `<USER_REQUEST>` wrapper.
    pub text: String,
    /// The step's `created_at`, when it is an RFC 3339 timestamp.
    pub created_at: Option<String>,
}

/// The prompts one read found and the cursor to persist after delivering them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRead {
    /// New prompts, in transcript order.
    pub prompts: Vec<TranscriptPrompt>,
    /// The cursor to store once `prompts` are delivered.
    pub cursor: TranscriptCursor,
}

/// The payload's `conversationId` when it is a plain id (ASCII
/// alphanumerics, `-`, `_`; Antigravity uses UUIDs). Anything else could
/// name another directory and yields `None`.
#[must_use]
pub fn antigravity_conversation_id(raw: &serde_json::Value) -> Option<&str> {
    let id = raw
        .get("conversationId")
        .and_then(serde_json::Value::as_str)?
        .trim();
    (!id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(id)
}

/// The transcript a payload's `transcriptPath` names, when it is the
/// conversation's own:
/// `~/.gemini/<app>/brain/<conversationId>/.system_generated/logs/<file>`
/// with `<app>` one of [`ANTIGRAVITY_APP_DATA_DIRS`] and `<file>` one of
/// [`ANTIGRAVITY_TRANSCRIPT_FILES`]. The containment check runs on
/// canonical paths, so a symlinked `brain/<id>`, `.system_generated` or
/// `logs` directory that leads elsewhere is refused, while a symlinked
/// `~/.gemini` (dotfile managers) still works. The file itself is checked
/// when it is opened. Returns the path under the canonical directory.
#[must_use]
pub fn antigravity_transcript_path(raw: &serde_json::Value, home: &Path) -> Option<PathBuf> {
    let conversation = antigravity_conversation_id(raw)?;
    let declared = raw
        .get("transcriptPath")
        .and_then(serde_json::Value::as_str)?;
    let path = resolve_artifact_dir(declared, Some(home))?;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    if !ANTIGRAVITY_TRANSCRIPT_FILES.contains(&name) {
        return None;
    }
    let logs = std::fs::canonicalize(path.parent()?).ok()?;
    ANTIGRAVITY_APP_DATA_DIRS.iter().find_map(|app| {
        let brain = std::fs::canonicalize(home.join(".gemini").join(app).join("brain")).ok()?;
        let expected = brain
            .join(conversation)
            .join(".system_generated")
            .join("logs");
        (logs == expected).then(|| expected.join(name))
    })
}

/// Open `path` only if it is a regular file, not a symlink, FIFO or device,
/// and is still the same file once open.
pub(crate) fn open_regular_file(path: &Path) -> Option<File> {
    let before = std::fs::symlink_metadata(path).ok()?;
    if !before.is_file() {
        return None;
    }
    let file = open_no_follow(path)?;
    let after = file.metadata().ok()?;
    if !after.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return None;
        }
    }
    // The directories were canonical when checked, but one swapped for a
    // symlink afterwards would still be followed by the open, and the inode
    // comparison above sees the same swapped file. The descriptor's own path
    // must be the one that was checked.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd as _;
        let opened = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()?;
        if opened != path {
            return None;
        }
    }
    Some(file)
}

/// Bytes of the first line that identify a transcript file.
const TRANSCRIPT_HEAD_BYTES: usize = 256;

/// Short fingerprint of the first complete line (or of its first
/// [`TRANSCRIPT_HEAD_BYTES`] when it is longer). `None` while the first line
/// is still being written, so a growing file is never mistaken for a
/// rewritten one. Leaves the file positioned at its start.
fn head_fingerprint(file: &mut File) -> std::io::Result<Option<String>> {
    use sha2::{Digest as _, Sha256};
    let mut buf = [0_u8; TRANSCRIPT_HEAD_BYTES];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    file.seek(SeekFrom::Start(0))?;
    let head = match buf[..filled].iter().position(|&b| b == b'\n') {
        Some(newline) => &buf[..newline],
        None if filled == buf.len() => &buf[..],
        None => return Ok(None),
    };
    let digest = format!("{:x}", Sha256::digest(head));
    Ok(Some(digest[..16].to_owned()))
}

/// `O_NOFOLLOW` refuses a final-component symlink swapped in after the
/// metadata check, and `O_NONBLOCK` keeps a FIFO swapped in from blocking the
/// open (and so the hook) indefinitely; reads of a regular file are
/// unaffected.
#[cfg(unix)]
fn open_no_follow(path: &Path) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> Option<File> {
    File::open(path).ok()
}

/// Read the user prompts written to `path` since `cursor`. At most
/// [`MAX_TRANSCRIPT_SCAN_BYTES`] are read and [`MAX_TRANSCRIPT_PROMPTS`]
/// returned; a line still being written is left for the next read. `None`
/// when the file is not a readable regular file.
#[must_use]
pub fn read_transcript_prompts(path: &Path, cursor: &TranscriptCursor) -> Option<TranscriptRead> {
    let name = path.file_name()?.to_str()?.to_owned();
    let mut file = open_regular_file(path)?;
    let len = file.metadata().ok()?.len();
    let head = head_fingerprint(&mut file).ok()?;
    let mut next = cursor.clone();
    let rewritten = matches!((&next.head, &head), (Some(old), Some(new)) if old != new);
    if next.file.as_deref() != Some(name.as_str()) || len < next.offset || rewritten {
        next.file = Some(name);
        next.offset = 0;
        next.resync = false;
        next.last_step = None;
    }
    if head.is_some() {
        next.head = head;
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    reader.seek(SeekFrom::Start(next.offset)).ok()?;
    let start = next.offset;
    let mut scanned = 0_u64;
    let mut prompts = Vec::new();
    if next.resync {
        let (consumed, found) =
            skip_through_newline(&mut reader, MAX_TRANSCRIPT_SCAN_BYTES).ok()?;
        scanned += consumed;
        next.offset += consumed;
        if !found {
            return Some(TranscriptRead {
                prompts,
                cursor: next,
            });
        }
        next.resync = false;
    }
    let mut line = Vec::new();
    while prompts.len() < MAX_TRANSCRIPT_PROMPTS {
        let line_offset = next.offset;
        let budget = MAX_TRANSCRIPT_SCAN_BYTES.saturating_sub(scanned);
        let consumed = match read_bounded_line(&mut reader, &mut line, budget).ok()? {
            LineRead::Eof => break,
            LineRead::Complete(consumed) => consumed,
            // A final line with no newline yet: take it only once it is a
            // whole JSON object, otherwise it is still being written.
            LineRead::Unterminated(consumed) => {
                if !serde_json::from_slice::<serde_json::Value>(&line)
                    .is_ok_and(|value| value.is_object())
                {
                    break;
                }
                consumed
            }
            LineRead::OutOfBudget(consumed) => {
                // A line longer than a whole read is skipped in pieces;
                // otherwise the next read starts on it with a full budget.
                if line_offset == start {
                    next.offset += consumed;
                    next.resync = true;
                }
                break;
            }
        };
        scanned += consumed;
        next.offset += consumed;
        let Some(prompt) = parse_transcript_prompt(&line, line_offset) else {
            continue;
        };
        if let (Some(step), Some(last)) = (prompt.step_index, next.last_step)
            && step <= last
        {
            continue;
        }
        if let Some(step) = prompt.step_index {
            next.last_step = Some(next.last_step.map_or(step, |last| last.max(step)));
        }
        next.prompts += 1;
        prompts.push(prompt);
    }
    Some(TranscriptRead {
        prompts,
        cursor: next,
    })
}

enum LineRead {
    Eof,
    /// A newline-terminated line; bytes consumed including the newline.
    Complete(u64),
    /// End of file inside a line.
    Unterminated(u64),
    /// The budget ran out inside a line.
    OutOfBudget(u64),
}

/// Read one line into `out` (without its newline). A line longer than
/// [`MAX_TRANSCRIPT_LINE_BYTES`] is consumed but leaves `out` empty.
fn read_bounded_line(
    reader: &mut impl BufRead,
    out: &mut Vec<u8>,
    budget: u64,
) -> std::io::Result<LineRead> {
    out.clear();
    let mut consumed = 0_u64;
    let mut oversized = false;
    loop {
        if consumed >= budget {
            return Ok(LineRead::OutOfBudget(consumed));
        }
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if oversized {
                out.clear();
            }
            return Ok(if consumed == 0 {
                LineRead::Eof
            } else {
                LineRead::Unterminated(consumed)
            });
        }
        let room = usize::try_from(budget - consumed).unwrap_or(usize::MAX);
        let window = &buf[..buf.len().min(room)];
        let newline = window.iter().position(|&b| b == b'\n');
        let chunk = &window[..newline.unwrap_or(window.len())];
        if !oversized {
            if out.len() + chunk.len() <= MAX_TRANSCRIPT_LINE_BYTES {
                out.extend_from_slice(chunk);
            } else {
                oversized = true;
                out.clear();
            }
        }
        let used = newline.map_or(window.len(), |at| at + 1);
        reader.consume(used);
        consumed += used as u64;
        if newline.is_some() {
            return Ok(LineRead::Complete(consumed));
        }
    }
}

/// Discard up to and including the next newline; returns the bytes consumed
/// and whether the newline was reached within `budget`.
fn skip_through_newline(reader: &mut impl BufRead, budget: u64) -> std::io::Result<(u64, bool)> {
    let mut consumed = 0_u64;
    while consumed < budget {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok((consumed, false));
        }
        let room = usize::try_from(budget - consumed).unwrap_or(usize::MAX);
        let window = &buf[..buf.len().min(room)];
        let newline = window.iter().position(|&b| b == b'\n');
        let used = newline.map_or(window.len(), |at| at + 1);
        reader.consume(used);
        consumed += used as u64;
        if newline.is_some() {
            return Ok((consumed, true));
        }
    }
    Ok((consumed, false))
}

/// A transcript line's prompt: a `USER_INPUT` step typed by the user
/// (`source` absent or `USER_EXPLICIT`, so steps a hook injected are left
/// out) with non-empty text.
fn parse_transcript_prompt(line: &[u8], line_offset: u64) -> Option<TranscriptPrompt> {
    // Most lines are tool steps; only parse the ones that can be a prompt.
    if !line
        .windows(b"USER_INPUT".len())
        .any(|window| window.eq_ignore_ascii_case(b"USER_INPUT"))
    {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let step = value.as_object()?;
    let kind = step.get("type")?.as_str()?.trim();
    if !(kind.eq_ignore_ascii_case("USER_INPUT")
        || kind.to_ascii_uppercase().ends_with("_USER_INPUT"))
    {
        return None;
    }
    if let Some(source) = step.get("source") {
        let source = source.as_str()?.trim().to_ascii_uppercase();
        if source != "USER_EXPLICIT" && !source.ends_with("_USER_EXPLICIT") {
            return None;
        }
    }
    let content = content_text(step.get("content")?)?;
    let text = user_request_text(&content).trim();
    if text.is_empty() {
        return None;
    }
    let step_index = step.get("step_index").and_then(|index| {
        index
            .as_u64()
            .or_else(|| index.as_str().and_then(|s| s.trim().parse().ok()))
    });
    let created_at = step
        .get("created_at")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|at| at.parse::<jiff::Timestamp>().is_ok())
        .map(str::to_owned);
    Some(TranscriptPrompt {
        step_index,
        line_offset,
        text: text.to_owned(),
        created_at,
    })
}

/// `content` as text: a string, or the strings and `text` fields of an
/// array or object of parts.
fn content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(part) => part
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        serde_json::Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|part| {
                    part.as_str()
                        .or_else(|| part.get("text").and_then(serde_json::Value::as_str))
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// The text inside `<USER_REQUEST>…</USER_REQUEST>` when the CLI's full
/// transcript wraps the prompt that way; the whole content otherwise.
fn user_request_text(content: &str) -> &str {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";
    let Some(start) = content.find(OPEN) else {
        return content;
    };
    let inner = &content[start + OPEN.len()..];
    inner.find(CLOSE).map_or(inner, |end| &inner[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_is_output_eligible() {
        // Arrange & Assert: Eligible tools (case-insensitive)
        assert!(is_output_eligible("run_command"));
        assert!(is_output_eligible("RUN_COMMAND"));
        assert!(is_output_eligible("view_file"));
        assert!(is_output_eligible("View_File"));
        assert!(is_output_eligible("list_dir"));
        assert!(is_output_eligible("find_by_name"));
        assert!(is_output_eligible("grep_search"));
        assert!(is_output_eligible("search_web"));
        assert!(is_output_eligible("manage_task"));
        assert!(is_output_eligible("manage_subagents"));

        // Arrange & Assert: Ineligible tools (edit tools, UI, media, unproven)
        assert!(!is_output_eligible("write_to_file"));
        assert!(!is_output_eligible("replace_file_content"));
        assert!(!is_output_eligible("multi_replace_file_content"));
        assert!(!is_output_eligible("generate_image"));
        assert!(!is_output_eligible("ask_question"));
        assert!(!is_output_eligible("schedule"));
        assert!(!is_output_eligible("read_url_content"));
        assert!(!is_output_eligible("read_resource"));
        assert!(!is_output_eligible("call_mcp_tool"));
        assert!(!is_output_eligible("unknown_tool"));
    }

    #[test]
    fn test_resolve_artifact_dir() {
        let fake_home = Path::new("/custom/home");

        // Act & Assert
        assert_eq!(
            resolve_artifact_dir("/abs/path", Some(fake_home)),
            Some(PathBuf::from("/abs/path"))
        );
        assert_eq!(
            resolve_artifact_dir("~/workspace/dir", Some(fake_home)),
            Some(PathBuf::from("/custom/home/workspace/dir"))
        );
        assert_eq!(
            resolve_artifact_dir("~", Some(fake_home)),
            Some(PathBuf::from("/custom/home"))
        );
        assert_eq!(resolve_artifact_dir("~/workspace/dir", None), None);
        assert_eq!(resolve_artifact_dir("", Some(fake_home)), None);
        assert_eq!(resolve_artifact_dir("   ", Some(fake_home)), None);
    }

    #[test]
    fn test_read_step_output_bounds_and_trims() {
        // Arrange
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("output.txt");
        let content = "Hello from output.txt   \n\n";
        fs::write(&file_path, content).unwrap();

        // Act
        let read = read_step_output(&file_path, 1024);

        // Assert
        assert_eq!(read, Some("Hello from output.txt".to_string()));

        // Act: max_bytes truncate
        let truncated = read_step_output(&file_path, 5);
        assert_eq!(truncated, Some("Hello".to_string()));
    }

    #[test]
    #[cfg(unix)]
    fn test_read_step_output_rejects_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let target = dir.path().join("real_output.txt");
        let link = dir.path().join("output.txt");
        fs::write(&target, "secret content").unwrap();
        symlink(&target, &link).unwrap();

        assert_eq!(read_step_output(&link, 1024), None);
    }

    #[test]
    fn test_read_step_output_handles_multibyte_utf8_truncation() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("output.txt");
        // "🦀" is 4 bytes. "abc🦀def" with limit 5 truncates inside the emoji.
        fs::write(&file_path, "abc🦀def").unwrap();

        let read = read_step_output(&file_path, 5);
        assert_eq!(read, Some("abc".to_string()));
    }

    /// Output that is not valid UTF-8 (cp1252 text from a Windows command,
    /// or binary data) must not lose everything after the first bad byte,
    /// nor all of it when the bad byte comes first.
    #[test]
    fn test_read_step_output_keeps_text_around_invalid_utf8() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("output.txt");

        fs::write(&file_path, b"ok\n\xff tail").unwrap();
        assert_eq!(
            read_step_output(&file_path, 1024),
            Some("ok\n\u{FFFD} tail".to_string())
        );

        fs::write(&file_path, b"\xe9chec: build failed").unwrap();
        assert_eq!(
            read_step_output(&file_path, 1024),
            Some("\u{FFFD}chec: build failed".to_string())
        );

        // An invalid byte and a code point cut by the cap in one read.
        let mut bytes = b"a\xffb".to_vec();
        bytes.extend_from_slice("🦀".as_bytes());
        fs::write(&file_path, &bytes).unwrap();
        assert_eq!(
            read_step_output(&file_path, 5),
            Some("a\u{FFFD}b".to_string())
        );
    }

    #[test]
    fn test_read_step_output_missing_or_empty() {
        // Arrange
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("empty.txt");
        fs::write(&file_path, "   \n\t").unwrap();

        // Act & Assert
        assert_eq!(read_step_output(&file_path, 1024), None);
        assert_eq!(
            read_step_output(&dir.path().join("nonexistent.txt"), 1024),
            None
        );
    }

    #[test]
    fn test_enrich_antigravity_step_output_success() {
        // Arrange
        let dir = tempdir().unwrap();
        let step_dir = dir.path().join(".system_generated").join("steps").join("5");
        fs::create_dir_all(&step_dir).unwrap();
        fs::write(
            step_dir.join("output.txt"),
            "npm test passed: 42 tests OK\n",
        )
        .unwrap();

        let mut payload = serde_json::json!({
            "toolCall": {
                "name": "run_command",
                "args": {
                    "CommandLine": "npm test"
                }
            },
            "stepIdx": 5,
            "artifactDirectoryPath": dir.path().to_str().unwrap()
        });

        // Act
        let enriched = enrich_antigravity_step_output(&mut payload, HookEvent::PostToolUse, None);

        // Assert
        assert!(enriched);
        assert_eq!(
            payload
                .get("tool_response")
                .and_then(serde_json::Value::as_str),
            Some("npm test passed: 42 tests OK")
        );
    }

    #[test]
    fn test_enrich_antigravity_step_output_falls_back_to_transcript_path() {
        // Arrange
        let dir = tempdir().unwrap();
        let logs_dir = dir.path().join(".system_generated").join("logs");
        let step_dir = dir
            .path()
            .join(".system_generated")
            .join("steps")
            .join("12");
        fs::create_dir_all(&logs_dir).unwrap();
        fs::create_dir_all(&step_dir).unwrap();
        let transcript = logs_dir.join("transcript.jsonl");
        fs::write(&transcript, "").unwrap();
        fs::write(step_dir.join("output.txt"), "found file content").unwrap();

        let mut payload = serde_json::json!({
            "toolCall": {
                "name": "view_file",
                "args": {"AbsolutePath": "/workspace/src/lib.rs"}
            },
            "stepIdx": "12",
            "transcriptPath": transcript.to_str().unwrap()
        });

        // Act
        let enriched = enrich_antigravity_step_output(&mut payload, HookEvent::PostToolUse, None);

        // Assert
        assert!(enriched);
        assert_eq!(
            payload
                .get("tool_response")
                .and_then(serde_json::Value::as_str),
            Some("found file content")
        );
    }

    #[test]
    fn test_enrich_antigravity_step_output_bypasses_ineligible_tools() {
        // Arrange: write_to_file must NEVER be enriched with output.txt
        let dir = tempdir().unwrap();
        let step_dir = dir.path().join(".system_generated").join("steps").join("1");
        fs::create_dir_all(&step_dir).unwrap();
        fs::write(step_dir.join("output.txt"), "Created file test.rs").unwrap();

        let mut payload = serde_json::json!({
            "toolCall": {
                "name": "write_to_file",
                "args": {
                    "TargetFile": "/workspace/test.rs",
                    "CodeContent": "fn main() {}"
                }
            },
            "stepIdx": 1,
            "artifactDirectoryPath": dir.path().to_str().unwrap()
        });

        // Act
        let enriched = enrich_antigravity_step_output(&mut payload, HookEvent::PostToolUse, None);

        // Assert
        assert!(!enriched);
        assert!(payload.get("tool_response").is_none());
    }

    #[test]
    fn test_enrich_antigravity_step_output_ignores_non_post_tool_use() {
        // Arrange
        let dir = tempdir().unwrap();
        let step_dir = dir.path().join(".system_generated").join("steps").join("1");
        fs::create_dir_all(&step_dir).unwrap();
        fs::write(step_dir.join("output.txt"), "output").unwrap();

        let mut payload = serde_json::json!({
            "toolCall": {
                "name": "run_command",
                "args": {"CommandLine": "ls"}
            },
            "stepIdx": 1,
            "artifactDirectoryPath": dir.path().to_str().unwrap()
        });

        // Act: PreToolUse
        let enriched = enrich_antigravity_step_output(&mut payload, HookEvent::PreToolUse, None);

        // Assert
        assert!(!enriched);
        assert!(payload.get("tool_response").is_none());
    }

    #[test]
    fn test_enrich_antigravity_step_output_does_not_overwrite_existing_response() {
        // Arrange
        let dir = tempdir().unwrap();
        let step_dir = dir.path().join(".system_generated").join("steps").join("1");
        fs::create_dir_all(&step_dir).unwrap();
        fs::write(step_dir.join("output.txt"), "from disk").unwrap();

        let mut payload = serde_json::json!({
            "toolCall": {
                "name": "run_command",
                "args": {"CommandLine": "ls"}
            },
            "stepIdx": 1,
            "artifactDirectoryPath": dir.path().to_str().unwrap(),
            "tool_response": "already populated"
        });

        // Act
        let enriched = enrich_antigravity_step_output(&mut payload, HookEvent::PostToolUse, None);

        // Assert
        assert!(!enriched);
        assert_eq!(
            payload
                .get("tool_response")
                .and_then(serde_json::Value::as_str),
            Some("already populated")
        );
    }

    #[test]
    fn stop_checkpoint_needs_a_normal_reason_and_a_fully_idle_agent() {
        let cases = [
            // Bundled hook reference spellings.
            (
                serde_json::json!({"terminationReason": "NO_TOOL_CALL", "fullyIdle": true}),
                true,
            ),
            (
                serde_json::json!({"terminationReason": "no_tool_call", "fullyIdle": true}),
                true,
            ),
            (
                serde_json::json!({"terminationReason": "TERMINATION_REASON_NO_TOOL_CALL", "fullyIdle": true}),
                true,
            ),
            // Web page spelling.
            (
                serde_json::json!({"terminationReason": "model_stop", "fullyIdle": true}),
                true,
            ),
            (
                serde_json::json!({"terminationReason": " MODEL_STOP ", "fullyIdle": true, "error": ""}),
                true,
            ),
            // Background work still running.
            (
                serde_json::json!({"terminationReason": "NO_TOOL_CALL", "fullyIdle": false}),
                false,
            ),
            (
                serde_json::json!({"terminationReason": "NO_TOOL_CALL"}),
                false,
            ),
            (
                serde_json::json!({"terminationReason": "NO_TOOL_CALL", "fullyIdle": "true"}),
                false,
            ),
            // Abnormal ends.
            (
                serde_json::json!({"terminationReason": "NO_TOOL_CALL", "fullyIdle": true, "error": "boom"}),
                false,
            ),
            (serde_json::json!({"fullyIdle": true}), false),
            (
                serde_json::json!({"terminationReason": 0, "fullyIdle": true}),
                false,
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(stop_is_turn_checkpoint(&raw), expected, "{raw}");
        }
        for reason in [
            "MAX_INVOCATIONS",
            "MAX_FORCED_INVOCATIONS",
            "MAX_TOKEN_BUDGET_EXCEEDED",
            "ERROR",
            "USER_CANCELED",
            "TERMINAL_STEP_TYPE",
            "TERMINAL_CUSTOM_HOOK",
            "INJECTED_RESPONSE",
            "max_steps_exceeded",
            "error",
            "NO_TOOL_CALLS",
            "",
        ] {
            let raw = serde_json::json!({"terminationReason": reason, "fullyIdle": true});
            assert!(!stop_is_turn_checkpoint(&raw), "{reason}");
        }
    }

    #[test]
    fn conversation_id_must_be_a_plain_path_segment() {
        let id = |value: serde_json::Value| {
            antigravity_conversation_id(&serde_json::json!({ "conversationId": value }))
                .map(str::to_owned)
        };
        assert_eq!(
            id("ec33ebf9-0cba-4100-8142-c61503f6c587".into()).as_deref(),
            Some("ec33ebf9-0cba-4100-8142-c61503f6c587")
        );
        assert_eq!(id(" conv_1 ".into()).as_deref(), Some("conv_1"));
        for bad in ["", "  ", "..", ".", "a/b", "a\\b", "../x", "c:x", "a b"] {
            assert_eq!(id(bad.into()), None, "{bad:?}");
        }
        assert_eq!(id("a".repeat(129).into()), None);
        assert_eq!(id(serde_json::json!(7)), None);
    }

    /// `<home>/.gemini/<app>/brain/<conversation>/.system_generated/logs/<file>`
    /// with `lines` written to it.
    fn write_transcript(
        home: &Path,
        app: &str,
        conversation: &str,
        file: &str,
        lines: &[&str],
    ) -> PathBuf {
        let logs = home
            .join(".gemini")
            .join(app)
            .join("brain")
            .join(conversation)
            .join(".system_generated")
            .join("logs");
        fs::create_dir_all(&logs).unwrap();
        let path = logs.join(file);
        let mut text = lines.join("\n");
        if !lines.is_empty() {
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
        path
    }

    fn payload_for(path: &Path, conversation: &str) -> serde_json::Value {
        serde_json::json!({
            "conversationId": conversation,
            "transcriptPath": path.to_str().unwrap(),
        })
    }

    fn canonical(path: &Path) -> PathBuf {
        fs::canonicalize(path.parent().unwrap())
            .unwrap()
            .join(path.file_name().unwrap())
    }

    #[test]
    fn transcript_path_accepts_each_surface_and_both_file_names() {
        let home = tempdir().unwrap();
        for app in ANTIGRAVITY_APP_DATA_DIRS {
            for file in ANTIGRAVITY_TRANSCRIPT_FILES {
                let path = write_transcript(home.path(), app, "conv-1", file, &[]);
                assert_eq!(
                    antigravity_transcript_path(&payload_for(&path, "conv-1"), home.path()),
                    Some(canonical(&path)),
                    "{app}/{file}"
                );
            }
        }
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-2",
            "transcript.jsonl",
            &[],
        );
        let tilde = serde_json::json!({
            "conversationId": "conv-2",
            "transcriptPath": "~/.gemini/antigravity/brain/conv-2/.system_generated/logs/transcript.jsonl",
        });
        assert_eq!(
            antigravity_transcript_path(&tilde, home.path()),
            Some(canonical(&path))
        );
    }

    /// Adversarial: `transcriptPath` is payload input. Anything but the
    /// named conversation's own transcript under the home's Antigravity
    /// data directories is refused, so a forged payload cannot make the hook
    /// read and upload another file.
    #[test]
    fn transcript_path_refuses_anything_outside_the_conversations_own_logs() {
        let home = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        let own = write_transcript(
            home.path(),
            "antigravity",
            "conv-a",
            "transcript.jsonl",
            &[],
        );
        let other = write_transcript(
            home.path(),
            "antigravity",
            "conv-b",
            "transcript.jsonl",
            &[],
        );
        // Same layout, but not under this home.
        let foreign = write_transcript(
            elsewhere.path(),
            "antigravity",
            "conv-a",
            "transcript.jsonl",
            &[],
        );
        // Same directory, other files.
        let logs = own.parent().unwrap();
        fs::write(logs.join("secrets.jsonl"), "{}\n").unwrap();
        // Another app data directory name.
        let unknown_app = write_transcript(
            home.path(),
            "antigravity-evil",
            "conv-a",
            "transcript.jsonl",
            &[],
        );

        let refused = [
            payload_for(&other, "conv-a"),
            payload_for(&foreign, "conv-a"),
            payload_for(&logs.join("secrets.jsonl"), "conv-a"),
            payload_for(&unknown_app, "conv-a"),
            payload_for(&logs.join("../logs/transcript.jsonl"), "conv-a"),
            payload_for(&own, "../conv-a"),
            payload_for(&own, ""),
            serde_json::json!({
                "conversationId": "conv-a",
                "transcriptPath": ".gemini/antigravity/brain/conv-a/.system_generated/logs/transcript.jsonl",
            }),
            serde_json::json!({ "conversationId": "conv-a" }),
            serde_json::json!({ "conversationId": "conv-a", "transcriptPath": 5 }),
        ];
        for raw in refused {
            assert_eq!(
                antigravity_transcript_path(&raw, home.path()),
                None,
                "{raw}"
            );
        }
        // Control: the conversation's own transcript is accepted.
        assert_eq!(
            antigravity_transcript_path(&payload_for(&own, "conv-a"), home.path()),
            Some(canonical(&own))
        );
    }

    #[test]
    #[cfg(unix)]
    fn transcript_path_refuses_symlinked_directories_that_leave_the_conversation() {
        use std::os::unix::fs::symlink;
        let home = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir_all(outside.path().join("logs")).unwrap();
        fs::write(outside.path().join("logs/transcript.jsonl"), "{}\n").unwrap();

        // `.system_generated` leads out of the conversation directory.
        let conv = home.path().join(".gemini/antigravity/brain/conv-a");
        fs::create_dir_all(&conv).unwrap();
        symlink(outside.path(), conv.join(".system_generated")).unwrap();
        let path = conv.join(".system_generated/logs/transcript.jsonl");
        assert_eq!(
            antigravity_transcript_path(&payload_for(&path, "conv-a"), home.path()),
            None
        );

        // `brain/<id>` itself leads elsewhere.
        let brain = home.path().join(".gemini/antigravity-cli/brain");
        fs::create_dir_all(&brain).unwrap();
        let target = outside.path().join("conv-b");
        fs::create_dir_all(target.join(".system_generated/logs")).unwrap();
        fs::write(
            target.join(".system_generated/logs/transcript.jsonl"),
            "{}\n",
        )
        .unwrap();
        symlink(&target, brain.join("conv-b")).unwrap();
        let path = brain.join("conv-b/.system_generated/logs/transcript.jsonl");
        assert_eq!(
            antigravity_transcript_path(&payload_for(&path, "conv-b"), home.path()),
            None
        );

        // Control: a symlinked `~/.gemini` (a dotfile manager) still works.
        let dotfiles = tempdir().unwrap();
        let real_home = tempdir().unwrap();
        let gemini = dotfiles.path().join("gemini");
        let logs = gemini.join("antigravity/brain/conv-c/.system_generated/logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("transcript.jsonl"), "").unwrap();
        symlink(&gemini, real_home.path().join(".gemini")).unwrap();
        let declared = real_home
            .path()
            .join(".gemini/antigravity/brain/conv-c/.system_generated/logs/transcript.jsonl");
        assert_eq!(
            antigravity_transcript_path(&payload_for(&declared, "conv-c"), real_home.path()),
            Some(fs::canonicalize(&logs).unwrap().join("transcript.jsonl"))
        );
    }

    fn user_input(step: u64, content: &str) -> String {
        serde_json::json!({
            "step_index": step,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "created_at": "2026-10-18T10:00:00Z",
            "content": content,
        })
        .to_string()
    }

    fn texts(read: &TranscriptRead) -> Vec<&str> {
        read.prompts.iter().map(|p| p.text.as_str()).collect()
    }

    /// A rewind truncates or rewrites the transcript and can reuse step
    /// numbers for new prompts: they must still be returned, whether the file
    /// got shorter or kept its length. An append is not a rewrite.
    #[test]
    fn transcript_rewrites_return_new_prompts_that_reuse_step_numbers() {
        let home = tempdir().unwrap();
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-rewind",
            "transcript.jsonl",
            &[
                &user_input(0, "first long prompt about the parser module"),
                &user_input(1, "second prompt"),
            ],
        );
        let first = read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap();
        assert_eq!(
            texts(&first),
            ["first long prompt about the parser module", "second prompt"]
        );

        // Control: an append returns only the new prompt.
        let mut appended = fs::read_to_string(&path).unwrap();
        appended.push_str(&format!("{}\n", user_input(2, "third prompt")));
        fs::write(&path, &appended).unwrap();
        let after_append = read_transcript_prompts(&path, &first.cursor).unwrap();
        assert_eq!(texts(&after_append), ["third prompt"]);

        // Shorter rewrite reusing step 0.
        fs::write(&path, format!("{}\n", user_input(0, "edited"))).unwrap();
        let shorter = read_transcript_prompts(&path, &after_append.cursor).unwrap();
        assert_eq!(texts(&shorter), ["edited"]);

        // Same-or-longer rewrite reusing steps 0 and 1.
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                user_input(0, "rewritten first prompt, now even longer than before"),
                user_input(1, "rewritten second prompt that keeps growing")
            ),
        )
        .unwrap();
        let longer = read_transcript_prompts(&path, &shorter.cursor).unwrap();
        assert_eq!(
            texts(&longer),
            [
                "rewritten first prompt, now even longer than before",
                "rewritten second prompt that keeps growing"
            ]
        );
    }

    #[test]
    fn reads_prompts_from_the_app_transcript_and_the_cli_full_transcript() {
        let home = tempdir().unwrap();
        let tool = serde_json::json!({
            "step_index": 2,
            "source": "MODEL",
            "type": "RUN_COMMAND",
            "content": "the word USER_INPUT inside tool output",
            "tool_calls": [{"name": "run_command"}],
        })
        .to_string();
        let app = write_transcript(
            home.path(),
            "antigravity",
            "conv-1",
            "transcript.jsonl",
            &[
                &user_input(0, "Fix the login bug"),
                &tool,
                &user_input(3, "Now add a test"),
            ],
        );
        let read = read_transcript_prompts(&app, &TranscriptCursor::default()).unwrap();
        assert_eq!(texts(&read), ["Fix the login bug", "Now add a test"]);
        assert_eq!(read.prompts[0].step_index, Some(0));
        assert_eq!(
            read.prompts[0].created_at.as_deref(),
            Some("2026-10-18T10:00:00Z")
        );
        assert_eq!(read.cursor.last_step, Some(3));
        assert_eq!(read.cursor.prompts, 2);
        assert_eq!(read.cursor.offset, fs::metadata(&app).unwrap().len());

        let wrapped = "<ADDITIONAL_METADATA>\nlocal time 10:00\n</ADDITIONAL_METADATA>\n\
                       <USER_REQUEST>\nRename the module\n</USER_REQUEST>";
        let cli = write_transcript(
            home.path(),
            "antigravity-cli",
            "conv-2",
            "transcript_full.jsonl",
            &[&user_input(0, wrapped)],
        );
        let read = read_transcript_prompts(&cli, &TranscriptCursor::default()).unwrap();
        assert_eq!(texts(&read), ["Rename the module"]);
    }

    #[test]
    fn skips_injected_malformed_empty_and_non_prompt_steps() {
        let home = tempdir().unwrap();
        let injected = serde_json::json!({
            "step_index": 1, "source": "HOOK", "type": "USER_INPUT", "content": "injected by a hook",
        })
        .to_string();
        let no_source = serde_json::json!({
            "step_index": 2, "type": "CORTEX_STEP_TYPE_USER_INPUT", "content": [{"text": "part one"}, "part two"],
        })
        .to_string();
        let string_index = serde_json::json!({
            "step_index": "7", "source": "USER_EXPLICIT", "type": "user_input",
            "created_at": "yesterday", "content": {"text": "object content"},
        })
        .to_string();
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-1",
            "transcript.jsonl",
            &[
                "{not json USER_INPUT",
                "",
                &injected,
                &user_input(3, "   "),
                &user_input(4, "<USER_REQUEST>  </USER_REQUEST>"),
                r#"{"type":"USER_INPUT","source":"USER_EXPLICIT","content":5}"#,
                r#"["USER_INPUT"]"#,
                &no_source,
                &string_index,
            ],
        );
        let read = read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap();
        assert_eq!(texts(&read), ["part one\npart two", "object content"]);
        assert_eq!(read.prompts[1].step_index, Some(7));
        assert_eq!(read.prompts[1].created_at, None);
    }

    #[test]
    fn cursor_resumes_waits_for_partial_lines_and_survives_rewrites() {
        let home = tempdir().unwrap();
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-1",
            "transcript.jsonl",
            &[&user_input(0, "first")],
        );
        let first = read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap();
        assert_eq!(texts(&first), ["first"]);
        let again = read_transcript_prompts(&path, &first.cursor).unwrap();
        assert!(again.prompts.is_empty(), "nothing new since the last read");

        // A line still being written is left for the next read.
        let second = user_input(1, "second");
        let (head, tail) = second.split_at(second.len() / 2);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut file, head.as_bytes()).unwrap();
        let partial = read_transcript_prompts(&path, &first.cursor).unwrap();
        assert!(partial.prompts.is_empty());
        assert_eq!(partial.cursor.offset, first.cursor.offset);
        std::io::Write::write_all(&mut file, tail.as_bytes()).unwrap();
        // Whole but not yet newline-terminated: taken once.
        let whole = read_transcript_prompts(&path, &partial.cursor).unwrap();
        assert_eq!(texts(&whole), ["second"]);
        std::io::Write::write_all(&mut file, b"\n").unwrap();
        std::io::Write::write_all(&mut file, user_input(2, "third").as_bytes()).unwrap();
        std::io::Write::write_all(&mut file, b"\n").unwrap();
        let next = read_transcript_prompts(&path, &whole.cursor).unwrap();
        assert_eq!(texts(&next), ["third"]);
        assert_eq!(next.cursor.prompts, 3);

        // Rewritten shorter: re-read from the start. A rewind can reuse step
        // numbers, so the file's prompts all come back; the ones already
        // delivered carry the same content-derived ingest key and the server
        // drops them.
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                user_input(2, "third"),
                user_input(5, "after rewrite")
            ),
        )
        .unwrap();
        let rewritten = read_transcript_prompts(&path, &next.cursor).unwrap();
        assert_eq!(texts(&rewritten), ["third", "after rewrite"]);

        // A cursor for the other transcript file starts over too.
        let mut other = rewritten.cursor.clone();
        other.file = Some("transcript_full.jsonl".into());
        other.last_step = None;
        let restarted = read_transcript_prompts(&path, &other).unwrap();
        assert_eq!(texts(&restarted), ["third", "after rewrite"]);
    }

    #[test]
    fn oversized_lines_are_skipped_without_losing_later_prompts() {
        let home = tempdir().unwrap();
        let huge = format!(
            r#"{{"step_index":1,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"{}"}}"#,
            "x".repeat(MAX_TRANSCRIPT_LINE_BYTES)
        );
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-1",
            "transcript.jsonl",
            &[&user_input(0, "before"), &huge, &user_input(2, "after")],
        );
        let read = read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap();
        assert_eq!(texts(&read), ["before", "after"]);
    }

    #[test]
    fn a_line_longer_than_the_scan_budget_is_skipped_across_reads() {
        let home = tempdir().unwrap();
        let budget = usize::try_from(MAX_TRANSCRIPT_SCAN_BYTES).unwrap();
        let giant = format!(
            r#"{{"type":"TOOL","content":"{}"}}"#,
            "y".repeat(budget + 1024)
        );
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-1",
            "transcript.jsonl",
            &[&giant, &user_input(1, "after the giant line")],
        );
        let first = read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap();
        assert!(first.prompts.is_empty());
        assert!(first.cursor.resync, "stopped inside the giant line");
        assert_eq!(first.cursor.offset, MAX_TRANSCRIPT_SCAN_BYTES);
        let second = read_transcript_prompts(&path, &first.cursor).unwrap();
        assert_eq!(texts(&second), ["after the giant line"]);
        assert!(!second.cursor.resync);
    }

    #[test]
    fn one_read_returns_a_bounded_number_of_prompts() {
        let home = tempdir().unwrap();
        let lines: Vec<String> = (0..MAX_TRANSCRIPT_PROMPTS as u64 + 6)
            .map(|step| user_input(step, &format!("prompt {step}")))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-1",
            "transcript.jsonl",
            &refs,
        );
        let first = read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap();
        assert_eq!(first.prompts.len(), MAX_TRANSCRIPT_PROMPTS);
        let second = read_transcript_prompts(&path, &first.cursor).unwrap();
        assert_eq!(second.prompts.len(), 6);
        assert_eq!(
            second.prompts[0].text,
            format!("prompt {MAX_TRANSCRIPT_PROMPTS}")
        );
    }

    #[test]
    #[cfg(unix)]
    fn transcript_reader_refuses_symlinks_and_special_files() {
        use std::os::unix::fs::symlink;
        let home = tempdir().unwrap();
        let secret = home.path().join("secret.jsonl");
        fs::write(&secret, format!("{}\n", user_input(0, "not a transcript"))).unwrap();
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-1",
            "transcript.jsonl",
            &[],
        );
        fs::remove_file(&path).unwrap();
        symlink(&secret, &path).unwrap();
        assert_eq!(
            read_transcript_prompts(&path, &TranscriptCursor::default()),
            None
        );
        assert_eq!(
            read_transcript_prompts(path.parent().unwrap(), &TranscriptCursor::default()),
            None
        );
        // Control: the same content as a regular file is read.
        fs::remove_file(&path).unwrap();
        fs::copy(&secret, &path).unwrap();
        assert_eq!(
            texts(&read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap()),
            ["not a transcript"]
        );
    }

    /// A FIFO named like the transcript must be refused without blocking the
    /// hook, and a directory swapped for a symlink after the containment check
    /// must not let the open reach a file outside the checked path.
    #[test]
    #[cfg(target_os = "linux")]
    fn transcript_reader_refuses_fifos_and_swapped_directories() {
        use std::os::unix::fs::symlink;
        let home = tempdir().unwrap();
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-fifo",
            "transcript.jsonl",
            &[],
        );
        fs::remove_file(&path).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let started = std::time::Instant::now();
        assert_eq!(
            read_transcript_prompts(&path, &TranscriptCursor::default()),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        // `.system_generated` swapped for a symlink to a foreign tree.
        let path = write_transcript(
            home.path(),
            "antigravity",
            "conv-swap",
            "transcript.jsonl",
            &[&user_input(0, "real prompt")],
        );
        let system_generated = path.parent().unwrap().parent().unwrap().to_path_buf();
        let foreign = home.path().join("foreign");
        fs::create_dir_all(foreign.join("logs")).unwrap();
        fs::write(
            foreign.join("logs").join("transcript.jsonl"),
            format!("{}\n", user_input(0, "foreign secret")),
        )
        .unwrap();
        // Control: before the swap the checked path is read.
        assert_eq!(
            texts(&read_transcript_prompts(&path, &TranscriptCursor::default()).unwrap()),
            ["real prompt"]
        );
        fs::remove_dir_all(&system_generated).unwrap();
        symlink(&foreign, &system_generated).unwrap();
        assert_eq!(
            read_transcript_prompts(&path, &TranscriptCursor::default()),
            None
        );
    }
}
