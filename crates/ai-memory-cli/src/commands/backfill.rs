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
    LaunchRoots, ManagedHarness, build_launch_plan_with_env, export_transcript,
    export_transcript_delta, export_transcript_range, list_native_sessions,
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
const HOOK_BATCH_BYTES: usize = 8 * 1024 * 1024;

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

/// The server this backfill delivers to.
///
/// A SessionStart-spawned run names the hook's own target: a server profile
/// (#992), whose stored token is the only credential it will present, or the
/// install-time hook URL, authenticated exactly like the hook's own events to
/// it: the persisted hook token, then OIDC. The config/env bearer is never
/// used there, because it may belong to a different server than the one the
/// hook is installed against. A manual run keeps resolving from config.
async fn backfill_endpoint(
    config: &Config,
    args: &crate::cli::BackfillArgs,
) -> Result<ServerEndpoint> {
    if let Some(raw) = args.server_profile.as_deref() {
        let name = crate::server_profiles::ProfileName::parse(raw)
            .with_context(|| format!("`{raw}` is not a valid server profile name"))?;
        let profile = crate::server_profiles::lookup(&config.data_dir, &name)
            .map_err(|r| anyhow::anyhow!("server profile `{name}` was refused ({})", r.as_str()))?;
        return Ok(ServerEndpoint::for_hook_target(
            profile.url,
            Some(profile.token),
        ));
    }
    if let Some(url) = args.server_url.as_deref() {
        let static_token = crate::config::read_hook_auth_token(&config.data_dir);
        let token = super::hook_spool::resolve_bearer(
            &reqwest::Client::new(),
            &config.data_dir,
            static_token.as_deref(),
        )
        .await;
        return Ok(ServerEndpoint::for_hook_target(url.to_owned(), token));
    }
    Ok(ServerEndpoint::from_config_resolving_auth(config).await)
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
    let endpoint = backfill_endpoint(config, &args).await?;

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
        // A multi-user server keeps `/admin/*` for the operator, so a
        // developer's own key is refused here. Ask the grant-checked web API
        // instead: it answers for the sessions this caller can see.
        Err(error) if is_forbidden(&error) => {
            project_is_empty_for_member(endpoint, workspace, project).await
        }
        Err(error) => Err(error).with_context(|| {
            format!("checking whether {workspace}/{project} already has captured sessions")
        }),
    }
}

fn is_forbidden(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::http_client::ServerResponseError>()
        .is_some_and(|response| response.status() == reqwest::StatusCode::FORBIDDEN)
}

/// The sessions the web API lists for a project, open ones included.
#[derive(Debug, Deserialize)]
struct SessionListResponse {
    sessions: Vec<serde_json::Value>,
}

/// [`project_is_empty`] through `GET /api/v1/workspaces/{ws}/projects/{p}/sessions`,
/// which any member may read. Its 404 means the project does not exist (or is
/// not this caller's to see); either way the hook ingress, not this check,
/// decides whether an import may write there. A 404 whose body is not a JSON
/// error — a server without the web API — stays an error: guessing "empty"
/// there could import history twice.
async fn project_is_empty_for_member(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
) -> Result<bool> {
    let path = member_sessions_path(workspace, project)?;
    match get_json::<SessionListResponse>(
        endpoint,
        &path,
        &[("limit", "1"), ("include_open", "true")],
    )
    .await
    {
        Ok(response) => Ok(response.sessions.is_empty()),
        Err(error) if is_json_not_found(&error) => Ok(true),
        Err(error) => Err(error).with_context(|| {
            format!("checking whether {workspace}/{project} already has captured sessions")
        }),
    }
}

/// The path with each name percent-encoded as one segment, so a name can
/// never reach another route.
fn member_sessions_path(workspace: &str, project: &str) -> Result<String> {
    let mut url = reqwest::Url::parse("http://placeholder/")?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("placeholder URL cannot hold a path"))?
        .clear()
        .extend([
            "api",
            "v1",
            "workspaces",
            workspace,
            "projects",
            project,
            "sessions",
        ]);
    Ok(url.path().to_owned())
}

fn is_json_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::http_client::ServerResponseError>()
        .is_some_and(|response| {
            response.status() == reqwest::StatusCode::NOT_FOUND
                && serde_json::from_str::<serde_json::Value>(response.body())
                    .is_ok_and(|body| body.get("error").is_some())
        })
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
#[derive(Clone, Debug, Serialize)]
struct HookItem {
    url: String,
    body: serde_json::Value,
}

/// The server's `/hook/batch` acknowledgement (subset we act on).
#[derive(Debug, Deserialize)]
struct HookBatchAck {
    accepted: usize,
    results: Vec<HookBatchResult>,
    #[serde(default)]
    accepted_indices: Option<Vec<usize>>,
    #[serde(default)]
    failed_index: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct HookBatchResult {
    index: usize,
    outcome: String,
}

fn acknowledged_items(ack: &HookBatchAck, items: &[&HookItem]) -> Result<Vec<usize>> {
    let indices = ack
        .accepted_indices
        .clone()
        .unwrap_or_else(|| (0..ack.accepted).collect());
    if ack.accepted > items.len()
        || indices.iter().any(|index| *index >= items.len())
        || indices.windows(2).any(|pair| pair[0] >= pair[1])
        || ack.accepted
            != indices
                .iter()
                .enumerate()
                .take_while(|(pos, index)| *pos == **index)
                .count()
        || ack.results.len() != indices.len()
        || ack
            .failed_index
            .is_some_and(|index| index >= items.len() || indices.contains(&index))
    {
        bail!("invalid hook acknowledgement; keeping transcript for retry");
    }
    for (result, index) in ack.results.iter().zip(&indices) {
        if result.index != *index {
            bail!("hook acknowledgement result indices disagree");
        }
        let is_end = reqwest::Url::parse(&items[*index].url)?
            .query_pairs()
            .any(|(key, value)| key == "event" && value == "session-end");
        match result.outcome.as_str() {
            "stored" | "replayed" => {}
            "ignored_end" if is_end => {}
            "resumed" => bail!("hook processing resumed; rerun to confirm durable replay"),
            outcome => {
                bail!("hook item {index} was not durably stored ({outcome}); keeping transcript")
            }
        }
    }
    if let Some(index) = ack.failed_index {
        bail!("hook processing failed at item {index}; keeping transcript for retry");
    }
    Ok(indices)
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
    let roots = LaunchRoots { home, cwd };
    let session_dir =
        build_launch_plan_with_env(session.harness, None, Vec::new(), None, &[], Some(roots))
            .ok()
            .and_then(|plan| plan.session_dir);
    import_exact_session(
        endpoint,
        workspace,
        project,
        home,
        cwd,
        session.harness,
        &session.native_session_id,
        session_dir.as_deref(),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn import_exact_session_range(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    home: &Path,
    cwd: &Path,
    harness: ManagedHarness,
    native_session_id: &str,
    recovery_interval_id: &str,
    session_dir: Option<&Path>,
    source_cursor: Option<&str>,
    final_cursor: &str,
    expected_digests: &[String],
) -> Result<usize> {
    let _ = wait_for_transcript_flush(harness, home, cwd, session_dir, native_session_id).await;
    let transcript = export_transcript_range(
        harness,
        home,
        cwd,
        session_dir,
        native_session_id,
        source_cursor,
        final_cursor,
    )
    .await
    .with_context(|| format!("reading the {} transcript", harness.as_str()))?;
    let actual = ai_memory_workstream::transcript_interval_digests(&transcript.events);
    if actual != expected_digests {
        bail!("native transcript range no longer matches its exact ordered semantic identity");
    }
    import_exported_transcript(
        endpoint,
        workspace,
        project,
        harness,
        native_session_id,
        Some(recovery_interval_id),
        transcript,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn import_exact_session(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    home: &Path,
    cwd: &Path,
    harness: ManagedHarness,
    native_session_id: &str,
    session_dir: Option<&Path>,
    source_cursor: Option<&str>,
) -> Result<usize> {
    let _ = wait_for_transcript_flush(harness, home, cwd, session_dir, native_session_id).await;
    let transcript = if let Some(cursor) = source_cursor {
        export_transcript_delta(harness, home, cwd, session_dir, native_session_id, cursor).await
    } else {
        export_transcript(harness, home, cwd, session_dir, native_session_id, None).await
    }
    .with_context(|| format!("reading the {} transcript", harness.as_str()))?;
    import_exported_transcript(
        endpoint,
        workspace,
        project,
        harness,
        native_session_id,
        None,
        transcript,
    )
    .await
}

async fn import_exported_transcript(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    harness: ManagedHarness,
    native_session_id: &str,
    recovery_interval_id: Option<&str>,
    transcript: ai_memory_workstream::ExportedTranscript,
) -> Result<usize> {
    let sid = native_session_id;
    let agent = harness.agent_kind().as_str();
    let resolved = resolve_occurred_at(&transcript.events);
    let mut items = Vec::with_capacity(transcript.events.len() + 2);
    items.push(hook_item(
        endpoint,
        workspace,
        project,
        agent,
        "session-start",
        sid,
        &replay_ingest_key(recovery_interval_id, sid, "session-start", "boundary"),
        None,
        serde_json::json!({ "session_id": sid, "occurred_at": resolved.earliest }),
    )?);
    let mut content = 0usize;
    for (event, occurred_at) in transcript.events.iter().zip(&resolved.per_event) {
        if let Some(mapped) = map_event(recovery_interval_id, sid, event, occurred_at.as_deref()) {
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
        &replay_ingest_key(recovery_interval_id, sid, "session-end", "boundary"),
        None,
        serde_json::json!({ "session_id": sid, "occurred_at": resolved.latest }),
    )?);

    post_hook_items(endpoint, &items).await?;
    Ok(content)
}

/// Replay only the user prompts of one session the server already knows,
/// for harnesses whose live hooks carry no prompt event (Antigravity CLI).
/// `finalize-session` sends these before its synthetic `session-end`, so the
/// summary page and the automatic handoff are built with the prompts.
///
/// No session boundaries are sent: the caller owns the `session-end`, and a
/// keyed one would be deduplicated on a later `--reopen`. Each prompt carries
/// the same [`replay_ingest_key`] that `backfill` mints for that event, so
/// repeated finalizes (and a backfill of the same session) store it once.
/// Returns the number of prompts sent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn replay_session_prompts(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    home: &Path,
    cwd: &Path,
    harness: ManagedHarness,
    native_session_id: &str,
) -> Result<usize> {
    let transcript = export_transcript(harness, home, cwd, None, native_session_id, None)
        .await
        .with_context(|| format!("reading the {} transcript", harness.as_str()))?;
    let items = prompt_hook_items(
        endpoint,
        workspace,
        project,
        harness,
        native_session_id,
        &transcript.events,
    )?;
    if !items.is_empty() {
        post_hook_items(endpoint, &items).await?;
    }
    Ok(items.len())
}

/// The `user-prompt` hook items of a transcript, in transcript order, each
/// dated at its own event time and keyed with [`replay_ingest_key`]. Every
/// other event kind is dropped.
fn prompt_hook_items(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    harness: ManagedHarness,
    native_session_id: &str,
    events: &[NewWorkstreamEvent],
) -> Result<Vec<HookItem>> {
    let agent = harness.agent_kind().as_str();
    let resolved = resolve_occurred_at(events);
    let mut items = Vec::new();
    for (event, occurred_at) in events.iter().zip(&resolved.per_event) {
        let Some(mapped) = map_event(None, native_session_id, event, occurred_at.as_deref()) else {
            continue;
        };
        if mapped.event != "user-prompt" {
            continue;
        }
        items.push(hook_item(
            endpoint,
            workspace,
            project,
            agent,
            &mapped.event,
            native_session_id,
            &mapped.ingest_key,
            None,
            mapped.body,
        )?);
    }
    Ok(items)
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
    recovery_interval_id: Option<&str>,
    session_id: &str,
    event: &NewWorkstreamEvent,
    occurred_at: Option<&str>,
) -> Option<MappedEvent> {
    let content = event.content.trim();
    if content.is_empty() {
        return None;
    }
    let role = event.role.as_deref().unwrap_or("");
    let ingest_key = replay_ingest_key(
        recovery_interval_id,
        session_id,
        event.kind.as_str(),
        &event.event_id,
    );
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

fn replay_ingest_key(
    recovery_interval_id: Option<&str>,
    session_id: &str,
    kind: &str,
    event_id: &str,
) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for value in [
        recovery_interval_id.unwrap_or_default(),
        session_id,
        kind,
        event_id,
    ] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("recovery_{}", &digest[..55])
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

/// POST events in server-sized batches. Recovery requires a modern per-item
/// acknowledgement and fails closed on partial or terminal-drop outcomes.
fn hook_batch_end(items: &[HookItem], offset: usize) -> Result<usize> {
    let mut end = offset;
    let mut bytes = 2_usize;
    while end < items.len() && end - offset < HOOK_BATCH_ITEMS {
        let item_bytes = serde_json::to_vec(&items[end])?.len().saturating_add(1);
        if item_bytes > HOOK_BATCH_BYTES {
            bail!("one sanitized hook item exceeds the recovery request byte limit");
        }
        if end > offset && bytes.saturating_add(item_bytes) > HOOK_BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(item_bytes);
        end += 1;
    }
    Ok(end)
}

async fn post_hook_items(endpoint: &ServerEndpoint, items: &[HookItem]) -> Result<()> {
    let mut offset = 0_usize;
    while offset < items.len() {
        let end = hook_batch_end(items, offset)?;
        let chunk = &items[offset..end];
        let mut stalled = 0u32;
        loop {
            let batch: Vec<&HookItem> = chunk.iter().collect();
            let ack: HookBatchAck = post_json(endpoint, "/hook/batch", &batch)
                .await
                .context("replaying transcript events through /hook/batch")?;
            let accepted = acknowledged_items(&ack, &batch)?;
            if accepted.len() == batch.len() {
                break;
            }
            if accepted.is_empty() {
                stalled += 1;
                if stalled >= 5 {
                    bail!(
                        "server acknowledged none of a hook batch after {stalled} attempts \
                         (rate limited or saturated); rerun to resume"
                    );
                }
                tokio::time::sleep(Duration::from_millis(200 * u64::from(stalled))).await;
                continue;
            }
            bail!(
                "server acknowledged only {} of {} transcript items; keeping transcript for a safe retry",
                accepted.len(),
                batch.len()
            );
        }
        offset = end;
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
    fn member_sessions_path_encodes_each_name_as_one_segment() {
        assert_eq!(
            member_sessions_path("team", "app").unwrap(),
            "/api/v1/workspaces/team/projects/app/sessions"
        );
        assert_eq!(
            member_sessions_path("a/b", "c d?").unwrap(),
            "/api/v1/workspaces/a%2Fb/projects/c%20d%3F/sessions"
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
            None,
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
            m.ingest_key,
            replay_ingest_key(None, "sid", "message", "evt-1"),
            "ingest key is stable per source event"
        );
        assert_eq!(m.ingest_key.len(), 64);
        assert!(
            m.ingest_key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
    }

    #[test]
    fn replay_keys_are_deterministic_valid_and_bounded_for_long_native_ids() {
        let session = "native:".repeat(200);
        let event = "event/with spaces:".repeat(200);
        let first = replay_ingest_key(None, &session, "message", &event);
        let second = replay_ingest_key(None, &session, "message", &event);
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert!(first.starts_with("recovery_"));
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        assert_ne!(
            first,
            replay_ingest_key(None, &session, "tool_call", &event)
        );
        let other_interval = replay_ingest_key(Some("journal-2"), &session, "message", &event);
        assert_ne!(first, other_interval);
        assert_eq!(
            other_interval,
            replay_ingest_key(Some("journal-2"), &session, "message", &event),
            "one journal interval must replay stably"
        );
    }

    #[test]
    fn prompt_replay_keeps_only_user_prompts_with_backfill_keys() {
        let endpoint = ServerEndpoint::for_hook_target("http://127.0.0.1:1".into(), None);
        let with_id = |id: &str, mut e: NewWorkstreamEvent| {
            e.event_id = id.to_string();
            e
        };
        let events = [
            with_id(
                "p1",
                event(WorkstreamEventKind::Message, Some("user"), "first ask"),
            ),
            with_id(
                "a1",
                event(WorkstreamEventKind::Message, Some("assistant"), "an answer"),
            ),
            with_id(
                "t1",
                event(WorkstreamEventKind::ToolCall, None, "run tests"),
            ),
            with_id(
                "p2",
                event(WorkstreamEventKind::Message, Some("user"), "second ask"),
            ),
        ];
        let items = prompt_hook_items(
            &endpoint,
            "ws",
            "proj",
            ManagedHarness::Antigravity,
            "sid",
            &events,
        )
        .unwrap();
        assert_eq!(items.len(), 2, "only the two user prompts are replayed");
        for (item, (id, prompt)) in items
            .iter()
            .zip([("p1", "first ask"), ("p2", "second ask")])
        {
            let url = reqwest::Url::parse(&item.url).unwrap();
            let query = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
            };
            assert_eq!(query("event").as_deref(), Some("user-prompt"));
            assert_eq!(query("agent").as_deref(), Some("antigravity-cli"));
            assert_eq!(
                query("extension"),
                None,
                "a prompt is not a backfill extension"
            );
            assert_eq!(
                query("ingest_key"),
                Some(replay_ingest_key(None, "sid", "message", id)),
                "the key matches what backfill mints for the same event"
            );
            assert_eq!(item.body["prompt"], prompt);
        }
    }

    #[test]
    fn recovery_hook_batches_respect_count_and_serialized_byte_limits() {
        let endpoint = ServerEndpoint::for_hook_target("http://127.0.0.1:1".into(), None);
        let make = |size| HookItem {
            url: endpoint.build_url("/hook?event=user-prompt"),
            body: serde_json::json!({"prompt": "x".repeat(size)}),
        };
        let items = (0..HOOK_BATCH_ITEMS + 1)
            .map(|_| make(1))
            .collect::<Vec<_>>();
        assert_eq!(hook_batch_end(&items, 0).unwrap(), HOOK_BATCH_ITEMS);

        let large = vec![make(HOOK_BATCH_BYTES / 2), make(HOOK_BATCH_BYTES / 2)];
        assert_eq!(hook_batch_end(&large, 0).unwrap(), 1);
        assert!(
            hook_batch_end(&[make(HOOK_BATCH_BYTES)], 0)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
    }

    #[test]
    fn acknowledgement_requires_results_and_rejects_terminal_drops() {
        let endpoint = ServerEndpoint::for_hook_target("http://127.0.0.1:1".into(), None);
        let item = hook_item(
            &endpoint,
            "ws",
            "project",
            "claude-code",
            "user-prompt",
            "sid",
            "key",
            None,
            serde_json::json!({"session_id": "sid", "prompt": "hello"}),
        )
        .unwrap();
        let items = [&item];
        for outcome in [
            "dropped_policy",
            "dropped_subagent",
            "dropped_unauthorized",
            "dropped_collision",
            "dropped_invalid",
            "resumed",
        ] {
            let ack = HookBatchAck {
                accepted: 1,
                results: vec![HookBatchResult {
                    index: 0,
                    outcome: outcome.into(),
                }],
                accepted_indices: None,
                failed_index: None,
            };
            assert!(acknowledged_items(&ack, &items).is_err(), "{outcome}");
        }
        let legacy = HookBatchAck {
            accepted: 1,
            results: Vec::new(),
            accepted_indices: None,
            failed_index: None,
        };
        assert!(acknowledged_items(&legacy, &items).is_err());
    }

    #[test]
    fn acknowledgement_allows_stored_replayed_and_only_terminal_ignored_end() {
        let endpoint = ServerEndpoint::for_hook_target("http://127.0.0.1:1".into(), None);
        let make = |event| {
            hook_item(
                &endpoint,
                "ws",
                "project",
                "claude-code",
                event,
                "sid",
                "key",
                None,
                serde_json::json!({"session_id": "sid"}),
            )
            .unwrap()
        };
        let prompt = make("user-prompt");
        let end = make("session-end");
        for outcome in ["stored", "replayed"] {
            let ack = HookBatchAck {
                accepted: 1,
                results: vec![HookBatchResult {
                    index: 0,
                    outcome: outcome.into(),
                }],
                accepted_indices: Some(vec![0]),
                failed_index: None,
            };
            assert_eq!(acknowledged_items(&ack, &[&prompt]).unwrap(), vec![0]);
        }
        let ignored = HookBatchAck {
            accepted: 1,
            results: vec![HookBatchResult {
                index: 0,
                outcome: "ignored_end".into(),
            }],
            accepted_indices: Some(vec![0]),
            failed_index: None,
        };
        assert!(acknowledged_items(&ignored, &[&prompt]).is_err());
        assert_eq!(acknowledged_items(&ignored, &[&end]).unwrap(), vec![0]);
    }

    #[tokio::test]
    async fn real_hook_router_replay_is_idempotent_and_reports_outcomes() {
        use ai_memory_core::{ActiveProject, IngestMetrics, Sanitizer, SessionId};
        use ai_memory_hooks::{
            HookState, IngestGates, IngestRateLimiter, ProjectCacheStore, SubagentSessionSet,
            hook_router,
        };
        use ai_memory_store::Store;
        use ai_memory_wiki::Wiki;
        use std::sync::Arc;
        use tower::ServiceExt as _;

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let workspace_id = store.writer.get_or_create_workspace("ws").await.unwrap();
        let project_id = store
            .writer
            .get_or_create_project(workspace_id, "project", None)
            .await
            .unwrap();
        let state = HookState {
            workspace_id,
            project_id,
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki: Wiki::new(temp.path(), store.writer.clone()).unwrap(),
            consolidator: None,
            sanitizer: Sanitizer::default(),
            project_cache: Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default())),
            active_project: ActiveProject::new(),
            ingest_metrics: Arc::new(IngestMetrics::default()),
            ingest_semaphore: Arc::new(tokio::sync::Semaphore::new(16)),
            ingest_gates: IngestGates::default(),
            ingest_rate: Arc::new(tokio::sync::Mutex::new(IngestRateLimiter::disabled())),
            consolidate_on_session_end: false,
            session_consolidation_notify: None,
            profile_notify: None,
            capture_assistant_enabled: false,
            claim_handoff_on_session_start: true,
            create_handoff_on_session_end: false,
            subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
            home_dir: None,
            trusted_proxy_identity: false,
            per_user_slots: false,
            mid_session_routing: ai_memory_core::MidSessionRouting::default(),
            profile: ai_memory_core::profile::ProfileSettings::default(),
        };
        let endpoint = ServerEndpoint::for_hook_target("http://localhost".into(), None);
        let sid = "native-session-with-a-very-long-identifier-that-would-break-the-old-key";
        let recovery_interval_id = Some("journal");
        let items = vec![
            hook_item(
                &endpoint,
                "ws",
                "project",
                "claude-code",
                "session-start",
                sid,
                &replay_ingest_key(recovery_interval_id, sid, "session-start", "boundary"),
                None,
                serde_json::json!({"session_id": sid, "cwd": "/repo"}),
            )
            .unwrap(),
            hook_item(
                &endpoint,
                "ws",
                "project",
                "claude-code",
                "user-prompt",
                sid,
                &replay_ingest_key(recovery_interval_id, sid, "message", "event-1"),
                None,
                serde_json::json!({"session_id": sid, "cwd": "/repo", "prompt": "same prompt"}),
            )
            .unwrap(),
            hook_item(
                &endpoint,
                "ws",
                "project",
                "claude-code",
                "session-end",
                sid,
                &replay_ingest_key(recovery_interval_id, sid, "session-end", "boundary"),
                None,
                serde_json::json!({"session_id": sid, "cwd": "/repo"}),
            )
            .unwrap(),
        ];
        let spool = crate::commands::hook_spool::spool_dir(temp.path());
        let overlapping = hook_item(
            &endpoint,
            "ws",
            "project",
            "claude-code",
            "user-prompt",
            sid,
            "random_live_key",
            None,
            serde_json::json!({"session_id": sid, "cwd": "/repo", "prompt": "same prompt"}),
        )
        .unwrap();
        crate::commands::hook_spool::enqueue(
            &spool,
            &crate::commands::hook_spool::entry_for(
                overlapping.url,
                overlapping.body.to_string(),
                None,
                false,
            ),
        )
        .unwrap();
        let quarantine = crate::commands::hook_spool::quarantine_session_entries(
            &Config::default(),
            &spool,
            "journal",
            0,
            u64::MAX,
            &endpoint.identity(),
            "claude-code",
            "ws",
            "project",
            Path::new("/repo"),
            sid,
        )
        .unwrap();
        assert_eq!(crate::commands::hook_spool::spool_len(&spool), 0);

        let body = serde_json::to_vec(&items).unwrap();
        let first = hook_router(state.clone())
            .oneshot(
                axum::http::Request::post("/hook/batch")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), axum::http::StatusCode::OK);
        let first: HookBatchAck = serde_json::from_slice(
            &axum::body::to_bytes(first.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            acknowledged_items(&first, &items.iter().collect::<Vec<_>>())
                .unwrap()
                .len(),
            3
        );
        let second = hook_router(state.clone())
            .oneshot(
                axum::http::Request::post("/hook/batch")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let second: HookBatchAck = serde_json::from_slice(
            &axum::body::to_bytes(second.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            second
                .results
                .iter()
                .any(|result| result.outcome == "replayed")
        );
        assert!(
            second
                .results
                .iter()
                .any(|result| result.outcome == "ignored_end")
        );
        let observations = store
            .reader
            .observations_for_session(SessionId::from_native(sid))
            .await
            .unwrap();
        assert_eq!(observations.len(), 3);
        assert_eq!(
            observations
                .iter()
                .filter(|observation| observation.body == "same prompt")
                .count(),
            1
        );
        quarantine.complete().unwrap();
    }

    /// Two sequential offline intervals on one resumed native session: the
    /// second interval's journal id gives its boundary and content keys their
    /// own identity, so its session-end is STORED (re-closing the session with
    /// its end-of-session effects) instead of being short-circuited as a
    /// replay of the first interval's end, while re-running either interval
    /// stays a pure replay.
    #[tokio::test]
    async fn two_sequential_recovery_intervals_on_one_resumed_session_replay_exactly_once() {
        use ai_memory_core::{ActiveProject, IngestMetrics, ObservationKind, Sanitizer, SessionId};
        use ai_memory_hooks::{
            HookState, IngestGates, IngestRateLimiter, ProjectCacheStore, SubagentSessionSet,
            hook_router,
        };
        use ai_memory_store::Store;
        use ai_memory_wiki::Wiki;
        use std::sync::Arc;
        use tower::ServiceExt as _;

        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let workspace_id = store.writer.get_or_create_workspace("ws").await.unwrap();
        let project_id = store
            .writer
            .get_or_create_project(workspace_id, "project", None)
            .await
            .unwrap();
        let state = HookState {
            workspace_id,
            project_id,
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki: Wiki::new(temp.path(), store.writer.clone()).unwrap(),
            consolidator: None,
            sanitizer: Sanitizer::default(),
            project_cache: Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default())),
            active_project: ActiveProject::new(),
            ingest_metrics: Arc::new(IngestMetrics::default()),
            ingest_semaphore: Arc::new(tokio::sync::Semaphore::new(16)),
            ingest_gates: IngestGates::default(),
            ingest_rate: Arc::new(tokio::sync::Mutex::new(IngestRateLimiter::disabled())),
            consolidate_on_session_end: false,
            session_consolidation_notify: None,
            profile_notify: None,
            capture_assistant_enabled: false,
            claim_handoff_on_session_start: true,
            create_handoff_on_session_end: false,
            subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
            home_dir: None,
            trusted_proxy_identity: false,
            per_user_slots: false,
            mid_session_routing: ai_memory_core::MidSessionRouting::default(),
            profile: ai_memory_core::profile::ProfileSettings::default(),
        };
        let endpoint = ServerEndpoint::for_hook_target("http://localhost".into(), None);
        let sid = "native-session-resumed-across-two-offline-intervals";
        let interval_items = |journal: &str, prompt: &str, event_id: &str| {
            vec![
                hook_item(
                    &endpoint,
                    "ws",
                    "project",
                    "claude-code",
                    "session-start",
                    sid,
                    &replay_ingest_key(Some(journal), sid, "session-start", "boundary"),
                    None,
                    serde_json::json!({"session_id": sid, "cwd": "/repo"}),
                )
                .unwrap(),
                hook_item(
                    &endpoint,
                    "ws",
                    "project",
                    "claude-code",
                    "user-prompt",
                    sid,
                    &replay_ingest_key(Some(journal), sid, "message", event_id),
                    None,
                    serde_json::json!({"session_id": sid, "cwd": "/repo", "prompt": prompt}),
                )
                .unwrap(),
                hook_item(
                    &endpoint,
                    "ws",
                    "project",
                    "claude-code",
                    "session-end",
                    sid,
                    &replay_ingest_key(Some(journal), sid, "session-end", "boundary"),
                    None,
                    serde_json::json!({"session_id": sid, "cwd": "/repo"}),
                )
                .unwrap(),
            ]
        };
        let router_state = state.clone();
        let post = move |items: Vec<HookItem>| {
            let state = router_state.clone();
            async move {
                let response = hook_router(state)
                    .oneshot(
                        axum::http::Request::post("/hook/batch")
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(serde_json::to_vec(&items).unwrap()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), axum::http::StatusCode::OK);
                let ack: HookBatchAck = serde_json::from_slice(
                    &axum::body::to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    acknowledged_items(&ack, &items.iter().collect::<Vec<_>>())
                        .unwrap()
                        .len(),
                    items.len()
                );
                ack.results
                    .into_iter()
                    .map(|result| result.outcome)
                    .collect::<Vec<_>>()
            }
        };

        let first = interval_items("journal-a", "first interval prompt", "event-1");
        assert_eq!(
            post(first.clone()).await,
            vec!["stored", "stored", "stored"],
            "the first offline interval recovers exactly once"
        );
        let second = interval_items("journal-b", "second interval prompt", "event-2");
        assert_eq!(
            post(second.clone()).await,
            vec!["stored", "stored", "stored"],
            "the resumed interval's content and session-end must be stored, not replayed"
        );

        let observations = store
            .reader
            .observations_for_session(SessionId::from_native(sid))
            .await
            .unwrap();
        assert_eq!(observations.len(), 6, "two start/end pairs and two prompts");
        assert_eq!(
            observations
                .iter()
                .filter(|observation| observation.body == "first interval prompt")
                .count(),
            1
        );
        assert_eq!(
            observations
                .iter()
                .filter(|observation| observation.body == "second interval prompt")
                .count(),
            1
        );
        assert_eq!(
            observations
                .iter()
                .filter(|observation| observation.kind == ObservationKind::SessionEnd)
                .count(),
            2,
            "each interval's session-end is its own stored observation"
        );

        for rerun in [first, second] {
            assert!(
                rerun
                    .iter()
                    .zip(post(rerun.clone()).await)
                    .all(|(item, outcome)| {
                        let is_end = reqwest::Url::parse(&item.url)
                            .unwrap()
                            .query_pairs()
                            .any(|(key, value)| key == "event" && value == "session-end");
                        outcome == "replayed" || (is_end && outcome == "ignored_end")
                    }),
                "re-running a recovered interval is a pure replay"
            );
        }
        let observations = store
            .reader
            .observations_for_session(SessionId::from_native(sid))
            .await
            .unwrap();
        assert_eq!(observations.len(), 6, "replays add no observations");
    }

    #[test]
    fn assistant_and_tool_events_map_to_backfill_extension_observations() {
        let a = map_event(
            None,
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
            None,
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
                None,
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
                map_event(None, "sid", &event(kind, None, "boundary"), None).is_none(),
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

    fn spawned_args(
        server_url: Option<&str>,
        server_profile: Option<&str>,
    ) -> crate::cli::BackfillArgs {
        crate::cli::BackfillArgs {
            workspace: None,
            project: None,
            session: None,
            force: false,
            dry_run: false,
            max_sessions: 25,
            json: false,
            quiet: true,
            auto: true,
            server_url: server_url.map(str::to_owned),
            server_profile: server_profile.map(str::to_owned),
        }
    }

    /// #992: a SessionStart-spawned backfill presents only the credential the
    /// hook itself uses for that server — never the config/env bearer, which
    /// may belong to another server entirely.
    #[tokio::test]
    async fn a_spawned_backfill_authenticates_like_the_hook_that_spawned_it() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let mut config =
            crate::config::Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data_dir.path().to_path_buf();
        config.auth.bearer_token = Some("CONFIG-SERVER-TOKEN".into());

        let url = "https://hook.example/wiki";
        let endpoint = backfill_endpoint(&config, &spawned_args(Some(url), None))
            .await
            .unwrap();
        assert_eq!(
            endpoint.auth_token, None,
            "no hook token: nothing, not config's"
        );
        assert_eq!(endpoint.url, "https://hook.example");
        assert_eq!(endpoint.base_path, "/wiki");

        crate::config::store_hook_auth_token(data_dir.path(), "HOOK-TOKEN").unwrap();
        let endpoint = backfill_endpoint(&config, &spawned_args(Some(url), None))
            .await
            .unwrap();
        assert_eq!(endpoint.auth_token.as_deref(), Some("HOOK-TOKEN"));

        let name = crate::server_profiles::ProfileName::parse("team-b").unwrap();
        crate::server_profiles::add(data_dir.path(), &name, "https://b.example", &[], Some("B"))
            .unwrap();
        let endpoint = backfill_endpoint(&config, &spawned_args(None, Some("team-b")))
            .await
            .unwrap();
        assert_eq!(endpoint.url, "https://b.example");
        assert_eq!(endpoint.auth_token.as_deref(), Some("B"));
        assert!(
            backfill_endpoint(&config, &spawned_args(None, Some("nobody")))
                .await
                .is_err(),
            "an unregistered profile is refused, not replaced by config"
        );
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

        run(&config, spawned_args(None, None))
            .await
            .expect("opted-out auto run must succeed without contacting the server");

        let cwd = std::env::current_dir().unwrap();
        assert!(
            sentinel_path(&config.data_dir, &cwd).exists(),
            "the opt-out must still record the attempt so it does not re-spawn"
        );
    }
}
