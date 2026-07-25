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

/// Drop a spooled event after this many failed drain passes — bounds retries of
/// a permanently-undeliverable event (e.g. a server URL that never comes back).
const MAX_ATTEMPTS: u32 = 8;
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

static ENQUEUE_SEQ: AtomicU64 = AtomicU64::new(0);

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
    /// Failed delivery attempts so far — incremented on each drain miss and used
    /// (with `created_ms`) to drop a permanently-undeliverable event.
    #[serde(default)]
    pub attempts: u32,
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
    list_entries(spool).map_or(0, |f| f.len())
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
pub fn enqueue(spool: &Path, entry: &SpoolEntry) -> std::io::Result<()> {
    create_spool_dir(spool)?;
    let seq = ENQUEUE_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "{:013}-{}-{seq:016x}.json",
        entry.created_ms,
        std::process::id()
    );
    let tmp = spool.join(format!("{name}.tmp"));
    let final_path = spool.join(&name);
    let bytes = serde_json::to_vec(entry)?;
    write_private(&tmp, &bytes)?;
    std::fs::rename(&tmp, &final_path)?;
    prune_spool_file_count(spool);
    Ok(())
}

/// Create the spool directory `0700` on Unix so the excerpt bodies that reside
/// there (up to `MAX_AGE_MS`) — and even the timestamp+pid metadata in the file
/// names — are only reachable by the owner (#196). The spool holds private
/// capture until it drains; a world-readable directory would leak that. On
/// non-Unix the mode is a no-op (falls back to `create_dir_all`). Idempotent:
/// an existing directory's mode is left untouched (never widened, never
/// narrowed) to avoid churning a path an operator may have set deliberately.
fn create_spool_dir(spool: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        if spool.is_dir() {
            return Ok(());
        }
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(spool)
        {
            Ok(()) => Ok(()),
            // A concurrent drainer/enqueue may have created it between the check
            // and the call; treat an existing directory as success.
            Err(e) if spool.is_dir() => {
                let _ = e;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(spool)
    }
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
    }
}

/// Outcome of a drain pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainResult {
    /// Events delivered (server answered 2xx) and removed from the spool.
    pub sent: usize,
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

/// Windows can report a contended fs2 byte-range lock as this native OS code
/// instead of mapping it to `WouldBlock`.
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;

fn is_drain_lock_busy_error(err: &std::io::Error) -> bool {
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
) -> Option<DrainResult> {
    match drain_exclusive_result(spool, data_dir, total_budget, per_event_timeout, wait).await {
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
) -> std::io::Result<LockedDrainResult> {
    let Some(_lock) = acquire_drain_lock(spool, wait)? else {
        return Ok(LockedDrainResult::LockBusy);
    };
    Ok(LockedDrainResult::Drained(
        drain(spool, data_dir, total_budget, per_event_timeout).await,
    ))
}

/// Run one exclusive drain pass while treating lock wait + drain as one budget.
pub async fn drain_exclusive_within_budget(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
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
        drain(spool, data_dir, remaining_budget, per_event_timeout).await,
    ))
}

/// Hold the drain lock and keep draining until the spool is quiescent or budget expires.
pub async fn drain_until_quiescent(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
    wait: DrainLockWait,
) -> std::io::Result<LockedDrainResult> {
    let Some(_lock) = acquire_drain_lock(spool, wait)? else {
        return Ok(LockedDrainResult::LockBusy);
    };
    let started = Instant::now();
    let mut combined = DrainResult::default();

    loop {
        let Some(remaining_budget) = total_budget.checked_sub(started.elapsed()) else {
            combined.remaining = spool_len(spool);
            return Ok(LockedDrainResult::Drained(combined));
        };
        if remaining_budget.is_zero() {
            combined.remaining = spool_len(spool);
            return Ok(LockedDrainResult::Drained(combined));
        }

        let result = drain(spool, data_dir, remaining_budget, per_event_timeout).await;
        combined.sent += result.sent;
        combined.dropped += result.dropped;
        combined.remaining = result.remaining;

        if result.remaining > 0 {
            return Ok(LockedDrainResult::Drained(combined));
        }

        let queued = spool_len(spool);
        if queued == 0 {
            combined.remaining = 0;
            return Ok(LockedDrainResult::Drained(combined));
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
/// (dropped at `MAX_ATTEMPTS`); a `429` (saturation) deletes any accepted prefix
/// the server reports, then retries the rest untouched so it never burns the
/// retry budget. OIDC bearer is resolved + refreshed at most once per drain and
/// cached.
///
/// Best-effort: returns counts and never errors, so a session boundary is never
/// blocked beyond the budget and never fails the agent.
pub async fn drain(
    spool: &Path,
    data_dir: &Path,
    total_budget: Duration,
    per_event_timeout: Duration,
) -> DrainResult {
    let mut files = match list_entries(spool) {
        Some(f) => f,
        None => return DrainResult::default(),
    };
    files.sort();

    let client = build_client();
    let started = Instant::now();
    let mut oidc_cache: Option<Option<String>> = None; // outer None = not yet resolved
    let mut result = DrainResult::default();

    let mut idx = 0;
    let mut batch_supported = true;
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
        if body_is_malformed(&entry) {
            bump_or_drop(&path, &entry, &mut result);
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
                    bump_or_drop(&next_path, &next_entry, &mut result);
                    continue;
                }
                if batch_endpoint(&next_entry.url) != base {
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
                bump_or_drop(&chunk[0].0, &chunk[0].1, &mut result);
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
                        bump_or_drop(&chunk[k].0, &chunk[k].1, &mut result);
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
                        bump_or_drop(&chunk[failed_index].0, &chunk[failed_index].1, &mut result);
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
                                bump_or_drop(path, entry, &mut result);
                            }
                        }
                    }
                }
                BatchOutcome::Failed => {
                    // The batch didn't land (transport error / unexpected status).
                    // The server may have processed none, some, or all items before
                    // the client saw the failure. Charge only the first item
                    // conservatively and leave the rest queued for a future pass so
                    // a timeout cannot burn attempts for events that may never have
                    // been attempted.
                    bump_or_drop(&chunk[0].0, &chunk[0].1, &mut result);
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
                PostOutcome::Failed => {
                    bump_or_drop(&path, &entry, &mut result);
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

fn load_live_entry(path: &Path, result: &mut DrainResult) -> Option<SpoolEntry> {
    let Ok(bytes) = std::fs::read(path) else {
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

/// The `/hook/batch` URL for a spooled per-event URL: strip the `?…` query and
/// append `/batch` (a spooled URL ends in `…/hook` before its query). Entries
/// whose endpoint string matches can ride one batch request.
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
/// `MAX_ATTEMPTS`, else persist the bumped count for the next boundary. Updates
/// `result.dropped` / `result.remaining` accordingly.
fn bump_or_drop(path: &Path, entry: &SpoolEntry, result: &mut DrainResult) {
    let mut bumped = entry.clone();
    bumped.attempts = bumped.attempts.saturating_add(1);
    if bumped.attempts >= MAX_ATTEMPTS {
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

fn prune_spool_file_count(spool: &Path) {
    let Some(mut files) = list_entries(spool) else {
        return;
    };
    let excess = files.len().saturating_sub(MAX_SPOOL_FILES);
    if excess == 0 {
        return;
    }
    files.sort();
    // The spool is at its hard cap: the oldest events are about to be deleted
    // WITHOUT ever reaching the server — silent capture loss otherwise. Surface
    // it on stderr (never stdout, which carries the hook's JSON protocol output)
    // so a sustained backlog dropping events is visible, not invisible.
    eprintln!(
        "ai-memory: hook-spool at capacity ({} > {MAX_SPOOL_FILES}); evicting {excess} oldest UNDELIVERED event(s)",
        files.len()
    );
    for path in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// List `*.json` spool files (ignoring in-flight `*.json.tmp`), or None when the
/// directory doesn't exist yet.
fn list_entries(spool: &Path) -> Option<Vec<PathBuf>> {
    let read = std::fs::read_dir(spool).ok()?;
    let mut out = Vec::new();
    for ent in read.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            out.push(path);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let files = list_entries(&spool).unwrap();
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
        let path = list_entries(&spool).unwrap().into_iter().next().unwrap();

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
        let path = list_entries(&spool).unwrap().into_iter().next().unwrap();

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
        let path = list_entries(&spool).unwrap().into_iter().next().unwrap();

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

        let files = list_entries(&spool).unwrap();
        assert_eq!(files.len(), MAX_SPOOL_FILES);
    }

    #[test]
    fn enqueue_prunes_oldest_files_when_spool_exceeds_limit() {
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

        let mut files = list_entries(&spool).unwrap();
        files.sort();
        assert_eq!(files.len(), MAX_SPOOL_FILES);
        let bodies: Vec<SpoolEntry> = files
            .iter()
            .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
            .collect();
        assert!(bodies.iter().all(|entry| entry.created_ms >= 2));
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
        assert_eq!(list_entries(&spool).unwrap().len(), 2);
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
        )
        .await
        .expect("lock acquired");
        let LockedDrainResult::Drained(result) = result else {
            panic!("lock should not be busy")
        };

        assert_eq!(result.sent, 2);
        assert_eq!(result.remaining, 0);
        assert_eq!(req_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(list_entries(&spool).unwrap().is_empty());
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
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let e = entry_for(
            "http://127.0.0.1:1/hook?event=dead".into(),
            "{}".into(),
            None,
            false,
        );
        enqueue(&spool, &e).unwrap();
        let mut dropped = 0;
        for _ in 0..MAX_ATTEMPTS {
            dropped += drain(
                &spool,
                tmp.path(),
                Duration::from_secs(2),
                Duration::from_millis(100),
            )
            .await
            .dropped;
        }
        assert_eq!(
            dropped, 1,
            "the dead event is dropped once it hits MAX_ATTEMPTS"
        );
        assert!(
            list_entries(&spool).unwrap().is_empty(),
            "spool is empty after the drop"
        );
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
        assert!(list_entries(&spool).unwrap().is_empty());
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
        let files = list_entries(&spool).unwrap();
        assert_eq!(files.len(), 1, "event still queued after many 429s");
        let loaded: SpoolEntry =
            serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(loaded.attempts, 0, "429 must not consume the retry budget");
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
        let mut files = list_entries(&spool).unwrap();
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
        let mut files = list_entries(&spool).unwrap();
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
        let mut files = list_entries(&spool).unwrap();
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
        let mut files = list_entries(spool).unwrap();
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
        assert_eq!(list_entries(&spool).unwrap().len(), 2);
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
        assert!(list_entries(&spool).unwrap().is_empty(), "spool emptied");
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
        assert!(list_entries(&spool).unwrap().is_empty());

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
        assert!(list_entries(&spool).unwrap().is_empty());
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

        let mut files = list_entries(&spool).unwrap();
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
        let mut files = list_entries(&spool).unwrap();
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
        assert!(list_entries(&spool).unwrap().is_empty());
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
        assert!(list_entries(&spool).unwrap().is_empty());
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

        let files = list_entries(&spool).unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let spool = spool_dir(tmp.path());
        let e = entry_for(
            "http://127.0.0.1:1/hook?event=stuck".into(),
            "{}".into(),
            None,
            false,
        );
        enqueue(&spool, &e).unwrap();

        // Make the atomic rewrite fail in a way that survives root (the Docker gate
        // runs as root, so a chmod read-only dir wouldn't fault): occupy the entry's
        // `<name>.json.tmp` path with a directory, so `rewrite_entry`'s temp-file
        // write (an `OpenOptions` create) can't be created. `list_entries` matches
        // only `*.json`, so the `.json.tmp` directory is ignored by the drain.
        let entry_path = list_entries(&spool).unwrap().into_iter().next().unwrap();
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
            list_entries(&spool).unwrap().len(),
            1,
            "the spool entry survives a failed rewrite"
        );
    }
}
