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
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use ai_memory_core::{NewWorkstreamEvent, WorkstreamEventKind};
use ai_memory_hooks::capture_policy::metadata_only_body;
use ai_memory_workstream::{
    LaunchRoots, ManagedHarness, OVERSIZED_RECORD_LOSS, build_launch_plan_with_env,
    export_transcript, export_transcript_delta, export_transcript_range, find_native_session,
    list_native_sessions, wait_for_transcript_flush,
};

use super::doctor::{SCANNED_HARNESSES, relocated_session_dir};
use super::run;
use super::transcript_capture::{EventCapture, TranscriptCapture};
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
    /// True when this install's capture mode admits nothing from this
    /// checkout (allowlist mode without a marker), so nothing was imported.
    skipped_not_admitted: bool,
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

/// How long an in-progress marker blocks a new automatic backfill for the same
/// checkout. A worker that died without releasing it (killed, crashed host)
/// stops blocking after this, so the marker can never wedge the trigger.
const INFLIGHT_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Cap on the emptiness probe. Without it a saturated server leaves every
/// spawned backfill parked on an open connection, which piles more load on it.
const EMPTINESS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn inflight_path(data_dir: &Path, cwd: &Path) -> PathBuf {
    let mut path = sentinel_path(data_dir, cwd).into_os_string();
    path.push(".inflight");
    PathBuf::from(path)
}

fn inflight_lock_path(data_dir: &Path, cwd: &Path) -> PathBuf {
    let mut path = inflight_path(data_dir, cwd).into_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

/// Unpredictable ownership nonce for one in-flight marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InflightClaim(String);

impl InflightClaim {
    fn new() -> Self {
        // UUID v4 draws from the operating system CSPRNG. The simple form is
        // path- and argv-safe while retaining 122 random bits.
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    fn parse(raw: &str) -> Option<Self> {
        uuid::Uuid::parse_str(raw)
            .ok()
            .map(|token| Self(token.simple().to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Run one marker mutation while holding the checkout's advisory lock.
///
/// Hook calls must remain non-blocking, so contention or an unusable state
/// directory fails closed instead of waiting. The lock file intentionally
/// remains in place: unlinking an advisory-lock file can split contenders
/// across two inodes and destroy serialization.
fn with_inflight_lock<T>(data_dir: &Path, cwd: &Path, operation: impl FnOnce() -> T) -> Option<T> {
    let lock_path = inflight_lock_path(data_dir, cwd);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    lock.try_lock_exclusive().ok()?;
    let result = operation();
    let _ = fs2::FileExt::unlock(&lock);
    Some(result)
}

/// Claim the right to spawn the automatic backfill for `cwd`.
///
/// Returns an ownership token when this caller created the marker and must
/// spawn (or call [`release_inflight`] if the spawn fails). The token travels
/// to the detached worker and prevents a delayed old worker from refreshing or
/// releasing a newer stale takeover. `O_CREAT|O_EXCL` plus the per-checkout
/// lock makes a burst of SessionStart hooks launch exactly one backfill.
pub(crate) fn claim_inflight(data_dir: &Path, cwd: &Path) -> Option<InflightClaim> {
    let path = inflight_path(data_dir, cwd);
    let claim = InflightClaim::new();
    with_inflight_lock(data_dir, cwd, || {
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write as _;
                    if file.write_all(claim.as_str().as_bytes()).is_err() {
                        let _ = std::fs::remove_file(&path);
                        return None;
                    }
                    return Some(claim.clone());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age >= INFLIGHT_STALE_AFTER);
                    if !stale {
                        return None;
                    }
                    let mut aside = path.clone().into_os_string();
                    aside.push(format!(".stale.{}", claim.as_str()));
                    if std::fs::rename(&path, &aside).is_err() {
                        return None;
                    }
                    let _ = std::fs::remove_file(&aside);
                }
                // An unwritable state dir must not turn into a spawn storm.
                Err(_) => return None,
            }
        }
        None
    })
    .flatten()
}

/// Refresh the marker only while `claim` still owns it.
fn refresh_inflight(data_dir: &Path, cwd: &Path, claim: &InflightClaim) -> bool {
    with_inflight_lock(data_dir, cwd, || {
        let path = inflight_path(data_dir, cwd);
        let Ok(contents) = std::fs::read_to_string(&path) else {
            return false;
        };
        if contents != claim.as_str() {
            return false;
        }
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(SystemTime::now()))
            .is_ok()
    })
    .unwrap_or(false)
}

/// Drop the in-progress marker taken by [`claim_inflight`].
pub(crate) fn release_inflight(data_dir: &Path, cwd: &Path, claim: &InflightClaim) -> bool {
    with_inflight_lock(data_dir, cwd, || {
        let path = inflight_path(data_dir, cwd);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(claim.as_str()) {
            return false;
        }
        std::fs::remove_file(path).is_ok()
    })
    .unwrap_or(false)
}

struct InflightGuard<'a> {
    data_dir: &'a Path,
    cwd: &'a Path,
    claim: InflightClaim,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        release_inflight(self.data_dir, self.cwd, &self.claim);
    }
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
    run_in(config, args, cwd).await
}

/// [`run`] for the checkout at `cwd`.
async fn run_in(config: &Config, args: crate::cli::BackfillArgs, cwd: PathBuf) -> Result<()> {
    // The SessionStart hook took the in-progress marker before spawning this
    // worker; free it however the run ends so a failed run can be retried.
    let inflight_claim = args
        .inflight_claim
        .as_deref()
        .and_then(InflightClaim::parse);
    let _inflight = inflight_claim.clone().map(|claim| InflightGuard {
        data_dir: &config.data_dir,
        cwd: &cwd,
        claim,
    });
    if let Some(claim) = &inflight_claim {
        refresh_inflight(&config.data_dir, &cwd, claim);
    }
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
    let mut report = BackfillReport {
        workspace: workspace.clone(),
        project: project.clone(),
        dry_run: args.dry_run,
        ..BackfillReport::default()
    };

    // The live hook's admission gate and `[capture] ignore_paths`, applied to
    // the import as the hook applies them to live events. A checkout the hook
    // would emit nothing for (allowlist capture mode without a marker) imports
    // nothing, before the server is contacted at all.
    let capture = TranscriptCapture::for_checkout(&config.data_dir, &cwd);
    if !capture.admits_capture() {
        report.skipped_not_admitted = true;
        return finish(&args, &report);
    }
    let endpoint = backfill_endpoint(config, &args).await?;

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
    let mut session_dirs: Vec<(ManagedHarness, Option<PathBuf>)> = Vec::new();
    for session in selected.into_iter().rev() {
        if report.imported_events >= MAX_EVENTS_TOTAL {
            report.skipped_for_cap += 1;
            continue;
        }
        let session_dir = cached_session_dir(&mut session_dirs, session.harness, || {
            import_session_dir(&home, &cwd, session.harness)
        });
        match import_one_with_session_dir(
            &endpoint,
            &workspace,
            &project,
            &home,
            &cwd,
            &capture,
            &session,
            session_dir.as_deref(),
        )
        .await
        {
            Ok(events) => {
                report.imported_sessions += 1;
                report.imported_events += events;
                if let Some(claim) = &inflight_claim {
                    refresh_inflight(&config.data_dir, &cwd, claim);
                }
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
    let probe = tokio::time::timeout(
        EMPTINESS_PROBE_TIMEOUT,
        get_json::<ByAgentResponse>(
            endpoint,
            "/admin/sessions/by-agent",
            &[("workspace", workspace), ("project", project)],
        ),
    )
    .await
    .unwrap_or_else(|_| {
        Err(anyhow::anyhow!(
            "the server did not answer within {}s",
            EMPTINESS_PROBE_TIMEOUT.as_secs()
        ))
    });
    match probe {
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
        if let Some(id) = only_session {
            // A named recovery target may be older than every session in the
            // bounded listing. Resolve it directly, with the adapter's same
            // checkout checks, rather than broadening the automatic scan.
            if let Ok(Some(session)) =
                find_native_session(harness, home, cwd, session_dir.as_deref(), id)
            {
                out.push(SessionRef {
                    harness,
                    native_session_id: session.native_session_id,
                    updated_at: session.updated_at,
                });
            }
            continue;
        }
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
/// capture, and `capture` drops or strips tool events the checkout's
/// `[capture] ignore_paths` excludes before anything is posted.
#[cfg(all(test, unix))]
async fn import_one(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    home: &Path,
    cwd: &Path,
    capture: &TranscriptCapture,
    session: &SessionRef,
) -> Result<usize> {
    let session_dir = import_session_dir(home, cwd, session.harness);
    import_one_with_session_dir(
        endpoint,
        workspace,
        project,
        home,
        cwd,
        capture,
        session,
        session_dir.as_deref(),
    )
    .await
}

fn import_session_dir(home: &Path, cwd: &Path, harness: ManagedHarness) -> Option<PathBuf> {
    let roots = LaunchRoots { home, cwd };
    build_launch_plan_with_env(harness, None, Vec::new(), None, &[], Some(roots))
        .ok()
        .and_then(|plan| plan.session_dir)
}

fn cached_session_dir(
    cache: &mut Vec<(ManagedHarness, Option<PathBuf>)>,
    harness: ManagedHarness,
    resolve: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some((_, session_dir)) = cache.iter().find(|(cached, _)| *cached == harness) {
        return session_dir.clone();
    }
    let session_dir = resolve();
    cache.push((harness, session_dir.clone()));
    session_dir
}

#[allow(clippy::too_many_arguments)]
async fn import_one_with_session_dir(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    home: &Path,
    cwd: &Path,
    capture: &TranscriptCapture,
    session: &SessionRef,
    session_dir: Option<&Path>,
) -> Result<usize> {
    import_exact_session(
        endpoint,
        workspace,
        project,
        home,
        cwd,
        session.harness,
        &session.native_session_id,
        session_dir,
        None,
        capture,
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
    capture: &TranscriptCapture,
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
        capture,
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
    capture: &TranscriptCapture,
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
        capture,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn import_exported_transcript(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    harness: ManagedHarness,
    native_session_id: &str,
    recovery_interval_id: Option<&str>,
    transcript: ai_memory_workstream::ExportedTranscript,
    capture: &TranscriptCapture,
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
    let captures = capture.decide(&transcript.events);
    for ((event, occurred_at), capture) in transcript
        .events
        .iter()
        .zip(&resolved.per_event)
        .zip(&captures)
    {
        if let Some(mapped) = map_event(
            recovery_interval_id,
            sid,
            event,
            occurred_at.as_deref(),
            capture,
        ) {
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
    // The import is partial by design here, so it is reported on stderr even
    // under `--quiet`: the automatic worker's stderr is the operator's log.
    let skipped = transcript
        .losses
        .iter()
        .filter(|loss| loss.starts_with(OVERSIZED_RECORD_LOSS))
        .count();
    if skipped > 0 {
        eprintln!(
            "ai-memory: {} session {}: skipped {skipped} oversized transcript record(s); the rest was imported",
            harness.as_str(),
            run::display_session_id(display_id(sid))
        );
    }
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
        // Only user prompts are forwarded below, and `[capture] ignore_paths`
        // governs file-tool events (the live hook does not filter prompts
        // either), so no capture decision applies here.
        let Some(mapped) = map_event(
            None,
            native_session_id,
            event,
            occurred_at.as_deref(),
            &EventCapture::Keep,
        ) else {
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
///
/// `capture` is the checkout's capture decision for this event: a dropped
/// event maps to nothing, and a metadata-only one carries the hook's
/// metadata-only body with its `_ai_memory_capture` protocol, which the server
/// validates and rebuilds from its allowlist exactly as for a live hook. A kept
/// event carries no protocol: its body is a normalized transcript record, not
/// the native tool payload the server's re-inspection reads, so a claim would
/// be judged against the wrong schema and stripped; the server never receives
/// the ignore patterns, so the client's decision is the only one possible.
/// An unrecognized tool's event carries only its family, as the live server
/// stores for an unknown-family tool, never its arguments or output.
fn map_event(
    recovery_interval_id: Option<&str>,
    session_id: &str,
    event: &NewWorkstreamEvent,
    occurred_at: Option<&str>,
    capture: &EventCapture,
) -> Option<MappedEvent> {
    let content = event.content.trim();
    if content.is_empty() {
        return None;
    }
    let decision = match capture {
        EventCapture::Drop => return None,
        EventCapture::MetadataOnly(decision) => Some(decision),
        EventCapture::Keep | EventCapture::Unrecognized => None,
    };
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
            let body = match decision {
                Some(decision) => {
                    let mut body = metadata_only_body(Some(session_id), None, decision);
                    if let Some(object) = body.as_object_mut() {
                        object.insert("occurred_at".into(), occurred_at.into());
                    }
                    body
                }
                None if matches!(capture, EventCapture::Unrecognized) => {
                    let outcome = if event.kind == WorkstreamEventKind::ToolResult {
                        "\noutcome: unknown"
                    } else {
                        ""
                    };
                    serde_json::json!({
                        "session_id": session_id,
                        "title": "tool unknown",
                        "message": format!("tool_family: unknown{outcome}"),
                        "occurred_at": occurred_at,
                    })
                }
                None => serde_json::json!({
                    "session_id": session_id,
                    "title": first_line(content),
                    "message": event.content,
                    "occurred_at": occurred_at,
                }),
            };
            Some(MappedEvent {
                event: format!("backfill.{source_event}"),
                source_event: Some(source_event),
                ingest_key,
                body,
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
    if report.skipped_not_admitted {
        println!(
            "ai-memory: capture mode is allowlist and this checkout has no .ai-memory.toml marker opting in (or its marker is unreadable) — nothing to backfill for {}/{}.",
            report.workspace, report.project
        );
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
    fn session_directory_resolution_is_cached_per_harness() {
        let mut cache = Vec::new();
        let mut calls = 0;
        let first = cached_session_dir(&mut cache, ManagedHarness::Crush, || {
            calls += 1;
            Some(PathBuf::from("crush-data"))
        });
        let second = cached_session_dir(&mut cache, ManagedHarness::Crush, || {
            calls += 1;
            Some(PathBuf::from("wrong"))
        });

        assert_eq!(first, Some(PathBuf::from("crush-data")));
        assert_eq!(second, first);
        assert_eq!(calls, 1);
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

    /// #1287: a burst of SessionStart hooks in one directory must spawn one
    /// backfill. Only the first claim wins; another directory is unaffected.
    #[test]
    fn only_one_session_start_claims_the_backfill_per_checkout() {
        let data = tempfile::tempdir().unwrap();
        let app = Path::new("/home/me/projects/app");
        let other = Path::new("/home/me/projects/other");

        let claim = claim_inflight(data.path(), app).unwrap();
        assert!(
            claim_inflight(data.path(), app).is_none(),
            "second burst member"
        );
        assert!(
            claim_inflight(data.path(), app).is_none(),
            "third burst member"
        );
        assert!(
            claim_inflight(data.path(), other).is_some(),
            "separate checkout"
        );

        assert!(release_inflight(data.path(), app, &claim));
        assert!(
            claim_inflight(data.path(), app).is_some(),
            "free again after release"
        );
    }

    /// A worker that died without releasing its marker must not block the
    /// checkout forever.
    #[test]
    fn a_stale_inflight_marker_is_replaced() {
        let data = tempfile::tempdir().unwrap();
        let cwd = Path::new("/home/me/projects/app");
        claim_inflight(data.path(), cwd).unwrap();

        let marker = inflight_path(data.path(), cwd);
        let old = SystemTime::now() - INFLIGHT_STALE_AFTER - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();

        assert!(
            claim_inflight(data.path(), cwd).is_some(),
            "stale marker is taken over"
        );
        assert!(
            claim_inflight(data.path(), cwd).is_none(),
            "the new claim holds"
        );
    }

    /// A worker's refresh makes an old marker fresh again, so a long backfill
    /// keeps its claim; the refreshed marker retains its ownership token.
    #[test]
    fn a_refreshed_marker_is_not_taken_over() {
        let data = tempfile::tempdir().unwrap();
        let cwd = Path::new("/home/me/projects/app");
        let claim = claim_inflight(data.path(), cwd).unwrap();
        let marker = inflight_path(data.path(), cwd);
        let old = SystemTime::now() - INFLIGHT_STALE_AFTER - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(refresh_inflight(data.path(), cwd, &claim));
        assert!(
            claim_inflight(data.path(), cwd).is_none(),
            "a live worker keeps it"
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), claim.as_str());
    }

    /// Once a stale marker has been taken over, the previous worker cannot
    /// refresh or release the replacement marker even if it resumes later.
    #[test]
    fn stale_takeover_refuses_the_previous_owners_refresh_and_release() {
        let data = tempfile::tempdir().unwrap();
        let cwd = Path::new("/home/me/projects/app");
        let previous = claim_inflight(data.path(), cwd).unwrap();
        let marker = inflight_path(data.path(), cwd);
        let old = SystemTime::now() - INFLIGHT_STALE_AFTER - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();

        let replacement = claim_inflight(data.path(), cwd).unwrap();
        assert_ne!(previous, replacement);
        assert!(!refresh_inflight(data.path(), cwd, &previous));
        assert!(!release_inflight(data.path(), cwd, &previous));
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            replacement.as_str(),
            "the replacement owner remains intact"
        );
        assert!(refresh_inflight(data.path(), cwd, &replacement));
        assert!(release_inflight(data.path(), cwd, &replacement));
        assert!(!marker.exists());
    }

    /// The stale takeover leaves no `.stale.*` file behind and exactly one
    /// marker in place.
    #[test]
    fn a_stale_takeover_leaves_one_marker_and_no_debris() {
        let data = tempfile::tempdir().unwrap();
        let cwd = Path::new("/home/me/projects/app");
        claim_inflight(data.path(), cwd).unwrap();
        let marker = inflight_path(data.path(), cwd);
        let old = SystemTime::now() - INFLIGHT_STALE_AFTER - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();
        claim_inflight(data.path(), cwd).unwrap();
        let names: Vec<String> = std::fs::read_dir(marker.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "marker plus its stable lock: {names:?}");
        assert!(
            names.iter().any(|name| name.ends_with(".inflight")),
            "{names:?}"
        );
        assert!(
            names.iter().any(|name| name.ends_with(".inflight.lock")),
            "{names:?}"
        );
        assert!(
            names.iter().all(|name| !name.contains(".stale.")),
            "{names:?}"
        );
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
            &EventCapture::Keep,
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
            CwdProbes, HookState, IngestGates, IngestRateLimiter, ProjectCacheStore,
            SubagentSessionSet, hook_router,
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
            cwd_probes: CwdProbes::default(),
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
            ingest_drop_list: ai_memory_hooks::IngestDropList::default(),
            record_tool_names: false,
            skip_wiki_checkpoints: false,
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
            CwdProbes, HookState, IngestGates, IngestRateLimiter, ProjectCacheStore,
            SubagentSessionSet, hook_router,
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
            cwd_probes: CwdProbes::default(),
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
            ingest_drop_list: ai_memory_hooks::IngestDropList::default(),
            record_tool_names: false,
            skip_wiki_checkpoints: false,
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
            &EventCapture::Keep,
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
            &EventCapture::Keep,
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
                &EventCapture::Keep,
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
                map_event(
                    None,
                    "sid",
                    &event(kind, None, "boundary"),
                    None,
                    &EventCapture::Keep
                )
                .is_none(),
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

    // These exercise the real listing cap with more than 200 files/rows.
    // Setup, listing and cleanup take more than a second alone on macOS,
    // even with the SQLite fixture inserted in one transaction.
    mod slow_explicit_session_lookup {
        use super::*;

        #[tokio::test]
        async fn collect_local_sessions_explicit_file_target_bypasses_listing_cap() {
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let session_dir = home
                .path()
                .join(".claude/projects")
                .join(ai_memory_workstream::claude_project_dir_name(cwd.path()));
            std::fs::create_dir_all(&session_dir).unwrap();
            let target = "11111111-2222-4333-8444-000000000000";
            for index in 0..=PER_HARNESS_SCAN_LIMIT {
                let id = format!("11111111-2222-4333-8444-{index:012}");
                let header = serde_json::json!({ "sessionId": id, "cwd": cwd.path() });
                let path = session_dir.join(format!("{id}.jsonl"));
                std::fs::write(&path, format!("{header}\n")).unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(
                        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + index as u64),
                    ))
                    .unwrap();
            }

            let (listed, limit_hit) =
                collect_local_sessions_with(home.path(), cwd.path(), None, |_| None).await;
            assert_eq!(listed.len(), PER_HARNESS_SCAN_LIMIT);
            assert_eq!(limit_hit, vec![ManagedHarness::Claude]);
            assert!(
                listed
                    .iter()
                    .all(|session| session.native_session_id != target)
            );

            let (selected, limit_hit) =
                collect_local_sessions_with(home.path(), cwd.path(), Some(target), |_| None).await;
            assert_eq!(selected.len(), 1, "the explicit old session must be found");
            assert_eq!(selected[0].harness, ManagedHarness::Claude);
            assert_eq!(selected[0].native_session_id, target);
            assert_eq!(
                selected[0].updated_at,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            );
            assert!(
                limit_hit.is_empty(),
                "an exact lookup is not a capped listing"
            );

            for missing in ["no-such-session", "../outside", ""] {
                let (selected, _) =
                    collect_local_sessions_with(home.path(), cwd.path(), Some(missing), |_| None)
                        .await;
                assert!(selected.is_empty(), "must not select {missing:?}");
            }
            let foreign = tempfile::tempdir().unwrap();
            let (selected, _) =
                collect_local_sessions_with(home.path(), foreign.path(), Some(target), |_| None)
                    .await;
            assert!(
                selected.is_empty(),
                "the exact ID must still belong to this cwd"
            );
        }

        #[tokio::test]
        async fn collect_local_sessions_explicit_sql_target_bypasses_listing_cap() {
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let store = home.path().join("opencode");
            std::fs::create_dir_all(&store).unwrap();
            let mut db = rusqlite::Connection::open(store.join("opencode.db")).unwrap();
            let transaction = db.transaction().unwrap();
            for table in ["session", "session_v2"] {
                transaction.execute_batch(&format!(
                "CREATE TABLE {table}(id TEXT PRIMARY KEY, directory TEXT, time_updated INTEGER);"
            ))
            .unwrap();
                for index in 0..=PER_HARNESS_SCAN_LIMIT {
                    transaction
                        .execute(
                            &format!("INSERT INTO {table} VALUES (?1, ?2, ?3)"),
                            rusqlite::params![
                                format!("{table}-{index:04}"),
                                cwd.path().to_string_lossy(),
                                1_700_000_000_000_i64 + index as i64,
                            ],
                        )
                        .unwrap();
                }
            }
            transaction.commit().unwrap();
            let session_dir_for = |harness| {
                matches!(
                    harness,
                    ManagedHarness::OpenCode | ManagedHarness::OpenCode2
                )
                .then(|| store.clone())
            };
            let (listed, limit_hit) =
                collect_local_sessions_with(home.path(), cwd.path(), None, session_dir_for).await;
            assert_eq!(listed.len(), 2 * PER_HARNESS_SCAN_LIMIT);
            assert_eq!(
                limit_hit,
                vec![ManagedHarness::OpenCode, ManagedHarness::OpenCode2]
            );

            for (table, harness) in [
                ("session", ManagedHarness::OpenCode),
                ("session_v2", ManagedHarness::OpenCode2),
            ] {
                let target = format!("{table}-0000");
                let (selected, limit_hit) = collect_local_sessions_with(
                    home.path(),
                    cwd.path(),
                    Some(&target),
                    session_dir_for,
                )
                .await;
                assert_eq!(selected.len(), 1, "the explicit old {harness:?} session");
                assert_eq!(selected[0].harness, harness);
                assert_eq!(selected[0].native_session_id, target);
                assert_eq!(
                    selected[0].updated_at,
                    SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
                );
                assert!(limit_hit.is_empty());

                let foreign = tempfile::tempdir().unwrap();
                let (selected, _) = collect_local_sessions_with(
                    home.path(),
                    foreign.path(),
                    Some(&target),
                    session_dir_for,
                )
                .await;
                assert!(
                    selected.is_empty(),
                    "must not import a foreign {harness:?} session"
                );
            }
        }
    }

    #[tokio::test]
    async fn collect_local_sessions_explicit_crush_target_keeps_subagents_excluded() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let store = cwd.path().join(".crush");
        std::fs::create_dir_all(&store).unwrap();
        let db = rusqlite::Connection::open(store.join("crush.db")).unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, parent_session_id TEXT, updated_at INTEGER); \
             INSERT INTO sessions VALUES ('parent', NULL, 1700000000); \
             INSERT INTO sessions VALUES ('child', 'parent', 1700000001); \
             INSERT INTO sessions VALUES ('invalid-time', NULL, -1);",
        )
        .unwrap();
        let session_dir_for = |harness| (harness == ManagedHarness::Crush).then(|| store.clone());
        let (selected, limit_hit) =
            collect_local_sessions_with(home.path(), cwd.path(), Some("parent"), session_dir_for)
                .await;
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].harness, ManagedHarness::Crush);
        assert_eq!(selected[0].native_session_id, "parent");
        assert_eq!(
            selected[0].updated_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        );
        assert!(limit_hit.is_empty());
        for id in ["child", "invalid-time", "missing"] {
            let (selected, _) =
                collect_local_sessions_with(home.path(), cwd.path(), Some(id), session_dir_for)
                    .await;
            assert!(selected.is_empty(), "must not select {id}");
        }
    }

    /// GHSA-gcpc-88hm-3xqr: a planted Claude transcript reads one file under
    /// the checkout's `[capture] ignore_paths` and one outside it. The
    /// excluded read's call and output never reach `/hook/batch`; the public
    /// read's do. A malformed read is posted as the hook's metadata-only body.
    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_never_posts_tool_content_the_capture_policy_excludes() {
        use axum::{Router, routing::post};
        use std::sync::{Arc, Mutex};

        let posted = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = posted.clone();
        let app = Router::new().route(
            "/hook/batch",
            post(
                move |axum::Json(items): axum::Json<Vec<serde_json::Value>>| {
                    let captured = captured.clone();
                    async move {
                        let accepted = items.len();
                        captured.lock().unwrap().extend(items);
                        let results: Vec<_> = (0..accepted)
                            .map(|index| serde_json::json!({ "index": index, "outcome": "stored" }))
                            .collect();
                        axum::Json(serde_json::json!({ "accepted": accepted, "results": results }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join(".ai-memory.toml"),
            "[capture]\nignore_paths = [\"docs/adr/**\"]\n",
        )
        .unwrap();
        let session_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join(repo.path().to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&session_dir).unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        let cwd = repo.path().to_string_lossy();
        let record = |uuid: &str, kind: &str, content: serde_json::Value| {
            serde_json::json!({
                "type": kind, "uuid": uuid, "sessionId": sid, "cwd": cwd,
                "timestamp": "2026-09-01T10:00:00Z",
                "message": {"role": kind, "content": content},
            })
            .to_string()
        };
        let read = |id: &str, input: serde_json::Value| serde_json::json!({"type": "tool_use", "id": id, "name": "Read", "input": input});
        let output = |id: &str, text: &str| serde_json::json!({"type": "tool_result", "tool_use_id": id, "content": text});
        let lines = [
            record("u1", "user", serde_json::json!("read the adr and lib")),
            record(
                "a1",
                "assistant",
                serde_json::json!([
                    read(
                        "toolu_adr",
                        serde_json::json!({"file_path": repo.path().join("docs/adr/1.md")})
                    ),
                    read(
                        "toolu_pub",
                        serde_json::json!({"file_path": repo.path().join("src/lib.rs")})
                    ),
                    read(
                        "toolu_bad",
                        serde_json::json!({"file_path": 7, "note": "ARGSENTINEL"})
                    ),
                ]),
            ),
            record(
                "u2",
                "user",
                serde_json::json!([
                    output("toolu_adr", "ADRSENTINEL private decision"),
                    output("toolu_pub", "PUBLICCONTROL public code"),
                    output("toolu_bad", "BADSENTINEL unprovable output"),
                ]),
            ),
        ];
        std::fs::write(
            session_dir.join(format!("{sid}.jsonl")),
            lines.join("\n") + "\n",
        )
        .unwrap();

        let endpoint = ServerEndpoint::for_hook_target(format!("http://{address}"), None);
        let session = SessionRef {
            harness: ManagedHarness::Claude,
            native_session_id: sid.into(),
            updated_at: SystemTime::now(),
        };
        let capture = TranscriptCapture::for_checkout(home.path(), repo.path());
        import_one(
            &endpoint,
            "ws",
            "proj",
            home.path(),
            repo.path(),
            &capture,
            &session,
        )
        .await
        .unwrap();
        server.abort();

        let posted = posted.lock().unwrap();
        let text = serde_json::to_string(&*posted).unwrap();
        for sentinel in ["ADRSENTINEL", "docs/adr", "ARGSENTINEL", "BADSENTINEL"] {
            assert!(!text.contains(sentinel), "{sentinel} was posted: {text}");
        }
        assert!(text.contains("PUBLICCONTROL"), "public output: {text}");
        assert!(text.contains("src/lib.rs"), "public call: {text}");
        let stripped: Vec<_> = posted
            .iter()
            .filter(|item| item["body"].get("_ai_memory_capture").is_some())
            .collect();
        assert_eq!(
            stripped.len(),
            2,
            "the malformed call and its output: {text}"
        );
        for item in stripped {
            assert_eq!(
                item["body"]["_ai_memory_capture"]["disposition"],
                "metadata-only"
            );
            assert_eq!(item["body"]["tool_family"], "file");
            assert_eq!(item["body"]["tool_call_id"], "toolu_bad");
        }
    }

    /// Allowlist capture mode (#446) makes an unmarked checkout emit nothing
    /// from the live hook, so a manual backfill of it must not contact the
    /// server at all. Controls: the same checkout with a marker, and the same
    /// unmarked checkout under the default denylist mode, both proceed to the
    /// server.
    #[tokio::test]
    async fn allowlist_mode_backfills_nothing_from_an_unmarked_checkout() {
        use axum::{Router, extract::Request};
        use std::sync::{Arc, Mutex};

        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = requests.clone();
        let app = Router::new().fallback(move |request: Request| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(request.uri().path().to_owned());
                axum::Json(serde_json::json!({ "by_agent": [], "accepted": 0 }))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let attempt = |mode: Option<&'static str>, marked: bool| {
            let requests = requests.clone();
            async move {
                let home = tempfile::tempdir().unwrap();
                let data_dir = tempfile::tempdir().unwrap();
                let repo = tempfile::tempdir().unwrap();
                if let Some(mode) = mode {
                    std::fs::write(
                        data_dir.path().join(super::super::hook::CAPTURE_MODE_FILE),
                        mode,
                    )
                    .unwrap();
                }
                if marked {
                    std::fs::write(repo.path().join(".ai-memory.toml"), "[capture]\n").unwrap();
                }
                let mut config =
                    crate::config::Config::load(None, Some(home.path().to_path_buf())).unwrap();
                config.data_dir = data_dir.path().to_path_buf();
                config.home_dir = Some(home.path().to_string_lossy().into_owned());
                config.server_url = format!("http://{address}");
                config.auth.bearer_token = None;
                let mut args = spawned_args(None, None);
                args.auto = false;
                args.workspace = Some("ws".into());
                args.project = Some("proj".into());
                requests.lock().unwrap().clear();
                run_in(&config, args, repo.path().to_path_buf())
                    .await
                    .unwrap();
                requests.lock().unwrap().clone()
            }
        };

        assert!(
            attempt(Some("allowlist\n"), false).await.is_empty(),
            "allowlist + unmarked checkout must not reach the server"
        );
        assert!(
            !attempt(Some("allowlist\n"), true).await.is_empty(),
            "control: a marked checkout proceeds under allowlist"
        );
        assert!(
            !attempt(None, false).await.is_empty(),
            "control: denylist (default) is unchanged"
        );
        server.abort();
    }

    /// Plant `lines` as one native session and backfill it from `repo`,
    /// returning everything posted to `/hook/batch` as one JSON string.
    #[cfg(unix)]
    async fn backfill_posted(
        harness: ManagedHarness,
        home: &Path,
        data_dir: &Path,
        repo: &Path,
        sid: &str,
        lines: &[serde_json::Value],
    ) -> String {
        use axum::{Router, routing::post};
        use std::sync::{Arc, Mutex};

        let session_file = match harness {
            ManagedHarness::Claude => home
                .join(".claude/projects")
                .join(repo.to_string_lossy().replace('/', "-"))
                .join(format!("{sid}.jsonl")),
            ManagedHarness::Codex => home
                .join(".codex/sessions/2026/09/01")
                .join(format!("rollout-2026-09-01T10-00-00-{sid}.jsonl")),
            other => panic!("no fixture layout for {}", other.as_str()),
        };
        std::fs::create_dir_all(session_file.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(&session_file, body).unwrap();

        let posted = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = posted.clone();
        let app = Router::new().route(
            "/hook/batch",
            post(
                move |axum::Json(items): axum::Json<Vec<serde_json::Value>>| {
                    let captured = captured.clone();
                    async move {
                        let accepted = items.len();
                        captured.lock().unwrap().extend(items);
                        let results: Vec<_> = (0..accepted)
                            .map(|index| serde_json::json!({ "index": index, "outcome": "stored" }))
                            .collect();
                        axum::Json(serde_json::json!({ "accepted": accepted, "results": results }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = ServerEndpoint::for_hook_target(format!("http://{address}"), None);
        let session = SessionRef {
            harness,
            native_session_id: sid.into(),
            updated_at: SystemTime::now(),
        };
        let capture = TranscriptCapture::for_checkout(data_dir, repo);
        import_one(&endpoint, "ws", "proj", home, repo, &capture, &session)
            .await
            .unwrap();
        server.abort();
        serde_json::to_string(&*posted.lock().unwrap()).unwrap()
    }

    #[cfg(unix)]
    fn marked_checkout(marker: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".ai-memory.toml"), marker).unwrap();
        dir
    }

    /// Codex records its shell tool as `exec_command` with `{cmd, workdir}`;
    /// the live hook receives it as `Bash`. A command whose `workdir`-relative
    /// argument names an ignored path never reaches `/hook/batch`, call or
    /// output; the same command run from a public directory does.
    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_inspects_codex_exec_command_as_the_hook_shell_call() {
        let home = tempfile::tempdir().unwrap();
        let repo = marked_checkout("[capture]\nignore_paths = [\"docs/adr/**\"]\n");
        for dir in ["docs/adr", "src/adr"] {
            std::fs::create_dir_all(repo.path().join(dir)).unwrap();
        }
        let sid = "019a0000-0000-7000-8000-000000000001";
        let item = |payload: serde_json::Value| {
            serde_json::json!({
                "type": "response_item", "timestamp": "2026-09-01T10:00:00Z", "payload": payload,
            })
        };
        let exec = |id: &str, workdir: &str| {
            item(serde_json::json!({
                "type": "function_call", "name": "exec_command", "call_id": id,
                "arguments": serde_json::json!({
                    "cmd": "cat adr/x.md", "workdir": repo.path().join(workdir),
                }).to_string(),
            }))
        };
        let output = |id: &str, text: &str| {
            item(serde_json::json!({"type": "function_call_output", "call_id": id, "output": text}))
        };
        let lines = [
            serde_json::json!({"type": "session_meta", "payload": {"id": sid, "cwd": repo.path()}}),
            item(serde_json::json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "show both adr notes"}],
            })),
            exec("call_adr", "docs"),
            output("call_adr", "ADRSENTINEL private decision"),
            exec("call_pub", "src"),
            output("call_pub", "PUBLICCONTROL public note"),
        ];
        let posted = backfill_posted(
            ManagedHarness::Codex,
            home.path(),
            home.path(),
            repo.path(),
            sid,
            &lines,
        )
        .await;
        assert!(!posted.contains("ADRSENTINEL"), "excluded output: {posted}");
        assert!(!posted.contains("/docs\\\""), "excluded call: {posted}");
        assert!(posted.contains("PUBLICCONTROL"), "public output: {posted}");
        assert!(posted.contains("/src"), "public call: {posted}");
    }

    /// A Claude Code transcript record: `content` is the message content.
    #[cfg(unix)]
    fn claude_record(
        sid: &str,
        uuid: &str,
        cwd: &Path,
        role: &str,
        content: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": role, "uuid": uuid, "sessionId": sid, "cwd": cwd,
            "timestamp": "2026-09-01T10:00:00Z",
            "message": {"role": role, "content": content},
        })
    }

    #[cfg(unix)]
    fn claude_tool(name: &str, id: &str, input: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    #[cfg(unix)]
    fn claude_output(id: &str, text: &str) -> serde_json::Value {
        serde_json::json!({"type": "tool_result", "tool_use_id": id, "content": text})
    }

    /// Claude Code records each line's `cwd`, which follows the session as it
    /// moves. The live hook resolves a relative argument from that cwd, so an
    /// import must too: `cat adr/x.md` run from `docs/` reads an ignored file
    /// and is never posted, while the same command run from the checkout root
    /// names a public path and is.
    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_resolves_each_call_from_its_record_cwd() {
        let home = tempfile::tempdir().unwrap();
        let repo = marked_checkout("[capture]\nignore_paths = [\"docs/adr/**\"]\n");
        let docs = repo.path().join("docs");
        std::fs::create_dir_all(docs.join("adr")).unwrap();
        let sid = "11111111-2222-3333-4444-555555555556";
        let cat = |id| claude_tool("Bash", id, serde_json::json!({"command": "cat adr/x.md"}));
        let lines = [
            claude_record(sid, "u1", repo.path(), "user", "read the adr".into()),
            claude_record(
                sid,
                "a1",
                &docs,
                "assistant",
                serde_json::json!([cat("toolu_docs")]),
            ),
            claude_record(
                sid,
                "u2",
                &docs,
                "user",
                serde_json::json!([claude_output("toolu_docs", "ADRSENTINEL private decision")]),
            ),
            claude_record(
                sid,
                "a2",
                repo.path(),
                "assistant",
                serde_json::json!([cat("toolu_root")]),
            ),
            claude_record(
                sid,
                "u3",
                repo.path(),
                "user",
                serde_json::json!([claude_output("toolu_root", "PUBLICCONTROL root note")]),
            ),
        ];
        let posted = backfill_posted(
            ManagedHarness::Claude,
            home.path(),
            home.path(),
            repo.path(),
            sid,
            &lines,
        )
        .await;
        assert!(!posted.contains("ADRSENTINEL"), "excluded output: {posted}");
        assert!(posted.contains("PUBLICCONTROL"), "control output: {posted}");
        assert_eq!(
            posted.matches("cat adr/x.md").count(),
            2,
            "only the root call, as title and message: {posted}"
        );
    }

    /// A call the live hook would reroute to another checkout must pass that
    /// checkout's admission gate and reach the same server, or it is never
    /// posted: under allowlist mode an unmarked repository admits nothing, a
    /// marker selecting an unregistered profile emits nothing, and a marker
    /// selecting a registered profile sends to another server. Control: a read
    /// in a marked checkout of the same project with no server selection is
    /// posted.
    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_drops_reroutes_the_live_hook_would_not_deliver_here() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            data_dir.path().join(super::super::hook::CAPTURE_MODE_FILE),
            "allowlist\n",
        )
        .unwrap();
        let name = crate::server_profiles::ProfileName::parse("team-e").unwrap();
        crate::server_profiles::add(data_dir.path(), &name, "https://e.example", &[], Some("E"))
            .unwrap();
        let source = marked_checkout("workspace = \"w\"\nproject = \"p\"\n");
        let unmarked = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(unmarked.path())
            .status()
            .unwrap();
        assert!(init.success());
        let unknown_profile = marked_checkout("server = \"team-b\"\n");
        let other_server = marked_checkout("server = \"team-e\"\n");
        let same_server = marked_checkout("workspace = \"w\"\nproject = \"p\"\n");
        let sid = "11111111-2222-3333-4444-555555555557";
        let reads = [
            (&unmarked, "UNMARKEDSENTINEL"),
            (&unknown_profile, "UNKNOWNPROFILESENTINEL"),
            (&other_server, "OTHERSERVERSENTINEL"),
            (&same_server, "SAMESERVERCONTROL"),
        ];
        let mut lines = vec![claude_record(
            sid,
            "u0",
            source.path(),
            "user",
            "read the notes".into(),
        )];
        for (index, (repo, text)) in reads.iter().enumerate() {
            let file = repo.path().join(format!("{text}.txt"));
            std::fs::write(&file, text).unwrap();
            let id = format!("toolu_{index}");
            lines.push(claude_record(
                sid,
                &format!("a{index}"),
                source.path(),
                "assistant",
                serde_json::json!([claude_tool(
                    "Read",
                    &id,
                    serde_json::json!({"file_path": file})
                )]),
            ));
            lines.push(claude_record(
                sid,
                &format!("u{}", index + 1),
                source.path(),
                "user",
                serde_json::json!([claude_output(&id, &format!("{text} content"))]),
            ));
        }
        let posted = backfill_posted(
            ManagedHarness::Claude,
            home.path(),
            data_dir.path(),
            source.path(),
            sid,
            &lines,
        )
        .await;
        for sentinel in [
            "UNMARKEDSENTINEL",
            "UNKNOWNPROFILESENTINEL",
            "OTHERSERVERSENTINEL",
        ] {
            assert!(
                !posted.contains(sentinel),
                "{sentinel} was posted: {posted}"
            );
        }
        assert!(
            posted.contains("SAMESERVERCONTROL content"),
            "control: {posted}"
        );
        assert!(posted.contains("read the notes"), "prompt kept: {posted}");
    }

    /// The live server stores only the family of an unknown-family tool, with
    /// no policy at all. A backfilled Codex MCP call and its output therefore
    /// post only `tool_family: unknown`, never their arguments or output, while
    /// a recognized shell call's output is posted as before.
    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_posts_no_content_for_unrecognized_tools() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let sid = "019a0000-0000-7000-8000-000000000002";
        let item = |payload: serde_json::Value| {
            serde_json::json!({
                "type": "response_item", "timestamp": "2026-09-01T10:00:00Z", "payload": payload,
            })
        };
        let call = |id: &str, name: &str, arguments: serde_json::Value| {
            item(serde_json::json!({
                "type": "function_call", "name": name, "call_id": id,
                "arguments": arguments.to_string(),
            }))
        };
        let output = |id: &str, text: &str| {
            item(serde_json::json!({"type": "function_call_output", "call_id": id, "output": text}))
        };
        let lines = [
            serde_json::json!({"type": "session_meta", "payload": {"id": sid, "cwd": repo.path()}}),
            call(
                "call_mcp",
                "mcp__vault__fetch",
                serde_json::json!({"query": "ARGSENTINEL"}),
            ),
            output("call_mcp", "MCPSENTINEL private record"),
            call(
                "call_pub",
                "exec_command",
                serde_json::json!({"cmd": "cargo test"}),
            ),
            output("call_pub", "PUBLICCONTROL test result"),
        ];
        let posted = backfill_posted(
            ManagedHarness::Codex,
            home.path(),
            home.path(),
            repo.path(),
            sid,
            &lines,
        )
        .await;
        for sentinel in ["ARGSENTINEL", "MCPSENTINEL", "mcp__vault"] {
            assert!(
                !posted.contains(sentinel),
                "{sentinel} was posted: {posted}"
            );
        }
        assert_eq!(
            posted.matches("\"title\":\"tool unknown\"").count(),
            2,
            "the call and its output are still recorded: {posted}"
        );
        assert!(posted.contains("PUBLICCONTROL"), "control: {posted}");
    }

    /// A reroute is delivered only when the destination's marker selects the
    /// same registered server profile as the source and the same project: a
    /// different registered profile is dropped, and so is a checkout of
    /// another project on the same profile, whose events the live hook files
    /// under that project. The same holds for records written in such a
    /// checkout, whatever their tool calls name.
    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_keeps_reroutes_to_the_same_server_profile_only() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let checkout = |name: &str, profile: &str, project: &str| {
            let dir = base.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(".ai-memory.toml"),
                format!("server = \"{profile}\"\nworkspace = \"w\"\nproject = \"{project}\"\n"),
            )
            .unwrap();
            dir
        };
        let source = checkout("source", "team-a", "p");
        let same = checkout("same", "team-a", "p");
        let other = checkout("other", "team-e", "p");
        let sibling = checkout("sibling", "team-a", "q");
        let root = |dir: &Path| dir.to_string_lossy().into_owned();
        for (name, roots) in [
            ("team-a", vec![root(&source), root(&same), root(&sibling)]),
            ("team-e", vec![root(&other)]),
        ] {
            let name = crate::server_profiles::ProfileName::parse(name).unwrap();
            crate::server_profiles::add(
                data_dir.path(),
                &name,
                "https://a.example",
                &roots,
                Some("T"),
            )
            .unwrap();
        }
        let sid = "11111111-2222-3333-4444-555555555558";
        let mut lines = vec![claude_record(sid, "u0", &source, "user", "read".into())];
        for (index, (repo, text)) in [
            (&same, "SAMEPROFILECONTROL"),
            (&other, "OTHERPROFILESENTINEL"),
            (&sibling, "OTHERPROJECTSENTINEL"),
        ]
        .into_iter()
        .enumerate()
        {
            let file = repo.join(format!("{text}.txt"));
            let id = format!("toolu_{index}");
            lines.push(claude_record(
                sid,
                &format!("a{index}"),
                &source,
                "assistant",
                serde_json::json!([claude_tool(
                    "Read",
                    &id,
                    serde_json::json!({"file_path": file})
                )]),
            ));
            lines.push(claude_record(
                sid,
                &format!("u{}", index + 1),
                &source,
                "user",
                serde_json::json!([claude_output(&id, &format!("{text} content"))]),
            ));
        }
        lines.push(claude_record(
            sid,
            "moved",
            &sibling,
            "user",
            "SIBLINGPROMPTSENTINEL".into(),
        ));
        let posted = backfill_posted(
            ManagedHarness::Claude,
            home.path(),
            data_dir.path(),
            &source,
            sid,
            &lines,
        )
        .await;
        assert!(
            posted.contains("SAMEPROFILECONTROL content"),
            "same profile: {posted}"
        );
        for sentinel in [
            "OTHERPROFILESENTINEL",
            "OTHERPROJECTSENTINEL",
            "SIBLINGPROMPTSENTINEL",
        ] {
            assert!(!posted.contains(sentinel), "{sentinel}: {posted}");
        }
    }

    /// Claude Code writes a monitored command's streamed output and a
    /// background agent's report as user turns, and its compact summary too;
    /// no prompt hook fires for any of them, so none is posted as a prompt.
    /// Control: the operator's own prompt is.
    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_posts_no_task_notification_or_compact_summary_as_a_prompt() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let sid = "11111111-2222-3333-4444-555555555559";
        let mut monitor = claude_record(
            sid,
            "n1",
            repo.path(),
            "user",
            "<task-notification>\n<event>MONITORSENTINEL stdout line</event>\n</task-notification>"
                .into(),
        );
        monitor["origin"] = serde_json::json!({"kind": "task-notification"});
        let report = claude_record(
            sid,
            "n2",
            repo.path(),
            "user",
            "<task-notification>\n<result>AGENTSENTINEL report</result>\n</task-notification>"
                .into(),
        );
        let mut summary = claude_record(
            sid,
            "c1",
            repo.path(),
            "user",
            "This session is being continued. SUMMARYSENTINEL".into(),
        );
        summary["isCompactSummary"] = true.into();
        let lines = [
            claude_record(sid, "u1", repo.path(), "user", "PROMPTCONTROL".into()),
            monitor,
            report,
            summary,
        ];
        let posted = backfill_posted(
            ManagedHarness::Claude,
            home.path(),
            home.path(),
            repo.path(),
            sid,
            &lines,
        )
        .await;
        for sentinel in ["MONITORSENTINEL", "AGENTSENTINEL", "SUMMARYSENTINEL"] {
            assert!(!posted.contains(sentinel), "{sentinel}: {posted}");
        }
        assert!(posted.contains("PROMPTCONTROL"), "control: {posted}");
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
            inflight_claim: None,
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

    /// The worker frees the marker the hook claimed, however it ends, so the
    /// next SessionStart can spawn again when one is needed.
    #[tokio::test]
    async fn an_auto_run_releases_the_marker_its_hook_claimed() {
        let home = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let mut config =
            crate::config::Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data_dir.path().to_path_buf();
        config.backfill_on_start = false;
        config.server_url = "http://127.0.0.1:9".to_string();
        let cwd = data_dir.path().join("checkout");
        std::fs::create_dir_all(&cwd).unwrap();
        let claim = claim_inflight(&config.data_dir, &cwd).unwrap();
        let mut args = spawned_args(None, None);
        args.inflight_claim = Some(claim.as_str().to_owned());

        run_in(&config, args, cwd.clone()).await.unwrap();
        assert!(
            !inflight_path(&config.data_dir, &cwd).exists(),
            "the worker must release its marker"
        );
        assert!(
            claim_inflight(&config.data_dir, &cwd).is_some(),
            "free for the next run"
        );
    }
}
