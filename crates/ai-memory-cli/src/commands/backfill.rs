//! `ai-memory backfill` — one-time import of a project's pre-hook local history.
//!
//! Capture is forward-only: hooks record events only from the moment they are
//! installed. If you have worked in a project for weeks and only now install
//! ai-memory, the tool that exists for cross-session continuity starts
//! amnesiac about the very session you are resuming. This command fills that
//! first-boot gap: when the project's store is **brand new (empty)**, it
//! imports the existing local harness transcript history once.
//!
//! For each local native session it **replays the transcript through `/hook`**,
//! the same ingress live capture uses (as the companion importer does for
//! external conversations):
//!
//! - `export_transcript` (ai-memory-workstream) reads the harness's native
//!   transcript read-only into a normalized, bounded visible-event ledger,
//!   doing all the per-harness parsing in one place;
//! - each event is posted to `/hook/batch` as a session-start, `user-prompt`,
//!   or backfill-extension observation, attributed to the original harness and
//!   its native session id — so it becomes a real session + observations that
//!   consolidate into pages and are searchable via `memory_query`, exactly like
//!   live capture. The server **sanitizes and bounds every event**, so
//!   retroactive text crosses the same trust boundary as live capture.
//!
//! (An earlier revision imported through the managed-workstream begin/finish
//! endpoints, but that populates the `ai-memory run` continuity ledger, not the
//! searchable memory pipeline — verified by a live smoke where the observation
//! count stayed zero. `/hook` replay is what makes the history recall-able.)
//!
//! Safety: the automatic path only ever bootstraps an **empty** project (never
//! overwrites an established one — hook capture from install-time forward and
//! backfill of before-install history do not overlap), and it runs at most once
//! per checkout (a local sentinel).

use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use ai_memory_core::{NewWorkstreamEvent, WorkstreamEventKind};
use ai_memory_workstream::{
    ManagedHarness, build_launch_plan, export_transcript, list_native_sessions,
    wait_for_transcript_flush,
};

use super::doctor::{SCANNED_HARNESSES, relocated_session_dir};
use super::run;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json, post_json};

/// Circuit breaker on total imported events across the whole bootstrap. Once
/// reached, no further sessions are started; the remaining sessions are
/// reported as skipped rather than partially imported.
const MAX_EVENTS_TOTAL: usize = 50_000;

/// Newest local sessions to enumerate per harness before applying `--max-sessions`.
const PER_HARNESS_SCAN_LIMIT: usize = 200;

/// One `POST /hook/batch` request carries at most this many events — matches the
/// server's `MAX_HOOK_BATCH_ITEMS` so a batch is never rejected for size.
const HOOK_BATCH_ITEMS: usize = 256;

/// The extension label backfilled non-user events (assistant/tool) are recorded
/// under, mirroring the companion importer's replay-through-`/hook` mechanism.
const BACKFILL_EXTENSION: &str = "ai-memory-backfill";

/// A local native session eligible for import.
#[derive(Debug, Clone)]
pub(crate) struct SessionRef {
    pub(crate) harness: ManagedHarness,
    pub(crate) native_session_id: String,
    pub(crate) updated_at: SystemTime,
}

/// The outcome of a backfill run, and the JSON output shape.
#[derive(Debug, Default, Serialize)]
struct BackfillReport {
    workspace: String,
    project: String,
    /// Sessions selected for import (after `--max-sessions`).
    selected: usize,
    /// Sessions actually imported.
    imported_sessions: usize,
    /// Events imported across all sessions.
    imported_events: usize,
    /// Sessions skipped because the total-events circuit breaker tripped.
    skipped_for_cap: usize,
    /// Per-session failures (import errors); the run continues past them.
    failed_sessions: usize,
    /// True when the store was already populated (or `--force` off) and nothing
    /// was imported.
    skipped_non_empty: bool,
    /// True for a `--dry-run` (planning only, no import).
    dry_run: bool,
}

/// The captured side: `GET /admin/sessions/by-agent` — summing its counts tells
/// us whether the project store is empty.
#[derive(Debug, Deserialize)]
struct ByAgentResponse {
    by_agent: Vec<AgentCount>,
}

#[derive(Debug, Deserialize)]
struct AgentCount {
    #[allow(dead_code)]
    agent: String,
    sessions: u64,
}

/// Newest-first, then cap to `max_sessions`. Pure, so the selection/cap policy
/// is unit-tested without a server.
pub(crate) fn select_sessions(mut all: Vec<SessionRef>, max_sessions: usize) -> Vec<SessionRef> {
    all.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.native_session_id.cmp(&b.native_session_id))
    });
    all.truncate(max_sessions);
    all
}

/// Filename under `<data_dir>/backfill-state/` marking that the automatic
/// backfill has already been attempted for this checkout on this machine, so
/// the SessionStart trigger does not re-spawn on every boot. Keyed by a hash of
/// the working directory — which both the hook (before it spawns) and the
/// worker (which inherits the hook's cwd) compute identically without resolving
/// scope — so arbitrary paths stay path-safe.
pub(crate) fn sentinel_path(data_dir: &Path, cwd: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    data_dir.join("backfill-state").join(format!("{digest:x}"))
}

fn write_sentinel(data_dir: &Path, cwd: &Path) {
    let path = sentinel_path(data_dir, cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Best-effort: a missing sentinel only means a redundant future probe, not
    // incorrect data, so a write failure is not worth failing the import over.
    let _ = std::fs::write(&path, b"");
}

/// Run the backfill.
///
/// # Errors
/// Returns an error when the scope cannot be resolved, the working directory
/// cannot be read, the server is unreachable for the emptiness check, or any
/// selected session fails to import. The report is emitted before import errors
/// are returned, including in JSON mode.
pub async fn run(config: &Config, args: crate::cli::BackfillArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving the current working directory")?;

    // The automatic (SessionStart-spawned) path honors the opt-out and records
    // that it has been attempted for this checkout, so it runs at most once per
    // machine regardless of outcome. Manual runs ignore both.
    if args.auto && !config.backfill_on_start {
        if !args.dry_run {
            write_sentinel(&config.data_dir, &cwd);
        }
        return Ok(());
    }

    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let home = run::native_home(config).context("locating the local harness session stores")?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;

    let mut report = BackfillReport {
        workspace: workspace.clone(),
        project: project.clone(),
        dry_run: args.dry_run,
        ..BackfillReport::default()
    };

    // Emptiness gate. Only bootstrap a brand-new store; an established project
    // is never retro-imported (that would duplicate hook-captured history).
    if !args.force {
        let empty = project_is_empty(&endpoint, &workspace, &project).await?;
        if !empty {
            report.skipped_non_empty = true;
            // Mark attempted so the auto-trigger stops probing this checkout.
            if !args.dry_run {
                write_sentinel(&config.data_dir, &cwd);
            }
            return finish(&args, &report);
        }
    }

    // Enumerate local sessions for this cwd across every supported harness.
    let (candidates, _limit_hit) =
        collect_local_sessions(&home, &cwd, args.session.as_deref()).await;
    let selected = select_sessions(candidates, args.max_sessions.max(1));
    report.selected = selected.len();

    if args.dry_run {
        return finish(&args, &report);
    }

    // Import each selected session, oldest-first so the imported history reads
    // chronologically, until the total-events circuit breaker trips.
    for session in selected.into_iter().rev() {
        if report.imported_events >= MAX_EVENTS_TOTAL {
            report.skipped_for_cap += 1;
            continue;
        }
        match import_one(&endpoint, &workspace, &project, &home, &cwd, &session).await {
            Ok(events) => {
                report.imported_sessions += 1;
                report.imported_events += events;
            }
            Err(error) => {
                report.failed_sessions += 1;
                // Quiet suppresses the success summary, not failures: the
                // detached worker's stderr is the operator's diagnostic log.
                eprintln!(
                    "ai-memory: backfill of {} session {} failed: {error:#}",
                    session.harness.as_str(),
                    display_id(&session.native_session_id)
                );
            }
        }
    }

    write_sentinel(&config.data_dir, &cwd);
    finish(&args, &report)?;
    if report.failed_sessions > 0 {
        bail!(
            "backfill failed to import {} of {} selected session(s)",
            report.failed_sessions,
            report.selected
        );
    }
    Ok(())
}

/// Sum the server's per-agent session counts for this scope; zero means empty.
async fn project_is_empty(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
) -> Result<bool> {
    match get_json::<ByAgentResponse>(
        endpoint,
        "/admin/sessions/by-agent",
        &[("workspace", workspace), ("project", project)],
    )
    .await
    {
        Ok(response) => Ok(response.by_agent.iter().map(|c| c.sessions).sum::<u64>() == 0),
        // A project that has never been written to does not exist server-side
        // yet, so the no-create lookup answers 404 — which is the strongest
        // possible "empty", and the common case for a first-time backfill.
        Err(error) if super::is_scope_not_found(&error) => Ok(true),
        Err(error) => Err(error).with_context(|| {
            format!("checking whether {workspace}/{project} already has captured sessions")
        }),
    }
}

/// Enumerate local native sessions for `cwd` across every scanned harness.
/// Read-only; a harness whose store is absent/unreadable contributes nothing.
///
/// Returns, alongside the sessions, the harnesses whose scan came back at
/// exactly [`PER_HARNESS_SCAN_LIMIT`] — a caller that cares about missing
/// older sessions (as opposed to `backfill`'s own newest-first + cap
/// selection, which does not) can surface that.
pub(crate) async fn collect_local_sessions(
    home: &Path,
    cwd: &Path,
    only_session: Option<&str>,
) -> (Vec<SessionRef>, Vec<ManagedHarness>) {
    collect_local_sessions_with(home, cwd, only_session, relocated_session_dir).await
}

/// [`collect_local_sessions`] with the relocation lookup passed in, for the
/// same reason as `doctor::scan_local_with`: tests pass `|_| None` so a
/// developer's `CLAUDE_CONFIG_DIR` cannot hide a fixture planted under a
/// temporary `$HOME`.
pub(crate) async fn collect_local_sessions_with(
    home: &Path,
    cwd: &Path,
    only_session: Option<&str>,
    session_dir_for: impl Fn(ManagedHarness) -> Option<PathBuf>,
) -> (Vec<SessionRef>, Vec<ManagedHarness>) {
    let mut out = Vec::new();
    let mut limit_hit = Vec::new();
    for &harness in SCANNED_HARNESSES {
        let session_dir = session_dir_for(harness);
        let Ok(sessions) = list_native_sessions(
            harness,
            home,
            cwd,
            session_dir.as_deref(),
            PER_HARNESS_SCAN_LIMIT,
        )
        .await
        else {
            continue;
        };
        if sessions.len() >= PER_HARNESS_SCAN_LIMIT {
            limit_hit.push(harness);
        }
        for session in sessions {
            if only_session.is_some_and(|want| want != session.native_session_id) {
                continue;
            }
            out.push(SessionRef {
                harness,
                native_session_id: session.native_session_id,
                updated_at: session.updated_at,
            });
        }
    }
    (out, limit_hit)
}

/// One item in a `POST /hook/batch` request: the full hook URL (whose query the
/// server parses for event/agent/scope/session) plus the JSON body.
#[derive(Debug, Serialize)]
struct HookItem {
    url: String,
    body: serde_json::Value,
}

/// The server's `/hook/batch` acknowledgement (subset we act on).
#[derive(Debug, Deserialize)]
struct HookBatchAck {
    /// Contiguous leading prefix committed, oldest-first.
    accepted: usize,
}

/// Import one native session by replaying its transcript through `/hook`, the
/// same ingress live capture uses — so the events become real sanitized
/// observations that consolidate into pages and are searchable, attributed to
/// the original harness and its native session id. Returns the number of
/// content events (messages/tools) imported. The server sanitizes and bounds
/// every event, so retroactive text crosses the same trust boundary as live
/// capture.
async fn import_one(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    home: &Path,
    cwd: &Path,
    session: &SessionRef,
) -> Result<usize> {
    let session_dir = build_launch_plan(session.harness, None, Vec::new(), None)
        .ok()
        .and_then(|plan| plan.session_dir);
    // These are historical sessions, so the flush wait is a quick no-op; ignore
    // its result and read whatever is on disk.
    let _ = wait_for_transcript_flush(
        session.harness,
        home,
        cwd,
        session_dir.as_deref(),
        &session.native_session_id,
    )
    .await;
    let transcript = export_transcript(
        session.harness,
        home,
        cwd,
        session_dir.as_deref(),
        &session.native_session_id,
        None,
    )
    .await
    .with_context(|| format!("reading the {} transcript", session.harness.as_str()))?;

    let sid = &session.native_session_id;
    let agent = session.harness.agent_kind().as_str();
    let resolved = resolve_occurred_at(&transcript.events);
    let mut items = Vec::with_capacity(transcript.events.len() + 2);
    items.push(hook_item(
        endpoint,
        workspace,
        project,
        agent,
        "session-start",
        sid,
        &format!("{sid}:session-start"),
        None,
        serde_json::json!({ "session_id": sid, "occurred_at": resolved.earliest }),
    )?);
    let mut content = 0usize;
    for (event, occurred_at) in transcript.events.iter().zip(&resolved.per_event) {
        if let Some(mapped) = map_event(sid, event, occurred_at.as_deref()) {
            items.push(hook_item(
                endpoint,
                workspace,
                project,
                agent,
                &mapped.event,
                sid,
                &mapped.ingest_key,
                mapped.source_event.as_deref(),
                mapped.body,
            )?);
            content += 1;
        }
    }
    items.push(hook_item(
        endpoint,
        workspace,
        project,
        agent,
        "session-end",
        sid,
        &format!("{sid}:session-end"),
        None,
        serde_json::json!({ "session_id": sid, "occurred_at": resolved.latest }),
    )?);

    post_hook_items(endpoint, &items).await?;
    Ok(content)
}

/// Per-event `occurred_at` resolution for one transcript, plus the session's
/// overall boundary times.
struct ResolvedOccurredAt {
    /// Effective `occurred_at` for each event, in transcript order (RFC 3339).
    per_event: Vec<Option<String>>,
    /// The earliest valid event time — the session-start's `occurred_at`.
    /// A transcript is not guaranteed to be time-sorted (a reordered or
    /// clock-skewed import), so the first *entry* is not reliably the
    /// earliest *time*.
    earliest: Option<String>,
    /// The latest valid event time — the session-end's `occurred_at`, same
    /// reasoning as `earliest`.
    latest: Option<String>,
}

/// Resolve each event's effective `occurred_at`, letting one missing or
/// unparsable own timestamp inherit the nearest preceding *valid* one; an
/// event before the first valid timestamp inherits that first one instead of
/// staying unresolved (there is nothing earlier to inherit from). A value
/// that fails to parse as RFC 3339 is treated exactly like a missing one — it
/// never reaches the hook body, so a malformed transcript timestamp cannot
/// masquerade as a validated one downstream.
fn resolve_occurred_at(events: &[NewWorkstreamEvent]) -> ResolvedOccurredAt {
    let valid: Vec<Option<jiff::Timestamp>> = events
        .iter()
        .map(|event| {
            event
                .occurred_at
                .as_deref()
                .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        })
        .collect();

    let mut per_event: Vec<Option<jiff::Timestamp>> = Vec::with_capacity(events.len());
    let mut last_valid: Option<jiff::Timestamp> = None;
    for ts in &valid {
        if ts.is_some() {
            last_valid = *ts;
        }
        per_event.push(last_valid);
    }
    // Backward-fill the leading gap: events before the first valid timestamp
    // had nothing preceding them to inherit above.
    if let Some(first_valid) = valid.iter().copied().flatten().next() {
        for slot in per_event.iter_mut() {
            match slot {
                Some(_) => break,
                None => *slot = Some(first_valid),
            }
        }
    }

    let (earliest, latest) = valid.into_iter().flatten().fold(
        (None, None),
        |(min, max): (Option<jiff::Timestamp>, Option<jiff::Timestamp>), ts| {
            (
                Some(min.map_or(ts, |m| m.min(ts))),
                Some(max.map_or(ts, |m| m.max(ts))),
            )
        },
    );

    ResolvedOccurredAt {
        per_event: per_event
            .into_iter()
            .map(|ts| ts.map(|t| t.to_string()))
            .collect(),
        earliest: earliest.map(|t| t.to_string()),
        latest: latest.map(|t| t.to_string()),
    }
}

/// A transcript event mapped to its `/hook` shape.
struct MappedEvent {
    event: String,
    source_event: Option<String>,
    ingest_key: String,
    body: serde_json::Value,
}

/// Map one transcript event to a hook event. User messages become the canonical
/// `user-prompt` observation; every other content-bearing event is recorded as
/// a backfill extension observation. Non-content boundary events (compaction,
/// checkpoint, annotation) are dropped — they are not session content.
///
/// `occurred_at` (RFC 3339), already resolved by the caller, rides in the hook
/// body so the imported observation is dated at the transcript's own event
/// time rather than at import time.
fn map_event(
    session_id: &str,
    event: &NewWorkstreamEvent,
    occurred_at: Option<&str>,
) -> Option<MappedEvent> {
    let content = event.content.trim();
    if content.is_empty() {
        return None;
    }
    let role = event.role.as_deref().unwrap_or("");
    let ingest_key = format!("{session_id}:{}", event.event_id);
    match event.kind {
        WorkstreamEventKind::Message if role == "user" || role == "human" => Some(MappedEvent {
            event: "user-prompt".to_string(),
            source_event: None,
            ingest_key,
            body: serde_json::json!({
                "session_id": session_id,
                "prompt": event.content,
                "occurred_at": occurred_at,
            }),
        }),
        WorkstreamEventKind::Message
        | WorkstreamEventKind::ToolCall
        | WorkstreamEventKind::ToolResult => {
            let source_event = match event.kind {
                WorkstreamEventKind::ToolCall => "tool_call".to_string(),
                WorkstreamEventKind::ToolResult => "tool_result".to_string(),
                _ if role.is_empty() => "message".to_string(),
                _ => format!("{role}-message"),
            };
            Some(MappedEvent {
                event: format!("backfill.{source_event}"),
                source_event: Some(source_event),
                ingest_key,
                body: serde_json::json!({
                    "session_id": session_id,
                    "title": first_line(content),
                    "message": event.content,
                    "occurred_at": occurred_at,
                }),
            })
        }
        WorkstreamEventKind::Compaction
        | WorkstreamEventKind::Checkpoint
        | WorkstreamEventKind::Annotation => None,
    }
}

/// A short one-line title from the event content.
fn first_line(content: &str) -> String {
    let line = content.lines().next().unwrap_or("").trim();
    let capped: String = line.chars().take(120).collect();
    if capped.is_empty() {
        "(backfilled event)".to_string()
    } else {
        capped
    }
}

/// Build a `/hook` URL whose query the server parses for event/agent/scope. The
/// URL is data the batch item carries, not the request target (that is
/// `/hook/batch`), so it is built against this server's origin exactly as a live
/// hook would have spooled it.
#[allow(clippy::too_many_arguments)]
fn hook_item(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    agent: &str,
    event: &str,
    session_id: &str,
    ingest_key: &str,
    source_event: Option<&str>,
    body: serde_json::Value,
) -> Result<HookItem> {
    let mut url = reqwest::Url::parse(&endpoint.build_url("/hook"))
        .context("building the hook URL for backfill")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("event", event)
            .append_pair("agent", agent)
            .append_pair("workspace", workspace)
            .append_pair("project", project)
            .append_pair("project_src", "marker")
            .append_pair("session_id", session_id)
            .append_pair("ingest_key", ingest_key);
        if let Some(source_event) = source_event {
            query
                .append_pair("extension", BACKFILL_EXTENSION)
                .append_pair("source_event", source_event);
        }
    }
    Ok(HookItem {
        url: url.into(),
        body,
    })
}

/// POST the events in server-sized batches, retrying the unaccepted suffix of a
/// partially accepted batch (per-source rate limiting can skip a tail) before
/// failing. Events are oldest-first, so a leading-prefix ack is safe to resume.
async fn post_hook_items(endpoint: &ServerEndpoint, items: &[HookItem]) -> Result<()> {
    for chunk in items.chunks(HOOK_BATCH_ITEMS) {
        let mut offset = 0usize;
        let mut stalled = 0u32;
        while offset < chunk.len() {
            let batch: Vec<&HookItem> = chunk[offset..].iter().collect();
            let ack: HookBatchAck = post_json(endpoint, "/hook/batch", &batch)
                .await
                .context("replaying transcript events through /hook/batch")?;
            if ack.accepted == 0 {
                stalled += 1;
                if stalled >= 5 {
                    bail!(
                        "server accepted none of a hook batch after {stalled} attempts \
                         (rate limited or saturated); rerun to resume"
                    );
                }
                tokio::time::sleep(Duration::from_millis(200 * u64::from(stalled))).await;
                continue;
            }
            stalled = 0;
            offset += ack.accepted.min(chunk.len() - offset);
        }
    }
    Ok(())
}

fn finish(args: &crate::cli::BackfillArgs, report: &BackfillReport) -> Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    if args.quiet {
        return Ok(());
    }
    if report.skipped_non_empty {
        println!(
            "ai-memory: {}/{} already has captured sessions — nothing to backfill (use --force to import anyway).",
            report.workspace, report.project
        );
        return Ok(());
    }
    if report.dry_run {
        println!(
            "ai-memory: would import {} local session(s) into {}/{} (dry run).",
            report.selected, report.workspace, report.project
        );
        return Ok(());
    }
    if report.imported_sessions == 0 && report.failed_sessions == 0 {
        println!(
            "ai-memory: no local session history found to backfill for {}/{}.",
            report.workspace, report.project
        );
        return Ok(());
    }
    let mut line = format!(
        "📼 ai-memory imported {} prior local session(s) (~{} events) for {}/{}.",
        report.imported_sessions, report.imported_events, report.workspace, report.project
    );
    if report.failed_sessions > 0 {
        line.push_str(&format!(" {} session(s) failed.", report.failed_sessions));
    }
    if report.skipped_for_cap > 0 {
        line.push_str(&format!(
            " {} older session(s) skipped (import cap).",
            report.skipped_for_cap
        ));
    }
    line.push_str(" Opt out with AI_MEMORY_BACKFILL_ON_START=false.");
    println!("{line}");
    Ok(())
}

fn display_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn session(id: &str, secs_ago: u64) -> SessionRef {
        SessionRef {
            harness: ManagedHarness::Claude,
            native_session_id: id.to_string(),
            updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - secs_ago),
        }
    }

    #[test]
    fn select_sessions_keeps_the_newest_up_to_the_cap() {
        let all = vec![session("old", 300), session("new", 10), session("mid", 100)];
        let picked = select_sessions(all, 2);
        assert_eq!(
            picked
                .iter()
                .map(|s| s.native_session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "mid"],
            "newest first, capped to 2"
        );
    }

    #[test]
    fn select_sessions_cap_zero_is_raised_to_one_by_caller_contract() {
        // The caller passes max_sessions.max(1); select_sessions itself honors
        // whatever it is given, so a literal 0 yields nothing.
        assert!(select_sessions(vec![session("a", 1)], 0).is_empty());
        assert_eq!(select_sessions(vec![session("a", 1)], 1).len(), 1);
    }

    #[test]
    fn sentinel_path_is_cwd_specific_and_path_safe() {
        let dir = Path::new("/data");
        let a = sentinel_path(dir, Path::new("/home/me/projects/app"));
        let b = sentinel_path(dir, Path::new("/home/me/projects/other"));
        let c = sentinel_path(dir, Path::new("/home/me/projects/app"));
        assert_eq!(a, c, "same cwd -> same path");
        assert_ne!(a, b, "different cwd -> different path");
        assert!(
            a.starts_with("/data/backfill-state/"),
            "under the data dir: {a:?}"
        );
        // A path with separators hashes to a single flat, path-safe leaf.
        assert_eq!(
            a.strip_prefix("/data/backfill-state/")
                .unwrap()
                .components()
                .count(),
            1,
            "the hashed leaf must be a single path component: {a:?}"
        );
    }

    fn event(kind: WorkstreamEventKind, role: Option<&str>, content: &str) -> NewWorkstreamEvent {
        NewWorkstreamEvent {
            event_id: "evt-1".to_string(),
            agent: ai_memory_core::AgentKind::ClaudeCode,
            native_session_id: "sid".to_string(),
            source_record_id: None,
            kind,
            role: role.map(str::to_string),
            content: content.to_string(),
            occurred_at: None,
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn user_message_maps_to_the_canonical_user_prompt() {
        let m = map_event(
            "sid",
            &event(WorkstreamEventKind::Message, Some("user"), "do the thing"),
            Some("2026-09-10T12:00:00Z"),
        )
        .expect("user message maps");
        assert_eq!(m.event, "user-prompt");
        assert!(
            m.source_event.is_none(),
            "user-prompt is a lifecycle event, not an extension"
        );
        assert_eq!(m.body["prompt"], "do the thing");
        assert_eq!(m.body["occurred_at"], "2026-09-10T12:00:00Z");
        assert_eq!(
            m.ingest_key, "sid:evt-1",
            "ingest key is stable per source event"
        );
    }

    #[test]
    fn assistant_and_tool_events_map_to_backfill_extension_observations() {
        let a = map_event(
            "sid",
            &event(
                WorkstreamEventKind::Message,
                Some("assistant"),
                "here is the plan\nline2",
            ),
            None,
        )
        .expect("assistant maps");
        assert_eq!(a.event, "backfill.assistant-message");
        assert_eq!(a.source_event.as_deref(), Some("assistant-message"));
        assert_eq!(
            a.body["title"], "here is the plan",
            "title is the first line"
        );
        assert_eq!(a.body["message"], "here is the plan\nline2");
        assert!(
            a.body["occurred_at"].is_null(),
            "no occurred_at was supplied"
        );

        let t = map_event(
            "sid",
            &event(WorkstreamEventKind::ToolCall, None, "grep foo"),
            None,
        )
        .expect("tool call maps");
        assert_eq!(t.event, "backfill.tool_call");
        assert_eq!(t.source_event.as_deref(), Some("tool_call"));
    }

    /// Round-trips a literal RFC 3339 string through `jiff::Timestamp` so
    /// expectations match `resolve_occurred_at`'s own parse-then-format
    /// output rather than assuming it echoes the input string verbatim.
    fn ts(literal: &str) -> String {
        literal.parse::<jiff::Timestamp>().unwrap().to_string()
    }

    #[test]
    fn resolve_occurred_at_fills_gaps_from_the_preceding_event() {
        let mut e1 = event(WorkstreamEventKind::Message, Some("user"), "one");
        e1.occurred_at = Some("2026-09-10T12:00:00Z".to_string());
        let e2 = event(WorkstreamEventKind::Message, Some("assistant"), "two"); // no timestamp
        let mut e3 = event(WorkstreamEventKind::ToolCall, None, "three");
        e3.occurred_at = Some("2026-09-10T12:05:00Z".to_string());
        let e4 = event(WorkstreamEventKind::ToolResult, None, "four"); // no timestamp

        let resolved = resolve_occurred_at(&[e1, e2, e3, e4]);
        assert_eq!(
            resolved.per_event,
            vec![
                Some(ts("2026-09-10T12:00:00Z")),
                Some(ts("2026-09-10T12:00:00Z")),
                Some(ts("2026-09-10T12:05:00Z")),
                Some(ts("2026-09-10T12:05:00Z")),
            ],
            "an event without its own timestamp inherits the nearest preceding one"
        );
        assert_eq!(resolved.earliest, Some(ts("2026-09-10T12:00:00Z")));
        assert_eq!(resolved.latest, Some(ts("2026-09-10T12:05:00Z")));
    }

    #[test]
    fn resolve_occurred_at_backfills_the_leading_gap_from_the_first_valid_timestamp() {
        let e1 = event(WorkstreamEventKind::Message, Some("user"), "one"); // no timestamp
        let e2 = event(WorkstreamEventKind::Message, Some("assistant"), "two"); // no timestamp
        let mut e3 = event(WorkstreamEventKind::ToolCall, None, "three");
        e3.occurred_at = Some("2026-09-10T12:05:00Z".to_string());

        let resolved = resolve_occurred_at(&[e1, e2, e3]);
        assert_eq!(
            resolved.per_event,
            vec![
                Some(ts("2026-09-10T12:05:00Z")),
                Some(ts("2026-09-10T12:05:00Z")),
                Some(ts("2026-09-10T12:05:00Z")),
            ],
            "events before the first valid timestamp inherit it backward, \
             not just the ones after"
        );
    }

    #[test]
    fn resolve_occurred_at_treats_an_unparsable_timestamp_as_missing() {
        let mut e1 = event(WorkstreamEventKind::Message, Some("user"), "one");
        e1.occurred_at = Some("2026-09-10T12:00:00Z".to_string());
        let mut e2 = event(WorkstreamEventKind::Message, Some("assistant"), "two");
        e2.occurred_at = Some("not-a-timestamp".to_string());

        let resolved = resolve_occurred_at(&[e1, e2]);
        assert_eq!(
            resolved.per_event,
            vec![
                Some(ts("2026-09-10T12:00:00Z")),
                Some(ts("2026-09-10T12:00:00Z"))
            ],
            "an unparsable timestamp must not reach the hook body; the \
             preceding valid one is inherited instead"
        );
        assert_eq!(resolved.latest, Some(ts("2026-09-10T12:00:00Z")));
    }

    #[test]
    fn resolve_occurred_at_uses_min_and_max_not_first_and_last_when_out_of_order() {
        // A transcript is not guaranteed to be time-sorted (clock skew,
        // reordering); the session boundary must reflect the actual extremes,
        // not just the first/last entries.
        let mut e1 = event(WorkstreamEventKind::Message, Some("user"), "one");
        e1.occurred_at = Some("2026-09-10T12:05:00Z".to_string());
        let mut e2 = event(WorkstreamEventKind::Message, Some("assistant"), "two");
        e2.occurred_at = Some("2026-09-10T12:00:00Z".to_string());

        let resolved = resolve_occurred_at(&[e1, e2]);
        assert_eq!(
            resolved.earliest,
            Some(ts("2026-09-10T12:00:00Z")),
            "earliest must be the minimum valid time, not the first entry"
        );
        assert_eq!(
            resolved.latest,
            Some(ts("2026-09-10T12:05:00Z")),
            "latest must be the maximum valid time, not the last entry"
        );
    }

    #[test]
    fn resolve_occurred_at_stays_none_when_nothing_has_a_timestamp() {
        let events = vec![
            event(WorkstreamEventKind::Message, Some("user"), "one"),
            event(WorkstreamEventKind::Message, Some("assistant"), "two"),
        ];
        let resolved = resolve_occurred_at(&events);
        assert_eq!(resolved.per_event, vec![None, None]);
        assert_eq!(resolved.earliest, None);
        assert_eq!(resolved.latest, None);
    }

    #[test]
    fn empty_and_boundary_events_are_dropped() {
        assert!(
            map_event(
                "sid",
                &event(WorkstreamEventKind::Message, Some("user"), "   "),
                None,
            )
            .is_none(),
            "whitespace-only content is not an observation"
        );
        for kind in [
            WorkstreamEventKind::Compaction,
            WorkstreamEventKind::Checkpoint,
            WorkstreamEventKind::Annotation,
        ] {
            assert!(
                map_event("sid", &event(kind, None, "boundary"), None).is_none(),
                "{kind:?} is not session content"
            );
        }
    }

    /// End-to-end proof that `collect_local_sessions` wires to the real
    /// workstream path encoder: a Claude transcript planted under the actual
    /// `~/.claude/projects/<enc-cwd>/` layout for this cwd is discovered, and a
    /// transcript for a different cwd is ignored.
    ///
    /// Unix-gated: the fixture uses a POSIX-encoded projects path; cross-platform
    /// native-store discovery is owned and tested by `ai-memory-workstream`. The
    /// pure selection/cap tests above run on every platform.
    #[cfg(unix)]
    #[tokio::test]
    async fn collect_local_sessions_finds_a_planted_claude_session_for_this_cwd() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let other_cwd = tempfile::tempdir().unwrap();

        let session_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join(cwd.path().to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&session_dir).unwrap();
        let header = serde_json::json!({
            "sessionId": "11111111-2222-3333-4444-555555555555",
            "cwd": cwd.path().to_string_lossy(),
        });
        std::fs::write(session_dir.join("sess.jsonl"), format!("{header}\n")).unwrap();
        let foreign = serde_json::json!({
            "sessionId": "99999999-8888-7777-6666-555555555555",
            "cwd": other_cwd.path().to_string_lossy(),
        });
        std::fs::write(session_dir.join("foreign.jsonl"), format!("{foreign}\n")).unwrap();

        let (found, _limit_hit) =
            collect_local_sessions_with(home.path(), cwd.path(), None, |_| None).await;
        let claude: Vec<_> = found
            .iter()
            .filter(|s| s.harness == ManagedHarness::Claude)
            .collect();
        assert_eq!(claude.len(), 1, "only the matching-cwd session: {found:?}");
        assert_eq!(
            claude[0].native_session_id,
            "11111111-2222-3333-4444-555555555555"
        );

        // `--session` narrows to one id.
        let (only, _limit_hit) =
            collect_local_sessions_with(home.path(), cwd.path(), Some("nope"), |_| None).await;
        assert!(only.is_empty(), "no session matches the filter: {only:?}");
    }

    /// The automatic path must honor the `backfill_on_start` opt-out: it records
    /// the attempt (so it never re-spawns) and returns without contacting the
    /// server at all.
    #[tokio::test]
    async fn auto_run_opted_out_writes_sentinel_and_makes_no_request() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let mut config =
            crate::config::Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data_dir.path().to_path_buf();
        config.backfill_on_start = false;
        // An unroutable server would make any request hang/fail; the opt-out
        // must return before we ever build the endpoint.
        config.server_url = "http://127.0.0.1:9".to_string();

        let args = crate::cli::BackfillArgs {
            workspace: None,
            project: None,
            session: None,
            force: false,
            dry_run: false,
            max_sessions: 25,
            json: false,
            quiet: true,
            auto: true,
        };
        run(&config, args)
            .await
            .expect("opted-out auto run must succeed without contacting the server");

        let cwd = std::env::current_dir().unwrap();
        assert!(
            sentinel_path(&config.data_dir, &cwd).exists(),
            "the opt-out must still record the attempt so it does not re-spawn"
        );
    }
}
