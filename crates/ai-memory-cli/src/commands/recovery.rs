//! Downtime recovery — the offline journal and `ai-memory recover`.
//!
//! When the server is unreachable, `ai-memory run` degrades instead of
//! failing: hooks spool locally and a degraded launch still runs the harness.
//! The spool returns by itself, but two kinds of history used to need a
//! hand-typed repair and, for the transcript, were simply lost:
//!
//! - a run whose lease existed but whose transcript `finish` could not reach
//!   the dead server (`finish-failed`);
//! - a degraded launch's session, which no server run ever recorded
//!   (`degraded-run`).
//!
//! Both now append a bounded, metadata-only entry to a local journal
//! (`<data_dir>/recovery-journal.json`). `ai-memory recover`, run once the
//! server is back, drains the spool and then works the journal: it re-exports
//! each `finish-failed` session's transcript from the native store through the
//! ordinary managed-run finish path, and replays each degraded launch's exact
//! native transcript through the ordinary sanitized `/hook/batch` backfill
//! path. Harnesses without transcript export are journaled explicitly as
//! spool-only and recovery never invokes their exporter. Stable ingest and
//! event keys make both paths idempotent. Entries clear as they succeed;
//! failures stay journaled with their reason.
//!
//! The journal never holds transcript content or secrets — only locators,
//! ordered semantic digests, and delivery evidence. Recovery never regenerates
//! the original repository checkpoint, so later repository state is not
//! attributed to the interrupted run. Transcripts are re-exported from the native store
//! at recover time instead of being copied into the data dir: the store is
//! the same durable local source the online path reads seconds after the
//! child exits, and re-export reuses the exact adapter path rather than
//! maintaining a second serialized copy of private conversation content.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use ai_memory_core::{ManagedRunId, Sanitizer};
use ai_memory_workstream::TranscriptCapability;
use anyhow::{Context as _, Result, anyhow, bail};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::cli::RecoverArgs;
use crate::commands::hook_spool::{self, DrainLockWait, MaxAttempts, RestoreOutcome};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, ServerProbe, probe_server};

/// Journal file name inside the data dir.
const JOURNAL_FILE: &str = "recovery-journal.json";
/// Exclusive-mutation lock file name inside the data dir.
const JOURNAL_LOCK_FILE: &str = "recovery-journal.lock";
/// Journal schema version.
const JOURNAL_VERSION: u32 = 1;
/// Most entries kept. Appends at capacity are refused so recovery work is
/// never silently evicted.
pub(crate) const MAX_JOURNAL_ENTRIES: usize = 256;
/// Refuse to load or overwrite a journal larger than this, mirroring the
/// client project registry's guard: a swollen file is corruption, not backlog.
const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
/// Reasons remembered per entry across recover attempts.
const MAX_ENTRY_FAILURES: usize = 3;
const MAX_FAILURE_REASON_BYTES: usize = 2_048;

/// How the entry's history was cut off from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JournalKind {
    /// A degraded launch made while the server was unreachable: no managed
    /// run ever existed, so recovery replays its exact native transcript
    /// through the hook backfill path after draining the spool.
    DegradedRun,
    /// A run whose transcript import failed because the server died after the
    /// child exited: the run still exists server-side, so recovery re-exports
    /// the transcript and finishes it again.
    FinishFailed,
}

impl JournalKind {
    /// Stable label for reports.
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DegradedRun => "degraded-run",
            Self::FinishFailed => "finish-failed",
        }
    }
}

/// One bounded, metadata-only record of history the server missed.
///
/// Constructed by `run` at the moment the loss is detected; consumed by
/// `recover` once the server answers again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JournalEntry {
    /// Unique entry id; recovery clears per entry.
    pub(crate) id: String,
    /// When the loss was recorded (RFC 3339, UTC).
    pub(crate) recorded_at: String,
    /// Inclusive lower bound for hook events belonging to this launch.
    pub(crate) interval_started_ms: u64,
    /// Inclusive upper bound for hook events belonging to this launch.
    pub(crate) interval_ended_ms: u64,
    /// How the run was cut off.
    pub(crate) kind: JournalKind,
    /// Managed harness name (`ManagedHarness::as_str`), including the exact
    /// Kiro engine flavor so recovery reads the right native store.
    pub(crate) harness: String,
    /// Native session locator for transcript re-export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) native_session_id: Option<String>,
    /// Session-store override the launch resolved (`--session-dir` and kin),
    /// without which the native transcript may be unreachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_dir: Option<String>,
    /// Canonical checkout directory the run launched from.
    pub(crate) cwd: String,
    /// Workspace name for degraded transcript replay and reporting.
    pub(crate) workspace: String,
    /// Project name for degraded transcript replay and reporting.
    pub(crate) project: String,
    /// Server URL the run targeted, for a mismatch warning at recover time.
    pub(crate) server_url: String,
    /// Managed run id (`finish-failed` only): the recovery import target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<String>,
    /// Ordered semantic digests for this exact transcript interval. `Some([])`
    /// proves a successful empty export; `None` means export was unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) event_digests: Option<Vec<String>>,
    /// Whether this typed harness capability permits spool evidence only and
    /// must never invoke a native transcript exporter.
    #[serde(default)]
    pub(crate) spool_only: bool,
    /// Exact native adapter cursor captured before a resumed launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_cursor: Option<String>,
    /// Exact native adapter cursor captured immediately after child exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) final_cursor: Option<String>,
    /// Whether correlated hook events have ever been observed locally.
    #[serde(default)]
    pub(crate) correlated_events_seen: bool,
    /// Whether a prior pass durably delivered all correlated hook evidence.
    #[serde(default)]
    pub(crate) correlated_delivery_confirmed: bool,
    /// Whether a prior pass lost potentially correlated hook evidence.
    #[serde(default)]
    pub(crate) correlated_loss: bool,
    /// Child exit code, replayed with the recovered import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    /// Reasons from previous recover attempts, newest last, bounded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) failures: Vec<String>,
}

/// Wire shape of the journal file.
#[derive(Debug, Serialize, Deserialize)]
struct JournalFile {
    version: u32,
    entries: Vec<JournalEntry>,
}

/// `<data_dir>/recovery-journal.json`.
#[must_use]
pub(crate) fn journal_path(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_FILE)
}

/// Read the journal; a missing file is an empty journal, a corrupt or swollen
/// one is an error the caller surfaces (never silently reset: the file is the
/// only record of what the server missed).
pub(crate) fn load_journal(data_dir: &Path) -> Result<Vec<JournalEntry>> {
    let path = journal_path(data_dir);
    refuse_symlink(data_dir, &path)?;
    let mut file = match open_journal_read(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    if file.metadata()?.len() > MAX_JOURNAL_BYTES {
        bail!(
            "recovery journal {} exceeds the {} byte limit",
            path.display(),
            MAX_JOURNAL_BYTES
        );
    }
    let mut bytes = Vec::new();
    use std::io::Read as _;
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        bail!(
            "recovery journal {} exceeds the {} byte limit",
            path.display(),
            MAX_JOURNAL_BYTES
        );
    }
    let journal: JournalFile = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parsing {}; refusing to discard the recorded recovery work",
            path.display()
        )
    })?;
    if journal.version != JOURNAL_VERSION {
        bail!(
            "unsupported recovery journal version {} in {} (expected {JOURNAL_VERSION})",
            journal.version,
            path.display()
        );
    }
    if journal.entries.len() > MAX_JOURNAL_ENTRIES {
        bail!(
            "recovery journal {} exceeds the {} entry limit",
            path.display(),
            MAX_JOURNAL_ENTRIES
        );
    }
    Ok(journal.entries)
}

/// Append one entry under the exclusive lock. Atomic (tmp + rename),
/// owner-only, and symlink-refusing so a swapped file cannot be trampled.
pub(crate) fn append_journal_entry(data_dir: &Path, entry: JournalEntry) -> Result<()> {
    mutate_journal(data_dir, |entries| {
        if entries.len() >= MAX_JOURNAL_ENTRIES {
            bail!(
                "recovery journal is full ({MAX_JOURNAL_ENTRIES} entries); run `ai-memory recover` before another degraded launch"
            );
        }
        entries.push(entry);
        Ok(())
    })
}

/// Rewrite the journal under the exclusive lock with `mutate` applied. The
/// lock spans the whole read-modify-write so two finishing launches cannot
/// lose each other's entry.
fn mutate_journal(
    data_dir: &Path,
    mutate: impl FnOnce(&mut Vec<JournalEntry>) -> Result<()>,
) -> Result<()> {
    validate_journal_entry_limit(data_dir)?;
    let path = journal_path(data_dir);
    let lock = open_private_lock(data_dir, &data_dir.join(JOURNAL_LOCK_FILE))?;
    lock.lock_exclusive()
        .with_context(|| format!("locking {}", data_dir.join(JOURNAL_LOCK_FILE).display()))?;
    let mut entries = load_journal(data_dir)?;
    mutate(&mut entries)?;
    let journal = JournalFile {
        version: JOURNAL_VERSION,
        entries,
    };
    let mut rendered =
        serde_json::to_vec_pretty(&journal).context("serializing the recovery journal")?;
    rendered.push(b'\n');
    refuse_symlink(data_dir, &path)?;
    if rendered.len() as u64 > MAX_JOURNAL_BYTES {
        bail!(
            "recovery journal {} would exceed the {} byte limit",
            path.display(),
            MAX_JOURNAL_BYTES
        );
    }
    write_journal_atomic(data_dir, &path, &rendered)
        .with_context(|| format!("writing {}", path.display()))?;
    make_private(&path)?;
    Ok(())
}

fn write_journal_atomic(data_dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("recovery journal path has no parent"))?;
    refuse_symlink(data_dir, parent)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".recovery-journal.")
        .tempfile_in(parent)?;
    {
        use std::io::Write as _;
        temp.write_all(bytes)?;
        temp.as_file().sync_data()?;
    }
    refuse_symlink(data_dir, path)?;
    temp.persist(path).map_err(|error| error.error)?;
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn validate_journal_entry_limit(data_dir: &Path) -> Result<()> {
    let path = journal_path(data_dir);
    refuse_symlink(data_dir, &path)?;
    match fs::metadata(&path) {
        Ok(metadata) if metadata.len() > MAX_JOURNAL_BYTES => bail!(
            "recovery journal {} exceeds the {} byte limit",
            path.display(),
            MAX_JOURNAL_BYTES
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn open_journal_read(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn open_private_lock(data_dir: &Path, path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        refuse_symlink(data_dir, parent)?;
        crate::commands::path_util::create_private_dir(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        refuse_symlink(data_dir, parent)?;
    }
    refuse_symlink(data_dir, path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    refuse_symlink(data_dir, path)?;
    make_private(path)?;
    Ok(file)
}

/// Refuse a symlink at or below `data_dir` on the way to `path`.
///
/// The data dir is the trust root. Its ancestors are the operator's and the
/// system's (`/var` is a symlink to `/private/var` on macOS, `/home` is often
/// a symlink), so they are not inspected. The data dir itself, and every
/// component inside it, must not be a symlink: that is where another local
/// user could plant one. The comparison is lexical; canonicalizing would
/// rewrite the ancestors this deliberately trusts.
fn refuse_symlink(data_dir: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(data_dir).with_context(|| {
        format!(
            "recovery journal path {} is outside the data dir {}",
            path.display(),
            data_dir.display()
        )
    })?;
    if !refuse_symlink_component(data_dir)? {
        return Ok(());
    }
    let mut current = data_dir.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("recovery journal path contains a non-normal component");
        };
        current.push(component);
        if !refuse_symlink_component(&current)? {
            break;
        }
    }
    Ok(())
}

/// Fail if `path` is a symlink; `Ok(false)` once it does not exist (nothing
/// below a missing component can exist either).
fn refuse_symlink_component(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("refusing recovery journal symlink {}", path.display());
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

#[cfg(unix)]
fn make_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(windows)]
fn make_private(path: &Path) -> Result<()> {
    let identity = std::process::Command::new("whoami")
        .output()
        .context("resolving the Windows recovery-journal owner")?;
    if !identity.status.success() {
        bail!("could not resolve the Windows recovery-journal owner");
    }
    let owner =
        String::from_utf8(identity.stdout).context("reading the Windows recovery-journal owner")?;
    let grant = format!("{}:(F)", owner.trim());
    let restricted = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(grant)
        .output()
        .with_context(|| format!("restricting permissions on {}", path.display()))?;
    if !restricted.status.success() {
        bail!("could not restrict permissions on {}", path.display());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn make_private(_path: &Path) -> Result<()> {
    Ok(())
}

/// Count journal entries for status lines; an unreadable journal reports zero
/// with its error carried back, never blocking status output.
pub(crate) fn journal_summary(data_dir: &Path) -> (usize, usize, usize, Option<String>) {
    match load_journal(data_dir) {
        Ok(entries) => {
            let degraded = entries
                .iter()
                .filter(|entry| entry.kind == JournalKind::DegradedRun)
                .count();
            let finish_failed = entries.len() - degraded;
            (entries.len(), degraded, finish_failed, None)
        }
        Err(error) => (0, 0, 0, Some(format!("{error:#}"))),
    }
}

/// Budgets for one `recover` invocation; injectable so tests can prove the
/// bounds bite.
#[derive(Debug, Clone, Copy)]
struct RecoverBudgets {
    /// Total spool-drain budget. Generous but bounded, matching the hook
    /// background-drain default: recovery must not hang forever on a wedged
    /// server, but a large outage backlog must fully flush.
    drain_total: Duration,
    /// Per-event POST timeout while draining.
    drain_event_timeout: Duration,
    /// Per-journal-entry transcript HTTP budget.
    entry_timeout: Duration,
    /// Total journal HTTP budget.
    journal_total: Duration,
    /// How long recovery waits on another process's drain lock.
    drain_lock_wait: Duration,
}

impl Default for RecoverBudgets {
    fn default() -> Self {
        Self {
            drain_total: Duration::from_secs(5 * 60),
            drain_event_timeout: Duration::from_secs(30),
            entry_timeout: Duration::from_secs(60),
            journal_total: Duration::from_secs(10 * 60),
            drain_lock_wait: Duration::from_secs(30),
        }
    }
}

/// What recovery did for one journal entry.
#[derive(Debug)]
enum EntryRecovery {
    /// Re-exported and imported; the count is newly imported content events.
    Imported(usize),
    /// The harness has no transcript exporter and its complete spool was delivered.
    RecoveredViaSpool,
    /// Recovery failed; the entry stays journaled with the reason.
    Failed(String),
}

/// Spool-drain counts for the report.
#[derive(Debug, Default, Clone, Copy, Serialize)]
struct SpoolOutcome {
    sent: usize,
    remaining: usize,
    dropped: usize,
}

/// One recovered (or failed) session, for the report.
#[derive(Debug, Serialize)]
struct SessionOutcome {
    kind: &'static str,
    harness: String,
    native_session_id: Option<String>,
    workspace: String,
    project: String,
    recorded_at: String,
    outcome: &'static str,
    imported_events: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// The full `ai-memory recover` report.
#[derive(Debug, Serialize)]
struct RecoverReport {
    server: String,
    reachable: bool,
    spool: SpoolOutcome,
    spool_lock_busy: bool,
    spool_incomplete: bool,
    journal_pending: usize,
    journal_degraded: usize,
    journal_finish_failed: usize,
    sessions: Vec<SessionOutcome>,
    journal_remaining: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    journal_error: Option<String>,
}

impl RecoverReport {
    fn failures(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.outcome == "failed")
            .count()
    }

    fn imported_total(&self) -> usize {
        self.sessions.iter().map(|s| s.imported_events).sum()
    }
}

/// Run the `recover` subcommand and return its process exit code.
///
/// # Errors
/// Returns an error only for local infrastructure failures.
pub async fn run(config: &Config, args: RecoverArgs) -> Result<i32> {
    let report = recover(config, &RecoverBudgets::default()).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }
    Ok(i32::from(
        report.reachable
            && (report.spool_incomplete || report.failures() > 0 || report.journal_error.is_some()),
    ))
}

async fn recover(config: &Config, budgets: &RecoverBudgets) -> Result<RecoverReport> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let spool_dir = hook_spool::spool_dir(&config.data_dir);
    let (mut journal, journal_error) = match load_journal(&config.data_dir) {
        Ok(entries) => (entries, None),
        // A broken journal still leaves the spool drain: the two recover
        // independent halves of the outage.
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    let journal_degraded = journal
        .iter()
        .filter(|entry| entry.kind == JournalKind::DegradedRun)
        .count();
    let journal_finish_failed = journal.len() - journal_degraded;

    if let ServerProbe::Unreachable(_) = probe_server(&endpoint).await {
        return Ok(RecoverReport {
            server: endpoint.url.clone(),
            reachable: false,
            spool: SpoolOutcome {
                remaining: hook_spool::spool_len(&spool_dir),
                ..SpoolOutcome::default()
            },
            spool_lock_busy: false,
            spool_incomplete: false,
            journal_pending: journal.len(),
            journal_degraded,
            journal_finish_failed,
            sessions: Vec::new(),
            journal_remaining: journal.len(),
            journal_error,
        });
    }

    let sanitizer = Sanitizer::new(&config.sanitize).map_err(|error| anyhow!(error.to_string()))?;
    let journal_pending = journal.len();
    let mut quarantined = HashMap::new();
    let drain_lock = match hook_spool::acquire_drain_lock(
        &spool_dir,
        DrainLockWait::Bounded(budgets.drain_lock_wait),
    ) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return Ok(lock_busy_report(
                &endpoint,
                &spool_dir,
                journal_pending,
                journal_degraded,
                journal_finish_failed,
                journal_error,
            ));
        }
        Err(error) => {
            return Err(error).context("acquiring the recovery spool lock");
        }
    };
    let restore_quarantine = |quarantined: HashMap<String, hook_spool::QuarantinedSpool>| {
        let mut first_error = None;
        for (_, quarantine) in quarantined {
            if let Err(error) = quarantine.restore(&spool_dir)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    };
    for entry in &journal {
        if entry.kind != JournalKind::DegradedRun && !entry.spool_only {
            continue;
        }
        let Some(native_session_id) = entry.native_session_id.as_deref() else {
            continue;
        };
        match hook_spool::quarantine_session_entries(
            config,
            &spool_dir,
            &entry.id,
            entry.interval_started_ms,
            entry.interval_ended_ms,
            &entry.server_url,
            super::run::harness_from_journal_name(&entry.harness)
                .map(|harness| harness.agent_kind().as_str())
                .unwrap_or(&entry.harness),
            &entry.workspace,
            &entry.project,
            Path::new(&entry.cwd),
            native_session_id,
        ) {
            Ok(quarantine) => {
                quarantined.insert(entry.id.clone(), quarantine);
            }
            Err(error) => {
                restore_quarantine(quarantined)
                    .context("restoring previously quarantined hook events")?;
                return Err(error).context("quarantining correlated hook events");
            }
        }
    }
    let mut evidence_started = Vec::new();
    for entry in &mut journal {
        if entry.kind != JournalKind::DegradedRun && !entry.spool_only {
            continue;
        }
        let correlated_count = quarantined.get(&entry.id).map_or(0, |value| value.len());
        if correlated_count > 0 {
            entry.correlated_events_seen = true;
            entry.correlated_loss = true;
            evidence_started.push(entry.id.clone());
        }
    }
    if !evidence_started.is_empty() {
        update_journal_evidence(&config.data_dir, &journal)?;
    }
    for entry in &mut journal {
        if !entry_supports_transcript_export(entry)
            && let Some(quarantine) = quarantined.get(&entry.id)
            && quarantine.len() > 0
        {
            let outcome = quarantine
                .drain_strict(
                    &config.data_dir,
                    budgets.drain_total,
                    budgets.drain_event_timeout,
                    MaxAttempts::new(config.hook_spool.max_attempts),
                )
                .await;
            if outcome.durable > 0 {
                entry.correlated_events_seen = true;
            }
            if outcome.dropped > 0 {
                entry.correlated_loss = true;
            }
            if outcome.remaining == 0
                && outcome.dropped == 0
                && outcome.durable > 0
                && quarantine.complete_spool_only()?
            {
                entry.correlated_delivery_confirmed = true;
                entry.correlated_loss = false;
            }
        }
    }
    update_journal_evidence(&config.data_dir, &journal)?;
    let drained = hook_spool::drain_until_quiescent_locked_with_live_token(
        &spool_dir,
        &config.data_dir,
        budgets.drain_total,
        budgets.drain_event_timeout,
        config.auth.bearer_token.as_deref(),
        MaxAttempts::new(config.hook_spool.max_attempts),
    )
    .await;
    let spool = SpoolOutcome {
        sent: drained.sent,
        remaining: drained.remaining,
        dropped: drained.dropped,
    };
    let spool_incomplete = spool_is_incomplete(&spool);
    if spool.dropped > 0 {
        eprintln!(
            "ai-memory: recovery: {count} hook event(s) were dropped; recovery remains incomplete unless an exact correlated transcript replaces them",
            count = spool.dropped
        );
    }

    let mut sessions = Vec::with_capacity(journal.len());
    let mut cleared: Vec<String> = Vec::with_capacity(journal.len());
    let mut annotated: Vec<(String, String)> = Vec::new();
    let journal_started = Instant::now();
    for entry in journal {
        let mut recovery = if spool_incomplete {
            EntryRecovery::Failed(format!(
                "hook spool recovery was incomplete ({} queued, {} dropped); transcript recovery was deferred",
                spool.remaining, spool.dropped
            ))
        } else if let Some(timeout) = entry_budget(journal_started, budgets) {
            if timeout.is_zero() {
                failed_reason(
                    &sanitizer,
                    "journal entry recovery timed out; rerun to continue",
                )
            } else {
                match tokio::time::timeout(
                    timeout,
                    recover_entry(config, &endpoint, &sanitizer, &entry),
                )
                .await
                {
                    Ok(recovery) => recovery,
                    Err(_) => failed_reason(
                        &sanitizer,
                        "journal entry recovery timed out; rerun to continue",
                    ),
                }
            }
        } else {
            failed_reason(
                &sanitizer,
                "total recovery budget exhausted; rerun to continue",
            )
        };
        if let Some(quarantine) = quarantined.remove(&entry.id) {
            let restored = match &recovery {
                EntryRecovery::Imported(_) => {
                    quarantine.complete().map(|()| RestoreOutcome::Restored)
                }
                EntryRecovery::RecoveredViaSpool => Ok(RestoreOutcome::Restored),
                EntryRecovery::Failed(_) => quarantine.restore(&spool_dir),
            }
            .context("restoring quarantined hook events")?;
            if matches!(restored, RestoreOutcome::AlreadyDelivered) {
                // The completed marker is durable proof this interval's
                // correlated evidence reached the server on a prior pass;
                // the failure only deferred work there is none left of, so
                // the entry clears instead of staying journaled behind it.
                recovery = EntryRecovery::RecoveredViaSpool;
            }
        }
        let outcome = match &recovery {
            EntryRecovery::Imported(_) => "imported",
            EntryRecovery::RecoveredViaSpool => "recovered-via-spool",
            EntryRecovery::Failed(_) => "failed",
        };
        let imported_events = match &recovery {
            EntryRecovery::Imported(events) => *events,
            _ => 0,
        };
        let detail = match &recovery {
            EntryRecovery::Imported(_) | EntryRecovery::RecoveredViaSpool => None,
            EntryRecovery::Failed(note) => Some(note.clone()),
        };
        if let EntryRecovery::Failed(reason) = &recovery {
            annotated.push((entry.id.clone(), reason.clone()));
        } else {
            cleared.push(entry.id.clone());
        }
        sessions.push(SessionOutcome {
            kind: entry.kind.as_str(),
            harness: entry.harness.clone(),
            native_session_id: entry.native_session_id.clone(),
            workspace: entry.workspace.clone(),
            project: entry.project.clone(),
            recorded_at: entry.recorded_at.clone(),
            outcome,
            imported_events,
            detail,
        });
    }

    restore_quarantine(quarantined)?;
    drop(drain_lock);

    // (d) Clear succeeded entries; failures stay with their reason.
    let journal_remaining = finish_journal(&config.data_dir, &sanitizer, &cleared, &annotated)?;

    Ok(RecoverReport {
        server: endpoint.url.clone(),
        reachable: true,
        spool,
        spool_lock_busy: false,
        spool_incomplete,
        journal_pending,
        journal_degraded,
        journal_finish_failed,
        sessions,
        journal_remaining,
        journal_error,
    })
}

fn entry_budget(started: Instant, budgets: &RecoverBudgets) -> Option<Duration> {
    if budgets.entry_timeout.is_zero() {
        return Some(Duration::ZERO);
    }
    let remaining = budgets.journal_total.checked_sub(started.elapsed())?;
    (!remaining.is_zero()).then(|| budgets.entry_timeout.min(remaining))
}

fn spool_is_incomplete(spool: &SpoolOutcome) -> bool {
    spool.remaining > 0 || spool.dropped > 0
}

fn lock_busy_report(
    endpoint: &ServerEndpoint,
    spool_dir: &Path,
    journal_pending: usize,
    journal_degraded: usize,
    journal_finish_failed: usize,
    journal_error: Option<String>,
) -> RecoverReport {
    RecoverReport {
        server: endpoint.url.clone(),
        reachable: true,
        spool: SpoolOutcome {
            remaining: hook_spool::spool_len(spool_dir),
            ..SpoolOutcome::default()
        },
        spool_lock_busy: true,
        spool_incomplete: true,
        journal_pending,
        journal_degraded,
        journal_finish_failed,
        sessions: Vec::new(),
        journal_remaining: journal_pending,
        journal_error,
    }
}

async fn recover_entry(
    config: &Config,
    endpoint: &ServerEndpoint,
    sanitizer: &Sanitizer,
    entry: &JournalEntry,
) -> EntryRecovery {
    let recorded = entry.server_url.trim_end_matches('/');
    let configured_identity = endpoint.identity();
    let configured = configured_identity.trim_end_matches('/');
    if recorded.is_empty() || recorded != configured {
        return failed_reason(
            sanitizer,
            &format!("refusing to replay an entry recorded for {recorded:?} against {configured}"),
        );
    }
    if entry.spool_only {
        return recover_spool_only(sanitizer, entry);
    }
    match entry.kind {
        JournalKind::FinishFailed => {
            recover_finish_failed(config, endpoint, sanitizer, entry).await
        }
        JournalKind::DegradedRun => recover_degraded_run(config, endpoint, sanitizer, entry).await,
    }
}

/// Re-export the transcript from the native store and finish the run again.
/// Reuses `run`'s import machinery verbatim: the server dedups by stable event
/// id, so a full re-export cannot duplicate history a partial import landed.
fn recovery_session_dir(entry: &JournalEntry) -> Option<PathBuf> {
    entry.session_dir.as_deref().map(|path| {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(&entry.cwd).join(path)
        }
    })
}

async fn recover_finish_failed(
    config: &Config,
    endpoint: &ServerEndpoint,
    sanitizer: &Sanitizer,
    entry: &JournalEntry,
) -> EntryRecovery {
    let Some(run_id) = entry.run_id.as_deref() else {
        return failed_reason(sanitizer, "finish-failed entry records no run id");
    };
    if ManagedRunId::from_str(run_id).is_err() {
        return failed_reason(
            sanitizer,
            &format!("journal records an invalid run id {run_id}"),
        );
    }
    let Some(harness) = super::run::harness_from_journal_name(&entry.harness) else {
        return failed_reason(
            sanitizer,
            &format!("journal names unsupported harness '{}'", entry.harness),
        );
    };
    let Some(home) = super::run::native_home(config) else {
        return failed_reason(
            sanitizer,
            "the native harness home directory could not be located",
        );
    };
    let Some(expected_digests) = entry.event_digests.as_ref() else {
        return failed_reason(
            sanitizer,
            "finish-failed entry has no exact ordered transcript identity; refusing full transcript replay",
        );
    };
    let Some(final_cursor) = entry.final_cursor.as_deref() else {
        return failed_reason(
            sanitizer,
            "finish-failed entry has no exact post-run transcript cursor; refusing replay",
        );
    };
    let transcript = match entry.native_session_id.as_deref() {
        Some(native_session_id) => {
            match ai_memory_workstream::export_transcript_range(
                harness,
                &home,
                Path::new(&entry.cwd),
                recovery_session_dir(entry).as_deref(),
                native_session_id,
                entry.source_cursor.as_deref(),
                final_cursor,
            )
            .await
            {
                Ok(transcript) => transcript,
                Err(error) => {
                    return failed_reason(
                        sanitizer,
                        &format!("native transcript re-export failed: {error:#}"),
                    );
                }
            }
        }
        None => {
            return failed_reason(
                sanitizer,
                "finish-failed entry records no native transcript locator",
            );
        }
    };
    let run_path = format!("/workstream/runs/{run_id}/recover");
    let actual = ai_memory_workstream::transcript_interval_digests(&transcript.events);
    if &actual != expected_digests {
        return failed_reason(
            sanitizer,
            "native transcript changed since the failed finish; refusing non-identical replay",
        );
    }
    let transcript = ai_memory_workstream::ExportedTranscript {
        source_cursor: Some(final_cursor.to_owned()),
        ..transcript
    };
    match super::run::import_batches(
        endpoint,
        &run_path,
        transcript,
        ai_memory_core::WorkstreamCheckpoint::default(),
        entry.exit_code,
        sanitizer,
    )
    .await
    {
        Ok(imported) => EntryRecovery::Imported(imported),
        Err(error) => failed_reason(sanitizer, &format!("managed finish failed: {error:#}")),
    }
}

fn failed_reason(sanitizer: &Sanitizer, reason: &str) -> EntryRecovery {
    EntryRecovery::Failed(ai_memory_core::truncate_utf8_bytes(
        &sanitizer.scrub(reason),
        MAX_FAILURE_REASON_BYTES,
    ))
}

/// A degraded launch had no server run, so recover its exact native transcript
/// through the ordinary backfill ingress. That path emits stable keyed
/// session-start/content/session-end items through `/hook/batch`, making a
/// repeated recovery idempotent while still crossing the normal sanitizer and
/// consolidation boundary.
fn entry_supports_transcript_export(entry: &JournalEntry) -> bool {
    !entry.spool_only
        && super::run::harness_from_journal_name(&entry.harness)
            .is_some_and(|harness| harness.transcript_capability() == TranscriptCapability::Export)
}

fn recover_spool_only(sanitizer: &Sanitizer, entry: &JournalEntry) -> EntryRecovery {
    if entry.correlated_delivery_confirmed && !entry.correlated_loss {
        EntryRecovery::RecoveredViaSpool
    } else {
        failed_reason(
            sanitizer,
            "no durably delivered correlated hook evidence exists; retain this entry and repair the native session manually",
        )
    }
}

async fn recover_degraded_run(
    config: &Config,
    endpoint: &ServerEndpoint,
    sanitizer: &Sanitizer,
    entry: &JournalEntry,
) -> EntryRecovery {
    let Some(harness) = super::run::harness_from_journal_name(&entry.harness) else {
        return failed_reason(
            sanitizer,
            &format!("journal names unsupported harness '{}'", entry.harness),
        );
    };
    if harness.transcript_capability() == TranscriptCapability::SpoolOnly {
        return recover_spool_only(sanitizer, entry);
    }
    let Some(native_session_id) = entry.native_session_id.as_deref() else {
        return failed_reason(
            sanitizer,
            "no native session was recorded for this degraded run",
        );
    };
    let Some(home) = super::run::native_home(config) else {
        return failed_reason(
            sanitizer,
            "the native harness home directory could not be located",
        );
    };
    let Some(expected_digests) = entry.event_digests.as_deref() else {
        return failed_reason(
            sanitizer,
            "degraded-run entry has no exact ordered transcript identity; repair the native session manually",
        );
    };
    let Some(final_cursor) = entry.final_cursor.as_deref() else {
        return failed_reason(
            sanitizer,
            "degraded-run entry has no exact post-run transcript cursor; repair the native session manually",
        );
    };
    match super::backfill::import_exact_session_range(
        endpoint,
        &entry.workspace,
        &entry.project,
        &home,
        Path::new(&entry.cwd),
        harness,
        native_session_id,
        &entry.id,
        recovery_session_dir(entry).as_deref(),
        entry.source_cursor.as_deref(),
        final_cursor,
        expected_digests,
    )
    .await
    {
        Ok(imported) => EntryRecovery::Imported(imported),
        Err(error) => failed_reason(
            sanitizer,
            &format!("native transcript replay failed: {error:#}"),
        ),
    }
}

/// Apply clears and failure annotations in one locked rewrite; returns the
/// remaining entry count.
fn update_journal_evidence(data_dir: &Path, current: &[JournalEntry]) -> Result<()> {
    mutate_journal(data_dir, |entries| {
        for entry in entries {
            if let Some(updated) = current.iter().find(|candidate| candidate.id == entry.id) {
                entry.correlated_events_seen = updated.correlated_events_seen;
                entry.correlated_delivery_confirmed = updated.correlated_delivery_confirmed;
                entry.correlated_loss = updated.correlated_loss;
            }
        }
        Ok(())
    })
}

fn finish_journal(
    data_dir: &Path,
    sanitizer: &Sanitizer,
    cleared: &[String],
    annotated: &[(String, String)],
) -> Result<usize> {
    if cleared.is_empty() && annotated.is_empty() {
        return Ok(load_journal(data_dir).map_or(0, |entries| entries.len()));
    }
    let mut remaining = 0_usize;
    mutate_journal(data_dir, |entries| {
        entries.retain(|entry| !cleared.contains(&entry.id));
        for (id, reason) in annotated {
            if let Some(entry) = entries.iter_mut().find(|entry| &entry.id == id) {
                let reason = ai_memory_core::truncate_utf8_bytes(
                    &sanitizer.scrub(reason),
                    MAX_FAILURE_REASON_BYTES,
                );
                if !entry.failures.contains(&reason) {
                    entry.failures.push(reason);
                }
                let overflow = entry.failures.len().saturating_sub(MAX_ENTRY_FAILURES);
                if overflow > 0 {
                    entry.failures.drain(..overflow);
                }
            }
        }
        remaining = entries.len();
        Ok(())
    })?;
    Ok(remaining)
}

fn print_report(report: &RecoverReport) {
    if !report.reachable {
        println!(
            "ai-memory: server at {} is unreachable — nothing to recover yet.",
            report.server
        );
        if let Some(error) = &report.journal_error {
            println!("  journal: unreadable ({error})");
        } else {
            println!(
                "  journal: {} entr{} await{} recovery ({} degraded launch(es), {} failed \
                 transcript import(s))",
                report.journal_pending,
                if report.journal_pending == 1 {
                    "y"
                } else {
                    "ies"
                },
                if report.journal_pending == 1 { "s" } else { "" },
                report.journal_degraded,
                report.journal_finish_failed,
            );
        }
        println!(
            "  spool:   {} hook event(s) queued locally",
            report.spool.remaining
        );
        println!(
            "Start the server (or fix AI_MEMORY_SERVER_URL), then run `ai-memory recover` again."
        );
        return;
    }
    println!("ai-memory: recovering against {}…", report.server);
    if report.spool_lock_busy {
        println!("  spool: another drainer held the lock; transcript recovery was deferred");
    } else {
        println!(
            "  spool: {} hook event(s) delivered, {} remain queued, {} dropped",
            report.spool.sent, report.spool.remaining, report.spool.dropped
        );
    }
    if report.spool_incomplete {
        println!(
            "  recovery is incomplete because hook events remain queued or were dropped; exact correlated transcripts may still replace quarantined events"
        );
    }
    for session in &report.sessions {
        let target = session
            .native_session_id
            .as_deref()
            .unwrap_or("(no session id)");
        let mut line = format!(
            "  [{}] {} {} in {}/{} — {}",
            session.kind,
            session.harness,
            target,
            session.workspace,
            session.project,
            session.outcome,
        );
        if session.imported_events > 0 {
            line.push_str(&format!(" ({} events)", session.imported_events));
        }
        println!("{line}");
        if let Some(detail) = &session.detail {
            println!("      {detail}");
        }
    }
    println!(
        "recovered {} event(s) across {} journal entr{}; {} failed; {} remain",
        report.imported_total(),
        report.sessions.len(),
        if report.sessions.len() == 1 {
            "y"
        } else {
            "ies"
        },
        report.failures(),
        report.journal_remaining,
    );
    if let Some(error) = &report.journal_error {
        println!("recovery journal error: {error}");
    }
    if report.failures() > 0 {
        println!(
            "failed entries stay journaled with their reason; fix the cause and re-run \
             `ai-memory recover` (re-running is safe — imports dedup)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(id: &str, kind: JournalKind) -> JournalEntry {
        JournalEntry {
            id: id.to_string(),
            recorded_at: "2026-10-07T12:00:00Z".to_string(),
            interval_started_ms: 1_000,
            interval_ended_ms: 2_000,
            kind,
            harness: "claude".to_string(),
            native_session_id: Some("sess-1".to_string()),
            session_dir: None,
            cwd: "/tmp/repo".to_string(),
            workspace: "ws".to_string(),
            project: "proj".to_string(),
            server_url: "http://127.0.0.1:49374".to_string(),
            run_id: (kind == JournalKind::FinishFailed)
                .then(|| "12345678-1234-4234-9234-123456789abe".to_string()),
            event_digests: Some(Vec::new()),
            spool_only: false,
            source_cursor: None,
            final_cursor: None,
            correlated_events_seen: false,
            correlated_delivery_confirmed: false,
            correlated_loss: false,
            exit_code: Some(0),
            failures: Vec::new(),
        }
    }

    #[test]
    fn append_then_load_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let entry = test_entry("run-1", JournalKind::FinishFailed);
        append_journal_entry(temp.path(), entry.clone()).unwrap();
        let loaded = load_journal(temp.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "run-1");
        assert_eq!(loaded[0].kind, JournalKind::FinishFailed);
        assert_eq!(loaded[0].harness, entry.harness);
        assert_eq!(loaded[0].native_session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn missing_journal_is_empty_and_append_creates_it_private() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load_journal(temp.path()).unwrap().is_empty());
        append_journal_entry(temp.path(), test_entry("a", JournalKind::DegradedRun)).unwrap();
        assert_eq!(load_journal(temp.path()).unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(journal_path(temp.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the journal is owner-only");
        }
    }

    #[test]
    fn full_journal_refuses_append_without_evicting() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..MAX_JOURNAL_ENTRIES {
            append_journal_entry(
                temp.path(),
                test_entry(&format!("entry-{index}"), JournalKind::DegradedRun),
            )
            .unwrap();
        }
        let error = append_journal_entry(
            temp.path(),
            test_entry("must-not-fit", JournalKind::DegradedRun),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("ai-memory recover"));
        let loaded = load_journal(temp.path()).unwrap();
        assert_eq!(loaded.len(), MAX_JOURNAL_ENTRIES);
        assert_eq!(loaded[0].id, "entry-0");
        assert_eq!(
            loaded.last().unwrap().id,
            format!("entry-{}", MAX_JOURNAL_ENTRIES - 1)
        );
    }

    #[test]
    fn appends_are_atomic_and_leave_no_temporary_files() {
        // The writer only ever renames a complete tmp file into place, so a
        // crash between appends cannot leave a half-written journal: the
        // previous complete file is still there and parseable.
        let temp = tempfile::tempdir().unwrap();
        append_journal_entry(temp.path(), test_entry("first", JournalKind::DegradedRun)).unwrap();
        let before = fs::read(journal_path(temp.path())).unwrap();
        append_journal_entry(temp.path(), test_entry("second", JournalKind::DegradedRun)).unwrap();
        let after = fs::read(journal_path(temp.path())).unwrap();
        assert!(before.len() < after.len());
        assert_eq!(load_journal(temp.path()).unwrap().len(), 2);
        assert!(
            !fs::read_dir(temp.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp")),
            "no temporary file is left behind"
        );
    }

    #[test]
    fn corrupt_oversized_or_wrong_version_journal_is_refused_not_reset() {
        let temp = tempfile::tempdir().unwrap();
        let path = journal_path(temp.path());
        fs::write(&path, b"{ not json").unwrap();
        let error = load_journal(temp.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("refusing to discard"),
            "{error:#}"
        );
        let error = append_journal_entry(temp.path(), test_entry("x", JournalKind::DegradedRun))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("refusing to discard"),
            "{error:#}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"{ not json");

        let entries = (0..=MAX_JOURNAL_ENTRIES)
            .map(|index| test_entry(&index.to_string(), JournalKind::DegradedRun))
            .collect();
        fs::write(
            &path,
            serde_json::to_vec(&JournalFile {
                version: JOURNAL_VERSION,
                entries,
            })
            .unwrap(),
        )
        .unwrap();
        let error = load_journal(temp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("entry limit"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_lock_is_refused_without_touching_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside-lock");
        fs::write(&outside, b"canary").unwrap();
        std::os::unix::fs::symlink(&outside, temp.path().join(JOURNAL_LOCK_FILE)).unwrap();
        let error =
            append_journal_entry(temp.path(), test_entry("outside", JournalKind::DegradedRun))
                .unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(fs::read(outside).unwrap(), b"canary");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_journal_and_ancestor_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real.json");
        fs::write(&real, b"{}").unwrap();
        std::os::unix::fs::symlink(&real, journal_path(temp.path())).unwrap();
        let error = load_journal(temp.path()).unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");

        let outside = tempfile::tempdir().unwrap();
        let linked_data = temp.path().join("linked-data");
        std::os::unix::fs::symlink(outside.path(), &linked_data).unwrap();
        let error = append_journal_entry(
            &linked_data,
            test_entry("outside", JournalKind::DegradedRun),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert!(!outside.path().join(JOURNAL_FILE).exists());
    }

    /// A symlink ABOVE the data dir is the operator's or the system's (macOS
    /// `/var` → `/private/var`): the journal works through it. The data dir
    /// is reached here only through `link/`, and nothing inside it is a link.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_ancestor_of_the_data_dir_is_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let data_dir = link.join("data");
        append_journal_entry(&data_dir, test_entry("ancestor", JournalKind::DegradedRun)).unwrap();
        assert_eq!(load_journal(&data_dir).unwrap().len(), 1);
        assert!(real.join("data").join(JOURNAL_FILE).exists());
    }

    /// The trust stops at the data dir: a link planted on the lock path
    /// inside a data dir reached through a symlinked ancestor is still
    /// refused, and its target is never touched.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_data_dir_under_a_symlinked_ancestor_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir_all(real.join("data")).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let data_dir = link.join("data");
        let outside = temp.path().join("outside-journal.json");
        fs::write(&outside, b"canary").unwrap();
        std::os::unix::fs::symlink(&outside, journal_path(&data_dir)).unwrap();
        let error =
            append_journal_entry(&data_dir, test_entry("planted", JournalKind::DegradedRun))
                .unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        assert_eq!(fs::read(&outside).unwrap(), b"canary");
    }

    #[cfg(windows)]
    #[test]
    fn journal_acl_is_restricted_to_the_current_owner() {
        let temp = tempfile::tempdir().unwrap();
        append_journal_entry(temp.path(), test_entry("acl", JournalKind::DegradedRun)).unwrap();
        let output = std::process::Command::new("icacls")
            .arg(journal_path(temp.path()))
            .output()
            .unwrap();
        assert!(output.status.success());
        let acl = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        assert!(!acl.contains("everyone"), "{acl}");
        assert!(!acl.contains("authenticated users"), "{acl}");
    }

    #[test]
    fn clearing_entries_and_annotating_failures_rewrites_the_journal() {
        let temp = tempfile::tempdir().unwrap();
        for id in ["keep", "clear", "fail"] {
            append_journal_entry(temp.path(), test_entry(id, JournalKind::DegradedRun)).unwrap();
        }
        finish_journal(
            temp.path(),
            &Sanitizer::builtin(),
            &["clear".to_string()],
            &[("fail".to_string(), "server said no".to_string())],
        )
        .unwrap();
        let loaded = load_journal(temp.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|entry| entry.id != "clear"));
        let failed = loaded.iter().find(|entry| entry.id == "fail").unwrap();
        assert_eq!(failed.failures, vec!["server said no".to_string()]);
        // The same failure reason is not recorded twice.
        finish_journal(
            temp.path(),
            &Sanitizer::builtin(),
            &[],
            &[("fail".to_string(), "server said no".to_string())],
        )
        .unwrap();
        let loaded = load_journal(temp.path()).unwrap();
        assert_eq!(
            loaded
                .iter()
                .find(|entry| entry.id == "fail")
                .unwrap()
                .failures,
            vec!["server said no".to_string()]
        );
    }

    #[test]
    fn failure_reasons_are_sanitized_and_bounded_per_entry() {
        let temp = tempfile::tempdir().unwrap();
        append_journal_entry(temp.path(), test_entry("e", JournalKind::DegradedRun)).unwrap();
        for index in 0..(MAX_ENTRY_FAILURES + 2) {
            finish_journal(
                temp.path(),
                &Sanitizer::builtin(),
                &[],
                &[(
                    "e".to_string(),
                    format!(
                        "reason {index} token sk-{} {}",
                        "a".repeat(32),
                        "x".repeat(MAX_FAILURE_REASON_BYTES * 2)
                    ),
                )],
            )
            .unwrap();
        }
        let loaded = load_journal(temp.path()).unwrap();
        assert_eq!(loaded[0].failures.len(), MAX_ENTRY_FAILURES);
        assert!(loaded[0].failures[0].starts_with("reason 2 token [REDACTED:api_key]"));
        assert!(
            loaded[0]
                .failures
                .iter()
                .all(|reason| reason.len() <= MAX_FAILURE_REASON_BYTES)
        );
    }

    #[test]
    fn unknown_and_known_empty_event_sets_remain_distinct() {
        let mut unknown = test_entry("unknown", JournalKind::FinishFailed);
        unknown.event_digests = None;
        let unknown_json = serde_json::to_value(&unknown).unwrap();
        assert!(unknown_json.get("event_digests").is_none());

        let empty = test_entry("empty", JournalKind::FinishFailed);
        let empty_json = serde_json::to_value(&empty).unwrap();
        assert_eq!(empty_json["event_digests"], serde_json::json!([]));
    }

    #[test]
    fn recovery_journal_omits_repository_state() {
        let bytes = serde_json::to_vec(&test_entry("id", JournalKind::FinishFailed)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("checkpoint"));
        assert!(!text.contains("changed_paths"));
        assert!(!text.contains("branch"));
    }

    #[test]
    fn legacy_relative_session_dir_resolves_against_recorded_cwd() {
        let mut entry = test_entry("legacy", JournalKind::DegradedRun);
        entry.cwd = "/original/repo".into();
        entry.session_dir = Some("stores/native".into());
        assert_eq!(
            recovery_session_dir(&entry),
            Some(PathBuf::from("/original/repo/stores/native"))
        );
    }

    #[test]
    fn journal_entry_serializes_without_optional_noise() {
        let bytes = serde_json::to_vec(&test_entry("id", JournalKind::DegradedRun)).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"kind\":\"degraded-run\""));
        assert!(
            !text.contains("run_id"),
            "absent fields are omitted: {text}"
        );
    }

    #[test]
    fn journal_summary_counts_kinds_and_carries_errors() {
        let temp = tempfile::tempdir().unwrap();
        append_journal_entry(temp.path(), test_entry("d1", JournalKind::DegradedRun)).unwrap();
        append_journal_entry(temp.path(), test_entry("f1", JournalKind::FinishFailed)).unwrap();
        assert_eq!(
            journal_summary(temp.path()),
            (2, 1, 1, None),
            "(total, degraded, finish-failed, error)"
        );
        fs::write(journal_path(temp.path()), b"nope").unwrap();
        let (total, _, _, error) = journal_summary(temp.path());
        assert_eq!(total, 0);
        assert!(error.is_some());
    }

    #[test]
    fn dropped_spool_events_make_recovery_incomplete() {
        assert!(spool_is_incomplete(&SpoolOutcome {
            sent: 3,
            remaining: 0,
            dropped: 1,
        }));
        assert!(!spool_is_incomplete(&SpoolOutcome {
            sent: 3,
            remaining: 0,
            dropped: 0,
        }));
    }

    #[tokio::test]
    async fn failed_exact_replay_restores_correlated_spool_entry() {
        let temp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap()
        });
        let server_url = format!("http://{address}");
        let spool = hook_spool::spool_dir(temp.path());
        let created_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap();
        let correlated = hook_spool::SpoolEntry {
            url: format!(
                "{server_url}/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=proj&session_id=sess-1"
            ),
            body: r#"{"session_id":"sess-1","cwd":"/tmp/repo","prompt":"same prompt"}"#.into(),
            created_ms,
            auth_mode: hook_spool::AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        hook_spool::enqueue(&spool, &correlated).unwrap();
        let entry = JournalEntry {
            server_url: server_url.clone(),
            interval_started_ms: created_ms.saturating_sub(1),
            interval_ended_ms: created_ms.saturating_add(1),
            ..test_entry("restore", JournalKind::DegradedRun)
        };
        append_journal_entry(temp.path(), entry).unwrap();
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url,
            ..Config::default()
        };
        let report = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert_eq!(report.failures(), 1);
        assert_eq!(report.journal_remaining, 1);
        assert_eq!(hook_spool::spool_len(&spool), 1);
        server.abort();
    }

    #[tokio::test]
    async fn spool_only_harness_clears_only_after_a_complete_drain() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new()
                    .route(
                        "/healthz",
                        axum::routing::get(|| async { axum::http::StatusCode::OK }),
                    )
                    .route(
                        "/hook/batch",
                        axum::routing::post(|| async {
                            axum::Json(serde_json::json!({
                                "accepted": 1,
                                "results": [{"index": 0, "outcome": "stored"}]
                            }))
                        }),
                    ),
            )
            .await
            .unwrap()
        });
        let server_url = format!("http://{address}");
        let spool = hook_spool::spool_dir(temp.path());
        let created_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap();
        hook_spool::enqueue(
            &spool,
            &hook_spool::SpoolEntry {
                url: format!(
                    "{server_url}/hook?event=user-prompt-submit&agent=antigravity-cli&workspace=ws&project=proj"
                ),
                body: serde_json::json!({
                    "conversationId": "sess-1",
                    "cwd": cwd,
                    "prompt": "captured"
                })
                .to_string(),
                created_ms,
                auth_mode: hook_spool::AuthMode::Anonymous,
                token: None,
                attempts: 0,
                profile: None,
            },
        )
        .unwrap();
        let entry = JournalEntry {
            harness: "antigravity".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            project: "proj".into(),
            server_url: server_url.clone(),
            interval_started_ms: created_ms.saturating_sub(1),
            interval_ended_ms: created_ms.saturating_add(1),
            ..test_entry("spool-only", JournalKind::DegradedRun)
        };
        append_journal_entry(temp.path(), entry).unwrap();
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url,
            ..Config::default()
        };
        let report = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert_eq!(
            report.spool.sent, 0,
            "correlated evidence drains separately"
        );
        assert_eq!(report.sessions[0].outcome, "recovered-via-spool");
        assert_eq!(report.journal_remaining, 0);
        assert!(load_journal(temp.path()).unwrap().is_empty());
        server.abort();
    }

    /// A spool-only entry dispositioned Failed because an *unrelated* queued
    /// event kept the drain incomplete must not wedge recovery after its own
    /// interval already completed: restoring the quarantine treats the
    /// completed marker as durable delivery, the rerun neither errors nor
    /// re-delivers, and the journal clears.
    #[tokio::test]
    async fn completed_spool_only_interval_survives_an_unrelated_incomplete_drain() {
        use axum::response::IntoResponse as _;

        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let deliveries = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler_deliveries = deliveries.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new()
                    .route(
                        "/healthz",
                        axum::routing::get(|| async { axum::http::StatusCode::OK }),
                    )
                    .route(
                        "/hook/batch",
                        axum::routing::post(move |body: axum::body::Bytes| {
                            let deliveries = handler_deliveries.clone();
                            async move {
                                let Ok(items) =
                                    serde_json::from_slice::<Vec<serde_json::Value>>(&body)
                                else {
                                    return (
                                        axum::http::StatusCode::BAD_REQUEST,
                                        "malformed batch",
                                    )
                                        .into_response();
                                };
                                let prompts: Vec<String> = items
                                    .iter()
                                    .filter_map(|item| {
                                        item.get("body")?.get("prompt")?.as_str().map(str::to_owned)
                                    })
                                    .collect();
                                if prompts.iter().any(|prompt| prompt == "unrelated") {
                                    return (
                                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                        "unrelated event refused",
                                    )
                                        .into_response();
                                }
                                deliveries.lock().unwrap().extend(prompts);
                                axum::Json(serde_json::json!({
                                    "accepted": items.len(),
                                    "results": (0..items.len())
                                        .map(|index| serde_json::json!({
                                            "index": index,
                                            "outcome": "stored"
                                        }))
                                        .collect::<Vec<_>>()
                                }))
                                .into_response()
                            }
                        }),
                    ),
            )
            .await
            .unwrap()
        });
        let server_url = format!("http://{address}");
        let spool = hook_spool::spool_dir(temp.path());
        let created_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap();
        let correlated = hook_spool::SpoolEntry {
            url: format!(
                "{server_url}/hook?event=user-prompt-submit&agent=antigravity-cli&workspace=ws&project=proj"
            ),
            body: serde_json::json!({
                "conversationId": "sess-1",
                "cwd": cwd,
                "prompt": "captured"
            })
            .to_string(),
            created_ms,
            auth_mode: hook_spool::AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        let unrelated = hook_spool::SpoolEntry {
            url: format!(
                "{server_url}/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=proj&session_id=unrelated-1"
            ),
            body: r#"{"session_id":"unrelated-1","cwd":"/tmp/repo","prompt":"unrelated"}"#.into(),
            created_ms: created_ms.saturating_add(1),
            auth_mode: hook_spool::AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        hook_spool::enqueue(&spool, &correlated).unwrap();
        hook_spool::enqueue(&spool, &unrelated).unwrap();
        let entry = JournalEntry {
            harness: "antigravity".into(),
            spool_only: true,
            native_session_id: Some("sess-1".into()),
            cwd: cwd.to_string_lossy().into_owned(),
            workspace: "ws".into(),
            project: "proj".into(),
            server_url: server_url.clone(),
            interval_started_ms: created_ms.saturating_sub(1),
            interval_ended_ms: created_ms.saturating_add(2),
            ..test_entry("completed-rerun", JournalKind::DegradedRun)
        };
        append_journal_entry(temp.path(), entry).unwrap();
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url,
            ..Config::default()
        };
        let first = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert_eq!(first.sessions[0].outcome, "recovered-via-spool");
        assert_eq!(first.journal_remaining, 0);
        assert!(first.spool_incomplete, "the unrelated event still queues");
        assert!(load_journal(temp.path()).unwrap().is_empty());

        let rerun = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert!(rerun.sessions.is_empty());
        assert_eq!(rerun.failures(), 0);
        assert_eq!(rerun.journal_remaining, 0);
        assert!(rerun.spool_incomplete);
        assert!(
            load_journal(temp.path()).unwrap().is_empty(),
            "the rerun must clear the journal"
        );
        assert_eq!(
            deliveries
                .lock()
                .unwrap()
                .iter()
                .filter(|prompt| *prompt == "captured")
                .count(),
            1,
            "the completed interval's evidence is never re-delivered"
        );
        assert_eq!(
            hook_spool::spool_len(&spool),
            1,
            "only the unrelated event stays queued"
        );
        server.abort();
    }

    #[tokio::test]
    async fn spool_only_without_correlated_evidence_never_auto_clears() {
        let temp = tempfile::tempdir().unwrap();
        let mut entry = test_entry("no-hooks", JournalKind::DegradedRun);
        entry.harness = "antigravity".into();
        append_journal_entry(temp.path(), entry).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap();
        });
        let server_url = format!("http://{address}");
        mutate_journal(temp.path(), |entries| {
            entries[0].server_url.clone_from(&server_url);
            Ok(())
        })
        .unwrap();
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url,
            ..Config::default()
        };
        for _ in 0..2 {
            let report = recover(&config, &RecoverBudgets::default()).await.unwrap();
            assert_eq!(report.failures(), 1);
            assert_eq!(report.journal_remaining, 1);
            assert!(
                report.sessions[0]
                    .detail
                    .as_deref()
                    .unwrap()
                    .contains("no durably delivered")
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_dropped_spool_event_stays_lost_on_the_next_empty_pass() {
        let temp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap();
        });
        let server_url = format!("http://{address}");
        let spool = hook_spool::spool_dir(temp.path());
        let created_ms = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap();
        hook_spool::enqueue(
            &spool,
            &hook_spool::SpoolEntry {
                url: format!(
                    "{server_url}/hook?event=user-prompt-submit&agent=antigravity-cli&workspace=ws&project=proj"
                ),
                body: r#"{"conversationId":"sess-1","cwd":"/tmp/repo","prompt":"lost"}"#.into(),
                created_ms,
                auth_mode: hook_spool::AuthMode::Anonymous,
                token: None,
                attempts: 7,
                profile: None,
            },
        )
        .unwrap();
        let entry = JournalEntry {
            harness: "antigravity".into(),
            server_url: server_url.clone(),
            interval_started_ms: created_ms.saturating_sub(1),
            interval_ended_ms: created_ms.saturating_add(1),
            ..test_entry("dropped", JournalKind::DegradedRun)
        };
        append_journal_entry(temp.path(), entry).unwrap();
        mutate_journal(temp.path(), |entries| {
            entries[0].correlated_events_seen = true;
            entries[0].correlated_loss = true;
            Ok(())
        })
        .unwrap();
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url,
            ..Config::default()
        };
        let first = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert_eq!(
            first.spool.sent, 0,
            "correlated evidence never rides the main drain"
        );
        assert_eq!(
            first.failures(),
            1,
            "the correlated loss keeps the journal entry"
        );
        assert!(load_journal(temp.path()).unwrap()[0].correlated_loss);
        let second = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert_eq!(second.spool.dropped, 0, "the dropped evidence stays lost");
        assert_eq!(second.journal_remaining, 1);
        assert!(load_journal(temp.path()).unwrap()[0].correlated_loss);
        server.abort();
    }

    #[tokio::test]
    async fn a_lossy_spool_pass_keeps_the_recovery_journal() {
        let temp = tempfile::tempdir().unwrap();
        let mut entry = test_entry("keep", JournalKind::DegradedRun);
        entry.harness = "antigravity".into();
        append_journal_entry(temp.path(), entry).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap();
        });
        let spool = hook_spool::spool_dir(temp.path());
        hook_spool::enqueue(
            &spool,
            &hook_spool::SpoolEntry {
                url: format!("http://{address}/hook?event=user-prompt-submit"),
                body: "not-json".into(),
                created_ms: u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap(),
                auth_mode: hook_spool::AuthMode::Anonymous,
                token: None,
                attempts: 7,
                profile: None,
            },
        )
        .unwrap();
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url: format!("http://{address}"),
            ..Config::default()
        };
        let report = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert_eq!(report.spool.dropped, 1);
        assert!(report.spool_incomplete);
        assert_eq!(report.journal_remaining, 1);
        assert!(
            report.sessions[0]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("1 dropped"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn per_entry_budget_times_out_and_keeps_the_entry() {
        let temp = tempfile::tempdir().unwrap();
        append_journal_entry(temp.path(), test_entry("one", JournalKind::DegradedRun)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap();
        });
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url: format!("http://{address}"),
            ..Config::default()
        };
        let report = recover(
            &config,
            &RecoverBudgets {
                entry_timeout: Duration::ZERO,
                ..RecoverBudgets::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(report.failures(), 1);
        assert!(
            report.sessions[0]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("timed out"))
        );
        assert_eq!(load_journal(temp.path()).unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn total_journal_budget_keeps_unattempted_entries() {
        let temp = tempfile::tempdir().unwrap();
        append_journal_entry(temp.path(), test_entry("one", JournalKind::DegradedRun)).unwrap();
        append_journal_entry(temp.path(), test_entry("two", JournalKind::DegradedRun)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap();
        });
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url: format!("http://{address}"),
            ..Config::default()
        };
        let report = recover(
            &config,
            &RecoverBudgets {
                journal_total: Duration::ZERO,
                ..RecoverBudgets::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(report.failures(), 2);
        assert!(report.sessions.iter().all(|session| {
            session
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("budget exhausted"))
        }));
        assert_eq!(load_journal(temp.path()).unwrap().len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn a_journal_entry_cannot_be_replayed_to_another_server() {
        let temp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap();
        });
        append_journal_entry(
            temp.path(),
            JournalEntry {
                server_url: "http://127.0.0.1:1".to_string(),
                ..test_entry("foreign-server", JournalKind::DegradedRun)
            },
        )
        .unwrap();
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url: format!("http://{address}"),
            ..Config::default()
        };
        let report = recover(&config, &RecoverBudgets::default()).await.unwrap();
        assert_eq!(report.failures(), 1);
        assert!(
            report.sessions[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("refusing to replay")
        );
        assert_eq!(load_journal(temp.path()).unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn failed_recovery_returns_a_testable_nonzero_code_without_exiting() {
        let temp = tempfile::tempdir().unwrap();
        append_journal_entry(
            temp.path(),
            JournalEntry {
                native_session_id: None,
                ..test_entry("bad", JournalKind::FinishFailed)
            },
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/healthz",
                    axum::routing::get(|| async { axum::http::StatusCode::OK }),
                ),
            )
            .await
            .unwrap();
        });
        let config = Config {
            data_dir: temp.path().to_path_buf(),
            server_url: format!("http://{address}"),
            ..Config::default()
        };
        let code = run(&config, RecoverArgs { json: false }).await.unwrap();
        assert_eq!(code, 1);
        assert_eq!(load_journal(temp.path()).unwrap().len(), 1);
        server.abort();
    }
}
