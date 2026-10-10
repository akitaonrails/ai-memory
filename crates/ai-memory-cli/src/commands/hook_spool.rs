//! Local spool for lifecycle-hook events — decouples capture from the network.
//!
//! Per-tool-call hooks (`pre-tool-use`, `post-tool-use`, `user-prompt-submit`)
//! append an event here (an instant local write) instead of POSTing
//! synchronously. The spool is drained to the server at **session boundaries**:
//! a cleanup pass at `session-start`, detached background flushes at
//! cancellation-prone boundaries (`stop`, `pre-compact`, `session-end`), and a
//! small threshold-triggered catch-up on busy `post-tool-use` streams. A few
//! seconds of latency is acceptable at boundaries, unlike the per-tool-call hot
//! path, which must never block the agent.
//!
//! This makes capture reliable against a remote/slow server (no event is lost:
//! a file persists until the server answers 2xx) without ever blocking a tool
//! call. It also fits ai-memory's model: consolidation runs on `session-end`,
//! after the drain has delivered the session's observations in order.
//!
//! Each event carries its own auth so a single global spool can hold events
//! for several instances: a static token is stored inline (file mode 0600);
//! an OIDC event stores only the mode and is resolved + refreshed from
//! `auth.json` at drain time (so a token that expired while the event waited is
//! renewed rather than rejected).

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use super::hook_capture::{BatchOutcome, PostOutcome, build_client, post_batch, post_hook};

/// Default number of failed drain passes before dropping an event — bounds
/// retries of an event a server keeps refusing or erroring on. An
/// unreachable endpoint never charges attempts; a dead address is bounded
/// by the spool's TTL and file cap instead.
#[cfg(test)]
const MAX_ATTEMPTS: u32 = crate::config::DEFAULT_HOOK_SPOOL_MAX_ATTEMPTS;

/// Failed drain-pass limit resolved at the hook command boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaxAttempts(u32);

impl MaxAttempts {
    /// Preserve an explicitly configured limit, including zero (no attempt drop).
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
}
/// Drop a spooled event older than this regardless of attempts (7 days), so a
/// long-dead instance can't leave the spool growing without bound.
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Hard cap on queued events per data dir. Enqueue prunes oldest files beyond
/// this so a down server cannot grow the hook spool without bound.
#[cfg(not(test))]
const MAX_SPOOL_FILES: usize = 10_000;
#[cfg(test)]
const MAX_SPOOL_FILES: usize = 3;

/// Max events per `POST /hook/batch` request (count bound; the byte bound below
/// also applies). Caps the blast radius of a failed batch and keeps one request
/// well under the server's body limit even with many small events.
const MAX_BATCH_ITEMS: usize = 256;
/// Soft byte budget for one `/hook/batch` body — stays under the server's 10 MiB
/// `DefaultBodyLimit` with margin for JSON framing. A chunk always carries at
/// least one event even if that event alone exceeds this.
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const RECOVERY_DIR: &str = ".recovery";
const RECOVERY_MATCH_FILE: &str = ".active";
const RECOVERY_COMPLETED_FILE: &str = ".completed";
const SPOOL_CAPACITY_LOCK_FILE: &str = ".capacity.lock";
const SPOOL_COORD_LOCK_WAIT: Duration = Duration::from_millis(5);
const MAX_SPOOL_ENTRY_BYTES: u64 = 10 * 1024 * 1024;
const RECOVERY_TOMBSTONE_AGE_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_RECOVERY_TOMBSTONES: usize = 512;
const MAX_RECOVERY_SWEEP_DIRS: usize = 1_024;

static ENQUEUE_SEQ: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static PAUSE_ENQUEUE_AFTER_MAIN_RENAME: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static ENQUEUE_REACHED_RECOVERY_WINDOW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// How a spooled event authenticates to the server when drained.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthMode {
    /// A static bearer stored inline (`token`) — service-account / edge token.
    #[serde(rename = "static")]
    Static,
    /// Resolve + refresh a stored OIDC device-grant token from `auth.json`.
    #[serde(rename = "oidc")]
    Oidc,
    /// No bearer (loopback / no-auth server).
    #[serde(rename = "none")]
    Anonymous,
}

/// One spooled hook event: the full request plus how to authenticate it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SpoolEntry {
    /// Full hook URL including the `?event=…&agent=…[&cwd&workspace&project]`
    /// query the agent's payload resolved to.
    pub url: String,
    /// The raw JSON event payload to POST.
    pub body: String,
    /// Enqueue time (Unix ms) — for ordering + future TTL pruning.
    pub created_ms: u64,
    /// How to authenticate this event at drain time.
    pub auth_mode: AuthMode,
    /// Static bearer, present only when `auth_mode == Static`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Failed delivery attempts so far — incremented only when a delivery
    /// actually reached a server and missed (never for an endpoint-level
    /// outage) and used (with `created_ms`) to drop a
    /// permanently-undeliverable event.
    #[serde(default)]
    pub attempts: u32,
    /// Server profile this event was routed to by its repository's marker
    /// (#992), `None` for the install default. Absent from the file when
    /// `None`, so entries without a profile are byte-identical to before.
    ///
    /// Marks the entry as bound to exactly one server: the drain never
    /// retries it with another server's credential, and never re-points it
    /// at the configured default address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// `<data_dir>/hook-spool` — the spool directory.
#[must_use]
pub fn spool_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("hook-spool")
}

/// Count queued spool entries (`*.json`), or 0 when the dir is missing/empty.
/// Cheaper than [`drain`] — a single `read_dir` — so the per-event hot path can
/// gate a mid-session drain on backlog size without building a client.
#[must_use]
pub fn spool_len(spool: &Path) -> usize {
    list_entries(spool).map_or(0, |(files, _)| files.len())
}

/// Snapshot of local hook-spool health for operator status reporting.
///
/// Deliberately content-free: counts, ages, and attempt sums only — never
/// payload excerpts, URLs, or token material (the spool holds private capture
/// until it drains).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SpoolHealth {
    /// Events queued locally awaiting delivery.
    pub pending: usize,
    /// Age (ms) of the oldest queued event, or `None` when the spool is empty.
    pub oldest_age_ms: Option<u64>,
    /// Sum of failed-delivery attempts across all queued events.
    pub retries_total: u64,
    /// Most events the spool holds; past it the oldest undelivered ones are
    /// evicted to make room.
    pub capacity: usize,
}

impl Default for SpoolHealth {
    fn default() -> Self {
        Self {
            pending: 0,
            oldest_age_ms: None,
            retries_total: 0,
            capacity: MAX_SPOOL_FILES,
        }
    }
}

impl SpoolHealth {
    /// The spool is full, so each new event evicts the oldest undelivered one.
    #[must_use]
    pub fn evicting(&self) -> bool {
        self.pending >= self.capacity
    }
}

/// Snapshot local spool health: queued count and oldest-event age from the
/// timestamp-embedded file names (no file reads), plus total retry attempts
/// (one bounded read per queued entry — fine for an operator-invoked status
/// command, never on the hook hot path).
#[must_use]
pub fn spool_health(spool: &Path) -> SpoolHealth {
    let Ok((files, _)) = list_entries(spool) else {
        return SpoolHealth::default();
    };
    if files.is_empty() {
        return SpoolHealth::default();
    }
    // Keep only files whose *names* follow the spool convention. A stray
    // `.json` that this queue did not write is not a pending hook event, and
    // letting one drive the age is actively misleading: an unparseable name
    // reads as `created_ms = 0`, so the oldest age becomes the whole Unix
    // epoch — roughly 56 years of phantom backlog on a status screen whose
    // entire job is telling an operator whether capture is keeping up.
    //
    // Not hypothetical on macOS: writing to a filesystem without extended
    // attributes (FAT, SMB, some NFS) leaves AppleDouble sidecars named
    // `._<original>`, which end in `.json` and sort before any digit.
    let mut files: Vec<(u64, PathBuf)> = files
        .into_iter()
        .filter_map(|path| {
            let created = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(created_ms_from_name)?;
            Some((created, path))
        })
        .collect();
    if files.is_empty() {
        return SpoolHealth::default();
    }
    files.sort();
    let now = now_ms();
    let oldest_created = files[0].0;
    let mut health = SpoolHealth {
        pending: files.len(),
        oldest_age_ms: Some(now.saturating_sub(oldest_created)),
        ..SpoolHealth::default()
    };
    for (_, path) in &files {
        if let Ok(bytes) = read_spool_entry_bytes(path)
            && let Ok(entry) = serde_json::from_slice::<SpoolEntry>(&bytes)
        {
            health.retries_total += u64::from(entry.attempts);
        }
    }
    health
}

/// Extract the enqueue timestamp embedded in a spool file name
/// (`{created_ms:013}-{pid}-{seq:016x}.json`), `None` when the name does not
/// follow that convention. Filenames are the only place `created_ms` is
/// readable without opening the file, so oldest-age reporting never pays a
/// read per queued event.
///
/// Returns `None` rather than `0` so a foreign file cannot be mistaken for an
/// entry enqueued at the Unix epoch — see [`spool_health`].
fn created_ms_from_name(file_name: &str) -> Option<u64> {
    let (stamp, rest) = file_name.split_once('-')?;
    if stamp.len() != 13 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !rest.ends_with(".json") || !rest.contains('-') {
        return None;
    }
    stamp.parse().ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Append an event to the spool, atomically (temp file + rename) and 0600 on
/// Unix. Never touches the network. File names include a per-process monotonic
/// suffix so tight loops or helper processes can enqueue several events in the
/// same millisecond without clobbering an earlier event.
///
/// # Errors
/// Returns an error only when the spool file cannot be written.
#[cfg(test)]
pub fn enqueue(spool: &Path, entry: &SpoolEntry) -> std::io::Result<()> {
    enqueue_with_config(spool, entry, None)
}

pub fn enqueue_with_config(
    spool: &Path,
    entry: &SpoolEntry,
    config: Option<&crate::config::Config>,
) -> std::io::Result<()> {
    create_spool_dir(spool)?;
    let seq = ENQUEUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let recovery_key = recovery_coordinate_key_for_entry(entry, config)?;
    let name = format!(
        "{:013}-{}-{seq:016x}-{recovery_key}.json",
        entry.created_ms,
        std::process::id()
    );
    let tmp = spool.join(format!("{name}.tmp"));
    let final_path = spool.join(&name);
    let bytes = serde_json::to_vec(entry)?;
    write_private(&tmp, &bytes)?;
    let capacity_lock = open_spool_capacity_lock(spool)?;
    let started = Instant::now();
    loop {
        match capacity_lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error)
                if is_drain_lock_busy_error(&error)
                    && started.elapsed() < SPOOL_COORD_LOCK_WAIT =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) if is_drain_lock_busy_error(&error) => {
                let _ = std::fs::remove_file(&tmp);
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
    if total_spool_entries(spool)? >= MAX_SPOOL_FILES {
        let _ = std::fs::remove_file(&tmp);
        eprintln!(
            "ai-memory: hook-spool at capacity ({MAX_SPOOL_FILES}); dropping one new UNDELIVERED event"
        );
        return Ok(());
    }
    std::fs::rename(&tmp, &final_path)?;
    #[cfg(test)]
    if PAUSE_ENQUEUE_AFTER_MAIN_RENAME.load(Ordering::SeqCst) {
        ENQUEUE_REACHED_RECOVERY_WINDOW.store(true, Ordering::SeqCst);
        while PAUSE_ENQUEUE_AFTER_MAIN_RENAME.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
    }
    let coord_lock = open_recovery_coord_lock(spool)?;
    if coord_lock.try_lock_exclusive().is_ok() {
        match recovery_target_for_entry(spool, entry, config)? {
            Some(RecoveryTarget::Active(dir)) => {
                std::fs::rename(&final_path, dir.join(&name))?;
            }
            Some(RecoveryTarget::Completed(_)) => match std::fs::remove_file(&final_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            },
            None => {}
        }
    }
    Ok(())
}

/// Create the spool directory `0700` on Unix so the excerpt bodies that reside
/// there (up to `MAX_AGE_MS`) — and even the timestamp+pid metadata in the file
/// names — are only reachable by the owner (#196). The spool holds private
/// capture until it drains; a world-readable directory would leak that.
fn create_spool_dir(spool: &Path) -> std::io::Result<()> {
    super::path_util::create_private_dir(spool)
}

fn open_spool_capacity_lock(spool: &Path) -> std::io::Result<File> {
    create_spool_dir(spool)?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(spool.with_extension(SPOOL_CAPACITY_LOCK_FILE))
}

fn total_spool_entries(spool: &Path) -> std::io::Result<usize> {
    let mut total = list_entries(spool).map_or(0, |(files, _)| files.len());
    let recovery = spool.join(RECOVERY_DIR);
    let directories = match std::fs::read_dir(recovery) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(total),
        Err(error) => return Err(error),
    };
    for (index, directory) in directories.enumerate() {
        if index >= MAX_RECOVERY_SWEEP_DIRS {
            return Ok(MAX_SPOOL_FILES);
        }
        let directory = directory?;
        if directory.file_type()?.is_dir() {
            total = total.saturating_add(
                list_entries(&directory.path()).map_or(0, |(files, _)| files.len()),
            );
        }
    }
    Ok(total)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Build a [`SpoolEntry`] for the current event, choosing the auth mode from
/// the hook's flags + stored credentials (no network, no token I/O):
/// an explicit `--auth-token` → `Static`; else a present OIDC `auth.json`
/// entry → `Oidc`; else `Anonymous`.
#[must_use]
pub fn entry_for(
    url: String,
    body: String,
    auth_token: Option<&str>,
    oidc_present: bool,
) -> SpoolEntry {
    let (auth_mode, token) = match auth_token {
        Some(t) => (AuthMode::Static, Some(t.to_string())),
        None if oidc_present => (AuthMode::Oidc, None),
        None => (AuthMode::Anonymous, None),
    };
    SpoolEntry {
        url,
        body,
        created_ms: now_ms(),
        auth_mode,
        token,
        attempts: 0,
        profile: None,
    }
}

impl SpoolEntry {
    /// Tag the entry with the server profile its marker routed it to (#992).
    #[must_use]
    pub fn routed_to(mut self, profile: Option<&str>) -> Self {
        self.profile = profile.map(str::to_owned);
        self
    }
}

/// Outcome of a drain pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainResult {
    /// Events delivered (server answered 2xx) and removed from the spool.
    pub sent: usize,
    /// Events whose modern per-item acknowledgement proves durable storage.
    pub durable: usize,
    /// Events still queued (failed this pass, or skipped when the budget ran out).
    pub remaining: usize,
    /// Events discarded as undeliverable (too old or too many failed attempts).
    pub dropped: usize,
}

/// How long to wait for the exclusive drain lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainLockWait {
    /// Do not wait if another drainer is active.
    NoWait,
    /// Poll for the lock until this bounded budget expires.
    Bounded(Duration),
}

/// An exclusive hook-spool drain lock. The OS releases it when dropped.
pub struct DrainLock {
    _file: File,
}

/// Outcome of [`QuarantinedSpool::restore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Quarantined events were moved back to the plain spool (or the
    /// quarantine directory no longer existed).
    Restored,
    /// The interval was already completed, so its correlated evidence is
    /// durably delivered and there is nothing left to restore or re-deliver.
    AlreadyDelivered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecoveryMatch {
    journal_id: String,
    coordinate_key: String,
    interval_started_ms: u64,
    interval_ended_ms: u64,
    server_identity: String,
    agent: String,
    workspace: String,
    project: String,
    cwd: String,
    native_session_id: String,
}

pub struct QuarantinedSpool {
    dir: PathBuf,
    spool: PathBuf,
    config: crate::config::Config,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CorrelatedDrainResult {
    pub durable: usize,
    pub remaining: usize,
    pub dropped: usize,
}

impl QuarantinedSpool {
    pub fn len(&self) -> usize {
        list_entries(&self.dir).map_or(0, |(files, _)| files.len())
    }

    pub fn complete(self) -> std::io::Result<()> {
        if self.dir.exists() {
            let lock = open_recovery_coord_lock(&self.spool)?;
            lock.lock_exclusive()?;
            let active = self.dir.join(RECOVERY_MATCH_FILE);
            let completed = self.dir.join(RECOVERY_COMPLETED_FILE);
            let spec = read_recovery_match(&active)?;
            std::fs::rename(&active, &completed)?;
            move_late_matching_entries(&self.spool, &self.dir, &self.config, &spec)?;
            for item in std::fs::read_dir(&self.dir)? {
                let item = item?;
                if item.path().extension().and_then(|value| value.to_str()) == Some("json") {
                    std::fs::remove_file(item.path())?;
                }
            }
            sweep_recovery_tombstones(&self.spool)?;
        }
        Ok(())
    }

    pub async fn drain_strict(
        &self,
        data_dir: &Path,
        total_budget: Duration,
        per_event_timeout: Duration,
        max_attempts: MaxAttempts,
    ) -> CorrelatedDrainResult {
        let Ok((mut files, unreadable)) = list_entries(&self.dir) else {
            return CorrelatedDrainResult {
                remaining: 1,
                ..CorrelatedDrainResult::default()
            };
        };
        files.sort();
        let client = build_client();
        let started = Instant::now();
        let mut oidc_cache = None;
        let mut result = DrainResult {
            remaining: unreadable,
            ..DrainResult::default()
        };
        for path in files {
            if started.elapsed() >= total_budget {
                result.remaining += 1;
                continue;
            }
            let Some(entry) = load_live_entry(&path, &mut result) else {
                continue;
            };
            if body_is_malformed(&entry) {
                bump_or_drop(&path, &entry, max_attempts.0, &mut result);
                continue;
            }
            let bearer = entry_bearer(&entry, &client, data_dir, &mut oidc_cache).await;
            let Some(payload) = batch_payload(&[(path.clone(), entry.clone())]) else {
                bump_or_drop(&path, &entry, max_attempts.0, &mut result);
                continue;
            };
            let remaining_budget = total_budget.saturating_sub(started.elapsed());
            let timeout = per_event_timeout.min(remaining_budget);
            match post_batch(
                &client,
                &batch_endpoint(&entry.url),
                &payload,
                bearer.as_deref(),
                timeout,
            )
            .await
            {
                BatchOutcome::Detailed {
                    outcomes,
                    failed_index: None,
                } if outcomes.len() == 1 => {
                    let outcome = outcomes[0].as_str();
                    let durable = matches!(outcome, "stored" | "replayed")
                        || outcome == "ignored_end"
                            && entry_event(&entry).as_deref() == Some("session-end");
                    if durable {
                        let _ = std::fs::remove_file(path);
                        result.sent += 1;
                        result.durable += 1;
                    } else if outcome.starts_with("dropped_") {
                        let _ = std::fs::remove_file(path);
                        result.dropped += 1;
                    } else {
                        bump_or_drop(&path, &entry, max_attempts.0, &mut result);
                    }
                }
                _ => bump_or_drop(&path, &entry, max_attempts.0, &mut result),
            }
        }
        CorrelatedDrainResult {
            durable: result.durable,
            remaining: result.remaining,
            dropped: result.dropped,
        }
    }

    pub fn complete_spool_only(&self) -> std::io::Result<bool> {
        let lock = open_recovery_coord_lock(&self.spool)?;
        lock.lock_exclusive()?;
        let active = self.dir.join(RECOVERY_MATCH_FILE);
        let completed = self.dir.join(RECOVERY_COMPLETED_FILE);
        let spec = read_recovery_match(&active)?;
        move_late_matching_entries(&self.spool, &self.dir, &self.config, &spec)?;
        if self.len() != 0 {
            return Ok(false);
        }
        std::fs::rename(active, completed)?;
        Ok(true)
    }

    pub fn restore(self, spool: &Path) -> std::io::Result<RestoreOutcome> {
        if !self.dir.exists() {
            return Ok(RestoreOutcome::Restored);
        }
        let lock = open_recovery_coord_lock(spool)?;
        lock.lock_exclusive()?;
        let active = self.dir.join(RECOVERY_MATCH_FILE);
        let completed = self.dir.join(RECOVERY_COMPLETED_FILE);
        // Completion is terminal: a `.completed` marker is durable proof the
        // interval's correlated evidence reached the server, so a restore has
        // nothing to put back. A `.active` beside the tombstone is a rerun
        // rewriting the same interval, not a re-opened one.
        if completed.is_file() {
            match std::fs::remove_file(&active) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            move_quarantined_entries_to_spool(&self.dir, spool)?;
            return Ok(RestoreOutcome::AlreadyDelivered);
        }
        if !active.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "recovery interval {} has neither an active nor a completed marker",
                    self.dir.display()
                ),
            ));
        }
        let spec = read_recovery_match(&active)?;
        move_late_matching_entries(spool, &self.dir, &self.config, &spec)?;
        let _ = std::fs::remove_file(active);
        move_quarantined_entries_to_spool(&self.dir, spool)?;
        std::fs::remove_dir(self.dir)?;
        Ok(RestoreOutcome::Restored)
    }
}

/// Move every quarantined `*.json` event back to the plain spool, leaving the
/// marker files where they are.
fn move_quarantined_entries_to_spool(dir: &Path, spool: &Path) -> std::io::Result<()> {
    for item in std::fs::read_dir(dir)? {
        let item = item?;
        if item.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let destination = spool.join(item.file_name());
        std::fs::rename(item.path(), destination)?;
    }
    Ok(())
}

fn read_recovery_match(path: &Path) -> std::io::Result<RecoveryMatch> {
    serde_json::from_slice(&read_spool_entry_bytes(path)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn move_late_matching_entries(
    spool: &Path,
    dir: &Path,
    config: &crate::config::Config,
    spec: &RecoveryMatch,
) -> std::io::Result<()> {
    let (files, _) = list_entries(spool)?;
    for path in files {
        let Ok(bytes) = read_spool_entry_bytes(&path) else {
            continue;
        };
        let Ok(entry) = serde_json::from_slice::<SpoolEntry>(&bytes) else {
            continue;
        };
        if entry.created_ms >= spec.interval_started_ms
            && entry.created_ms <= spec.interval_ended_ms
            && spool_entry_matches_session(
                &entry,
                &spec.server_identity,
                &spec.agent,
                Some(config),
                &spec.workspace,
                &spec.project,
                Path::new(&spec.cwd),
                &spec.native_session_id,
            )
            && recovery_target_for_entry(spool, &entry, Some(config))?
                .is_some_and(|target| target.dir() == dir)
            && let Some(name) = path.file_name()
        {
            std::fs::rename(&path, dir.join(name))?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn quarantine_session_entries(
    config: &crate::config::Config,
    spool: &Path,
    journal_id: &str,
    interval_started_ms: u64,
    interval_ended_ms: u64,
    server_identity: &str,
    agent: &str,
    workspace: &str,
    project: &str,
    cwd: &Path,
    native_session_id: &str,
) -> std::io::Result<QuarantinedSpool> {
    let coord_lock = open_recovery_coord_lock(spool)?;
    coord_lock.lock_exclusive()?;
    sweep_recovery_tombstones(spool)?;
    let coordinate_key = recovery_coordinate_key(
        server_identity,
        agent,
        workspace,
        project,
        cwd.to_string_lossy().as_ref(),
        native_session_id,
    )?;
    let dir = spool
        .join(RECOVERY_DIR)
        .join(recovery_interval_key(journal_id)?);
    create_spool_dir(&dir)?;
    let match_spec = RecoveryMatch {
        journal_id: journal_id.to_owned(),
        coordinate_key,
        interval_started_ms,
        interval_ended_ms,
        server_identity: server_identity.to_owned(),
        agent: agent.to_owned(),
        workspace: workspace.to_owned(),
        project: project.to_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        native_session_id: native_session_id.to_owned(),
    };
    write_private(
        &dir.join(RECOVERY_MATCH_FILE),
        &serde_json::to_vec(&match_spec)?,
    )?;
    if let Err(error) = move_late_matching_entries(spool, &dir, config, &match_spec) {
        let _ = std::fs::remove_file(dir.join(RECOVERY_MATCH_FILE));
        return Err(error);
    }
    Ok(QuarantinedSpool {
        dir,
        spool: spool.to_path_buf(),
        config: config.clone(),
    })
}

fn open_recovery_coord_lock(spool: &Path) -> std::io::Result<File> {
    create_spool_dir(spool)?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(spool.with_extension("recovery.lock"))
}

fn sweep_recovery_tombstones(spool: &Path) -> std::io::Result<()> {
    let recovery = spool.join(RECOVERY_DIR);
    let entries = match std::fs::read_dir(&recovery) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let now = SystemTime::now();
    let mut completed = Vec::new();
    for entry in entries.take(MAX_RECOVERY_SWEEP_DIRS) {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let marker = entry.path().join(RECOVERY_COMPLETED_FILE);
        let Ok(metadata) = marker.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        if now.duration_since(modified).unwrap_or_default().as_millis() as u64
            > RECOVERY_TOMBSTONE_AGE_MS
        {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            completed.push((modified, entry.path()));
        }
    }
    if completed.len() > MAX_RECOVERY_TOMBSTONES {
        completed.sort_by_key(|(modified, _)| *modified);
        let excess = completed.len() - MAX_RECOVERY_TOMBSTONES;
        for (_, path) in completed.into_iter().take(excess) {
            std::fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

enum RecoveryTarget {
    Active(PathBuf),
    Completed(PathBuf),
}

impl RecoveryTarget {
    fn dir(&self) -> &Path {
        match self {
            Self::Active(dir) | Self::Completed(dir) => dir,
        }
    }
}

fn recovery_target_for_entry(
    spool: &Path,
    entry: &SpoolEntry,
    config: Option<&crate::config::Config>,
) -> std::io::Result<Option<RecoveryTarget>> {
    let coordinate_key = recovery_coordinate_key_for_entry(entry, config)?;
    let recovery = spool.join(RECOVERY_DIR);
    let directories = match std::fs::read_dir(recovery) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut matched = None;
    for directory in directories.take(MAX_RECOVERY_SWEEP_DIRS) {
        let directory = directory?;
        if !directory.file_type()?.is_dir() {
            continue;
        }
        let dir = directory.path();
        let marker = if dir.join(RECOVERY_MATCH_FILE).is_file() {
            RECOVERY_MATCH_FILE
        } else if dir.join(RECOVERY_COMPLETED_FILE).is_file() {
            RECOVERY_COMPLETED_FILE
        } else {
            continue;
        };
        let Ok(spec) = read_recovery_match(&dir.join(marker)) else {
            continue;
        };
        if spec.coordinate_key != coordinate_key
            || entry.created_ms < spec.interval_started_ms
            || entry.created_ms > spec.interval_ended_ms
        {
            continue;
        }
        let candidate = if marker == RECOVERY_MATCH_FILE {
            RecoveryTarget::Active(dir)
        } else {
            RecoveryTarget::Completed(dir)
        };
        if matched.is_some() {
            return Ok(None);
        }
        matched = Some(candidate);
    }
    Ok(matched)
}

fn recovery_coordinate_key_for_entry(
    entry: &SpoolEntry,
    config: Option<&crate::config::Config>,
) -> std::io::Result<String> {
    let url = reqwest::Url::parse(&entry.url)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut identity = url.clone();
    identity.set_query(None);
    if identity.path().ends_with("/hook") {
        let path = identity.path().trim_end_matches("/hook").to_owned();
        identity.set_path(&path);
    }
    let mut agent = None;
    let mut workspace = None;
    let mut project = None;
    let mut session = None;
    let mut cwd = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "agent" => agent = Some(value.into_owned()),
            "workspace" => workspace = Some(value.into_owned()),
            "project" => project = Some(value.into_owned()),
            "session_id" => session = Some(value.into_owned()),
            "cwd" => cwd = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Ok(body) = serde_json::from_str::<serde_json::Value>(&entry.body) {
        let context = ai_memory_hooks::hook_payload_context(&body);
        session = context.session_id.or(session);
        cwd = context.cwd.or(cwd);
    }
    let (workspace, project) = match (config, cwd.as_deref()) {
        (Some(config), Some(cwd)) => super::resolve_scope_for_path_with_explicit(
            config,
            Path::new(cwd),
            workspace.as_deref(),
            project.as_deref(),
        )
        .unwrap_or_else(|_| {
            (
                workspace.clone().unwrap_or_default(),
                project.clone().unwrap_or_default(),
            )
        }),
        _ => (workspace.unwrap_or_default(), project.unwrap_or_default()),
    };
    recovery_coordinate_key(
        identity.as_str().trim_end_matches('/'),
        agent.as_deref().unwrap_or_default(),
        &workspace,
        &project,
        cwd.as_deref().unwrap_or_default(),
        session.as_deref().unwrap_or_default(),
    )
}

fn recovery_interval_key(journal_id: &str) -> std::io::Result<String> {
    if journal_id.is_empty()
        || journal_id.len() > 128
        || !journal_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid recovery journal interval id",
        ));
    }
    use sha2::{Digest as _, Sha256};
    Ok(format!("{:x}", Sha256::digest(journal_id.as_bytes())))
}

fn recovery_coordinate_key(
    server_identity: &str,
    agent: &str,
    workspace: &str,
    project: &str,
    cwd: &str,
    native_session_id: &str,
) -> std::io::Result<String> {
    use sha2::{Digest as _, Sha256};
    let cwd = crate::marker::absolute_normalized(Path::new(cwd));
    let mut hasher = Sha256::new();
    for value in [
        server_identity,
        agent,
        workspace,
        project,
        cwd.to_string_lossy().as_ref(),
        native_session_id,
    ] {
        hasher.update(value.len().to_be_bytes());
        hasher.update(value.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[allow(clippy::too_many_arguments)]
fn spool_entry_matches_session(
    entry: &SpoolEntry,
    server_identity: &str,
    agent: &str,
    config: Option<&crate::config::Config>,
    workspace: &str,
    project: &str,
    cwd: &Path,
    native_session_id: &str,
) -> bool {
    let Ok(url) = reqwest::Url::parse(&entry.url) else {
        return false;
    };
    let mut identity = url.clone();
    identity.set_query(None);
    if identity.path().ends_with("/hook") {
        let path = identity.path().trim_end_matches("/hook").to_owned();
        identity.set_path(&path);
    }
    let identity = identity.as_str().trim_end_matches('/');
    if identity != server_identity.trim_end_matches('/') {
        return false;
    }
    let mut session = None;
    let mut event_agent = None;
    let mut ws = None;
    let mut proj = None;
    let mut event_cwd = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "agent" => event_agent = Some(value.into_owned()),
            "session_id" => session = Some(value.into_owned()),
            "workspace" => ws = Some(value.into_owned()),
            "project" => proj = Some(value.into_owned()),
            "cwd" => event_cwd = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Ok(body) = serde_json::from_str::<serde_json::Value>(&entry.body) {
        let context = ai_memory_hooks::hook_payload_context(&body);
        session = context.session_id.or(session);
        event_cwd = context.cwd.or(event_cwd);
    }
    let expected_cwd = crate::marker::absolute_normalized(cwd);
    let same_cwd = event_cwd
        .as_deref()
        .is_some_and(|value| crate::marker::absolute_normalized(Path::new(value)) == expected_cwd);
    let explicit_match = ws.as_deref() == Some(workspace) && proj.as_deref() == Some(project);
    let scope_matches = if explicit_match {
        true
    } else if let (Some(config), Some(event_cwd)) = (config, event_cwd.as_deref()) {
        super::resolve_scope_for_path_with_explicit(
            config,
            Path::new(event_cwd),
            ws.as_deref(),
            proj.as_deref(),
        )
        .is_ok_and(|resolved| resolved == (workspace.to_owned(), project.to_owned()))
    } else {
        ws.as_deref().is_none_or(|value| value == workspace)
            && proj.as_deref().is_none_or(|value| value == project)
            && ((ws.is_some() && proj.is_some()) || same_cwd)
    };
    event_agent.as_deref() == Some(agent)
        && session.as_deref() == Some(native_session_id)
        && scope_matches
        && same_cwd
}

/// Windows can report a contended fs2 byte-range lock as this native OS code
/// instead of mapping it to `WouldBlock`.
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

pub(crate) fn is_drain_lock_busy_error(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }

    #[cfg(windows)]
    if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
        return true;
    }

    false
}

/// Outcome for lock-aware drain wrappers.
#[derive(Debug, PartialEq, Eq)]
pub enum LockedDrainResult {
    /// The lock was acquired and a drain ran.
    Drained(DrainResult),
    /// Another process is draining; this is expected and not an error.
    LockBusy,
}

/// Try to acquire the single-flight drain lock.
pub fn acquire_drain_lock(spool: &Path, wait: DrainLockWait) -> std::io::Result<Option<DrainLock>> {
    std::fs::create_dir_all(spool)?;
    let path = spool.join(".drain.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let started = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(Some(DrainLock { _file: file })),
            Err(err) if is_drain_lock_busy_error(&err) => match wait {
                DrainLockWait::NoWait => return Ok(None),
                DrainLockWait::Bounded(limit) if started.elapsed() >= limit => return Ok(None),
                DrainLockWait::Bounded(limit) => {
                    let sleep =
                        Duration::from_millis(25).min(limit.saturating_sub(started.elapsed()));
                    if sleep.is_zero() {
                        return Ok(None);
                    }
                    std::thread::sleep(sleep);
                }
            },
            Err(err) => return Err(err),
        }
    }
}

/// Run one exclusive drain pass if the single-flight lock is available.
pub async fn drain_exclusive(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    wait: DrainLockWait,
    max_attempts: MaxAttempts,
) -> Option<DrainResult> {
    match drain_exclusive_result(
        spool,
        data_dir,
        total_budget,
        per_event_timeout,
        wait,
        max_attempts,
    )
    .await
    {
        Ok(LockedDrainResult::Drained(result)) => Some(result),
        Ok(LockedDrainResult::LockBusy) | Err(_) => None,
    }
}

/// Run one exclusive drain pass, distinguishing lock contention from lock IO errors.
pub async fn drain_exclusive_result(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    wait: DrainLockWait,
    max_attempts: MaxAttempts,
) -> std::io::Result<LockedDrainResult> {
    let Some(_lock) = acquire_drain_lock(spool, wait)? else {
        return Ok(LockedDrainResult::LockBusy);
    };
    Ok(LockedDrainResult::Drained(
        drain_with_max_attempts(
            spool,
            data_dir,
            total_budget,
            per_event_timeout,
            max_attempts,
        )
        .await,
    ))
}

/// Run one exclusive drain pass while treating lock wait + drain as one budget.
pub async fn drain_exclusive_within_budget(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    max_attempts: MaxAttempts,
) -> std::io::Result<LockedDrainResult> {
    let started = Instant::now();
    let Some(_lock) = acquire_drain_lock(spool, DrainLockWait::Bounded(total_budget))? else {
        return Ok(LockedDrainResult::LockBusy);
    };
    let Some(remaining_budget) = total_budget.checked_sub(started.elapsed()) else {
        return Ok(LockedDrainResult::Drained(DrainResult {
            remaining: spool_len(spool),
            ..DrainResult::default()
        }));
    };
    if remaining_budget.is_zero() {
        return Ok(LockedDrainResult::Drained(DrainResult {
            remaining: spool_len(spool),
            ..DrainResult::default()
        }));
    }
    Ok(LockedDrainResult::Drained(
        drain_with_max_attempts(
            spool,
            data_dir,
            remaining_budget,
            per_event_timeout,
            max_attempts,
        )
        .await,
    ))
}

/// Hold the drain lock and keep draining until the spool is quiescent or budget expires.
pub async fn drain_until_quiescent(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    wait: DrainLockWait,
    max_attempts: MaxAttempts,
) -> std::io::Result<LockedDrainResult> {
    drain_until_quiescent_with_live_token(
        spool,
        data_dir,
        total_budget,
        per_event_timeout,
        wait,
        live_static_token().as_deref(),
        max_attempts,
    )
    .await
}

pub async fn drain_until_quiescent_with_live_token(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    wait: DrainLockWait,
    live_token: Option<&str>,
    max_attempts: MaxAttempts,
) -> std::io::Result<LockedDrainResult> {
    let Some(_lock) = acquire_drain_lock(spool, wait)? else {
        return Ok(LockedDrainResult::LockBusy);
    };
    Ok(LockedDrainResult::Drained(
        drain_until_quiescent_locked_with_live_token(
            spool,
            data_dir,
            total_budget,
            per_event_timeout,
            live_token,
            max_attempts,
        )
        .await,
    ))
}

pub async fn drain_until_quiescent_locked_with_live_token(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    live_token: Option<&str>,
    max_attempts: MaxAttempts,
) -> DrainResult {
    let started = Instant::now();
    let mut combined = DrainResult::default();

    loop {
        let Some(remaining_budget) = total_budget.checked_sub(started.elapsed()) else {
            combined.remaining = spool_len(spool);
            return combined;
        };
        if remaining_budget.is_zero() {
            combined.remaining = spool_len(spool);
            return combined;
        }

        let result = drain_with_live_token(
            spool,
            data_dir,
            remaining_budget,
            per_event_timeout,
            live_token,
            max_attempts,
        )
        .await;
        combined.sent += result.sent;
        combined.durable += result.durable;
        combined.dropped += result.dropped;
        combined.remaining = result.remaining;

        if result.remaining > 0 {
            return combined;
        }

        let queued = spool_len(spool);
        if queued == 0 {
            combined.remaining = 0;
            return combined;
        }
        combined.remaining = queued;
    }
}

/// Drain the spool to the server, oldest-first, within `total_budget`.
///
/// Events are delivered in **batches** via `POST /hook/batch`: one request
/// carries many spooled events, so the per-request cost (TLS + network RTT + the
/// edge auth hop) is amortized over the whole batch instead of paid per event.
/// That is the throughput fix — a sequential per-event drain falls behind when
/// many parallel sessions share one spool against a remote, gated server, and
/// the spool then grows to its cap and evicts undelivered events. A server
/// without `/hook/batch` (a pre-upgrade build) answers `404`/`405`, and the
/// drain transparently falls back to per-event `POST /hook`.
///
/// A delivered event is deleted; a failed one is charged a retry attempt
/// (dropped at the configured limit, unless it is zero); a `429` (saturation)
/// deletes any accepted prefix the server reports, then retries the rest
/// untouched so it never burns the retry budget. OIDC bearer is resolved and
/// refreshed at most once per drain and cached.
///
/// Best-effort: returns counts and never errors, so a session boundary is never
/// blocked beyond the budget and never fails the agent.
#[cfg(test)]
async fn drain(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
) -> DrainResult {
    drain_with_max_attempts(
        spool,
        data_dir,
        total_budget,
        per_event_timeout,
        MaxAttempts::new(MAX_ATTEMPTS),
    )
    .await
}

pub async fn drain_with_max_attempts(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    max_attempts: MaxAttempts,
) -> DrainResult {
    drain_with_live_token(
        spool,
        data_dir,
        total_budget,
        per_event_timeout,
        live_static_token().as_deref(),
        max_attempts,
    )
    .await
}

/// [`drain_with_max_attempts`] with the current static bearer supplied explicitly.
///
/// The live token and retry policy are resolved at the command boundary and
/// passed down as data. Tests drive this directly, so drain behavior is isolated
/// from ambient process configuration.
pub async fn drain_with_live_token(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    live_token: Option<&str>,
    max_attempts: MaxAttempts,
) -> DrainResult {
    let (mut files, unreadable) = match list_entries(spool) {
        Ok(listed) => listed,
        Err(e) => {
            // Never silent: a spool we cannot read is an outage, not an idle
            // pass, and the difference is invisible from the exit code.
            eprintln!(
                "ai-memory hook-drain warning: could not list the spool directory {}: {e}; \
                 no events were attempted",
                spool.display()
            );
            return DrainResult::default();
        }
    };
    if unreadable > 0 {
        eprintln!(
            "ai-memory hook-drain warning: skipped {unreadable} unreadable spool entr{} in {}",
            if unreadable == 1 { "y" } else { "ies" },
            spool.display()
        );
    }
    files.sort();

    let client = build_client();
    let started = Instant::now();
    let mut oidc_cache: Option<Option<String>> = None; // outer None = not yet resolved
    let mut profile_tokens = ProfileTokens::new(data_dir);
    let mut result = DrainResult::default();
    let max_attempts = max_attempts.0;

    let mut idx = 0;
    let mut batch_supported = true;
    // Endpoints that proved unreachable during THIS pass. A spooled entry
    // carries the URL it was captured against, so a spool can hold entries
    // addressed to a port nothing listens on any more (an old `--bind`, a
    // restarted server on a new port). Those can never be delivered, and
    // before #493 the drain stopped at the first one — charging a single
    // attempt and returning, which left every deliverable entry behind them
    // stuck until it aged out of the 7-day window and was dropped undelivered.
    //
    // Recording the dead endpoint lets the pass skip its entries (without
    // charging attempts they did not earn) and carry on to entries addressed
    // somewhere that answers. A total outage keeps the same pass shape —
    // every entry shares the one dead endpoint — but no longer costs an
    // attempt: an endpoint-level miss never charges the entry's retry
    // budget, so an outage is ridden out (bounded by the 10k file cap and
    // the 7-day TTL) instead of deleting the oldest events.
    let mut unreachable_endpoints: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Resolved on first need only — a healthy drain never reads the config.
    let mut configured_server: Option<Option<String>> = None;
    'drain: while idx < files.len() {
        if started.elapsed() >= total_budget {
            result.remaining += files.len() - idx;
            break;
        }

        let path = files[idx].clone();
        idx += 1;
        let Some(entry) = load_live_entry(&path, &mut result) else {
            continue;
        };
        if unreachable_endpoints.contains(&batch_endpoint(&entry.url)) {
            // Already proven undeliverable this pass. Leave it queued and do
            // not bump attempts: it was never actually attempted.
            result.remaining += 1;
            continue;
        }
        if body_is_malformed(&entry) {
            bump_or_drop(&path, &entry, max_attempts, &mut result);
            continue;
        }

        let bearer = entry_bearer(&entry, &client, data_dir, &mut oidc_cache).await;

        if batch_supported {
            // Extend the chunk over consecutive entries sharing the same batch
            // endpoint AND bearer (one request carries one Authorization header),
            // bounded by item count and body bytes.
            let base = batch_endpoint(&entry.url);
            let mut bytes = entry_wire_len(&entry);
            let mut chunk = Vec::with_capacity(MAX_BATCH_ITEMS.min(files.len() - idx + 1));
            chunk.push((path, entry));
            while idx < files.len() && chunk.len() < MAX_BATCH_ITEMS {
                if started.elapsed() >= total_budget {
                    break;
                }
                let next_path = files[idx].clone();
                idx += 1;
                let Some(next_entry) = load_live_entry(&next_path, &mut result) else {
                    continue;
                };
                if body_is_malformed(&next_entry) {
                    bump_or_drop(&next_path, &next_entry, max_attempts, &mut result);
                    continue;
                }
                // A profile entry never shares a request with another route,
                // even at the same address: the retry and reroot decisions
                // below are made once per chunk, from its first entry.
                if batch_endpoint(&next_entry.url) != base
                    || next_entry.profile != chunk[0].1.profile
                {
                    idx -= 1;
                    break;
                }
                let next_bearer =
                    entry_bearer(&next_entry, &client, data_dir, &mut oidc_cache).await;
                if next_bearer != bearer {
                    idx -= 1;
                    break;
                }
                let next_len = entry_wire_len(&next_entry);
                if bytes + next_len > MAX_BATCH_BYTES {
                    idx -= 1;
                    break;
                }
                bytes += next_len;
                chunk.push((next_path, next_entry));
            }

            let Some(payload) = batch_payload(&chunk) else {
                bump_or_drop(&chunk[0].0, &chunk[0].1, max_attempts, &mut result);
                result.remaining += chunk.len().saturating_sub(1) + files.len().saturating_sub(idx);
                break;
            };
            let Some(remaining_budget) = total_budget.checked_sub(started.elapsed()) else {
                result.remaining += chunk.len() + files.len().saturating_sub(idx);
                break;
            };
            let batch_timeout =
                batch_request_timeout(per_event_timeout, remaining_budget, chunk.len());
            match post_batch(&client, &base, &payload, bearer.as_deref(), batch_timeout).await {
                BatchOutcome::Detailed {
                    outcomes,
                    failed_index,
                } => {
                    if outcomes.len() > chunk.len()
                        || failed_index.is_some_and(|index| index >= chunk.len())
                    {
                        bump_or_drop(&chunk[0].0, &chunk[0].1, max_attempts, &mut result);
                        result.remaining +=
                            chunk.len().saturating_sub(1) + files.len().saturating_sub(idx);
                        break;
                    }
                    for (position, outcome) in outcomes.iter().enumerate() {
                        let terminal = matches!(outcome.as_str(), "stored" | "replayed")
                            || outcome == "ignored_end"
                                && entry_event(&chunk[position].1).as_deref()
                                    == Some("session-end");
                        if terminal {
                            let _ = std::fs::remove_file(&chunk[position].0);
                            result.sent += 1;
                            result.durable += 1;
                        } else {
                            bump_or_drop(
                                &chunk[position].0,
                                &chunk[position].1,
                                max_attempts,
                                &mut result,
                            );
                        }
                    }
                    if let Some(failed_index) = failed_index {
                        bump_or_drop(
                            &chunk[failed_index].0,
                            &chunk[failed_index].1,
                            max_attempts,
                            &mut result,
                        );
                    }
                    result.remaining += chunk
                        .len()
                        .saturating_sub(outcomes.len())
                        .saturating_sub(usize::from(failed_index.is_some()));
                }
                BatchOutcome::Accepted(k) => {
                    let k = k.min(chunk.len());
                    for (path, _) in &chunk[..k] {
                        let _ = std::fs::remove_file(path);
                    }
                    result.sent += k;
                    if k < chunk.len() {
                        // The (idx+k)th event is the one the server stopped on
                        // (fail-fast). Charge it a failed attempt and skip past it
                        // so a single bad event can't wedge the rest — the
                        // per-event loop also advances past a failed entry.
                        bump_or_drop(&chunk[k].0, &chunk[k].1, max_attempts, &mut result);
                        result.remaining +=
                            chunk.len().saturating_sub(k + 1) + files.len().saturating_sub(idx);
                        break;
                    }
                }
                BatchOutcome::AcceptedIndices {
                    indices,
                    failed_index,
                } => {
                    let sent = delete_accepted_indices(&chunk, &indices);
                    result.sent += sent;
                    if let Some(failed_index) = failed_index.and_then(|i| chunk.get(i).map(|_| i)) {
                        bump_or_drop(
                            &chunk[failed_index].0,
                            &chunk[failed_index].1,
                            max_attempts,
                            &mut result,
                        );
                        result.remaining += chunk.len().saturating_sub(sent).saturating_sub(1)
                            + files.len().saturating_sub(idx);
                        break;
                    }
                    result.remaining += chunk.len().saturating_sub(sent);
                }
                BatchOutcome::Saturated(k) => {
                    // Server ingest filled after committing a leading prefix.
                    // Delete only that prefix; leave the saturated item and all
                    // later entries queued without bumping attempts (parity with
                    // per-event 429 handling).
                    let k = k.min(chunk.len());
                    for (path, _) in &chunk[..k] {
                        let _ = std::fs::remove_file(path);
                    }
                    result.sent += k;
                    result.remaining +=
                        chunk.len().saturating_sub(k) + files.len().saturating_sub(idx);
                    break;
                }
                BatchOutcome::SaturatedIndices(indices) => {
                    let sent = delete_accepted_indices(&chunk, &indices);
                    result.sent += sent;
                    result.remaining +=
                        chunk.len().saturating_sub(sent) + files.len().saturating_sub(idx);
                    break;
                }
                BatchOutcome::Unsupported => {
                    // Pre-upgrade server with no /hook/batch: fall back to the
                    // per-event path for the current chunk and the rest of the
                    // drain. Do not rewind: entries skipped while building this
                    // chunk (unparseable/stale/malformed) have already been
                    // handled locally and must not be charged twice.
                    batch_supported = false;
                    for (pos, (path, entry)) in chunk.iter().enumerate() {
                        if started.elapsed() >= total_budget {
                            result.remaining += chunk.len() - pos + files.len().saturating_sub(idx);
                            break 'drain;
                        }
                        let item_bearer =
                            entry_bearer(entry, &client, data_dir, &mut oidc_cache).await;
                        match post_hook(
                            &client,
                            &entry.url,
                            &entry.body,
                            item_bearer.as_deref(),
                            per_event_timeout,
                        )
                        .await
                        {
                            PostOutcome::Delivered => {
                                let _ = std::fs::remove_file(path);
                                result.sent += 1;
                            }
                            PostOutcome::Saturated => {
                                result.remaining += 1;
                            }
                            PostOutcome::Failed => {
                                bump_or_drop(path, entry, max_attempts, &mut result);
                            }
                            PostOutcome::Refused => {
                                // Never retried: see `PostOutcome::Refused`.
                                // Dropped rather than charged an attempt, so
                                // it cannot sit in the spool being re-sent.
                                let _ = std::fs::remove_file(path);
                                result.dropped += 1;
                            }
                            PostOutcome::Unreachable => {
                                // Same reasoning as the batch arm: the address
                                // is dead, not the entry — an endpoint-level
                                // outage never charges the retry budget. Leave
                                // it queued and skip its siblings for the rest
                                // of the pass.
                                result.remaining += 1;
                                unreachable_endpoints.insert(batch_endpoint(&entry.url));
                            }
                            PostOutcome::Unauthorized => {
                                // Credential rejected, not the entry. Retry once
                                // with this drain's live token (#542).
                                let retry = static_retry_token(
                                    entry,
                                    item_bearer.as_deref(),
                                    live_token,
                                    &mut profile_tokens,
                                );
                                let recovered = match retry {
                                    Some(token) => matches!(
                                        post_hook(
                                            &client,
                                            &entry.url,
                                            &entry.body,
                                            Some(&token),
                                            per_event_timeout,
                                        )
                                        .await,
                                        PostOutcome::Delivered
                                    ),
                                    None => false,
                                };
                                if recovered {
                                    let _ = std::fs::remove_file(path);
                                    result.sent += 1;
                                } else {
                                    bump_or_drop(path, entry, max_attempts, &mut result);
                                }
                            }
                        }
                    }
                }
                BatchOutcome::Unreachable => {
                    // Nothing is listening where this entry was captured. The
                    // envelope froze the address at capture time, so a server
                    // that came back on a different port leaves the whole
                    // spool addressed somewhere dead (#493).
                    //
                    // Before giving up, retry once against the address this
                    // store is actually configured to use — but only when the
                    // recorded host is loopback, where the authority can only
                    // ever have meant "this machine" and a stale port is
                    // unambiguous. A remote host is left alone: re-pointing it
                    // would misdeliver a spool captured against another server
                    // on purpose.
                    //
                    // This runs only for an endpoint that has already failed to
                    // connect, so a healthy drain is untouched by it.
                    let configured = configured_server
                        .get_or_insert_with(|| configured_server_url(data_dir))
                        .clone();
                    // A profile entry is left alone too: `config.toml`'s
                    // address is the install default, and re-pointing a
                    // profile's capture there is the cross-server delivery
                    // profiles exist to prevent (#992).
                    let retry = configured.as_deref().and_then(|server| {
                        if chunk[0].1.profile.is_some() || !is_loopback_url(&chunk[0].1.url) {
                            return None;
                        }
                        let rerooted = batch_endpoint(&reroot_url(&chunk[0].1.url, server)?);
                        (rerooted != base).then_some(rerooted)
                    });

                    // Whether the configured-address retry reached a server
                    // that answered — any HTTP status is post-connect and
                    // keeps today's charging semantics for the entry tried.
                    let mut reroot_answered = false;
                    if let Some(rerooted) = retry {
                        match post_batch(
                            &client,
                            &rerooted,
                            &payload,
                            bearer.as_deref(),
                            batch_timeout,
                        )
                        .await
                        {
                            BatchOutcome::Detailed { outcomes, .. } => {
                                let durable = outcomes
                                    .iter()
                                    .enumerate()
                                    .filter(|(position, outcome)| {
                                        matches!(outcome.as_str(), "stored" | "replayed")
                                            || outcome.as_str() == "ignored_end"
                                                && entry_event(&chunk[*position].1).as_deref()
                                                    == Some("session-end")
                                    })
                                    .map(|(position, _)| position)
                                    .collect::<Vec<_>>();
                                let sent = delete_accepted_indices(&chunk, &durable);
                                result.sent += sent;
                                result.durable += sent;
                                result.remaining += chunk.len().saturating_sub(sent);
                                continue;
                            }
                            BatchOutcome::Accepted(k) => {
                                let k = k.min(chunk.len());
                                for (path, _) in &chunk[..k] {
                                    let _ = std::fs::remove_file(path);
                                }
                                result.sent += k;
                                result.remaining += chunk.len().saturating_sub(k);
                                continue;
                            }
                            BatchOutcome::AcceptedIndices { indices, .. } => {
                                let sent = delete_accepted_indices(&chunk, &indices);
                                result.sent += sent;
                                result.remaining += chunk.len().saturating_sub(sent);
                                continue;
                            }
                            // The configured address is itself unreachable:
                            // still an endpoint-level outage — fall through to
                            // the no-charge skip path below.
                            BatchOutcome::Unreachable => {}
                            // The configured address answered but did not take
                            // the batch: post-connect, charge conservatively as
                            // before the outage fix.
                            _ => {
                                reroot_answered = true;
                            }
                        }
                    }

                    if reroot_answered {
                        // A server answered and refused: charge the entry
                        // actually tried, mark the endpoint, and keep going:
                        // entries addressed elsewhere are still deliverable.
                        bump_or_drop(&chunk[0].0, &chunk[0].1, max_attempts, &mut result);
                        result.remaining += chunk.len().saturating_sub(1);
                    } else {
                        // Endpoint-level outage: no server was ever reached,
                        // so the entry did not fail delivery — leave the whole
                        // chunk queued WITHOUT charging attempts. Charging
                        // here burnt the queue head's budget once per pass,
                        // deleting the OLDEST events at ~1 per `max_attempts`
                        // passes while the server was down — structural data
                        // loss ahead of the 7-day TTL. A permanently dead
                        // address stays bounded by that TTL and the 10k file
                        // cap instead.
                        result.remaining += chunk.len();
                    }
                    unreachable_endpoints.insert(base.clone());
                    continue;
                }
                BatchOutcome::Unauthorized => {
                    // The envelope froze its credential at capture time, so a
                    // rotated token leaves every entry captured under the old
                    // one failing identically — the credential half of the
                    // same problem #493 fixed for the address (#542).
                    //
                    // Retry once with the token this drain was handed, which is
                    // current by construction. Runs only for a batch that has
                    // already been rejected, so a healthy drain never pays for
                    // it.
                    let retry = static_retry_token(
                        &chunk[0].1,
                        bearer.as_deref(),
                        live_token,
                        &mut profile_tokens,
                    );
                    if let Some(token) = retry {
                        match post_batch(&client, &base, &payload, Some(&token), batch_timeout)
                            .await
                        {
                            BatchOutcome::Detailed { outcomes, .. } => {
                                let durable = outcomes
                                    .iter()
                                    .enumerate()
                                    .filter(|(position, outcome)| {
                                        matches!(outcome.as_str(), "stored" | "replayed")
                                            || outcome.as_str() == "ignored_end"
                                                && entry_event(&chunk[*position].1).as_deref()
                                                    == Some("session-end")
                                    })
                                    .map(|(position, _)| position)
                                    .collect::<Vec<_>>();
                                let sent = delete_accepted_indices(&chunk, &durable);
                                result.sent += sent;
                                result.durable += sent;
                                result.remaining += chunk.len().saturating_sub(sent);
                                continue;
                            }
                            BatchOutcome::Accepted(k) => {
                                let k = k.min(chunk.len());
                                for (path, _) in &chunk[..k] {
                                    let _ = std::fs::remove_file(path);
                                }
                                result.sent += k;
                                result.remaining += chunk.len().saturating_sub(k);
                                continue;
                            }
                            BatchOutcome::AcceptedIndices { indices, .. } => {
                                let sent = delete_accepted_indices(&chunk, &indices);
                                result.sent += sent;
                                result.remaining += chunk.len().saturating_sub(sent);
                                continue;
                            }
                            // The current token is no better — the server is
                            // rejecting more than a stale credential. Fall
                            // through and charge conservatively.
                            _ => {}
                        }
                    }

                    // Same conservative accounting as `Failed`: the server
                    // refused the whole batch, so nothing in it was processed.
                    bump_or_drop(&chunk[0].0, &chunk[0].1, max_attempts, &mut result);
                    result.remaining +=
                        chunk.len().saturating_sub(1) + files.len().saturating_sub(idx);
                    break;
                }
                BatchOutcome::Failed => {
                    // The batch didn't land (server answered with an unexpected
                    // status, or the body did not parse).
                    // The server may have processed none, some, or all items before
                    // the client saw the failure. Charge only the first item
                    // conservatively and leave the rest queued for a future pass so
                    // a timeout cannot burn attempts for events that may never have
                    // been attempted.
                    bump_or_drop(&chunk[0].0, &chunk[0].1, max_attempts, &mut result);
                    result.remaining +=
                        chunk.len().saturating_sub(1) + files.len().saturating_sub(idx);
                    break;
                }
            }
        } else {
            // Per-event fallback (server without /hook/batch). Mirrors the
            // original drain's per-entry semantics exactly.
            match post_hook(
                &client,
                &entry.url,
                &entry.body,
                bearer.as_deref(),
                per_event_timeout,
            )
            .await
            {
                PostOutcome::Delivered => {
                    let _ = std::fs::remove_file(path);
                    result.sent += 1;
                }
                PostOutcome::Saturated => {
                    result.remaining += 1;
                }
                PostOutcome::Refused => {
                    // Terminal; see `PostOutcome::Refused`.
                    let _ = std::fs::remove_file(path);
                    result.dropped += 1;
                }
                PostOutcome::Failed => {
                    bump_or_drop(&path, &entry, max_attempts, &mut result);
                }
                PostOutcome::Unreachable => {
                    // Endpoint-level outage: the address is dead, not the
                    // entry — leave it queued without charging attempts (the
                    // batch arm above explains why), and skip its siblings
                    // for the rest of the pass.
                    result.remaining += 1;
                    unreachable_endpoints.insert(batch_endpoint(&entry.url));
                }
                PostOutcome::Unauthorized => {
                    // Credential rejected, not the entry. Retry once with this
                    // drain's live token (#542).
                    let retry = static_retry_token(
                        &entry,
                        bearer.as_deref(),
                        live_token,
                        &mut profile_tokens,
                    );
                    let recovered = match retry {
                        Some(token) => matches!(
                            post_hook(
                                &client,
                                &entry.url,
                                &entry.body,
                                Some(&token),
                                per_event_timeout,
                            )
                            .await,
                            PostOutcome::Delivered
                        ),
                        None => false,
                    };
                    if recovered {
                        let _ = std::fs::remove_file(&path);
                        result.sent += 1;
                    } else {
                        bump_or_drop(&path, &entry, max_attempts, &mut result);
                    }
                }
            }
        }
    }
    result
}

fn delete_accepted_indices(chunk: &[(PathBuf, SpoolEntry)], indices: &[usize]) -> usize {
    let mut sent = 0usize;
    for idx in indices.iter().copied() {
        if let Some((path, _)) = chunk.get(idx) {
            let _ = std::fs::remove_file(path);
            sent += 1;
        }
    }
    sent
}

fn batch_request_timeout(
    per_event_timeout: Duration,
    remaining_budget: Duration,
    item_count: usize,
) -> Duration {
    let item_count = u32::try_from(item_count.max(1)).unwrap_or(u32::MAX);
    per_event_timeout
        .checked_mul(item_count)
        .unwrap_or(Duration::MAX)
        .min(remaining_budget)
}

fn completed_tombstone_for_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let Some(coordinate_key) = stem.rsplit('-').next() else {
        return false;
    };
    let Some(created_ms) = created_ms_from_name(name) else {
        return false;
    };
    let Some(spool) = path.parent() else {
        return false;
    };
    let Ok(directories) = std::fs::read_dir(spool.join(RECOVERY_DIR)) else {
        return false;
    };
    let mut matches = 0usize;
    for directory in directories.take(MAX_RECOVERY_SWEEP_DIRS).flatten() {
        let marker = directory.path().join(RECOVERY_COMPLETED_FILE);
        let Ok(spec) = read_recovery_match(&marker) else {
            continue;
        };
        if spec.coordinate_key == coordinate_key
            && created_ms >= spec.interval_started_ms
            && created_ms <= spec.interval_ended_ms
        {
            matches += 1;
            if matches > 1 {
                return false;
            }
        }
    }
    matches == 1
}

fn read_spool_entry_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_SPOOL_ENTRY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "spool entry exceeds its byte limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SPOOL_ENTRY_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SPOOL_ENTRY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "spool entry exceeds its byte limit",
        ));
    }
    Ok(bytes)
}

fn load_live_entry(path: &Path, result: &mut DrainResult) -> Option<SpoolEntry> {
    if completed_tombstone_for_path(path) {
        let _ = std::fs::remove_file(path);
        return None;
    }
    let Ok(bytes) = read_spool_entry_bytes(path) else {
        result.remaining += 1;
        return None;
    };
    let Ok(mut entry) = serde_json::from_slice::<SpoolEntry>(&bytes) else {
        // Unparseable spool file: drop it so it can't wedge the queue.
        let _ = std::fs::remove_file(path);
        result.dropped += 1;
        return None;
    };
    if now_ms().saturating_sub(entry.created_ms) > MAX_AGE_MS {
        let _ = std::fs::remove_file(path);
        result.dropped += 1;
        return None;
    }
    // Backstop for entries spooled by a pre-#196 binary: strip any raw
    // assistant-message field before this entry can reach either drain path
    // (`/hook/batch` chunk or the per-event fallback). Loading is the single
    // choke point both paths pass through, so one strip here covers both.
    strip_assistant_from_entry(&mut entry);
    Some(entry)
}

/// Drop any raw assistant-message field (#196) from a spooled entry's body.
/// Reserializes only when the field was actually present, so untouched entries
/// keep byte-exact bodies. A body that no longer parses is left as-is; the
/// downstream `body_is_malformed` check already drops/retries it.
fn strip_assistant_from_entry(entry: &mut SpoolEntry) {
    let Ok(mut body) = serde_json::from_str::<serde_json::Value>(&entry.body) else {
        return;
    };
    if ai_memory_hooks::strip_assistant_message_raw(&mut body)
        && let Ok(reserialized) = serde_json::to_string(&body)
    {
        entry.body = reserialized;
    }
}

fn body_is_malformed(entry: &SpoolEntry) -> bool {
    serde_json::from_str::<serde_json::Value>(&entry.body).is_err()
}

/// Resolve the bearer for a spooled entry at drain time: a `Static` token is
/// stored inline; an `Oidc` entry resolves (and refreshes) the stored token
/// once per drain via `oidc_cache`; `Anonymous` is None.
async fn entry_bearer(
    entry: &SpoolEntry,
    client: &reqwest::Client,
    data_dir: &Path,
    oidc_cache: &mut Option<Option<String>>,
) -> Option<String> {
    match entry.auth_mode {
        AuthMode::Static => entry.token.clone(),
        AuthMode::Anonymous => None,
        AuthMode::Oidc => {
            if oidc_cache.is_none() {
                *oidc_cache = Some(
                    crate::auth_bearer::resolve_oidc(client, &data_dir.join("auth.json")).await,
                );
            }
            oidc_cache.clone().flatten()
        }
    }
}

/// Environment variable carrying the *current* static bearer into a spawned
/// `hook-drain`.
///
/// Deliberately the environment and not a command-line flag: the drain is a
/// separate process, and an argument would put the credential in the process
/// table for anyone running `ps` while it runs.
pub(crate) const LIVE_TOKEN_ENV: &str = "AI_MEMORY_AUTH_TOKEN";

/// The static bearer this drain process was handed, if any.
///
/// `hook-drain` is spawned by a lifecycle hook that already holds the current
/// token, and passes it down in the child's environment. A drain started by
/// hand, or by a hook running against a server that needs no auth, simply has
/// none — and the re-resolve below is skipped rather than guessed at.
fn live_static_token() -> Option<String> {
    std::env::var(LIVE_TOKEN_ENV).ok().filter(|t| !t.is_empty())
}

/// The token to retry a `401` with, or `None` when retrying is pointless.
///
/// A spooled envelope freezes its credential at capture time, so rotating the
/// token leaves every already-spooled entry authenticating with the old one
/// (#542). The drain process, by contrast, was handed the current token by the
/// hook that spawned it moments ago.
///
/// Two guards keep this from turning into a blind second attempt: only
/// `Static` entries qualify — an `Oidc` bearer is already resolved and
/// refreshed once per pass, so a 401 there is a real rejection — and the live
/// token must actually differ from the one that just failed. Retrying an
/// identical credential would only repeat the failure at double the cost.
///
/// A profile-routed entry (#992) never sees the live token, which belongs to
/// whichever install-default hook spawned this drain: presenting it to the
/// entry's server would hand one server's credential to another. It retries
/// with its own profile's current token instead, re-read from the data dir,
/// and a profile that has since been removed means no retry at all.
fn static_retry_token(
    entry: &SpoolEntry,
    rejected: Option<&str>,
    live: Option<&str>,
    profile_tokens: &mut ProfileTokens<'_>,
) -> Option<String> {
    if entry.auth_mode != AuthMode::Static {
        return None;
    }
    let current = match entry.profile.as_deref() {
        Some(raw) => profile_tokens.current(raw, &entry.url)?,
        None => live?.to_owned(),
    };
    (Some(current.as_str()) != rejected).then_some(current)
}

/// Each profile's current URL and token, read at most once per drain pass so
/// a backlog of rejected entries costs one registry read per profile, not per
/// entry.
struct ProfileTokens<'a> {
    data_dir: &'a Path,
    cache: std::collections::HashMap<String, Option<crate::server_profiles::ResolvedServer>>,
}

impl<'a> ProfileTokens<'a> {
    fn new(data_dir: &'a Path) -> Self {
        Self {
            data_dir,
            cache: std::collections::HashMap::new(),
        }
    }

    /// The profile's current token, only while the profile still points at
    /// the server `entry_url` was captured against. After a URL change the
    /// current token belongs to the *new* server, and replaying it to the old
    /// address would hand it to a server it was never issued for.
    fn current(&mut self, raw: &str, entry_url: &str) -> Option<String> {
        let data_dir = self.data_dir;
        let resolved = self
            .cache
            .entry(raw.to_owned())
            .or_insert_with(|| {
                let name = crate::server_profiles::ProfileName::parse(raw)?;
                crate::server_profiles::lookup(data_dir, &name).ok()
            })
            .as_ref()?;
        let rest = entry_url.strip_prefix(resolved.url.as_str())?;
        rest.starts_with('/').then(|| resolved.token.clone())
    }
}

/// Is this URL's host a loopback address?
///
/// A loopback authority is only meaningful on the machine that recorded it, so
/// a stale port in one is unambiguous — nothing else could have been meant. A
/// remote host is left alone: re-pointing it would misdeliver a spool that was
/// deliberately captured against another server.
fn is_loopback_url(url: &str) -> bool {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "::1"
}

/// Rewrite `url`'s scheme and authority onto `server_url`, keeping path and
/// query. Used only to retry an endpoint that has already proven unreachable.
fn reroot_url(url: &str, server_url: &str) -> Option<String> {
    let base = server_url.trim_end_matches('/');
    let rest = url.split_once("://").map(|(_, r)| r)?;
    let path_and_query = rest.find(['/', '?']).map(|i| &rest[i..]).unwrap_or("");
    Some(format!("{base}{path_and_query}"))
}

/// The server URL declared in **this store's own** `config.toml`, or `None`
/// when the file has no `server_url`.
///
/// Deliberately reads only that file rather than going through
/// `Config::load`, which also merges the process environment. A drain that
/// honoured `AI_MEMORY_SERVER_URL` would redirect captured private events to
/// whatever address happened to be exported into the process that ran it —
/// including, in a test run, a real server. The retry below sends data
/// somewhere other than where it was addressed, so its target must come from
/// the store's own on-disk configuration and nowhere else.
///
/// Resolved lazily and only after an endpoint has already failed to connect,
/// so a healthy drain never pays for it and the hot hook path is untouched.
fn configured_server_url(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join("config.toml")).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    doc.get("server_url")?
        .as_str()
        .map(std::string::ToString::to_string)
}

/// The `/hook/batch` URL for a spooled per-event URL: strip the `?…` query and
/// append `/batch` (a spooled URL ends in `…/hook` before its query). Entries
/// whose endpoint string matches can ride one batch request.
fn entry_event(entry: &SpoolEntry) -> Option<String> {
    reqwest::Url::parse(&entry.url)
        .ok()?
        .query_pairs()
        .find_map(|(key, value)| (key == "event").then(|| value.into_owned()))
}

fn batch_endpoint(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    format!("{path}/batch")
}

/// Rough wire size of one event inside a batch body (`{"url":…,"body":…}` plus
/// framing) — used only to keep a chunk under [`MAX_BATCH_BYTES`].
fn entry_wire_len(entry: &SpoolEntry) -> usize {
    entry.url.len() + entry.body.len() + 32
}

/// Serialize a chunk of entries into the `/hook/batch` request body — a JSON
/// array of `{url, body}`. Each `body` is re-parsed from its stored text; a
/// malformed body is a local retry/drop condition, never synthesized as `null`.
fn batch_payload(items: &[(PathBuf, SpoolEntry)]) -> Option<String> {
    let arr: Result<Vec<serde_json::Value>, serde_json::Error> = items
        .iter()
        .map(|(_, e)| {
            let body = serde_json::from_str::<serde_json::Value>(&e.body)?;
            Ok(serde_json::json!({ "url": e.url, "body": body }))
        })
        .collect();
    serde_json::to_string(&arr.ok()?).ok()
}

/// Charge a spooled entry a failed delivery attempt: drop it once it reaches
/// the configured limit (unless zero), else persist the bumped count. Updates
/// `result.dropped` / `result.remaining` accordingly.
fn bump_or_drop(path: &Path, entry: &SpoolEntry, max_attempts: u32, result: &mut DrainResult) {
    let mut bumped = entry.clone();
    bumped.attempts = bumped.attempts.saturating_add(1);
    if max_attempts != 0 && bumped.attempts >= max_attempts {
        let _ = std::fs::remove_file(path);
        result.dropped += 1;
    } else {
        let _ = note_retry_persist(rewrite_entry(path, &bumped));
        result.remaining += 1;
    }
}

/// Overwrite a spool file in place with the updated entry (atomic temp+rename),
/// used to persist a bumped attempt count after a failed delivery.
fn rewrite_entry(path: &Path, entry: &SpoolEntry) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let bytes = serde_json::to_vec(entry)?;
    write_private(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)
}

/// Report whether persisting a bumped retry count landed; on failure, emit a
/// sanitized stderr warning (no path — a raw spool path can be a Windows verbatim
/// `\\?\…` path) instead of swallowing it, so a poison entry can't retry
/// invisibly until it ages out. Fire-and-forget: warns only, never panics or
/// blocks; the returned bool is consumed only by tests.
fn note_retry_persist(outcome: std::io::Result<()>) -> bool {
    if outcome.is_err() {
        eprintln!(
            "ai-memory hook warning: failed to persist spool retry count; \
             event may retry until it ages out"
        );
        return false;
    }
    true
}

/// Resolve the bearer for a synchronous request (the session-start handoff
/// GET): a static `--auth-token` wins, else the stored OIDC token
/// (refreshed if stale), else none.
pub async fn resolve_bearer(
    client: &reqwest::Client,
    data_dir: &Path,
    auth_token: Option<&str>,
) -> Option<String> {
    crate::auth_bearer::resolve_bearer(client, &data_dir.join("auth.json"), auth_token).await
}

/// List `*.json` spool files (ignoring in-flight `*.json.tmp`), or None when the
/// directory doesn't exist yet.
/// Spool files awaiting delivery, or the IO error that prevented listing them.
///
/// This used to be `read_dir(spool).ok()?`, which turned *any* failure —
/// permissions, a missing directory, a transient FS error — into "no entries".
/// `drain` then returned an empty result: nothing sent, nothing dropped,
/// nothing remaining, exit 0, spool files untouched, and not one byte on the
/// wire. Indistinguishable from a healthy idle pass, and unfalsifiable from
/// outside the process (#493).
///
/// A per-entry error is still skipped rather than fatal — one unreadable
/// dirent must not strand the rest of the queue — but it is counted so the
/// caller can say so instead of silently shortening the queue.
fn list_entries(spool: &Path) -> std::io::Result<(Vec<PathBuf>, usize)> {
    let read = std::fs::read_dir(spool)?;
    let mut out = Vec::new();
    let mut unreadable = 0usize;
    for ent in read {
        match ent {
            Ok(ent) => {
                let path = ent.path();
                if path.extension().and_then(|e| e.to_str()) == Some("json") {
                    out.push(path);
                    if out.len() > MAX_SPOOL_FILES {
                        break;
                    }
                }
            }
            Err(_) => unreadable += 1,
        }
    }
    Ok((out, unreadable))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlated_session_entries_quarantine_and_restore_without_touching_others() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("hook-spool");
        let matching = SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=project&session_id=native-1".into(),
            body: r#"{"session_id":"native-1","cwd":"/repo","prompt":"same prompt"}"#.into(),
            created_ms: now_ms(),
            auth_mode: AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        let other = SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&workspace=ws&project=project&session_id=native-2".into(),
            body: r#"{"session_id":"native-2","cwd":"/repo","prompt":"other"}"#.into(),
            ..matching.clone()
        };
        enqueue(&spool, &matching).unwrap();
        enqueue(&spool, &other).unwrap();
        let quarantine = quarantine_session_entries(
            &crate::config::Config::default(),
            &spool,
            "journal-1",
            matching.created_ms.saturating_sub(1),
            matching.created_ms.saturating_add(1),
            "http://127.0.0.1:49374",
            "claude-code",
            "ws",
            "project",
            Path::new("/repo"),
            "native-1",
        )
        .unwrap();
        assert_eq!(spool_len(&spool), 1);
        quarantine.restore(&spool).unwrap();
        assert_eq!(spool_len(&spool), 2);
    }

    /// A quarantine whose `.active` marker is gone because the interval
    /// already completed restores as already-delivered instead of erroring —
    /// including when a rerun rewrote a fresh `.active` over the completed
    /// tombstone — while a directory with neither marker is still an error.
    #[test]
    fn restore_treats_a_completed_interval_as_already_delivered() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("hook-spool");
        let quarantine = |journal: &str| {
            quarantine_session_entries(
                &crate::config::Config::default(),
                &spool,
                journal,
                0,
                u64::MAX,
                "http://127.0.0.1:49374",
                "claude-code",
                "ws",
                "project",
                Path::new("/repo"),
                "native-1",
            )
        };
        let interval_dir = |journal: &str| {
            spool
                .join(RECOVERY_DIR)
                .join(recovery_interval_key(journal).unwrap())
        };
        let completed = quarantine("journal-done").unwrap();
        assert!(completed.complete_spool_only().unwrap());
        assert_eq!(
            completed.restore(&spool).unwrap(),
            RestoreOutcome::AlreadyDelivered,
            "a missing .active beside a completed marker means durable delivery"
        );
        let rerun = quarantine("journal-done").unwrap();
        assert!(
            interval_dir("journal-done")
                .join(RECOVERY_MATCH_FILE)
                .is_file(),
            "the rerun rewrites .active over the completed tombstone"
        );
        assert_eq!(
            rerun.restore(&spool).unwrap(),
            RestoreOutcome::AlreadyDelivered,
            "a rerun's rewritten .active never re-opens a completed interval"
        );
        assert!(
            interval_dir("journal-done")
                .join(RECOVERY_COMPLETED_FILE)
                .is_file()
        );
        assert!(
            !interval_dir("journal-done")
                .join(RECOVERY_MATCH_FILE)
                .is_file()
        );

        let broken = quarantine("journal-broken").unwrap();
        std::fs::remove_file(interval_dir("journal-broken").join(RECOVERY_MATCH_FILE)).unwrap();
        assert!(
            broken.restore(&spool).is_err(),
            "a directory with neither marker is still an error"
        );
    }

    #[test]
    fn quarantine_matching_reuses_every_canonical_nested_hook_context_shape() {
        let base = SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=project".into(),
            body: String::new(),
            created_ms: now_ms(),
            auth_mode: AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        let matching = [
            serde_json::json!({"info": {"id": "native-1", "directory": "/repo"}}),
            serde_json::json!({"properties": {"session_id": "native-1", "cwd": "/repo"}}),
            serde_json::json!({"properties": {"sessionID": "native-1", "cwd": "/repo"}}),
            serde_json::json!({"properties": {"info": {"id": "native-1", "directory": "/repo"}}}),
            serde_json::json!({"event": {"sessionId": "native-1", "working_dir": "/repo"}}),
            serde_json::json!({"event": {"properties": {"sessionID": "native-1", "cwd": "/repo"}}}),
            serde_json::json!({"event": {"properties": {"info": {"id": "native-1", "directory": "/repo"}}}}),
            serde_json::json!({"payload": {"conversationId": "native-1", "directory": "/repo"}}),
            serde_json::json!({"payload": {"path": {"session": "native-1", "cwd": "/repo"}}}),
            serde_json::json!({"payload": {"info": {"id": "native-1", "directory": "/repo"}}}),
            serde_json::json!({"payload": {"properties": {"sessionID": "native-1", "cwd": "/repo"}}}),
            serde_json::json!({"payload": {"properties": {"info": {"id": "native-1", "directory": "/repo"}}}}),
        ];
        for body in matching {
            let entry = SpoolEntry {
                body: body.to_string(),
                ..base.clone()
            };
            assert!(spool_entry_matches_session(
                &entry,
                "http://127.0.0.1:49374",
                "claude-code",
                None,
                "ws",
                "project",
                Path::new("/repo"),
                "native-1",
            ));
        }
        for body in [
            serde_json::json!({"payload": {"info": {"id": "other", "directory": "/repo"}}}),
            serde_json::json!({"payload": {"info": {"id": "native-1", "directory": "/other"}}}),
            serde_json::json!({"payload": {"info": {"id": "native-1"}}, "unrelated": {"cwd": "/repo"}}),
        ] {
            let entry = SpoolEntry {
                body: body.to_string(),
                ..base.clone()
            };
            assert!(!spool_entry_matches_session(
                &entry,
                "http://127.0.0.1:49374",
                "claude-code",
                None,
                "ws",
                "project",
                Path::new("/repo"),
                "native-1",
            ));
        }
    }

    #[test]
    fn quarantine_matches_partial_scope_and_captures_late_enqueue() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("project");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".ai-memory.toml"),
            "workspace = \"ws\"\nproject = \"project\"\n",
        )
        .unwrap();
        let spool = temp.path().join("hook-spool");
        let config = crate::config::Config::default();
        let base = SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws".into(),
            body: serde_json::json!({"session_id":"native-1","cwd":repo,"prompt":"one"})
                .to_string(),
            created_ms: now_ms(),
            auth_mode: AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        enqueue_with_config(&spool, &base, Some(&config)).unwrap();
        let quarantine = quarantine_session_entries(
            &config,
            &spool,
            "journal-partial",
            base.created_ms.saturating_sub(1),
            base.created_ms.saturating_add(1),
            "http://127.0.0.1:49374",
            "claude-code",
            "ws",
            "project",
            &repo,
            "native-1",
        )
        .unwrap();
        assert_eq!(
            quarantine.len(),
            1,
            "workspace-only event is correlated by cwd"
        );
        enqueue_with_config(
            &spool,
            &SpoolEntry {
                url: "http://127.0.0.1:49374/hook?event=stop&agent=claude-code&project=project"
                    .into(),
                body: serde_json::json!({"session_id":"native-1","cwd":repo}).to_string(),
                ..base.clone()
            },
            Some(&config),
        )
        .unwrap();
        assert_eq!(
            quarantine.len(),
            2,
            "late enqueue joins the active quarantine"
        );
        assert_eq!(spool_len(&spool), 0, "neither correlated event can drain");
        quarantine.restore(&spool).unwrap();
        assert_eq!(spool_len(&spool), 2);
    }

    #[test]
    fn enqueue_is_bounded_when_recovery_coordination_lock_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("hook-spool");
        let lock = open_recovery_coord_lock(&spool).unwrap();
        lock.lock_exclusive().unwrap();
        let entry = entry_for(
            "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=project".into(),
            r#"{"session_id":"native-1","cwd":"/repo","prompt":"late"}"#.into(),
            None,
            false,
        );
        let started = Instant::now();
        enqueue(&spool, &entry).unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(spool_len(&spool), 1, "content is retained for recovery");
    }

    #[test]
    fn completion_race_leaves_contended_enqueue_safe_for_tombstone_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("hook-spool");
        let config = crate::config::Config::default();
        let entry = SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=project".into(),
            body: r#"{"session_id":"native-1","cwd":"/repo","prompt":"late"}"#.into(),
            created_ms: now_ms(),
            auth_mode: AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        let quarantine = quarantine_session_entries(
            &config,
            &spool,
            "journal",
            entry.created_ms.saturating_sub(1),
            entry.created_ms.saturating_add(1),
            "http://127.0.0.1:49374",
            "claude-code",
            "ws",
            "project",
            Path::new("/repo"),
            "native-1",
        )
        .unwrap();
        PAUSE_ENQUEUE_AFTER_MAIN_RENAME.store(true, Ordering::SeqCst);
        ENQUEUE_REACHED_RECOVERY_WINDOW.store(false, Ordering::SeqCst);
        let enqueue_spool = spool.clone();
        let enqueue_config = config.clone();
        let enqueue = std::thread::spawn(move || {
            enqueue_with_config(&enqueue_spool, &entry, Some(&enqueue_config)).unwrap()
        });
        while !ENQUEUE_REACHED_RECOVERY_WINDOW.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        quarantine.complete().unwrap();
        PAUSE_ENQUEUE_AFTER_MAIN_RENAME.store(false, Ordering::SeqCst);
        enqueue.join().unwrap();
        let mut result = DrainResult::default();
        if let Some(path) = list_entries(&spool).unwrap().0.into_iter().next() {
            assert!(load_live_entry(&path, &mut result).is_none());
        }
        assert_eq!(spool_len(&spool), 0, "completed tombstone removes overlap");
    }

    #[test]
    fn completed_recovery_tombstone_retires_late_overlap() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("hook-spool");
        let entry = SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=project".into(),
            body: r#"{"session_id":"native-1","cwd":"/repo","prompt":"late"}"#.into(),
            created_ms: now_ms(),
            auth_mode: AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        let quarantine = quarantine_session_entries(
            &crate::config::Config::default(),
            &spool,
            "journal",
            entry.created_ms.saturating_sub(1),
            entry.created_ms.saturating_add(1),
            "http://127.0.0.1:49374",
            "claude-code",
            "ws",
            "project",
            Path::new("/repo"),
            "native-1",
        )
        .unwrap();
        quarantine.complete().unwrap();
        enqueue(&spool, &entry).unwrap();
        assert_eq!(spool_len(&spool), 0, "late overlap must not be delivered");
    }

    /// The completed tombstone is bounded by its interval: a hook from a
    /// LATER resume of the same native session is fresh evidence and must be
    /// delivered, never swallowed by the earlier interval's tombstone.
    #[test]
    fn completed_tombstone_keeps_a_later_resume_of_the_same_session() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("hook-spool");
        let interval_ended = now_ms().saturating_sub(60_000);
        let quarantine = quarantine_session_entries(
            &crate::config::Config::default(),
            &spool,
            "journal",
            interval_ended.saturating_sub(1_000),
            interval_ended,
            "http://127.0.0.1:49374",
            "claude-code",
            "ws",
            "project",
            Path::new("/repo"),
            "native-1",
        )
        .unwrap();
        quarantine.complete().unwrap();
        let later_resume = SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=project".into(),
            body: r#"{"session_id":"native-1","cwd":"/repo","prompt":"from a later resume"}"#.into(),
            created_ms: now_ms(),
            auth_mode: AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        };
        enqueue(&spool, &later_resume).unwrap();
        assert_eq!(
            spool_len(&spool),
            1,
            "a hook from a later resume must stay queued for delivery"
        );
        let path = list_entries(&spool).unwrap().0.into_iter().next().unwrap();
        let mut result = DrainResult::default();
        assert!(
            load_live_entry(&path, &mut result).is_some(),
            "the drain path must deliver, not drop, post-interval evidence"
        );
        assert_eq!(result.dropped, 0);
    }

    /// Completing one interval only ever consumes its own correlated entries:
    /// another interval's quarantine keeps its evidence, keeps capturing its
    /// late hooks, and an entry attributable to two intervals at once fails
    /// closed into the main spool instead of being attributed by guesswork.
    #[test]
    fn completing_one_recovery_interval_leaves_the_other_intervals_entries_intact() {
        let temp = tempfile::tempdir().unwrap();
        let spool = temp.path().join("hook-spool");
        let config = crate::config::Config::default();
        let first_ms = now_ms().saturating_sub(120_000);
        let second_ms = first_ms + 60_000;
        let entry = |created_ms: u64, prompt: &str| {
            SpoolEntry {
            url: "http://127.0.0.1:49374/hook?event=user-prompt-submit&agent=claude-code&workspace=ws&project=project".into(),
            body: format!(r#"{{"session_id":"native-1","cwd":"/repo","prompt":"{prompt}"}}"#),
            created_ms,
            auth_mode: AuthMode::Anonymous,
            token: None,
            attempts: 0,
            profile: None,
        }
        };
        let quarantine = |journal: &str, started: u64, ended: u64| {
            quarantine_session_entries(
                &config,
                &spool,
                journal,
                started,
                ended,
                "http://127.0.0.1:49374",
                "claude-code",
                "ws",
                "project",
                Path::new("/repo"),
                "native-1",
            )
        };
        enqueue(&spool, &entry(first_ms, "first interval")).unwrap();
        enqueue(&spool, &entry(second_ms, "second interval")).unwrap();
        let first = quarantine("journal-a", first_ms - 1, first_ms + 1).unwrap();
        let second = quarantine("journal-b", second_ms - 1, second_ms + 1).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(spool_len(&spool), 0);

        first.complete().unwrap();
        assert_eq!(
            second.len(),
            1,
            "completing one interval never deletes another interval's correlated entries"
        );
        enqueue(&spool, &entry(second_ms, "second interval late hook")).unwrap();
        assert_eq!(
            second.len(),
            2,
            "the still-active interval keeps capturing its own late hooks"
        );
        assert_eq!(spool_len(&spool), 0);

        let overlapping = quarantine("journal-c", second_ms - 1, second_ms + 1).unwrap();
        assert_eq!(
            overlapping.len(),
            0,
            "the new interval claims nothing already attributed to journal-b"
        );
        enqueue(&spool, &entry(second_ms, "attributable to both")).unwrap();
        assert_eq!(
            spool_len(&spool),
            1,
            "an entry two intervals could own fails closed into the main spool"
        );
        second.restore(&spool).unwrap();
        assert_eq!(spool_len(&spool), 3);
    }

    #[test]
    fn entry_for_picks_auth_mode() {
        let s = entry_for("u".into(), "{}".into(), Some("tok"), false);
        assert_eq!(s.auth_mode, AuthMode::Static);
        assert_eq!(s.token.as_deref(), Some("tok"));

        let o = entry_for("u".into(), "{}".into(), None, true);
        assert_eq!(o.auth_mode, AuthMode::Oidc);
        assert!(o.token.is_none());

        let a = entry_for("u".into(), "{}".into(), None, false);
        assert_eq!(a.auth_mode, AuthMode::Anonymous);
    }

    #[tokio::test]
    async fn resolve_bearer_delegates_static_and_absent_oidc_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let client = reqwest::Client::new();

        let static_bearer = resolve_bearer(&client, tmp.path(), Some("static-token")).await;
        let absent_bearer = resolve_bearer(&client, tmp.path(), None).await;

        assert_eq!(static_bearer.as_deref(), Some("static-token"));
        assert!(absent_bearer.is_none());
    }

    #[test]
    fn enqueue_then_list_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let entry = entry_for(
            "https://x/hook?event=stop".into(),
            "{\"session_id\":\"s\"}".into(),
            Some("tok"),
            false,
        );
        enqueue(&spool, &entry).unwrap();
        let (files, _) = list_entries(&spool).unwrap();
        assert_eq!(files.len(), 1);
        let loaded: SpoolEntry =
            serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(loaded.url, "https://x/hook?event=stop");
        assert_eq!(loaded.auth_mode, AuthMode::Static);
        assert_eq!(loaded.token.as_deref(), Some("tok"));
    }

    #[test]
    fn load_live_entry_strips_legacy_assistant_message() {
        // Simulate a spool entry written by a pre-#196 binary: the raw body
        // still carries `last_assistant_message`. Loading it for a drain must
        // strip the field so neither drain path (batch or per-event fallback)
        // can put it on the wire.
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let dirty = entry_for(
            "https://x/hook?event=stop&agent=claude-code".into(),
            r#"{"session_id":"legacy","last_assistant_message":"SENTINEL_ASSISTANT_MESSAGE"}"#
                .into(),
            None,
            false,
        );
        enqueue(&spool, &dirty).unwrap();
        let path = list_entries(&spool).unwrap().0.into_iter().next().unwrap();

        let mut result = DrainResult::default();
        let loaded = load_live_entry(&path, &mut result).expect("entry is live");
        assert!(
            !loaded.body.contains("SENTINEL_ASSISTANT_MESSAGE"),
            "drain load left the assistant message in the body: {}",
            loaded.body
        );
        assert!(
            !loaded.body.contains("last_assistant_message"),
            "drain load left the raw field key in the body: {}",
            loaded.body
        );
        assert!(
            loaded.body.contains("legacy"),
            "unrelated field was dropped"
        );
    }

    #[test]
    fn load_live_entry_keeps_clean_body_byte_exact() {
        // An entry with no assistant field must not be reserialized (its bytes
        // are preserved), so the strip never churns unrelated events.
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let body = r#"{"session_id":"s","prompt":"hi"}"#;
        let clean = entry_for(
            "https://x/hook?event=user-prompt-submit&agent=claude-code".into(),
            body.into(),
            None,
            false,
        );
        enqueue(&spool, &clean).unwrap();
        let path = list_entries(&spool).unwrap().0.into_iter().next().unwrap();

        let mut result = DrainResult::default();
        let loaded = load_live_entry(&path, &mut result).expect("entry is live");
        assert_eq!(loaded.body, body, "clean body must stay byte-exact");
    }

    #[test]
    fn drain_preserves_capture_flag_and_protocol_round_trip() {
        // An opted-in Stop entry carries `capture_assistant=1` on the URL and the
        // sanitized `_ai_memory_assistant` marker in the body. The drain must
        // preserve BOTH through spool → load → batch: the load-time strip only
        // targets the raw `last_assistant_message`, never the protocol (#196).
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let entry = entry_for(
            "https://x/hook?event=stop&agent=claude-code&capture_assistant=1".into(),
            r#"{"session_id":"s","_ai_memory_assistant":{"version":1,"excerpt":"done"}}"#.into(),
            None,
            false,
        );
        enqueue(&spool, &entry).unwrap();
        let path = list_entries(&spool).unwrap().0.into_iter().next().unwrap();

        let mut result = DrainResult::default();
        let loaded = load_live_entry(&path, &mut result).expect("entry is live");
        assert!(loaded.url.contains("capture_assistant=1"), "flag dropped");
        assert!(
            loaded.body.contains("_ai_memory_assistant"),
            "protocol dropped"
        );

        let batch = batch_payload(&[(path, loaded)]).expect("batch built");
        assert!(
            batch.contains("capture_assistant=1"),
            "batch lost the capture flag: {batch}"
        );
        assert!(
            batch.contains("_ai_memory_assistant"),
            "batch lost the protocol: {batch}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn enqueue_creates_spool_dir_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        // A nested path that does not exist yet, so `create_spool_dir` builds it.
        let spool = tmp.path().join("nested").join("hook-spool");
        let entry = entry_for("https://x/hook".into(), "{}".into(), None, false);
        enqueue(&spool, &entry).unwrap();
        let mode = std::fs::metadata(&spool).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "spool dir must be owner-only");
    }

    #[test]
    fn enqueue_names_are_unique_in_tight_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..MAX_SPOOL_FILES {
            let mut entry = entry_for(
                format!("https://x/hook?event=e{i}"),
                "{}".into(),
                None,
                false,
            );
            entry.created_ms = 42;
            enqueue(&spool, &entry).unwrap();
        }

        let (files, _) = list_entries(&spool).unwrap();
        assert_eq!(files.len(), MAX_SPOOL_FILES);
    }

    #[test]
    fn enqueue_refuses_new_entries_when_spool_reaches_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..(MAX_SPOOL_FILES + 2) {
            let mut entry = entry_for(
                format!("https://x/hook?event=e{i}"),
                "{}".into(),
                None,
                false,
            );
            entry.created_ms = i as u64;
            enqueue(&spool, &entry).unwrap();
        }

        let (mut files, _) = list_entries(&spool).unwrap();
        files.sort();
        assert_eq!(files.len(), MAX_SPOOL_FILES);
        let bodies: Vec<SpoolEntry> = files
            .iter()
            .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
            .collect();
        assert!(
            bodies
                .iter()
                .all(|entry| entry.created_ms < MAX_SPOOL_FILES as u64)
        );
    }

    #[test]
    fn capacity_tracks_current_global_occupancy_across_restart_and_quarantine() {
        let temp = tempfile::tempdir().unwrap();
        let spool = spool_dir(temp.path());
        for cycle in 0..(MAX_SPOOL_FILES * 3) {
            let entry = entry_for(
                format!("https://x/hook?event={cycle}"),
                "{}".into(),
                None,
                false,
            );
            enqueue(&spool, &entry).unwrap();
            let path = list_entries(&spool).unwrap().0.into_iter().next().unwrap();
            std::fs::remove_file(path).unwrap();
        }
        let accepted = entry_for(
            "https://x/hook?event=accepted".into(),
            "{}".into(),
            None,
            false,
        );
        enqueue(&spool, &accepted).unwrap();
        assert_eq!(total_spool_entries(&spool).unwrap(), 1);
        std::fs::remove_file(list_entries(&spool).unwrap().0[0].clone()).unwrap();

        let quarantine = spool.join(RECOVERY_DIR).join("full");
        std::fs::create_dir_all(&quarantine).unwrap();
        for index in 0..MAX_SPOOL_FILES {
            std::fs::write(quarantine.join(format!("{index}.json")), b"{}").unwrap();
        }
        enqueue(&spool, &accepted).unwrap();
        assert_eq!(
            spool_len(&spool),
            0,
            "active quarantines count toward the cap"
        );
    }

    #[test]
    fn oversized_spool_entry_is_refused_before_read_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("oversized.json");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_SPOOL_ENTRY_BYTES + 1).unwrap();
        let error = read_spool_entry_bytes(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn tombstone_sweep_is_global_and_bounded_by_age_and_count() {
        let temp = tempfile::tempdir().unwrap();
        let spool = spool_dir(temp.path());
        let recovery = spool.join(RECOVERY_DIR);
        std::fs::create_dir_all(&recovery).unwrap();
        for index in 0..(MAX_RECOVERY_TOMBSTONES + 2) {
            let dir = recovery.join(format!("done-{index:04}"));
            std::fs::create_dir(&dir).unwrap();
            write_private(&dir.join(RECOVERY_COMPLETED_FILE), b"done").unwrap();
        }
        sweep_recovery_tombstones(&spool).unwrap();
        assert!(
            std::fs::read_dir(&recovery).unwrap().count() <= MAX_RECOVERY_TOMBSTONES,
            "completed tombstones are globally count-bounded"
        );
    }

    #[tokio::test]
    async fn drain_unreachable_leaves_events_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        // Two anonymous events pointing at an unroutable port.
        for i in 0..2 {
            let e = entry_for(
                format!("http://127.0.0.1:1/hook?event=e{i}"),
                "{}".into(),
                None,
                false,
            );
            enqueue(&spool, &e).unwrap();
        }
        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(2),
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(r.sent, 0);
        assert_eq!(r.remaining, 2);
        // Files survive for the next boundary.
        assert_eq!(list_entries(&spool).unwrap().0.len(), 2);
    }

    /// Adversarial regression for the outage-charging defect: while the
    /// server is totally down, every drain pass hits the queue head with an
    /// endpoint-level unreachable (connect refused — nothing is listening).
    /// With the default `max_attempts`, the pre-fix drain charged the head
    /// once per pass, deleting the OLDEST events at ~1 per `max_attempts`
    /// passes DURING the outage — structural data loss well inside the
    /// 7-day TTL. An unreachable must not charge the attempt budget.
    #[tokio::test]
    async fn unreachable_outage_never_charges_attempt_budget() {
        // A port with nothing on it: bind, read the address, then drop.
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };

        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{dead}/hook?event=e{i}"),
            );
        }

        // Far more passes than the default MAX_ATTEMPTS: an outage, however
        // long, must never consume retry budget.
        for _ in 0..(MAX_ATTEMPTS + 2) {
            let r = drain_with_max_attempts(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(200),
                MaxAttempts::new(MAX_ATTEMPTS),
            )
            .await;
            assert_eq!(r.sent, 0);
            assert_eq!(r.dropped, 0, "an unreachable endpoint must not drop events");
            assert_eq!(r.remaining, 3);
        }

        // ALL entries survive the outage untouched.
        assert_eq!(
            sorted_attempts(&spool),
            vec![0, 0, 0],
            "no entry may be charged while the server is unreachable"
        );
    }

    /// The recovery half of the outage contract: once the server comes back
    /// on the same port, the next pass delivers the entire backlog that
    /// rode out the outage.
    #[tokio::test]
    async fn outage_then_recovery_delivers_the_entire_backlog() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // Phase 1 — outage: nothing listens where the events were captured.
        let dead_addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };

        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{dead_addr}/hook?event=e{i}"),
            );
        }

        for _ in 0..(MAX_ATTEMPTS + 2) {
            let r = drain_with_max_attempts(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(200),
                MaxAttempts::new(MAX_ATTEMPTS),
            )
            .await;
            assert_eq!(r.dropped, 0);
            assert_eq!(r.remaining, 3);
        }
        assert_eq!(sorted_attempts(&spool), vec![0, 0, 0]);

        // Phase 2 — recovery: the server restarts on the same port. The
        // listener never accepted, so the port carries no TIME_WAIT and the
        // rebind is immediate.
        let listener = tokio::net::TcpListener::bind(dead_addr)
            .await
            .expect("restart on the same port after the outage");
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 65536];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let (status, body) = if req
                        .lines()
                        .next()
                        .is_some_and(|l| l.contains("/hook/batch"))
                    {
                        let payload = req.split("\r\n\r\n").nth(1).unwrap_or("");
                        let accepted = serde_json::from_str::<serde_json::Value>(payload)
                            .ok()
                            .and_then(|v| v.as_array().map(Vec::len))
                            .unwrap_or(0);
                        ("200 OK", format!("{{\"accepted\":{accepted}}}"))
                    } else {
                        ("202 Accepted", "queued".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });

        let r = drain_with_max_attempts(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await;

        assert_eq!(r.sent, 3, "the whole backlog is delivered after recovery");
        assert_eq!(r.remaining, 0);
        assert!(
            list_entries(&spool).unwrap().0.is_empty(),
            "spool emptied after the server came back"
        );
    }

    #[tokio::test]
    async fn drain_keeps_entry_when_first_response_is_408() {
        // A 408 from the server is retryable, not a terminal refusal: the
        // drain must keep the entry queued (charged one attempt) so a later
        // pass can still deliver it, subject to the retry limit. Treating
        // every 4xx as permanent deleted such events on the spot (#1092
        // regression).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            // Answer every request (batch first) with 408 until the drain
            // gives up on this pass.
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf).await;
                let response = "HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        enqueue(
            &spool,
            &entry_for(
                format!("http://{addr}/hook?event=e0"),
                "{}".into(),
                None,
                false,
            ),
        )
        .unwrap();

        let r = drain_with_max_attempts(
            &spool,
            tmp.path(),
            Duration::from_secs(2),
            Duration::from_millis(500),
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await;

        assert_eq!(r.sent, 0);
        assert_eq!(r.dropped, 0, "a 408 first response must not drop the entry");
        let (files, _) = list_entries(&spool).unwrap();
        assert_eq!(files.len(), 1, "the entry stays queued for the next pass");
        let entry: SpoolEntry = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(
            entry.attempts, 1,
            "the retryable miss is charged one attempt"
        );
    }

    /// #493: an unreadable spool must not read as an idle pass.
    ///
    /// `list_entries` used to be `read_dir(spool).ok()?`, so a permissions or
    /// IO failure produced "no entries" and `drain` returned an empty result —
    /// zero sent, zero remaining, zero dropped, exit 0, files untouched, no
    /// socket opened. Indistinguishable from a healthy queue, and invisible
    /// from outside the process.
    #[test]
    fn an_unreadable_spool_is_an_error_not_an_empty_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-such-spool");
        let err = list_entries(&missing).expect_err("a missing spool must not list as empty");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// The happy path still works, so the error case above cannot be satisfied
    /// by breaking listing outright.
    #[test]
    fn a_readable_spool_lists_json_entries_and_no_unreadable_count() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = tmp.path().join("hook-spool");
        std::fs::create_dir_all(&spool).unwrap();
        std::fs::write(spool.join("0000000000001-1-0000.json"), b"{}").unwrap();
        std::fs::write(spool.join("notes.txt"), b"ignored").unwrap();
        let (files, unreadable) = list_entries(&spool).unwrap();
        assert_eq!(files.len(), 1, "only .json entries are queue files");
        assert_eq!(unreadable, 0);
    }

    #[tokio::test]
    async fn drain_empty_spool_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let r = drain(
            &spool_dir(tmp.path()),
            tmp.path(),
            Duration::from_secs(1),
            Duration::from_millis(200),
        )
        .await;
        assert_eq!(r, DrainResult::default());
    }

    #[test]
    fn drain_lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());

        let first = acquire_drain_lock(&spool, DrainLockWait::NoWait)
            .unwrap()
            .expect("first lock acquired");
        assert!(
            acquire_drain_lock(&spool, DrainLockWait::NoWait)
                .unwrap()
                .is_none(),
            "overlapping lock should not acquire"
        );

        drop(first);
        // Bounded wait instead of NoWait: a child process fork+exec'd by a
        // concurrently running test briefly inherits this flock's fd between
        // fork and exec (the lock lives until the duplicated descriptor is
        // closed by exec's CLOEXEC), so an instantaneous re-acquire can
        // spuriously see the lock still held. The bounded window still
        // proves release-on-drop; only a leaked/undropped lock would hold
        // for a full five seconds.
        assert!(
            acquire_drain_lock(&spool, DrainLockWait::Bounded(Duration::from_secs(5)))
                .unwrap()
                .is_some(),
            "lock should release on drop"
        );
    }

    #[test]
    fn would_block_lock_error_is_lock_busy() {
        let err = std::io::Error::from(std::io::ErrorKind::WouldBlock);
        assert!(is_drain_lock_busy_error(&err));
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_violation_error_is_lock_busy() {
        let err = std::io::Error::from_raw_os_error(ERROR_LOCK_VIOLATION);
        assert!(is_drain_lock_busy_error(&err));
    }

    #[test]
    fn unrelated_lock_error_is_not_lock_busy() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_drain_lock_busy_error(&err));
    }

    #[tokio::test]
    async fn drain_exclusive_skips_when_lock_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let _held = acquire_drain_lock(&spool, DrainLockWait::NoWait)
            .unwrap()
            .expect("held lock");

        let result = drain_exclusive(
            &spool,
            tmp.path(),
            Duration::from_secs(1),
            Duration::from_millis(100),
            DrainLockWait::NoWait,
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn drain_exclusive_within_budget_zero_budget_leaves_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(
            &spool,
            "evt-0.json",
            "http://127.0.0.1:1/hook?event=e0".into(),
        );

        let result = drain_exclusive_within_budget(
            &spool,
            tmp.path(),
            Duration::ZERO,
            Duration::from_millis(100),
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await
        .unwrap();
        let LockedDrainResult::Drained(result) = result else {
            panic!("lock should not be busy")
        };

        assert_eq!(result.sent, 0);
        assert_eq!(result.remaining, 1);
        assert_eq!(spool_len(&spool), 1);
    }

    #[tokio::test]
    async fn drain_until_quiescent_catches_new_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook_and_enqueue_on_first(req_count.clone(), spool.clone()).await;

        write_spool_entry(
            &spool,
            "first.json",
            format!("http://{addr}/hook?event=first"),
        );

        let result = drain_until_quiescent(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            DrainLockWait::NoWait,
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await
        .expect("lock acquired");
        let LockedDrainResult::Drained(result) = result else {
            panic!("lock should not be busy")
        };

        assert_eq!(result.sent, 2);
        assert_eq!(result.remaining, 0);
        assert_eq!(req_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(list_entries(&spool).unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn drain_until_quiescent_reports_lock_io_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let not_a_dir = tmp.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"file").unwrap();

        let err = drain_until_quiescent(
            &not_a_dir,
            tmp.path(),
            Duration::from_secs(1),
            Duration::from_millis(100),
            DrainLockWait::NoWait,
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotADirectory
        ));
    }

    #[tokio::test]
    async fn drain_drops_event_after_max_attempts() {
        // Control for the outage fix: a server that ANSWERS with 5xx is a
        // post-connect failure and keeps today's charging semantics — one
        // attempt per pass, drop at the limit. (An endpoint-level
        // unreachable never charges; see
        // `unreachable_outage_never_charges_attempt_budget`.)
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "500 Internal Server Error").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(&spool, "evt-0.json", format!("http://{addr}/hook?event=e0"));
        for pass in 1..=MAX_ATTEMPTS {
            let r = drain_with_max_attempts(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(500),
                MaxAttempts::new(MAX_ATTEMPTS),
            )
            .await;
            assert_eq!(
                r.dropped,
                usize::from(pass == MAX_ATTEMPTS),
                "the erroring event is dropped exactly at MAX_ATTEMPTS"
            );
            if pass < MAX_ATTEMPTS {
                assert_eq!(sorted_attempts(&spool), vec![pass]);
            }
        }
        assert_eq!(
            req_count.load(std::sync::atomic::Ordering::SeqCst),
            MAX_ATTEMPTS as usize,
            "the server answered every pass — post-connect, not an outage"
        );
        assert!(
            list_entries(&spool).unwrap().0.is_empty(),
            "spool is empty after the drop"
        );
    }

    #[tokio::test]
    async fn zero_max_attempts_keeps_failing_event_after_twenty_drains() {
        // An explicit zero means "never drop for attempts": a server that
        // keeps answering 5xx charges the entry on every pass (post-connect
        // failures still bump attempts), and twenty charged passes later it
        // is still queued. Uses the 5xx server because an unreachable
        // endpoint no longer charges attempts at all.
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "500 Internal Server Error").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(
            &spool,
            "evt-0.json",
            format!("http://{addr}/hook?event=dead"),
        );

        for _ in 0..20 {
            let result = drain_with_max_attempts(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(500),
                MaxAttempts::new(0),
            )
            .await;
            assert_eq!(result.dropped, 0);
            assert_eq!(result.remaining, 1);
        }
        let files = list_entries(&spool).unwrap().0;
        assert_eq!(files.len(), 1);
        let entry: SpoolEntry = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(entry.attempts, 20);
    }

    #[tokio::test]
    async fn explicit_max_attempts_isolated_from_ambient_zero() {
        const CHILD_MARKER: &str = "AI_MEMORY_TEST_HOOK_SPOOL_AMBIENT_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            assert_eq!(
                std::env::var("AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS").as_deref(),
                Ok("0")
            );
            // A 5xx server, not a dead port: an unreachable endpoint no
            // longer charges attempts, and this test asserts a one-pass drop.
            let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let addr = serve_counting_hook(req_count, "500 Internal Server Error").await;
            let tmp = tempfile::tempdir().unwrap();
            let spool = spool_dir(tmp.path());
            write_spool_entry(
                &spool,
                "evt-0.json",
                format!("http://{addr}/hook?event=dead"),
            );

            let result = drain_with_max_attempts(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(100),
                MaxAttempts::new(1),
            )
            .await;

            assert_eq!(result.dropped, 1);
            assert!(list_entries(&spool).unwrap().0.is_empty());
            println!("ambient-zero-isolated");
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("commands::hook_spool::tests::explicit_max_attempts_isolated_from_ambient_zero")
            .arg("--test-threads=1")
            .arg("--nocapture")
            .env(CHILD_MARKER, "1")
            .env("AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child test failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("ambient-zero-isolated"),
            "child stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[tokio::test]
    async fn configured_max_attempts_drops_after_three_drains() {
        // A 5xx server keeps charging one attempt per pass (post-connect),
        // so an explicit limit of 3 drops the entry on the third pass.
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count, "500 Internal Server Error").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(
            &spool,
            "evt-0.json",
            format!("http://{addr}/hook?event=dead"),
        );

        for pass in 1..=3 {
            let result = drain_with_max_attempts(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(100),
                MaxAttempts::new(3),
            )
            .await;
            assert_eq!(result.dropped, usize::from(pass == 3));
            assert_eq!(result.remaining, usize::from(pass < 3));
        }
        assert!(list_entries(&spool).unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn zero_max_attempts_still_drops_stale_event() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        std::fs::create_dir_all(&spool).unwrap();
        let mut entry = entry_for(
            "http://127.0.0.1:1/hook?event=old".into(),
            "{}".into(),
            None,
            false,
        );
        entry.created_ms = now_ms().saturating_sub(MAX_AGE_MS + 1);
        std::fs::write(
            spool.join("stale.json"),
            serde_json::to_vec(&entry).unwrap(),
        )
        .unwrap();

        let result = drain_with_max_attempts(
            &spool,
            tmp.path(),
            Duration::from_secs(2),
            Duration::from_millis(100),
            MaxAttempts::new(0),
        )
        .await;

        assert_eq!(result.dropped, 1);
        assert!(list_entries(&spool).unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn drain_drops_stale_event() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        std::fs::create_dir_all(&spool).unwrap();
        let mut e = entry_for(
            "http://127.0.0.1:1/hook?event=old".into(),
            "{}".into(),
            None,
            false,
        );
        e.created_ms = now_ms().saturating_sub(MAX_AGE_MS + 1);
        std::fs::write(spool.join("stale.json"), serde_json::to_vec(&e).unwrap()).unwrap();
        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(r.dropped, 1);
        assert_eq!(r.sent, 0);
        assert!(list_entries(&spool).unwrap().0.is_empty());
    }

    #[tokio::test]
    async fn drain_429_keeps_event_queued_without_bumping_attempts() {
        // A server that always answers 429 (saturation / `hook queue full`).
        // The event must ride every pass untouched: never dropped, attempts
        // never incremented — saturation must not burn the retry budget.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0_u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfull",
                        )
                        .await;
                });
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let e = entry_for(
            format!("http://{addr}/hook?event=x"),
            "{}".into(),
            None,
            false,
        );
        enqueue(&spool, &e).unwrap();

        // Far more passes than MAX_ATTEMPTS — a 429 must never consume budget.
        for _ in 0..(MAX_ATTEMPTS + 2) {
            let r = drain(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(500),
            )
            .await;
            assert_eq!(r.sent, 0);
            assert_eq!(r.dropped, 0, "a 429 must never drop the event");
            assert_eq!(r.remaining, 1);
        }
        let (files, _) = list_entries(&spool).unwrap();
        assert_eq!(files.len(), 1, "event still queued after many 429s");
        let loaded: SpoolEntry =
            serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(loaded.attempts, 0, "429 must not consume the retry budget");
    }

    /// #493: a spooled entry carries the URL it was captured against, so a
    /// spool can hold entries addressed to a port nothing listens on any more.
    /// Before the fix the drain stopped at the first one, so events queued
    /// behind a dead-port head were never delivered — they sat until the
    /// 7-day window dropped them undelivered.
    #[tokio::test]
    async fn dead_endpoint_at_the_head_does_not_block_deliverable_entries_behind_it() {
        // A live server for the entries that *can* be delivered.
        let live = serve_counting_hook(
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            "200 OK",
        )
        .await;

        // A port with nothing on it: bind, read the address, then drop.
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };

        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        // Oldest first: the head is undeliverable, the rest are fine — the
        // reported shape. Kept to `MAX_SPOOL_FILES` (3 under cfg(test)) so
        // enqueue does not prune the fixture out from under the test.
        for _ in 0..1 {
            enqueue(
                &spool,
                &entry_for(
                    format!("http://{dead}/hook?event=x"),
                    "{}".into(),
                    None,
                    false,
                ),
            )
            .unwrap();
        }
        for _ in 0..2 {
            enqueue(
                &spool,
                &entry_for(
                    format!("http://{live}/hook?event=x"),
                    "{}".into(),
                    None,
                    false,
                ),
            )
            .unwrap();
        }

        // Generous budgets: the assertion is about ORDERING (a dead head
        // must not block the deliverable tail), not latency. Tight timeouts
        // (500ms per event) flaked under `--all-targets` CPU contention when
        // a live localhost POST briefly exceeded them and a deliverable
        // entry timed out spuriously. The dead head still connection-refuses
        // immediately regardless of the ceiling.
        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(30),
            Duration::from_secs(5),
        )
        .await;

        assert_eq!(
            r.sent, 2,
            "the deliverable entries must be sent in the same pass, not stranded \
             behind the dead endpoint"
        );
        let (files, _) = list_entries(&spool).unwrap();
        assert_eq!(files.len(), 1, "only the dead-port entry stays queued");
        let loaded: SpoolEntry =
            serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(
            loaded.attempts, 0,
            "an endpoint-level miss must not charge the entry actually attempted"
        );
    }

    /// #493: the envelope freezes the address at capture time, so a server
    /// that came back on a different port leaves the whole spool addressed
    /// somewhere dead. Skipping those entries stops them blocking the queue,
    /// but they are still never delivered — they age out at `MAX_AGE_MS`.
    /// When the recorded host is loopback the intent is unambiguous, so the
    /// drain retries once against the configured server.
    #[tokio::test]
    async fn a_dead_loopback_port_is_retried_against_the_configured_server() {
        let live = serve_counting_hook(
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            "200 OK",
        )
        .await;
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };

        let tmp = tempfile::tempdir().unwrap();
        // Point this store's config at the server that is actually up.
        std::fs::write(
            tmp.path().join("config.toml"),
            format!("server_url = \"http://{live}\"\n"),
        )
        .unwrap();

        let spool = spool_dir(tmp.path());
        enqueue(
            &spool,
            &entry_for(
                format!("http://{dead}/hook?event=x"),
                "{}".into(),
                None,
                false,
            ),
        )
        .unwrap();

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(500),
        )
        .await;

        assert_eq!(
            r.sent, 1,
            "an entry frozen against a dead loopback port must be delivered to \
             the configured server, not merely skipped"
        );
        assert!(
            list_entries(&spool).unwrap().0.is_empty(),
            "delivered entries are removed from the spool"
        );
    }

    /// #992, adversarial: the same shape as the test above, but the entry
    /// was routed to a profile. `config.toml`'s address is the install
    /// default, so the reroot must not fire — the live default server gets
    /// nothing even though it would accept the batch.
    #[tokio::test]
    async fn a_profile_entry_on_a_dead_loopback_port_is_not_rerouted_to_the_default() {
        let default_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let live = serve_counting_hook(default_hits.clone(), "200 OK").await;
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            format!("server_url = \"http://{live}\"\n"),
        )
        .unwrap();
        let spool = spool_dir(tmp.path());
        enqueue(
            &spool,
            &entry_for(
                format!("http://{dead}/hook?event=x"),
                "{}".into(),
                Some("b-token"),
                false,
            )
            .routed_to(Some("team-b")),
        )
        .unwrap();

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(500),
        )
        .await;

        assert_eq!(r.sent, 0);
        assert_eq!(
            default_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the install-default server must never receive a profile's capture"
        );
        assert_eq!(list_entries(&spool).unwrap().0.len(), 1);
    }

    /// A profile entry and an install-default entry never share a batch, even
    /// when they share an address and a bearer: the retry and reroot
    /// decisions are taken once per chunk.
    #[tokio::test]
    async fn profile_and_default_entries_at_one_address_ride_separate_batches() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(hits.clone(), "200 OK").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for profile in [None, Some("team-b"), None] {
            enqueue(
                &spool,
                &entry_for(
                    format!("http://{addr}/hook?event=x"),
                    "{}".into(),
                    Some("same"),
                    false,
                )
                .routed_to(profile),
            )
            .unwrap();
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(500),
        )
        .await;

        assert_eq!(r.sent, 3);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// The retry must not re-point a spool captured against another host on
    /// purpose. Only a loopback authority is unambiguous.
    #[tokio::test]
    async fn a_dead_remote_host_is_not_repointed_at_the_local_server() {
        let live = serve_counting_hook(
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            "200 OK",
        )
        .await;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            format!("server_url = \"http://{live}\"\n"),
        )
        .unwrap();

        let spool = spool_dir(tmp.path());
        // TEST-NET-1 (RFC 5737): guaranteed not routable, and not loopback.
        enqueue(
            &spool,
            &entry_for(
                "http://192.0.2.1:49374/hook?event=x".into(),
                "{}".into(),
                None,
                false,
            ),
        )
        .unwrap();

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(300),
        )
        .await;

        assert_eq!(
            r.sent, 0,
            "a remote host must never be silently re-pointed at the local server"
        );
        assert_eq!(list_entries(&spool).unwrap().0.len(), 1, "still queued");
    }

    #[tokio::test]
    async fn partial_429_deletes_prefix_without_bumping_saturated_item() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr =
            serve_counting_hook_with_accept(req_count.clone(), "429 Too Many Requests", Some(1))
                .await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 1);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.remaining, 2);
        assert_eq!(req_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (mut files, _) = list_entries(&spool).unwrap();
        files.sort();
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("evt-1.json"));
        assert!(files[1].ends_with("evt-2.json"));
        assert_eq!(
            sorted_attempts(&spool),
            vec![0, 0],
            "saturation must not bump retry attempts for the first unaccepted item"
        );
    }

    #[tokio::test]
    async fn non_contiguous_batch_ack_deletes_only_accepted_indices() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 65536];
                    let _ = s.read(&mut buf).await;
                    let body = r#"{"accepted":0,"accepted_indices":[1]}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..2 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 1);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.remaining, 1);
        let (mut files, _) = list_entries(&spool).unwrap();
        files.sort();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("evt-0.json"));
        assert_eq!(sorted_attempts(&spool), vec![0]);
    }

    #[tokio::test]
    async fn non_contiguous_batch_ack_charges_reported_failed_index() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 65536];
                    let _ = s.read(&mut buf).await;
                    let body = r#"{"accepted":0,"accepted_indices":[],"failed_index":1}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 0);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.remaining, 3);
        let (mut files, _) = list_entries(&spool).unwrap();
        files.sort();
        assert_eq!(files.len(), 3);
        let attempts: Vec<u32> = files
            .iter()
            .map(|path| {
                serde_json::from_slice::<SpoolEntry>(&std::fs::read(path).unwrap())
                    .unwrap()
                    .attempts
            })
            .collect();
        assert_eq!(attempts, vec![0, 1, 0]);
    }

    /// A mock hook server that accepts exactly one bearer and answers `401` to
    /// everything else. Models a rotated credential: the token frozen into the
    /// spool is no longer valid, the drain's live one is.
    async fn serve_token_gated_hook(
        accepted_token: &'static str,
        req_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let rc = req_count.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 65536];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let authorized = req.lines().any(|l| {
                        l.trim().eq_ignore_ascii_case(&format!(
                            "authorization: Bearer {accepted_token}"
                        ))
                    });
                    let is_batch = req
                        .lines()
                        .next()
                        .is_some_and(|l| l.contains("/hook/batch"));
                    let (status, body) = if !authorized {
                        ("401 Unauthorized", "{\"error\":\"bad token\"}".to_string())
                    } else if is_batch {
                        let payload = req.split("\r\n\r\n").nth(1).unwrap_or("");
                        let accepted = serde_json::from_str::<serde_json::Value>(payload)
                            .ok()
                            .and_then(|v| v.as_array().map(Vec::len))
                            .unwrap_or(0);
                        ("200 OK", format!("{{\"accepted\":{accepted}}}"))
                    } else {
                        ("202 Accepted", "queued".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr.to_string()
    }

    /// #542: the spool freezes its credential at capture time, so rotating the
    /// token strands every already-spooled entry on the old one. The drain is
    /// handed the current token by the hook that spawned it, and retries a
    /// rejected entry with it.
    #[tokio::test]
    async fn drain_recovers_an_entry_stranded_by_a_rotated_token() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_token_gated_hook("rotated-new", count.clone()).await;

        let entry = entry_for(
            format!("http://{addr}/hook?event=e0"),
            "{}".into(),
            Some("stale-old"),
            false,
        );
        enqueue(&spool, &entry).unwrap();

        let r = drain_with_live_token(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(500),
            Some("rotated-new"),
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await;

        assert_eq!(r.sent, 1, "the stranded entry is recovered");
        assert_eq!(list_entries(&spool).unwrap().0.len(), 0, "and removed");
    }

    /// The control for the test above, and the behaviour before this change:
    /// with no live token the drain has nothing to retry with, so the entry
    /// stays queued and is charged one attempt. Pinned so the recovery test
    /// cannot pass merely because the server was lenient.
    #[tokio::test]
    async fn drain_without_a_live_token_leaves_a_rejected_entry_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_token_gated_hook("rotated-new", count.clone()).await;

        let entry = entry_for(
            format!("http://{addr}/hook?event=e0"),
            "{}".into(),
            Some("stale-old"),
            false,
        );
        enqueue(&spool, &entry).unwrap();

        let r = drain_with_live_token(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(500),
            None,
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await;

        assert_eq!(r.sent, 0, "nothing to retry with");
        assert_eq!(list_entries(&spool).unwrap().0.len(), 1, "entry survives");
    }

    /// #992, adversarial: server B accepts exactly the install default's live
    /// token. Had the drain presented it to B, the entry would deliver; it
    /// must stay queued instead, because that token belongs to another server.
    #[tokio::test]
    async fn a_profile_entry_is_never_retried_with_the_install_live_token() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_b = serve_token_gated_hook("A-LIVE", count.clone()).await;

        let entry = entry_for(
            format!("http://{server_b}/hook?event=e0"),
            "{}".into(),
            Some("b-old"),
            false,
        )
        .routed_to(Some("team-b"));
        enqueue(&spool, &entry).unwrap();

        let r = drain_with_live_token(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(500),
            Some("A-LIVE"),
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await;

        assert_eq!(r.sent, 0, "server B must never see the install's token");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(list_entries(&spool).unwrap().0.len(), 1, "entry survives");
    }

    /// The control: a rotated *profile* token is recovered, from the profile
    /// store rather than the drain's environment.
    #[tokio::test]
    async fn a_profile_entry_recovers_with_its_own_rotated_token() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_b = serve_token_gated_hook("b-new", count.clone()).await;
        let name = crate::server_profiles::ProfileName::parse("team-b").unwrap();
        crate::server_profiles::add(
            tmp.path(),
            &name,
            &format!("http://{server_b}"),
            &[],
            Some("b-new"),
        )
        .unwrap();

        let entry = entry_for(
            format!("http://{server_b}/hook?event=e0"),
            "{}".into(),
            Some("b-old"),
            false,
        )
        .routed_to(Some("team-b"));
        enqueue(&spool, &entry).unwrap();

        let r = drain_with_live_token(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_millis(500),
            Some("A-LIVE"),
            MaxAttempts::new(MAX_ATTEMPTS),
        )
        .await;

        assert_eq!(r.sent, 1);
        assert_eq!(list_entries(&spool).unwrap().0.len(), 0);
    }

    /// An identical token is not worth a second request, and a non-static
    /// entry is not this mechanism's business: an OIDC bearer is already
    /// resolved and refreshed once per pass, so a 401 there is a real
    /// rejection rather than a stale freeze.
    #[test]
    fn static_retry_token_only_fires_when_it_can_change_the_outcome() {
        let dd = tempfile::tempdir().unwrap();
        let static_entry = entry_for("http://x/hook".into(), "{}".into(), Some("old"), false);
        let oidc_entry = entry_for("http://x/hook".into(), "{}".into(), None, true);
        let anon_entry = entry_for("http://x/hook".into(), "{}".into(), None, false);

        assert_eq!(
            static_retry_token(
                &static_entry,
                Some("old"),
                Some("new"),
                &mut ProfileTokens::new(dd.path())
            )
            .as_deref(),
            Some("new"),
            "a rotated token is worth one retry"
        );
        assert_eq!(
            static_retry_token(
                &static_entry,
                Some("old"),
                Some("old"),
                &mut ProfileTokens::new(dd.path())
            ),
            None,
            "an identical token would only repeat the failure"
        );
        assert_eq!(
            static_retry_token(
                &static_entry,
                Some("old"),
                None,
                &mut ProfileTokens::new(dd.path())
            ),
            None,
            "no live token, nothing to retry with"
        );
        assert_eq!(
            static_retry_token(
                &oidc_entry,
                Some("resolved"),
                Some("new"),
                &mut ProfileTokens::new(dd.path())
            ),
            None,
            "OIDC is already re-resolved per pass; a 401 there is genuine"
        );
        assert_eq!(
            static_retry_token(
                &anon_entry,
                None,
                Some("new"),
                &mut ProfileTokens::new(dd.path())
            ),
            None,
            "an anonymous entry was never authenticated"
        );
    }

    /// #992: a profile entry retries only with its own profile's current
    /// token. The live token belongs to the install default; handing it to a
    /// profile's server would leak one server's credential to another.
    #[test]
    fn a_profile_entry_never_retries_with_the_live_token() {
        let dd = tempfile::tempdir().unwrap();
        let entry = entry_for("http://b/hook".into(), "{}".into(), Some("b-old"), false)
            .routed_to(Some("team-b"));

        assert_eq!(
            static_retry_token(
                &entry,
                Some("b-old"),
                Some("A-LIVE"),
                &mut ProfileTokens::new(dd.path())
            ),
            None,
            "an unregistered profile has nothing to retry with"
        );

        let name = crate::server_profiles::ProfileName::parse("team-b").unwrap();
        crate::server_profiles::add(dd.path(), &name, "http://b", &[], Some("b-new")).unwrap();
        assert_eq!(
            static_retry_token(
                &entry,
                Some("b-old"),
                Some("A-LIVE"),
                &mut ProfileTokens::new(dd.path())
            )
            .as_deref(),
            Some("b-new"),
            "a rotated profile token is re-read from the store"
        );
        assert_eq!(
            static_retry_token(
                &entry,
                Some("b-new"),
                Some("A-LIVE"),
                &mut ProfileTokens::new(dd.path())
            ),
            None,
            "the profile's current token already failed"
        );
    }

    /// After the profile moves to another server, its current token belongs
    /// to that server: an entry still addressed to the old one must not be
    /// retried with it.
    #[test]
    fn a_profile_entry_is_not_retried_with_a_token_issued_for_a_new_url() {
        let dd = tempfile::tempdir().unwrap();
        let name = crate::server_profiles::ProfileName::parse("team-b").unwrap();
        crate::server_profiles::add(dd.path(), &name, "https://new.example", &[], Some("NEW"))
            .unwrap();
        let stale = entry_for(
            "https://old.example/hook?event=e".into(),
            "{}".into(),
            Some("OLD"),
            false,
        )
        .routed_to(Some("team-b"));
        let lookalike = entry_for(
            "https://new.example.evil/hook?event=e".into(),
            "{}".into(),
            Some("OLD"),
            false,
        )
        .routed_to(Some("team-b"));
        let current = entry_for(
            "https://new.example/hook?event=e".into(),
            "{}".into(),
            Some("OLD"),
            false,
        )
        .routed_to(Some("team-b"));

        let mut tokens = ProfileTokens::new(dd.path());
        assert_eq!(
            static_retry_token(&stale, Some("OLD"), None, &mut tokens),
            None
        );
        assert_eq!(
            static_retry_token(&lookalike, Some("OLD"), None, &mut tokens),
            None
        );
        assert_eq!(
            static_retry_token(&current, Some("OLD"), None, &mut tokens).as_deref(),
            Some("NEW"),
            "control: the same server still gets its rotated token"
        );
    }

    /// Entries without a profile serialize exactly as they did before #992,
    /// so an older binary draining a mixed spool reads them unchanged.
    #[test]
    fn an_install_default_entry_serializes_without_a_profile_field() {
        let entry = entry_for("http://a/hook".into(), "{}".into(), Some("t"), false);
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("profile").is_none(), "{json}");
        let routed = serde_json::to_value(entry.routed_to(Some("team-b"))).unwrap();
        assert_eq!(routed["profile"], "team-b");
    }

    /// A `401` must be distinguishable from any other refusal, or the drain
    /// cannot tell "this credential is stale" from "this event is bad".
    #[tokio::test]
    async fn a_401_is_reported_as_unauthorized_not_failed() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_token_gated_hook("right", count.clone()).await;
        let client = build_client();

        assert_eq!(
            crate::commands::hook_capture::post_hook(
                &client,
                &format!("http://{addr}/hook?event=e"),
                "{}",
                Some("wrong"),
                Duration::from_millis(500),
            )
            .await,
            crate::commands::hook_capture::PostOutcome::Unauthorized
        );
        assert_eq!(
            crate::commands::hook_capture::post_hook(
                &client,
                &format!("http://{addr}/hook?event=e"),
                "{}",
                Some("right"),
                Duration::from_millis(500),
            )
            .await,
            crate::commands::hook_capture::PostOutcome::Delivered,
            "the gate itself works, so the 401 above is about the token"
        );
    }

    /// A mock hook server: answers `200 {"accepted":N}` to `POST /hook/batch`
    /// (N = array length in the body) and `202 queued` to a per-event
    /// `POST /hook`. Counts every request so a test can assert batching. Reads
    /// the whole request in one shot (small test payloads), mirroring the other
    /// raw-TCP mocks in this module.
    async fn serve_counting_hook(
        req_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        batch_status: &'static str,
    ) -> String {
        serve_counting_hook_with_accept(req_count, batch_status, None).await
    }

    async fn serve_counting_hook_with_accept(
        req_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        batch_status: &'static str,
        accepted_override: Option<usize>,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let rc = req_count.clone();
                let accepted_override = accepted_override;
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 65536];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let is_batch = req
                        .lines()
                        .next()
                        .is_some_and(|l| l.contains("/hook/batch"));
                    let (status, body) = if is_batch {
                        let payload = req.split("\r\n\r\n").nth(1).unwrap_or("");
                        let accepted = accepted_override.unwrap_or_else(|| {
                            serde_json::from_str::<serde_json::Value>(payload)
                                .ok()
                                .and_then(|v| v.as_array().map(Vec::len))
                                .unwrap_or(0)
                        });
                        (batch_status, format!("{{\"accepted\":{accepted}}}"))
                    } else {
                        ("202 Accepted", "queued".to_string())
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr.to_string()
    }

    async fn serve_delayed_batch_hook(
        req_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        delay: Duration,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let rc = req_count.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 65536];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let payload = req.split("\r\n\r\n").nth(1).unwrap_or("");
                    let accepted = serde_json::from_str::<serde_json::Value>(payload)
                        .ok()
                        .and_then(|v| v.as_array().map(Vec::len))
                        .unwrap_or(0);
                    tokio::time::sleep(delay).await;
                    let body = format!("{{\"accepted\":{accepted}}}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr.to_string()
    }

    async fn serve_counting_hook_and_enqueue_on_first(
        req_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        spool: PathBuf,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let rc = req_count.clone();
                let spool = spool.clone();
                let addr = addr.to_string();
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 65536];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let previous = rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if previous == 0 {
                        write_spool_entry(
                            &spool,
                            "second.json",
                            format!("http://{addr}/hook?event=second"),
                        );
                    }
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let payload = req.split("\r\n\r\n").nth(1).unwrap_or("");
                    let accepted = serde_json::from_str::<serde_json::Value>(payload)
                        .ok()
                        .and_then(|v| v.as_array().map(Vec::len))
                        .unwrap_or(0);
                    let body = format!("{{\"accepted\":{accepted}}}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr.to_string()
    }

    fn write_spool_entry(spool: &Path, name: &str, url: String) {
        write_spool_entry_with_body(spool, name, url, "{}".into());
    }

    fn write_spool_entry_with_body(spool: &Path, name: &str, url: String, body: String) {
        std::fs::create_dir_all(spool).unwrap();
        let e = entry_for(url, body, None, false);
        std::fs::write(spool.join(name), serde_json::to_vec(&e).unwrap()).unwrap();
    }

    fn write_spool_entry_with_token(spool: &Path, name: &str, url: String, token: &str) {
        std::fs::create_dir_all(spool).unwrap();
        let e = entry_for(url, "{}".into(), Some(token), false);
        std::fs::write(spool.join(name), serde_json::to_vec(&e).unwrap()).unwrap();
    }

    fn sorted_attempts(spool: &Path) -> Vec<u32> {
        let (mut files, _) = list_entries(spool).unwrap();
        files.sort();
        files
            .iter()
            .map(|path| {
                serde_json::from_slice::<SpoolEntry>(&std::fs::read(path).unwrap())
                    .unwrap()
                    .attempts
            })
            .collect()
    }

    #[tokio::test]
    async fn zero_budget_does_not_deserialize_or_drop_entries() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "200 OK").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(&spool, "evt-0.json", format!("http://{addr}/hook?event=e0"));
        std::fs::write(spool.join("evt-1.json"), b"not a spool entry").unwrap();

        let r = drain(&spool, tmp.path(), Duration::ZERO, Duration::from_secs(2)).await;

        assert_eq!(r.sent, 0);
        assert_eq!(
            r.dropped, 0,
            "zero budget must not pre-parse and drop bad files"
        );
        assert_eq!(r.remaining, 2);
        assert_eq!(req_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(list_entries(&spool).unwrap().0.len(), 2);
    }

    #[tokio::test]
    async fn drain_delivers_all_events_in_one_batch() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "200 OK").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 3, "all three events delivered");
        assert_eq!(r.remaining, 0);
        assert!(list_entries(&spool).unwrap().0.is_empty(), "spool emptied");
        assert_eq!(
            req_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "three events ride ONE /hook/batch request (RTT amortized)"
        );
    }

    #[tokio::test]
    async fn slow_batch_success_uses_chunk_scaled_timeout_and_deletes_once() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_delayed_batch_hook(req_count.clone(), Duration::from_millis(200)).await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
        .await;

        assert_eq!(r.sent, 3);
        assert_eq!(r.remaining, 0);
        assert!(list_entries(&spool).unwrap().0.is_empty());

        let second = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(second, DrainResult::default());
        assert_eq!(
            req_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a slow successful batch is not retried after the response lands"
        );
    }

    #[tokio::test]
    async fn drain_falls_back_to_per_event_when_batch_unsupported() {
        // Pre-upgrade server: /hook/batch is 404, per-event /hook is 202.
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "404 Not Found").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..2 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 2, "both events delivered via per-event fallback");
        assert_eq!(r.remaining, 0);
        assert!(list_entries(&spool).unwrap().0.is_empty());
        // 1 rejected batch probe + 2 per-event POSTs.
        assert_eq!(
            req_count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "one /hook/batch 404, then a per-event POST per remaining event"
        );
    }

    #[tokio::test]
    async fn failed_batch_charges_only_first_item() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "500 Internal Server Error").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 0);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.remaining, 3);
        assert_eq!(req_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (mut files, _) = list_entries(&spool).unwrap();
        files.sort();
        let attempts: Vec<u32> = files
            .iter()
            .map(|path| {
                serde_json::from_slice::<SpoolEntry>(&std::fs::read(path).unwrap())
                    .unwrap()
                    .attempts
            })
            .collect();
        assert_eq!(attempts, vec![1, 0, 0]);
    }

    #[tokio::test]
    async fn partial_batch_ack_deletes_prefix_and_charges_only_failed_item() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook_with_accept(req_count.clone(), "200 OK", Some(1)).await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        for i in 0..3 {
            write_spool_entry(
                &spool,
                &format!("evt-{i}.json"),
                format!("http://{addr}/hook?event=e{i}"),
            );
        }

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 1);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.remaining, 2);
        assert_eq!(req_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (mut files, _) = list_entries(&spool).unwrap();
        files.sort();
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("evt-1.json"));
        assert!(files[1].ends_with("evt-2.json"));
        assert_eq!(sorted_attempts(&spool), vec![1, 0]);
    }

    #[tokio::test]
    async fn endpoint_change_splits_batches_without_losing_rewound_entry() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "200 OK").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(
            &spool,
            "evt-0.json",
            format!("http://{addr}/a/hook?event=e0"),
        );
        write_spool_entry(
            &spool,
            "evt-1.json",
            format!("http://{addr}/b/hook?event=e1"),
        );
        write_spool_entry(
            &spool,
            "evt-2.json",
            format!("http://{addr}/a/hook?event=e2"),
        );

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 3);
        assert_eq!(r.remaining, 0);
        assert!(list_entries(&spool).unwrap().0.is_empty());
        assert_eq!(
            req_count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "each endpoint boundary should start a separate batch"
        );
    }

    #[tokio::test]
    async fn bearer_change_splits_batches_without_burning_attempts() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "200 OK").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry_with_token(
            &spool,
            "evt-0.json",
            format!("http://{addr}/hook?event=e0"),
            "token-a",
        );
        write_spool_entry_with_token(
            &spool,
            "evt-1.json",
            format!("http://{addr}/hook?event=e1"),
            "token-b",
        );

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 2);
        assert_eq!(r.remaining, 0);
        assert!(list_entries(&spool).unwrap().0.is_empty());
        assert_eq!(
            req_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "different bearer tokens require separate batch requests"
        );
    }

    #[tokio::test]
    async fn malformed_body_is_charged_locally_not_sent_as_null() {
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count.clone(), "200 OK").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry_with_body(
            &spool,
            "evt-0.json",
            format!("http://{addr}/hook?event=e0"),
            "{\"ok\":true}".into(),
        );
        write_spool_entry_with_body(
            &spool,
            "evt-1.json",
            format!("http://{addr}/hook?event=e1"),
            "not-json".into(),
        );
        write_spool_entry_with_body(
            &spool,
            "evt-2.json",
            format!("http://{addr}/hook?event=e2"),
            "{\"ok\":true}".into(),
        );

        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(5),
            Duration::from_secs(2),
        )
        .await;

        assert_eq!(r.sent, 2);
        assert_eq!(r.dropped, 0);
        assert_eq!(r.remaining, 1);
        assert_eq!(
            req_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only valid bodies are sent in the batch"
        );

        let (files, _) = list_entries(&spool).unwrap();
        assert_eq!(files.len(), 1);
        let remaining: SpoolEntry =
            serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(remaining.body, "not-json");
        assert_eq!(remaining.attempts, 1);
    }

    #[test]
    fn batch_endpoint_derives_from_event_url() {
        assert_eq!(
            batch_endpoint("https://h/hook?event=stop&agent=claude-code"),
            "https://h/hook/batch"
        );
        assert_eq!(batch_endpoint("https://h/hook"), "https://h/hook/batch");
    }

    #[test]
    fn spool_len_counts_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        assert_eq!(spool_len(&spool), 0, "missing dir counts as 0");
        std::fs::create_dir_all(&spool).unwrap();
        // Write distinct files directly (enqueue's ms+pid names would collide in
        // a tight loop, and its prune caps at the test MAX_SPOOL_FILES).
        for i in 0..3 {
            let e = entry_for(
                format!("http://x/hook?event=e{i}"),
                "{}".into(),
                None,
                false,
            );
            std::fs::write(
                spool.join(format!("evt-{i}.json")),
                serde_json::to_vec(&e).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(spool_len(&spool), 3);
    }

    #[test]
    fn note_retry_persist_reports_failure() {
        // Root-proof: feed a synthetic error so the warn / not-persisted branch is
        // exercised without provoking a real FS fault (the Docker CI gate runs as
        // root and ignores chmod-based read-only dirs).
        let failed: std::io::Result<()> = Err(std::io::Error::other("simulated rewrite failure"));
        assert!(
            !note_retry_persist(failed),
            "a failed persist is reported as not-persisted, not swallowed"
        );
    }

    #[test]
    fn note_retry_persist_reports_success() {
        assert!(
            note_retry_persist(Ok(())),
            "a successful persist is reported as persisted"
        );
    }

    #[tokio::test]
    async fn drain_stays_robust_when_retry_count_cannot_persist() {
        // A 5xx server, not a dead port: the pass must actually attempt the
        // bump-persist path this test blocks, and an unreachable endpoint no
        // longer charges attempts at all.
        let req_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let addr = serve_counting_hook(req_count, "500 Internal Server Error").await;
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(
            &spool,
            "evt-0.json",
            format!("http://{addr}/hook?event=stuck"),
        );

        // Make the atomic rewrite fail in a way that survives root (the Docker gate
        // runs as root, so a chmod read-only dir wouldn't fault): occupy the entry's
        // `<name>.json.tmp` path with a directory, so `rewrite_entry`'s temp-file
        // write (an `OpenOptions` create) can't be created. `list_entries` matches
        // only `*.json`, so the `.json.tmp` directory is ignored by the drain.
        let entry_path = list_entries(&spool).unwrap().0.into_iter().next().unwrap();
        let mut blocker = entry_path.into_os_string();
        blocker.push(".tmp");
        std::fs::create_dir(PathBuf::from(blocker)).unwrap();

        // The persist fails, but drain must stay fire-and-forget: no panic, the
        // event is still counted as remaining, nothing dropped, the entry survives.
        let r = drain(
            &spool,
            tmp.path(),
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(r.sent, 0);
        assert_eq!(r.dropped, 0, "a persist failure must not drop the event");
        assert_eq!(
            r.remaining, 1,
            "the event stays queued for the next boundary"
        );
        assert_eq!(
            list_entries(&spool).unwrap().0.len(),
            1,
            "the spool entry survives a failed rewrite"
        );
    }

    #[test]
    fn spool_health_is_default_when_dir_missing_or_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("no-spool");
        assert_eq!(spool_health(&missing), SpoolHealth::default());

        let empty = spool_dir(tmp.path());
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(spool_health(&empty), SpoolHealth::default());
    }

    #[test]
    fn spool_health_reports_pending_oldest_age_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        // Two events with distinct enqueue times; the second carries two failed
        // attempts. Names embed `created_ms`, so oldest-age comes from the name.
        let mut old = entry_for("https://x/hook?event=old".into(), "{}".into(), None, false);
        old.created_ms = now_ms().saturating_sub(5 * 60 * 1000);
        let mut new = entry_for("https://x/hook?event=new".into(), "{}".into(), None, false);
        new.created_ms = now_ms().saturating_sub(60 * 1000);
        new.attempts = 2;
        enqueue(&spool, &old).unwrap();
        enqueue(&spool, &new).unwrap();

        let before = now_ms();
        let health = spool_health(&spool);
        assert_eq!(health.pending, 2);
        assert_eq!(health.retries_total, 2);
        let oldest = health
            .oldest_age_ms
            .expect("non-empty spool reports an age");
        assert!(
            (5 * 60 * 1000..=5 * 60 * 1000 + 1_000).contains(&oldest),
            "oldest age ~5m, got {oldest}ms (measured at {before})"
        );
    }

    #[test]
    fn spool_health_ignores_unparseable_entries_for_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        write_spool_entry(
            &spool,
            "0000000000042-1-0000000000000001.json",
            "https://x/hook".into(),
        );
        std::fs::write(
            spool.join("0000000000043-1-0000000000000002.json"),
            b"not a spool entry",
        )
        .unwrap();

        let before = now_ms();
        let health = spool_health(&spool);
        assert_eq!(health.pending, 2);
        assert_eq!(health.retries_total, 0);
        let oldest = health
            .oldest_age_ms
            .expect("non-empty spool reports an age");
        assert!(
            oldest >= before.saturating_sub(42),
            "oldest name parses to 42ms, age {oldest}ms"
        );
    }

    #[test]
    fn created_ms_from_name_handles_malformed_names() {
        assert_eq!(
            created_ms_from_name("0000000000042-1-0000000000000001.json"),
            Some(42)
        );
        assert_eq!(created_ms_from_name("garbage.json"), None);
        // A macOS AppleDouble sidecar: ends in `.json`, sorts before any
        // digit, and must never be read as an epoch-0 entry.
        assert_eq!(created_ms_from_name("._0000000000042-1-0002.json"), None);
        // Digits alone are not enough — the writer zero-pads to 13.
        assert_eq!(created_ms_from_name("42-1-0002.json"), None);
        assert_eq!(created_ms_from_name("0000000000042-1-0002.txt"), None);
    }

    /// Regression: a foreign `.json` in the spool directory must not be
    /// counted as a pending event, and must never drive the oldest age.
    /// Before this guard an AppleDouble sidecar parsed as `created_ms = 0`
    /// and reported roughly 56 years of backlog that did not exist.
    #[test]
    fn spool_health_ignores_files_this_queue_did_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        std::fs::create_dir_all(&spool).unwrap();

        let mut real = entry_for("https://x/hook".into(), "{}".into(), None, false);
        real.created_ms = now_ms().saturating_sub(1_000);
        enqueue(&spool, &real).unwrap();

        // Sorts before any digit and still ends in `.json`.
        std::fs::write(spool.join("._sidecar.json"), b"\x00\x05").unwrap();
        std::fs::write(spool.join("notes.json"), b"{}").unwrap();

        let health = spool_health(&spool);
        assert_eq!(health.pending, 1, "only the real entry is pending");
        let age = health.oldest_age_ms.expect("one real entry");
        assert!(
            age < 60_000,
            "age must come from the real entry (~1s), got {age}ms"
        );
    }

    /// A directory holding only foreign files reports empty, not epoch-aged.
    #[test]
    fn spool_health_is_default_when_only_foreign_files_present() {
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        std::fs::create_dir_all(&spool).unwrap();
        std::fs::write(spool.join("._only.json"), b"x").unwrap();
        assert_eq!(spool_health(&spool), SpoolHealth::default());
    }
}
