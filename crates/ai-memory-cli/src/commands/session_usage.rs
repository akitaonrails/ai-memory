//! Client-side session token-usage extraction (token-cost visibility).
//!
//! Reads a harness's own transcript file to compute cumulative token totals
//! for one session. Live capture reads bounded forward batches on model/tool boundaries.
//! The legacy full-session fallback runs at `session-end`; it stays off
//! the frequent `stop`/`post-tool-use` hot path. A
//! bounded linear scan (Claude Code) or a bounded tail read (Codex, whose
//! rollout files can reach hundreds of MB for a long session) is cheap
//! enough to run synchronously inline in the native hook before it spools
//! the session-end event. Best-effort throughout: any read/parse failure
//! yields `None`, never a wrong number, and a hook must never fail on its
//! account.
//!
//! Field names and the "sum by dedup'd message.id" / "take the last
//! cumulative record" strategies below were verified against real local
//! transcripts (a Claude Code JSONL and Codex rollout files up to 103 MB),
//! not just documentation.

use std::collections::HashSet;
use std::io::{BufRead as _, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

#[path = "session_usage_context.rs"]
mod response_context;

/// One bounded transcript batch. Publish its cursor only after the lifecycle
/// event has been durably spooled; a crash then causes safe response-id replay.
pub struct LiveCallBatch {
    /// Usage metadata to attach to this hook event.
    pub calls: Vec<ai_memory_core::TokenCall>,
    cursor_path: PathBuf,
    next: LiveCursor,
    /// Whether this read moved past any transcript bytes.
    advanced: bool,
    /// Cursors of the subagent transcripts whose responses joined `calls`.
    subagents: Vec<(PathBuf, LiveCursor)>,
}

impl LiveCallBatch {
    /// Persist progress after enqueue. Concurrent writers may rewind progress,
    /// but can never skip unspooled records; response ids make replay safe.
    pub fn commit(self) {
        if let Ok(bytes) = serde_json::to_vec(&self.next) {
            if ai_memory_wiki::write_atomic(&self.cursor_path, &bytes).is_err() {
                return;
            }
            // A subagent cursor that fails to write replays its responses.
            let mut written = Vec::with_capacity(self.subagents.len());
            for (path, next) in self.subagents {
                let Ok(bytes) = serde_json::to_vec(&next) else {
                    continue;
                };
                if ai_memory_wiki::write_atomic(&path, &bytes).is_ok() {
                    written.push(path);
                }
            }
            // Eviction only causes idempotent replay on the next hook.
            if let Some(dir) = self.cursor_path.parent()
                && let Ok(entries) = std::fs::read_dir(dir)
            {
                let mut cursors = entries
                    .take(1024)
                    .filter_map(Result::ok)
                    .filter(|e| {
                        e.path().extension().is_some_and(|s| s == "json")
                            && e.path() != self.cursor_path
                            && !written.contains(&e.path())
                    })
                    .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                    .collect::<Vec<_>>();
                cursors.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
                let keep = 511_usize.saturating_sub(written.len());
                for (_, stale) in cursors.into_iter().skip(keep) {
                    let _ = std::fs::remove_file(stale);
                }
            }
        }
    }

    /// Add the responses of the subagents a Claude Code session ran.
    ///
    /// The session transcript holds only the main conversation; each
    /// subagent's responses live in its own transcript beside it (see
    /// `claude_subagent_transcripts`). Each one keeps its own cursor and
    /// commits with this batch. They join only a batch that already holds a
    /// main-conversation response, because the server takes the session's
    /// model from a batch's first response, and the batch stays within
    /// `LIVE_BATCH_CALLS`, the most the server reads from one event.
    #[must_use]
    pub fn with_claude_subagents(
        mut self,
        transcript_path: &Path,
        data_dir: &Path,
        routing_key: &str,
    ) -> Self {
        if self.calls.is_empty() {
            return self;
        }
        for transcript in claude_subagent_transcripts(transcript_path, LIVE_SUBAGENT_TRANSCRIPTS) {
            let room = LIVE_BATCH_CALLS.saturating_sub(self.calls.len());
            if room == 0 {
                break;
            }
            let Some(batch) = read_live_calls(
                &transcript,
                ai_memory_core::AgentKind::ClaudeCode,
                data_dir,
                routing_key,
                room,
            ) else {
                continue;
            };
            if batch.advanced {
                self.calls.extend(batch.calls);
                self.subagents.push((batch.cursor_path, batch.next));
            }
        }
        self
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct LiveCursor {
    offset: u64,
    previous: Option<[u64; 4]>,
    model: Option<String>,
    prefix: String,
    anchor: String,
    skipping_long_line: bool,
    #[serde(default)]
    context: response_context::ScanContext,
}

const LIVE_BATCH_CALLS: usize = 256;
const LIVE_CURSOR_BYTES: u64 = 64 * 1024;
/// Most subagent transcripts one live hook reads, newest first. One already
/// read to its end costs a cursor check and no transcript bytes.
const LIVE_SUBAGENT_TRANSCRIPTS: usize = 64;

fn fingerprint(file: &mut std::fs::File, start: u64, len: u64) -> std::io::Result<String> {
    use sha2::{Digest as _, Sha256};
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Read forward in bounded batches, including after a capture gap. The routing
/// key includes server, project and native session so progress is never shared
/// between independent destinations. No transcript text is stored in the cursor.
pub fn extract_live_calls(
    path: &Path,
    agent: ai_memory_core::AgentKind,
    data_dir: &Path,
    routing_key: &str,
) -> Option<LiveCallBatch> {
    read_live_calls(path, agent, data_dir, routing_key, LIVE_BATCH_CALLS)
}

/// [`extract_live_calls`], stopping at `max_calls` responses.
fn read_live_calls(
    path: &Path,
    agent: ai_memory_core::AgentKind,
    data_dir: &Path,
    routing_key: &str,
    max_calls: usize,
) -> Option<LiveCallBatch> {
    use sha2::{Digest as _, Sha256};
    let key = serde_json::to_vec(&(path, agent.as_str(), routing_key)).ok()?;
    let cursor_path = data_dir
        .join("hook-state")
        .join("token-usage")
        .join(format!("{:x}.json", Sha256::digest(key)));
    let mut cursor = std::fs::File::open(&cursor_path)
        .ok()
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(LIVE_CURSOR_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            (bytes.len() as u64 <= LIVE_CURSOR_BYTES)
                .then(|| serde_json::from_slice::<LiveCursor>(&bytes).ok())
                .flatten()
        })
        .unwrap_or_default();
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let prefix = fingerprint(&mut file, 0, len.min(64)).ok()?;
    let anchor_start = cursor.offset.saturating_sub(64);
    if cursor.offset > len
        || cursor.prefix != prefix
        || cursor.anchor
            != fingerprint(&mut file, anchor_start, cursor.offset - anchor_start).ok()?
    {
        cursor = LiveCursor::default();
    }
    file.seek(SeekFrom::Start(cursor.offset)).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(CODEX_TAIL_SCAN_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let mut result = std::collections::BTreeMap::<String, ai_memory_core::TokenCall>::new();
    let mut order = Vec::new();
    let mut previous = if cursor.offset == 0 {
        Some([0; 4])
    } else {
        cursor.previous
    };
    let mut model = cursor.model.take();
    let mut consumed = 0;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        if !line.ends_with(b"\n") {
            // Retry an incomplete record. An oversized line is skipped in
            // bounded chunks, rather than blocking every subsequent record.
            if consumed == 0 && bytes.len() as u64 == CODEX_TAIL_SCAN_BYTES {
                cursor.skipping_long_line = true;
                cursor.context = response_context::ScanContext::default();
                previous = None;
                model = None;
                consumed += line.len();
            }
            break;
        }
        consumed += line.len();
        if cursor.skipping_long_line {
            cursor.skipping_long_line = false;
            continue;
        }
        if result.len() >= max_calls {
            consumed -= line.len();
            break;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            cursor.context = response_context::ScanContext::default();
            continue;
        };
        if let Some(update) = cursor.context.observe(&v, agent) {
            if !result.contains_key(&update.call_id) {
                order.push(update.call_id.clone());
            }
            result
                .entry(update.call_id.clone())
                .and_modify(|old| response_context::merge_context(old, &update))
                .or_insert(update);
        }
        if v.get("type").and_then(|v| v.as_str()) == Some("turn_context") {
            model = v
                .pointer("/payload/model")
                .and_then(|v| v.as_str())
                .filter(|s| s.len() <= 512)
                .map(str::to_owned);
        }
        let (id, usage, current_model, codex) = if agent == ai_memory_core::AgentKind::ClaudeCode {
            if v.get("type").and_then(|v| v.as_str()) != Some("assistant")
                || v.get("isMeta").and_then(|v| v.as_bool()) == Some(true)
            {
                continue;
            }
            let Some(id) = v.pointer("/message/id").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(u) = v.pointer("/message/usage") else {
                continue;
            };
            (
                id.to_owned(),
                u.clone(),
                v.pointer("/message/model")
                    .and_then(|v| v.as_str())
                    .filter(|s| s.len() <= 512)
                    .map(str::to_owned),
                false,
            )
        } else {
            let total = v
                .pointer("/payload/info/total_token_usage")
                .or_else(|| v.pointer("/payload/thread_token_usage"));
            let Some(total) = total else { continue };
            let cached = u64_field(total, "cached_input_tokens");
            let totals = [
                u64_field(total, "input_tokens").saturating_sub(cached),
                u64_field(total, "output_tokens"),
                u64_field(total, "cache_write_input_tokens"),
                cached,
            ];
            let id = format!(
                "codex:{}:{}:{}:{}",
                totals[0], totals[1], totals[2], totals[3]
            );
            let usage = if let Some(last) = v
                .pointer("/payload/info/last_token_usage")
                .or_else(|| v.pointer("/payload/usage"))
            {
                last.clone()
            } else if let Some(old) = previous {
                serde_json::json!({"input_tokens":totals[0].saturating_sub(old[0])+totals[3].saturating_sub(old[3]),"output_tokens":totals[1].saturating_sub(old[1]),"cache_write_input_tokens":totals[2].saturating_sub(old[2]),"cached_input_tokens":totals[3].saturating_sub(old[3])})
            } else {
                previous = Some(totals);
                continue;
            };
            // A duplicate cumulative checkpoint is not a second response.
            if previous == Some(totals) {
                continue;
            }
            previous = Some(totals);
            (id, usage, model.clone(), true)
        };
        if id.len() > 128 {
            continue;
        }
        let cached = u64_field(
            &usage,
            if codex {
                "cached_input_tokens"
            } else {
                "cache_read_input_tokens"
            },
        );
        let input = u64_field(&usage, "input_tokens");
        let call = ai_memory_core::TokenCall {
            context: Some(cursor.context.for_call(&v, agent)),
            call_id: id.clone(),
            input_tokens: if codex {
                input.saturating_sub(cached)
            } else {
                input
            },
            output_tokens: u64_field(&usage, "output_tokens"),
            cache_write_tokens: u64_field(
                &usage,
                if codex {
                    "cache_write_input_tokens"
                } else {
                    "cache_creation_input_tokens"
                },
            ),
            cache_read_tokens: cached,
            model: current_model,
        };
        if call
            .input_tokens
            .saturating_add(call.output_tokens)
            .saturating_add(call.cache_read_tokens)
            .saturating_add(call.cache_write_tokens)
            == 0
        {
            continue;
        }
        if !result.contains_key(&id) {
            order.push(id.clone());
        }
        cursor.context.remember(&call);
        result
            .entry(id)
            .and_modify(|old| {
                response_context::merge_context(old, &call);
                old.input_tokens = old.input_tokens.max(call.input_tokens);
                old.output_tokens = old.output_tokens.max(call.output_tokens);
                old.cache_write_tokens = old.cache_write_tokens.max(call.cache_write_tokens);
                old.cache_read_tokens = old.cache_read_tokens.max(call.cache_read_tokens);
            })
            .or_insert(call);
    }
    cursor.offset += consumed as u64;
    cursor.previous = previous;
    cursor.model = model;
    cursor.prefix = prefix;
    let anchor_start = cursor.offset.saturating_sub(64);
    cursor.anchor = fingerprint(&mut file, anchor_start, cursor.offset - anchor_start).ok()?;
    Some(LiveCallBatch {
        calls: order
            .into_iter()
            .filter_map(|id| result.remove(&id))
            .collect(),
        cursor_path,
        next: cursor,
        advanced: consumed > 0,
        subagents: Vec::new(),
    })
}

/// Cumulative token usage for one session, as reported by the harness's own
/// transcript. An extractor returning `None` means "could not determine",
/// never "zero" — callers must not report a zeroed `SessionUsage`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionUsage {
    /// Fresh (non-cached) input tokens, summed/read from the transcript.
    pub input_tokens: u64,
    /// Output tokens, including any reasoning/thinking tokens the harness
    /// reports as part of the same total (verified against real data: both
    /// Claude Code's `output_tokens_details.thinking_tokens` and Codex's
    /// `reasoning_output_tokens` are a breakdown of `output_tokens`, not
    /// additive to it).
    pub output_tokens: u64,
    /// Tokens written to a prompt cache.
    pub cache_write_tokens: u64,
    /// Tokens served from a prompt cache.
    pub cache_read_tokens: u64,
    /// Last-seen model name, when the transcript records one.
    pub model: Option<String>,
}

/// Refuse to scan a Claude Code transcript past this size. Guards against a
/// pathological file; real sessions are far smaller than this.
const CLAUDE_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Most subagent transcripts a full-session Claude Code scan sums.
const MAX_SUBAGENT_TRANSCRIPTS: usize = 256;

/// How far from the end of a Codex rollout file to scan for the most recent
/// `token_usage_record`. Codex writes one after nearly every turn, so this
/// window holds one in any session that has had at least one model turn,
/// while staying bounded for the multi-hundred-MB rollout files a long
/// session produces (observed up to 103 MB locally).
const CODEX_TAIL_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// Sum `message.usage` across every non-meta assistant record in a Claude
/// Code JSONL transcript, deduplicating by `message.id`. Verified against a
/// real 2096-line/531-assistant-record local transcript: Claude Code's own
/// writer emits the same finalized message twice in a row (streaming, then
/// finalized) with byte-identical usage on every duplicate observed —
/// summing without the dedup roughly doubled the true total there.
///
/// The session's subagent transcripts are summed with it, since the session
/// transcript holds none of their responses. They share its
/// `CLAUDE_MAX_BYTES` budget and its `message.id` dedup, and the model stays
/// the main conversation's.
#[must_use]
pub fn extract_claude_code_usage(transcript_path: &Path) -> Option<SessionUsage> {
    let file = std::fs::File::open(transcript_path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut budget = CLAUDE_MAX_BYTES.checked_sub(len)?;
    let mut totals = ClaudeCodeTotals::default();
    let model = totals.add(std::io::BufReader::new(file));
    for transcript in claude_subagent_transcripts(transcript_path, MAX_SUBAGENT_TRANSCRIPTS) {
        let Ok(file) = std::fs::File::open(&transcript) else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        let Some(rest) = budget.checked_sub(metadata.len()) else {
            continue;
        };
        budget = rest;
        totals.add(std::io::BufReader::new(file));
    }
    totals.usage.model = model;
    totals.any.then_some(totals.usage)
}

/// Responses summed across the transcripts of one Claude Code session.
#[derive(Default)]
struct ClaudeCodeTotals {
    usage: SessionUsage,
    seen_ids: HashSet<String>,
    any: bool,
}

impl ClaudeCodeTotals {
    /// Add one transcript's responses, skipping ids already counted. Returns
    /// the last model it names.
    fn add(&mut self, reader: impl std::io::BufRead) -> Option<String> {
        let mut model = None;
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if line.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if value.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
                continue;
            }
            if value.get("isMeta").and_then(serde_json::Value::as_bool) == Some(true) {
                continue;
            }
            let Some(message) = value.get("message") else {
                continue;
            };
            let Some(id) = message.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if !self.seen_ids.insert(id.to_owned()) {
                continue;
            }
            let Some(usage) = message.get("usage") else {
                continue;
            };
            self.usage.input_tokens += u64_field(usage, "input_tokens");
            self.usage.output_tokens += u64_field(usage, "output_tokens");
            self.usage.cache_write_tokens += u64_field(usage, "cache_creation_input_tokens");
            self.usage.cache_read_tokens += u64_field(usage, "cache_read_input_tokens");
            if let Some(name) = message.get("model").and_then(serde_json::Value::as_str) {
                model = Some(name.to_owned());
            }
            self.any = true;
        }
        model
    }
}

/// The subagent transcripts of a Claude Code session, newest first.
///
/// Claude Code keeps the main conversation in `<session>.jsonl` and writes
/// each subagent's to `<session>/subagents/agent-<id>.jsonl`, the
/// `agent_transcript_path` its `SubagentStop` hook receives
/// (<https://code.claude.com/docs/en/hooks#subagentstop>). Only regular files
/// count, at most `limit` of them.
fn claude_subagent_transcripts(transcript_path: &Path, limit: usize) -> Vec<PathBuf> {
    let Some(session) = transcript_path.file_stem() else {
        return Vec::new();
    };
    let dir = transcript_path.with_file_name(session).join("subagents");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut transcripts = entries
        .take(1024)
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("agent-") && name.ends_with(".jsonl")
        })
        .filter_map(|entry| {
            // `DirEntry::metadata` does not follow a symlink.
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some((metadata.modified().ok(), entry.path()))
        })
        .collect::<Vec<_>>();
    transcripts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    transcripts.truncate(limit);
    transcripts.into_iter().map(|(_, path)| path).collect()
}

/// Read the most recent legacy usage record or modern `token_count` event
/// from a Codex rollout JSONL file. Its total usage is already cumulative
/// for this rollout file, so — unlike Claude Code — this needs no summing,
/// only the latest record. Scans backward from a bounded tail window rather
/// than the whole file, since a long Codex session's rollout can reach
/// hundreds of megabytes (observed locally); the window may begin mid-line,
/// but that partial first line simply fails to parse as JSON and is
/// skipped, which is safe because every LATER (more complete) line in an
/// append-only file this process never writes concurrently to is intact.
#[must_use]
pub fn extract_codex_usage(transcript_path: &Path) -> Option<SessionUsage> {
    let mut file = std::fs::File::open(transcript_path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(CODEX_TAIL_SCAN_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let record_type = value.get("type").and_then(serde_json::Value::as_str);
        let modern = record_type == Some("event_msg")
            && value
                .pointer("/payload/type")
                .and_then(serde_json::Value::as_str)
                == Some("token_count");
        if record_type != Some("token_usage_record") && !modern {
            continue;
        }
        let Some(usage) = value
            .pointer("/payload/thread_token_usage")
            .or_else(|| value.pointer("/payload/info/total_token_usage"))
        else {
            continue;
        };
        return Some(SessionUsage {
            input_tokens: u64_field(usage, "input_tokens")
                .saturating_sub(u64_field(usage, "cached_input_tokens")),
            // `reasoning_output_tokens` is a breakdown of `output_tokens`
            // (verified against real data: input_tokens + output_tokens ==
            // total_tokens exactly), not additive to it.
            output_tokens: u64_field(usage, "output_tokens"),
            cache_write_tokens: u64_field(usage, "cache_write_input_tokens"),
            cache_read_tokens: u64_field(usage, "cached_input_tokens"),
            // Not carried on this record; left unset rather than guessed.
            model: None,
        });
    }
    None
}

fn u64_field(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
        .min(1_000_000_000)
}

/// Bound how many rollout files a Codex session-id lookup will stat before
/// giving up, mirroring the scan caps used elsewhere in this codebase
/// (`MAX_SCAN_FILES` in `ai-memory-workstream`) — a install with years of
/// rollout history must not turn a session-end hook into an unbounded walk.
const MAX_CODEX_ROLLOUT_SCAN: usize = 20_000;

/// Locate a Codex rollout file by the session id ai-memory's own hooks
/// receive. Codex names each rollout `rollout-<timestamp>-<uuid>.jsonl`
/// under `~/.codex/sessions/<year>/<month>/<day>/`; the trailing UUID is
/// the same id Codex's hook payloads send as `session_id` (verified: the
/// server-visible session id for a real local Codex session matched the
/// UUID suffix of that session's own rollout file, not the different
/// cross-resume id recorded inside `token_usage_record.payload.session_id`
/// on files from an earlier, resumed thread).
#[must_use]
pub fn locate_codex_rollout(codex_home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let suffix = format!("-{session_id}.jsonl");
    let root = codex_home.join("sessions");
    // UUIDv7 carries the creation date; most current Codex sessions can be
    // found in one day directory instead of walking years of rollouts.
    if let Ok(id) = uuid::Uuid::parse_str(session_id)
        && id.get_version_num() == 7
    {
        let mut ms = 0_i64;
        for byte in &id.as_bytes()[..6] {
            ms = (ms << 8) | i64::from(*byte);
        }
        if let Ok(time) = jiff::Timestamp::from_millisecond(ms) {
            let day = root.join(
                time.to_zoned(jiff::tz::TimeZone::UTC)
                    .strftime("%Y/%m/%d")
                    .to_string(),
            );
            if let Ok(entries) = std::fs::read_dir(day) {
                for entry in entries.take(MAX_CODEX_ROLLOUT_SCAN).flatten() {
                    if entry.file_name().to_string_lossy().ends_with(&suffix) {
                        return Some(entry.path());
                    }
                }
            }
        }
    }
    let mut scanned = 0usize;
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if scanned >= MAX_CODEX_ROLLOUT_SCAN {
                return None;
            }
            scanned += 1;
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Locate a Claude Code transcript file by session id and cwd, for a
/// session other than the current one (client-side backfill-close of an
/// abandoned session — the current session's own
/// `session-end` payload already carries `transcript_path` directly, so
/// this is only needed for a *different*, earlier session). Claude Code
/// stores each session at `<claude_config_dir>/projects/<flattened
/// cwd>/<session_id>.jsonl`, flattening `cwd` by turning every character
/// outside `[A-Za-z0-9]` into `-`.
#[must_use]
pub fn locate_claude_code_transcript(
    claude_config_dir: &Path,
    cwd: &str,
    session_id: &str,
) -> Option<PathBuf> {
    if cwd.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let flattened = ai_memory_workstream::claude_project_dir_name(Path::new(cwd));
    let path = claude_config_dir
        .join("projects")
        .join(flattened)
        .join(format!("{session_id}.jsonl"));
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_context_pairs_tools_across_committed_hooks_and_resets_unknown_turns() {
        use ai_memory_core::{AgentKind, TokenToolRelation};
        use serde_json::json;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("context.jsonl");
        let rows = [
            json!({"type":"user","uuid":"user-turn-1","message":{"content":[{"type":"text","text":"PRIVATE_PROMPT"}]}}),
            json!({"type":"assistant","parentUuid":"user-turn-1","timestamp":"2026-10-01T12:00:00Z","message":{"id":"response-1","model":"model-1","usage":{"input_tokens":100,"output_tokens":5},"content":[{"type":"tool_use","id":"tool-1","name":"Read","input":{"path":"PRIVATE_PATH"}}]}}),
        ];
        let data = rows.iter().map(|v| format!("{v}\n")).collect::<String>();
        std::fs::write(&path, &data).unwrap();
        let first = extract_live_calls(&path, AgentKind::ClaudeCode, tmp.path(), "ctx").unwrap();
        let c = first.calls[0].context.as_ref().unwrap();
        assert_eq!(c.turn_id.as_deref(), Some("user-turn-1"));
        assert_eq!(c.response_id.as_deref(), Some("response-1"));
        assert_eq!(c.tools[0].relation, TokenToolRelation::Requested);
        assert_eq!(c.recorded_at.as_deref(), Some("2026-10-01T12:00:00Z"));
        first.commit();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        writeln!(file,"{}",json!({"type":"user","uuid":"result-parent","message":{"content":[{"type":"tool_result","tool_use_id":"tool-1","is_error":true,"content":"PRIVATE_OUTPUT"}]}})).unwrap();
        writeln!(file,"{}",json!({"type":"assistant","parentUuid":"result-parent","message":{"id":"response-2","usage":{"input_tokens":200,"output_tokens":7}}})).unwrap();
        let next = extract_live_calls(&path, AgentKind::ClaudeCode, tmp.path(), "ctx").unwrap();
        assert_eq!(next.calls.len(), 2);
        let old = next
            .calls
            .iter()
            .find(|c| c.call_id == "response-1")
            .unwrap();
        assert_eq!(old.context.as_ref().unwrap().tools[0].is_error, Some(true));
        let input = next
            .calls
            .iter()
            .find(|c| c.call_id == "response-2")
            .unwrap();
        assert_eq!(
            input.context.as_ref().unwrap().tools[0].relation,
            TokenToolRelation::Input
        );
        let serialized = serde_json::to_string(&next.next).unwrap();
        assert!(
            !serialized.contains("PRIVATE_"),
            "Cursor must never retain prompt, tool arguments or output"
        );
        next.commit();
        writeln!(file, "malformed gap").unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"assistant","message":{"id":"response-3","usage":{"input_tokens":3}}})
        )
        .unwrap();
        let after = extract_live_calls(&path, AgentKind::ClaudeCode, tmp.path(), "ctx").unwrap();
        assert!(after.calls[0].context.as_ref().unwrap().turn_id.is_none());
        assert!(after.calls[0].context.as_ref().unwrap().tools.is_empty());
    }

    #[test]
    fn live_codex_context_uses_native_coordinates_and_only_turn_association() {
        use ai_memory_core::{AgentKind, TokenToolRelation};
        use serde_json::json;
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("codex-context.jsonl");
        let context = json!({"type":"turn_context","payload":{"turn_id":"native-turn","model":"codex-model"}});
        let usage = json!({"type":"token_usage","timestamp":"2026-10-01T12:00:00Z","payload":{"turn_id":"native-turn","response_id":"native-response","thread_token_usage":{"input_tokens":50,"output_tokens":10},"usage":{"input_tokens":50,"output_tokens":10}}});
        std::fs::write(&path, format!("{context}\n{usage}\n")).unwrap();
        let first = extract_live_calls(&path, AgentKind::Codex, tmp.path(), "ctx").unwrap();
        assert_eq!(first.calls.len(), 1);
        assert_eq!(
            first.calls[0]
                .context
                .as_ref()
                .unwrap()
                .response_id
                .as_deref(),
            Some("native-response")
        );
        first.commit();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file,"{}",json!({"type":"response_item","payload":{"type":"function_call","call_id":"exec-1","name":"exec_command","arguments":"PRIVATE_ARGUMENTS"}})).unwrap();
        let update = extract_live_calls(&path, AgentKind::Codex, tmp.path(), "ctx").unwrap();
        assert_eq!(update.calls.len(), 1);
        let tool = &update.calls[0].context.as_ref().unwrap().tools[0];
        assert_eq!(tool.relation, TokenToolRelation::Turn);
        assert_eq!(tool.name, "exec_command");
        update.commit();
        writeln!(
            file,
            "{}",
            json!({"type":"turn_context","payload":{"model":"no-turn"}})
        )
        .unwrap();
        let mut unknown = usage;
        unknown["payload"]
            .as_object_mut()
            .unwrap()
            .remove("turn_id");
        unknown["payload"]["thread_token_usage"]["input_tokens"] = json!(100);
        writeln!(file, "{unknown}").unwrap();
        let after = extract_live_calls(&path, AgentKind::Codex, tmp.path(), "ctx").unwrap();
        assert!(after.calls[0].context.as_ref().unwrap().turn_id.is_none());
        assert!(after.calls[0].context.as_ref().unwrap().tools.is_empty());
    }

    #[test]
    fn live_token_calls_merge_streaming_usage_and_skip_partial_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            format!(
                "broken prefix\n{}\n{}\n{}\n{{partial",
                claude_line("response-a", 50, 5, 10, 100),
                claude_line("response-a", 50, 20, 10, 100),
                claude_line("response-b", 70, 30, 0, 200)
            ),
        )
        .unwrap();
        let calls = extract_live_calls(
            &path,
            ai_memory_core::AgentKind::ClaudeCode,
            tmp.path(),
            "fixture",
        )
        .unwrap()
        .calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].call_id, "response-b");
        assert_eq!(calls[0].output_tokens, 20);
    }

    #[test]
    fn live_token_codex_reads_modern_usage_without_counting_cached_input_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        let record=serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{
            "total_token_usage":{"input_tokens":1000,"output_tokens":40,"cached_input_tokens":800},
            "last_token_usage":{"input_tokens":1000,"output_tokens":40,"cached_input_tokens":800,"reasoning_output_tokens":30}
        }}}).to_string();
        std::fs::write(&path, format!("{record}\n{record}\n")).unwrap();
        let calls = extract_live_calls(
            &path,
            ai_memory_core::AgentKind::Codex,
            tmp.path(),
            "fixture",
        )
        .unwrap()
        .calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input_tokens, 200);
        assert_eq!(calls[0].output_tokens, 40);
        assert_eq!(calls[0].cache_read_tokens, 800);
        assert_eq!(extract_codex_usage(&path).unwrap().input_tokens, 200);
    }

    #[test]
    fn live_token_cursor_replays_until_commit_then_recovers_every_response() {
        for agent in [
            ai_memory_core::AgentKind::ClaudeCode,
            ai_memory_core::AgentKind::Codex,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("transcript.jsonl");
            let mut text = String::from(
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"cursor-model\"}}\n",
            );
            for n in 1..=300 {
                let line = if agent == ai_memory_core::AgentKind::ClaudeCode {
                    claude_line(&format!("r-{n}"), 100, 20, 0, 10)
                } else {
                    serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100*n,"output_tokens":20*n,"cached_input_tokens":80*n}}}}).to_string()
                };
                text.push_str(&line);
                text.push('\n');
            }
            std::fs::write(&path, text).unwrap();
            let read =
                || extract_live_calls(&path, agent, tmp.path(), "server/project/session").unwrap();
            let first = read();
            assert_eq!(first.calls.len(), 256);
            assert_eq!(
                read().calls[0].call_id,
                first.calls[0].call_id,
                "Uncommitted data must replay"
            );
            first.commit();
            let rest = read();
            assert_eq!(rest.calls.len(), 44);
            if agent == ai_memory_core::AgentKind::Codex {
                assert!(rest.calls.iter().all(|c| c.input_tokens == 20
                    && c.output_tokens == 20
                    && c.cache_read_tokens == 80
                    && c.model.as_deref() == Some("cursor-model")));
            }
            rest.commit();
            assert!(read().calls.is_empty());
            assert_eq!(
                extract_live_calls(&path, agent, tmp.path(), "other/project/session")
                    .unwrap()
                    .calls
                    .len(),
                256,
                "Routes must not share progress"
            );
            std::fs::write(
                &path,
                format!("{}\n", claude_line("replacement", 1, 2, 0, 0)),
            )
            .unwrap();
            if agent == ai_memory_core::AgentKind::ClaudeCode {
                assert_eq!(read().calls[0].call_id, "replacement");
            }
        }
    }

    #[test]
    fn live_token_cursor_crosses_byte_windows_and_retries_partial_usage() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        // Ordinary large records cross the 4 MiB read window; none may be
        // skipped merely because it was outside the latest tail.
        for n in 0..3 {
            writeln!(file, "{}", claude_line(&format!("before-{n}"), 10, 1, 0, 0)).unwrap();
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"tool","body":"x".repeat(2*1024*1024)})
            )
            .unwrap();
        }
        let last = claude_line("after-gap", 20, 5, 0, 0);
        write!(file, "{last}").unwrap();
        drop(file);
        let read = || {
            extract_live_calls(
                &path,
                ai_memory_core::AgentKind::ClaudeCode,
                tmp.path(),
                "route",
            )
            .unwrap()
        };
        let mut ids = std::collections::HashSet::new();
        for _ in 0..4 {
            let batch = read();
            ids.extend(batch.calls.iter().map(|c| c.call_id.clone()));
            batch.commit();
        }
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains("after-gap"));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file).unwrap();
        let batch = read();
        assert_eq!(batch.calls[0].call_id, "after-gap");
        batch.commit();
        writeln!(file, "{}", claude_line("after-gap", 20, 25, 0, 0)).unwrap();
        let batch = read();
        assert_eq!(batch.calls[0].output_tokens, 25);
        batch.commit();
        assert!(read().calls.is_empty());
    }

    fn claude_line(
        msg_id: &str,
        input: u64,
        output: u64,
        cache_write: u64,
        cache_read: u64,
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "id": msg_id,
                "model": "claude-sonnet-5",
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": cache_write,
                    "cache_read_input_tokens": cache_read,
                    "output_tokens_details": {"thinking_tokens": output / 2}
                }
            }
        })
        .to_string()
    }

    #[test]
    fn claude_code_sums_usage_and_dedups_by_message_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        // The same finalized message written twice (streaming then final),
        // exactly as observed live — the duplicate must not double-count.
        let body = format!(
            "{}\n{}\n{}\n",
            claude_line("msg_1", 2, 298, 67828, 0),
            claude_line("msg_1", 2, 298, 67828, 0),
            claude_line("msg_2", 2, 188, 371, 67828),
        );
        std::fs::write(&path, body).unwrap();

        let usage = extract_claude_code_usage(&path).unwrap();
        assert_eq!(usage.input_tokens, 4);
        assert_eq!(usage.output_tokens, 486);
        assert_eq!(usage.cache_write_tokens, 68199);
        assert_eq!(usage.cache_read_tokens, 67828);
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-5"));
    }

    #[test]
    fn claude_code_ignores_meta_and_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let body = format!(
            "not json\n{}\n{{\"type\":\"assistant\",\"isMeta\":true,\"message\":{{\"id\":\"m\",\"usage\":{{\"input_tokens\":999}}}}}}\n{}\n",
            claude_line("real", 5, 5, 0, 0),
            "",
        );
        std::fs::write(&path, body).unwrap();

        let usage = extract_claude_code_usage(&path).unwrap();
        assert_eq!(
            usage.input_tokens, 5,
            "isMeta and unparseable lines must be skipped"
        );
    }

    #[test]
    fn claude_code_missing_file_is_none_not_zero() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(extract_claude_code_usage(&tmp.path().join("nope.jsonl")).is_none());
    }

    /// A subagent line as Claude Code writes it to
    /// `<session>/subagents/agent-<id>.jsonl`, on another model.
    fn subagent_line(msg_id: &str, input: u64, output: u64) -> String {
        serde_json::json!({
            "type": "assistant",
            "isSidechain": true,
            "agentId": "a1",
            "message": {
                "id": msg_id,
                "model": "claude-haiku-5",
                "usage": {"input_tokens": input, "output_tokens": output}
            }
        })
        .to_string()
    }

    /// #2031: the session transcript holds none of its subagents' responses,
    /// so the full-session sum reads `<session>/subagents/agent-*.jsonl` too.
    #[test]
    fn claude_code_sums_the_sessions_subagent_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let main_1 = claude_line("main-1", 10, 20, 30, 40);
        std::fs::write(&path, format!("{main_1}\n")).unwrap();
        let subagents = tmp.path().join("session").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let sub_1 = subagent_line("sub-1", 1, 2);
        // `main-1` again: a response both transcripts recorded counts once.
        std::fs::write(
            subagents.join("agent-a1.jsonl"),
            format!("{sub_1}\n{main_1}\n"),
        )
        .unwrap();
        let sub_2 = subagent_line("sub-2", 3, 4);
        std::fs::write(subagents.join("agent-a2.jsonl"), format!("{sub_2}\n")).unwrap();
        // Claude Code's metadata file beside a transcript is not one.
        let metadata = subagent_line("not-a-transcript", 99, 99);
        std::fs::write(subagents.join("agent-a1.json"), format!("{metadata}\n")).unwrap();
        // Another session's subagents stay out.
        let other = tmp.path().join("other").join("subagents");
        std::fs::create_dir_all(&other).unwrap();
        let foreign = subagent_line("foreign", 99, 99);
        std::fs::write(other.join("agent-b1.jsonl"), format!("{foreign}\n")).unwrap();

        let usage = extract_claude_code_usage(&path).unwrap();
        assert_eq!(usage.input_tokens, 14);
        assert_eq!(usage.output_tokens, 26);
        assert_eq!(usage.cache_write_tokens, 30);
        assert_eq!(usage.cache_read_tokens, 40);
        assert_eq!(
            usage.model.as_deref(),
            Some("claude-sonnet-5"),
            "the session keeps the main conversation's model"
        );
    }

    /// One live read of a Claude Code session, its subagents included.
    fn read_claude_session(path: &Path, data_dir: &Path) -> LiveCallBatch {
        use ai_memory_core::AgentKind;
        extract_live_calls(path, AgentKind::ClaudeCode, data_dir, "key")
            .unwrap()
            .with_claude_subagents(path, data_dir, "key")
    }

    /// #2031: live batches carry the subagents' responses behind the main
    /// conversation's, each subagent transcript with its own cursor.
    #[test]
    fn live_token_calls_add_subagent_responses_beside_a_main_response() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let main_1 = claude_line("main-1", 10, 20, 0, 0);
        std::fs::write(&path, format!("{main_1}\n")).unwrap();
        let subagents = tmp.path().join("session").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let subagent = subagents.join("agent-a1.jsonl");
        let sub_1 = subagent_line("sub-1", 1, 2);
        let sub_2 = subagent_line("sub-2", 3, 4);
        std::fs::write(&subagent, format!("{sub_1}\n{sub_2}\n")).unwrap();
        let read = || read_claude_session(&path, tmp.path());
        let ids = |batch: &LiveCallBatch| {
            batch
                .calls
                .iter()
                .map(|call| call.call_id.clone())
                .collect::<Vec<_>>()
        };

        let first = read();
        assert_eq!(ids(&first), ["main-1", "sub-1", "sub-2"]);
        assert_eq!(first.calls[1].model.as_deref(), Some("claude-haiku-5"));
        assert_eq!(ids(&read()), ids(&first), "uncommitted data must replay");
        first.commit();
        assert!(read().calls.is_empty());

        // Without a new main response, a subagent response waits for one.
        let sub_3 = subagent_line("sub-3", 5, 6);
        std::fs::write(&subagent, format!("{sub_1}\n{sub_2}\n{sub_3}\n")).unwrap();
        let waiting = read();
        assert!(waiting.calls.is_empty());
        waiting.commit();
        let main_2 = claude_line("main-2", 10, 20, 0, 0);
        std::fs::write(&path, format!("{main_1}\n{main_2}\n")).unwrap();
        let next = read();
        assert_eq!(ids(&next), ["main-2", "sub-3"]);
        next.commit();
        assert!(read().calls.is_empty());
    }

    /// #2031: the server reads at most `LIVE_BATCH_CALLS` responses from one
    /// event, so subagent responses past that wait for the next one.
    #[test]
    fn live_token_calls_keep_subagent_responses_within_one_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let main = |count: u32| {
            (1..=count)
                .map(|n| claude_line(&format!("main-{n}"), 1, 1, 0, 0) + "\n")
                .collect::<String>()
        };
        std::fs::write(&path, main(200)).unwrap();
        let subagents = tmp.path().join("session").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let sub = (1..=100)
            .map(|n| subagent_line(&format!("sub-{n}"), 1, 1) + "\n")
            .collect::<String>();
        std::fs::write(subagents.join("agent-a1.jsonl"), sub).unwrap();

        let first = read_claude_session(&path, tmp.path());
        assert_eq!(first.calls.len(), LIVE_BATCH_CALLS);
        assert_eq!(first.calls[200].call_id, "sub-1");
        first.commit();
        std::fs::write(&path, main(201)).unwrap();
        let rest = read_claude_session(&path, tmp.path());
        assert_eq!(rest.calls.len(), 45);
        assert_eq!(rest.calls[0].call_id, "main-201");
        assert_eq!(rest.calls[1].call_id, "sub-57");
    }

    fn codex_record(input: u64, output: u64, cache_write: u64, cached: u64) -> String {
        serde_json::json!({
            "type": "token_usage_record",
            "payload": {
                "thread_token_usage": {
                    "input_tokens": input,
                    "cached_input_tokens": cached,
                    "cache_write_input_tokens": cache_write,
                    "output_tokens": output,
                    "reasoning_output_tokens": output / 3,
                    "total_tokens": input + output,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn codex_takes_the_last_cumulative_record() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        let body = format!(
            "{}\n{{\"type\":\"response_item\",\"payload\":{{}}}}\n{}\n",
            codex_record(26852, 145, 0, 4864),
            codex_record(2_027_386, 1411, 0, 1_792_256),
        );
        std::fs::write(&path, body).unwrap();

        let usage = extract_codex_usage(&path).unwrap();
        assert_eq!(usage.input_tokens, 235_130);
        assert_eq!(
            usage.output_tokens, 1411,
            "reasoning tokens are a breakdown of output_tokens, not additive"
        );
        assert_eq!(usage.cache_read_tokens, 1_792_256);
    }

    #[test]
    fn codex_scans_only_the_tail_window() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rollout.jsonl");
        // Padding well past the tail-scan window, followed by the real
        // record: proves the extractor never needed to read the padding.
        let padding = "x".repeat((CODEX_TAIL_SCAN_BYTES as usize) * 3);
        let body = format!("{padding}\n{}\n", codex_record(100, 50, 0, 10));
        std::fs::write(&path, body).unwrap();

        let usage = extract_codex_usage(&path).unwrap();
        assert_eq!(usage.input_tokens, 90);
    }

    #[test]
    fn codex_missing_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(extract_codex_usage(&tmp.path().join("nope.jsonl")).is_none());
    }

    #[test]
    fn locate_codex_rollout_finds_nested_file_by_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let day_dir = tmp.path().join("sessions/2026/09/22");
        std::fs::create_dir_all(&day_dir).unwrap();
        let target =
            day_dir.join("rollout-2026-09-22T17-39-48-01a0cad8-9932-7433-9c14-97b62a6d5049.jsonl");
        std::fs::write(&target, "").unwrap();
        // A sibling file with a different id must not match.
        std::fs::write(
            day_dir.join("rollout-2026-09-22T10-00-00-deadbeef-0000-0000-0000-000000000000.jsonl"),
            "",
        )
        .unwrap();

        let found =
            locate_codex_rollout(tmp.path(), "01a0cad8-9932-7433-9c14-97b62a6d5049").unwrap();
        assert_eq!(found, target);
    }

    #[test]
    fn locate_codex_rollout_rejects_unsafe_session_ids() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(locate_codex_rollout(tmp.path(), "").is_none());
        assert!(locate_codex_rollout(tmp.path(), "../../etc/passwd").is_none());
        assert!(locate_codex_rollout(tmp.path(), "has spaces").is_none());
    }

    #[test]
    fn locate_codex_rollout_missing_session_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions/2026/01/01")).unwrap();
        assert!(locate_codex_rollout(tmp.path(), "no-such-session").is_none());
    }

    #[test]
    fn locate_claude_code_transcript_uses_claudes_project_folder_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let session_id = "9fc72c68-53ac-4d50-82aa-135f7ab03b7d";
        for (cwd, folder) in [
            (
                "/home/me/src/github.com/acme/api",
                "-home-me-src-github-com-acme-api",
            ),
            ("/home/me/my_app", "-home-me-my-app"),
            ("/home/me/my repo", "-home-me-my-repo"),
            ("/home/me/café", "-home-me-caf-"),
            ("/home/me/😀proj", "-home-me---proj"),
            (r"C:\Users\me\my.repo", "C--Users-me-my-repo"),
        ] {
            let project_dir = tmp.path().join("projects").join(folder);
            std::fs::create_dir_all(&project_dir).unwrap();
            let target = project_dir.join(format!("{session_id}.jsonl"));
            std::fs::write(&target, "").unwrap();

            assert_eq!(
                locate_claude_code_transcript(tmp.path(), cwd, session_id),
                Some(target),
                "{cwd}"
            );
        }
    }

    #[test]
    fn locate_claude_code_transcript_rejects_unsafe_input_and_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(locate_claude_code_transcript(tmp.path(), "", "sid").is_none());
        assert!(locate_claude_code_transcript(tmp.path(), "/proj", "").is_none());
        assert!(locate_claude_code_transcript(tmp.path(), "/proj", "../../etc/passwd").is_none());
        assert!(locate_claude_code_transcript(tmp.path(), "/no/such/proj", "sid").is_none());
    }
}
