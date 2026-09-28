//! Filesystem watcher with debouncing and a periodic reconciliation pass.
//!
//! Two parts work together:
//!
//! 1. **Debounced events** via [`notify_debouncer_full`]. When a markdown
//!    file under the wiki root is created or modified, we read it from
//!    disk, parse the frontmatter, and `reindex_page` against the store.
//!    Own-writes are absorbed by the store's sha256 short-circuit, so
//!    the loop terminates after one no-op reindex.
//! 2. **Reconciliation tick** every 30s walks the entire wiki tree and
//!    reindexes page markdown files (excluding `_meta.md`, bootstrap files,
//!    raw event ledgers, and symlinks). Catches any events the OS dropped
//!    (basic-memory #580 — file watchers go stale under FSEvents buffer
//!    overflow, hidden-dir globs, etc.). Hidden-directory paths are
//!    explicitly NOT skipped (#798 lesson).
//!
//! Deletions are not reconciled (see #929): the watcher only handles
//! create/modify events, so a page whose file disappears stays indexed until
//! `ai-memory delete-page` removes it explicitly.
//!
//! A reindexed page version — new file or rewrite alike — is embedded the
//! same way `write_page` embeds one, via `Wiki::embed_page_version` (#929):
//! before, only pages written through the API got embeddings, and a
//! watcher-driven rewrite's new version silently had none until a manual
//! `ai-memory embed`.
//!
//! The watcher never *writes* to disk — that loop would be unbounded.
//! External writes drive store updates; internal writes drive disk +
//! store updates via [`Wiki::write_page`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use ai_memory_core::{PageId, PagePath, ProjectId, WorkspaceId};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer_opt};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::error::{WikiError, WikiResult};
use crate::wiki::Wiki;

/// Reconciliation tick interval.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Debounce window for filesystem events.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);

#[cfg(all(test, target_os = "macos"))]
type PlatformWatcher = notify::PollWatcher;
#[cfg(not(all(test, target_os = "macos")))]
type PlatformWatcher = notify::RecommendedWatcher;

/// Handle representing an active watcher; drop to stop.
pub struct WatcherHandle {
    _debouncer: Debouncer<PlatformWatcher, RecommendedCache>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl WatcherHandle {
    /// Start watching `wiki.root()` recursively. Spawns one tokio task
    /// that consumes debounced events and runs the reconciliation timer.
    ///
    /// Events are attributed to their `(workspace_id, project_id)` by
    /// parsing the first two path segments as UUIDs. Events outside the
    /// `<ws_uuid>/<proj_uuid>/...` layout are silently ignored.
    ///
    /// # Errors
    /// Propagates any notify error encountered when installing the OS
    /// watcher.
    pub fn start(wiki: Wiki) -> WikiResult<Self> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let mut debouncer = new_debouncer_opt::<_, PlatformWatcher, RecommendedCache>(
            DEBOUNCE_WINDOW,
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    for event in events {
                        let _ = event_tx.send(event);
                    }
                }
                Err(errors) => {
                    for e in errors {
                        warn!(error = %e, "notify error");
                    }
                }
            },
            RecommendedCache::new(),
            watcher_config(),
        )
        .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))?;

        debouncer
            .watch(wiki.root(), RecursiveMode::Recursive)
            .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(run_loop(wiki, event_rx, shutdown_rx));

        Ok(Self {
            _debouncer: debouncer,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Stop the watcher and wait for the event loop to drain.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.task.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

fn watcher_config() -> notify::Config {
    let config = notify::Config::default();
    #[cfg(all(test, target_os = "macos"))]
    {
        // GitHub macOS runners have flaky FSEvents delivery for tempdir unit
        // tests. Use the polling backend there so the test covers our watcher
        // loop without depending on runner-specific FSEvents behavior.
        config.with_poll_interval(DEBOUNCE_WINDOW)
    }
    #[cfg(not(all(test, target_os = "macos")))]
    {
        config
    }
}

async fn run_loop(
    wiki: Wiki,
    mut rx: mpsc::UnboundedReceiver<notify_debouncer_full::DebouncedEvent>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; consume it so we don't reconcile at boot.
    tick.tick().await;

    // Track consecutive failures of the reconciliation pass so we can
    // surface a clear "watcher is degraded" event after a streak, in
    // addition to the per-failure error log. Without this, a broken
    // disk → store bridge can stay broken indefinitely with only a
    // line per 30s in the warn stream — easy to miss in busy logs.
    let mut consecutive_failures: u32 = 0;
    const DEGRADED_AFTER: u32 = 5;

    // Reconcile-delete safety net (#929, opt-in — see `MissingStreaks`):
    // per-`(workspace, project, path)` count of consecutive passes a
    // candidate page's file has looked missing. Lives here, not on `Wiki`,
    // for the same reason `consecutive_failures` does: it is this task's own
    // pass-to-pass memory, not shared server state.
    let mut missing_streaks: MissingStreaks = HashMap::new();

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                debug!("watcher shutting down");
                return;
            }
            Some(event) = rx.recv() => {
                handle_event(&wiki, event).await;
            }
            _ = tick.tick() => {
                match reconcile(&wiki, &mut missing_streaks).await {
                    Ok(_) => {
                        if consecutive_failures > 0 {
                            tracing::info!(
                                prior_failures = consecutive_failures,
                                "reconciliation recovered after consecutive failures",
                            );
                            consecutive_failures = 0;
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        tracing::error!(
                            error = %e,
                            consecutive_failures,
                            "reconciliation failed",
                        );
                        if consecutive_failures == DEGRADED_AFTER {
                            tracing::error!(
                                consecutive_failures,
                                event = "watcher_degraded",
                                "wiki↔store reconciliation has failed {DEGRADED_AFTER} \
                                 times in a row; the disk and SQLite index may now be \
                                 out of sync. Investigate disk permissions, DB lock \
                                 contention, or filesystem health. The watcher will \
                                 keep retrying every {RECONCILE_INTERVAL:?}.",
                            );
                        }
                    }
                }
            }
            else => return,
        }
    }
}

/// Inside the wiki's own git directory or any nested git metadata: neither indexed nor reported.
fn is_git_internal(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .any(|c| ai_memory_core::is_git_reserved_component(&c.as_os_str().to_string_lossy()))
    })
}

async fn handle_event(wiki: &Wiki, event: notify_debouncer_full::DebouncedEvent) {
    // Nothing to index, but the next auto-commit must stage it.
    if matches!(event.kind, EventKind::Remove(_)) {
        for raw_path in &event.paths {
            if !is_tempfile(raw_path) && !is_git_internal(wiki.root(), raw_path) {
                wiki.git().mark_written(raw_path);
            }
        }
        return;
    }
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Other
    ) {
        return;
    }
    for raw_path in &event.paths {
        if is_git_internal(wiki.root(), raw_path) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(raw_path) else {
            // Likely a transient state (mv, atomic rename in flight).
            continue;
        };
        let ft = metadata.file_type();
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            let Some((ws, proj, proj_root)) = extract_project_dir_ids(wiki.root(), raw_path) else {
                continue;
            };
            reindex_project_dir(wiki, ws, proj, proj_root).await;
            continue;
        }
        if !ft.is_file() || is_tempfile(raw_path) {
            continue;
        }
        // Reported before the indexer's filters, which skip ledgers,
        // pending and non-markdown files.
        wiki.git().mark_written(raw_path);
        if !is_markdown(raw_path) {
            continue;
        }
        let Some((ws, proj, page_path)) = extract_project_ids(wiki.root(), raw_path) else {
            continue;
        };
        if is_pending_path(&page_path) {
            continue;
        }
        if is_reserved_page_file(raw_path, &page_path) {
            continue;
        }
        // A tombstoned session's page must not come back from a file its
        // purge could not remove (#701). The path shape decides whether the
        // lookup is worth a round trip: only `sessions/<id>.md` can be that
        // file, so an ordinary edit to `index.md` or a decision page never
        // queries. The sweep paths amortise the same set over a whole
        // directory instead; this one sees a single file per event.
        if let Some(session) = crate::wiki::session_id_for_page(&page_path) {
            match wiki.purged_sessions(ws, proj).await {
                Ok(purged) if purged.contains(&session) => {
                    debug!(path = %page_path, "ignoring event for a purged session page");
                    continue;
                }
                Ok(_) => {}
                // Fails open, deliberately: a transient lookup error must not
                // stop the watcher from indexing edits. The cost of failing
                // open here is bounded — the next reconcile pass loads the set
                // again and skips the page then.
                Err(e) => warn!(path = %page_path, error = %e, "purged-session lookup failed"),
            }
        }
        match wiki.reindex_page(ws, proj, page_path.clone()).await {
            Ok(_) => debug!(path = %page_path, "reindexed via watcher"),
            Err(e) => warn!(path = %page_path, error = %e, "watcher reindex failed"),
        }
    }
}

/// Returns `false` when the directory was skipped because the store has no
/// row for it — the same orphan case `reconcile` counts as `skipped_orphans`,
/// surfaced here so the skip is testable on this path too.
async fn reindex_project_dir(
    wiki: &Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
    proj_root: std::path::PathBuf,
) -> bool {
    // Same orphan guard `reconcile` applies (#613), for the other way a
    // project directory reaches the indexer. A filesystem event on a rowless
    // directory would otherwise walk it and warn once per page, which is the
    // behaviour that pass was about — quieter here only because it needs an
    // event rather than firing every 30s. Checking once per directory also
    // saves walking a tree whose every page is going to fail scope resolution.
    //
    // Rows only, for the same reason `reconcile` uses this form: the guard
    // runs before `reindex_page` takes the mutation lock, so writing a
    // `_meta.md` here could land it in a directory a concurrent project move
    // is renaming away.
    if let Err(e) = wiki.ensure_project_scope_rows(ws, proj).await {
        debug!(
            workspace = %ws,
            project = %proj,
            error = %e,
            "skipping directory event for a project directory with no store row",
        );
        return false;
    }
    let pages = match tokio::task::spawn_blocking(move || walk_markdown(&proj_root)).await {
        Ok(Ok(pages)) => pages,
        Ok(Err(e)) => {
            warn!(error = %e, "watcher directory walk failed");
            return true;
        }
        Err(e) => {
            warn!(error = %e, "watcher directory walk task failed");
            return true;
        }
    };

    // Fails open for the same reason the single-event path does: a lookup
    // error must not stop a directory event from indexing. `Wiki::reindex_all`
    // is the one caller that fails closed, because an operator-triggered
    // reindex should report the failure rather than quietly skip the gate.
    let purged = wiki.purged_sessions(ws, proj).await.unwrap_or_else(|e| {
        warn!(error = %e, "purged-session lookup failed; not gating this pass");
        std::collections::HashSet::new()
    });
    for path in pages {
        if crate::wiki::is_purged_session_page(&path, &purged) {
            debug!(path = %path, "skipping a purged session page");
            continue;
        }
        match wiki.reindex_page(ws, proj, path.clone()).await {
            Ok(_) => debug!(path = %path, "reindexed via watcher directory event"),
            Err(e) => warn!(path = %path, error = %e, "watcher directory reindex failed"),
        }
    }
    true
}

/// Outcome of one reconciliation pass, for the caller's telemetry and tests.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ReconcileStats {
    /// Pages successfully (re)indexed from resolvable project directories.
    pub indexed: usize,
    /// Project directories present on disk that the store has no row for.
    /// These are skipped wholesale rather than failing scope resolution on
    /// every page, every pass, forever (see #613).
    pub skipped_orphans: usize,
    /// Session pages left on disk by a purge whose file cleanup failed, and
    /// deliberately not re-indexed (#701).
    pub skipped_purged_sessions: usize,
    /// Pages tombstoned by the reconcile-delete safety net (#929) after their
    /// file was missing on two consecutive passes. Always `0` unless
    /// `[maintenance] reconcile_tombstones_deleted_pages` is enabled.
    pub tombstoned_missing: usize,
    /// Scopes where the reconcile-delete circuit breaker tripped this pass
    /// (design item #4) — too many candidates looked missing at once, so
    /// nothing in that scope was tombstoned or counted toward a streak.
    pub circuit_broken_scopes: usize,
}

/// Per-`(workspace, project, path)` streak of consecutive reconcile passes a
/// candidate page's file has looked missing, plus the [`PageId`] last
/// observed missing (so a path superseded by a new version between passes
/// restarts its streak rather than crediting the old version's absence to
/// the new one). Design item #5 requires two consecutive passes before a
/// tombstone is even attempted; this is that memory, owned by the watcher's
/// `run_loop` across ticks the same way `consecutive_failures` is.
type MissingStreaks = HashMap<(WorkspaceId, ProjectId, PagePath), (PageId, u32)>;

/// Reconcile-delete circuit breaker (design item #4): absolute floor. Even a
/// scope where every one of a handful of candidates looks "missing" must
/// clear this many before the breaker can trip — a two- or three-page
/// project isn't permanently blocked from ever tombstoning a real deletion
/// just because 100% of a tiny candidate set matches.
const RECONCILE_DELETE_BREAKER_MIN_CANDIDATES: usize = 3;

/// Reconcile-delete circuit breaker (design item #4): fraction floor, applied
/// alongside the absolute floor above. Half (or more) of a scope's candidate
/// pages vanishing inside one 30s reconcile window is far more likely a
/// walk/mount problem — an unmounted volume, a git checkout mid-walk, a
/// project directory being renamed — than that many genuine deletions
/// landing in the same pass, so the pass is treated as suspect and nothing
/// in that scope is tombstoned this round.
const RECONCILE_DELETE_BREAKER_FRACTION: f64 = 0.5;

/// The missing-candidate count strictly above which the circuit breaker
/// trips, for a scope with `total_candidates` walk-eligible pages tracked
/// before the walk ran this pass.
fn reconcile_delete_breaker_threshold(total_candidates: usize) -> usize {
    #[allow(clippy::cast_precision_loss)]
    let total = total_candidates as f64;
    let fraction_floor = (total * RECONCILE_DELETE_BREAKER_FRACTION).ceil();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let fraction_floor = fraction_floor as usize;
    fraction_floor.max(RECONCILE_DELETE_BREAKER_MIN_CANDIDATES)
}

/// Whether `path` is a shape [`walk_markdown`] could ever have returned,
/// applied to a DB row instead of a filesystem entry (design item #1).
///
/// `bootstrap.md`, `_meta.md`, and `_pending/` sidecars are indexed rows the
/// walk structurally never returns, so their absence from a walk's page list
/// proves nothing about whether their file still exists — without this
/// filter every one of them would look "missing" on every single pass.
///
/// More conservative than the walk's own ledger check
/// ([`is_reserved_page_file`]), which reads a candidate ledger file's first
/// line to tell a real hook ledger from an ordinary page that merely happens
/// to be named `log.md`. Here the file is gone by definition — there is
/// nothing left to read — so any `log.md` / `log-YYYY-MM.md`-shaped path is
/// excluded from candidacy unconditionally, erring toward never tombstoning
/// rather than guessing.
///
/// `sessions/<id>.md` pages are excluded too, for an unrelated reason (#929
/// review, B2): a same-workspace `move-session` re-home updates the store
/// row's `(workspace_id, project_id)` but deliberately does not move the
/// file (the wiki layer skips that on purpose — see `Wiki::move_session_page`
/// and its callers), so a session summary page can have a perfectly correct,
/// live DB row with no file at its *new* scope. That is a separate,
/// pre-existing bug in `move-session`'s file relocation, not fixed here —
/// but this mechanism would otherwise read that gap as "the file was
/// deleted" and tombstone a real, live page. This safety net is meant for
/// OKF-imported content pages (concepts, decisions, gotchas — the paths
/// #929's own repro used), never session-summary pages, so the whole shape
/// is excluded until `move-session` is fixed to relocate the file too
/// (tracked as a follow-up).
fn could_have_been_walked(path: &PagePath) -> bool {
    if is_pending_path(path) {
        return false;
    }
    if is_manifest_filename(path) || path.as_str() == "bootstrap.md" {
        return false;
    }
    if crate::wiki::session_id_for_page(path).is_some() {
        return false;
    }
    if crate::ledger::is_log_ledger_filename(path) {
        return false;
    }
    true
}

/// The reconcile-delete safety net (#929, design items #1–#6) for one
/// project scope, run after that scope's walk has completed. No-op unless
/// `[maintenance] reconcile_tombstones_deleted_pages` is enabled.
///
/// `pre_walk_snapshot` must have been taken from the store *before*
/// `walked`'s walk started (design item #2) — that ordering is what makes a
/// page written via the API mid-walk safe: it is either absent from the
/// snapshot (never a candidate) or present in `walked` (found on disk), and
/// either way it can never look like a deletion.
#[allow(clippy::too_many_arguments)]
async fn reconcile_missing_pages(
    wiki: &Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
    pre_walk_snapshot: &[(PageId, PagePath)],
    walked: &HashSet<PagePath>,
    walk_partial: bool,
    streaks: &mut MissingStreaks,
    stats: &mut ReconcileStats,
) {
    if pre_walk_snapshot.is_empty() {
        return;
    }

    // A page found on disk this pass can never contribute to a future
    // tombstone from an earlier streak — clear it regardless of what happens
    // to the rest of this scope below. This runs even on a partial walk
    // (S1, #929 review): only ABSENCE evidence from a partial walk is
    // suspect (design item #3, below) — presence evidence a partial walk DID
    // manage to observe is exactly as trustworthy as any other pass's, and
    // losing it would needlessly re-arm a streak the page had already
    // cleared.
    for (_, path) in pre_walk_snapshot.iter().filter(|(_, p)| walked.contains(p)) {
        streaks.remove(&(ws, proj, path.clone()));
    }

    if walk_partial {
        // Design item #3: a `NotFound` anywhere in this scope's walk (a
        // vanished subdirectory, or the whole project directory) means some
        // branch of the tree was unreadable rather than every page under it
        // having actually been deleted one by one. Skip NEW missing-page
        // evidence for this pass — streaks for pages already missing before
        // this pass are left exactly as they were (neither incremented nor
        // cleared), so a later, complete pass still needs its own two
        // consecutive observations before anything is tombstoned.
        debug!(
            workspace = %ws,
            project = %proj,
            "reconcile-delete: partial walk this pass (a directory was unreadable); not \
             treating this scope's absent pages as new deletion evidence",
        );
        return;
    }

    let missing: Vec<&(PageId, PagePath)> = pre_walk_snapshot
        .iter()
        .filter(|(_, p)| !walked.contains(p))
        .collect();
    if missing.is_empty() {
        return;
    }

    // Design item #4: circuit breaker. A suspect pass makes zero progress
    // toward the two-pass threshold for any of its candidates — not just
    // "don't tombstone yet", but "don't count this pass at all" — so a
    // walk/mount blip can never contribute half of the two observations a
    // real deletion needs.
    //
    // S3 (#929 review): an empty `walked` set with a non-empty snapshot is
    // ALWAYS suspect, regardless of the `max(3, 50%)` math below — that
    // fraction still has a floor of `RECONCILE_DELETE_BREAKER_MIN_CANDIDATES`,
    // so a scope with only one or two tracked candidates could have every
    // single one look missing without ever tripping the percentage breaker.
    // "the walk found NOTHING at all" is exactly the "directory exists but
    // came back empty" mount-failure signature (an unmounted volume, a
    // bind-mount that briefly resolves to an empty stub) that the threshold
    // alone would miss for a small scope.
    let threshold = reconcile_delete_breaker_threshold(pre_walk_snapshot.len());
    let empty_walk_is_suspect = walked.is_empty();
    if missing.len() > threshold || empty_walk_is_suspect {
        tracing::warn!(
            workspace = %ws,
            project = %proj,
            missing = missing.len(),
            total_candidates = pre_walk_snapshot.len(),
            threshold,
            empty_walk = empty_walk_is_suspect,
            "reconcile-delete: circuit breaker tripped — treating this as a walk/mount \
             problem rather than that many genuine deletions in one pass; nothing in this \
             scope is tombstoned or counted toward a streak this round",
        );
        stats.circuit_broken_scopes += 1;
        return;
    }

    for (id, path) in missing {
        let key = (ws, proj, path.clone());
        let streak = streaks.entry(key.clone()).or_insert((*id, 0));
        if streak.0 != *id {
            // A different page version now occupies this path than the one
            // whose absence started this streak (e.g. recreated then
            // rewritten between passes) — restart rather than credit the old
            // version's absence to the new one.
            *streak = (*id, 0);
        }
        streak.1 += 1;
        if streak.1 < 2 {
            continue;
        }
        // Missing on two consecutive passes: attempt the tombstone. Whatever
        // the outcome, drop the streak — a page still missing after this
        // starts a fresh streak next pass rather than retrying every tick.
        streaks.remove(&key);
        match wiki
            .tombstone_missing_page_if_latest(ws, proj, path, *id)
            .await
        {
            Ok(true) => {
                stats.tombstoned_missing += 1;
                tracing::info!(
                    workspace = %ws,
                    project = %proj,
                    path = %path,
                    "reconcile-delete: tombstoned a page whose file was missing on two \
                     consecutive reconcile passes",
                );
            }
            Ok(false) => {
                debug!(
                    workspace = %ws,
                    project = %proj,
                    path = %path,
                    "reconcile-delete: tombstone candidate skipped (page changed, or its file \
                     reappeared before the final recheck)",
                );
            }
            Err(e) => {
                warn!(
                    workspace = %ws,
                    project = %proj,
                    path = %path,
                    error = %e,
                    "reconcile-delete: tombstone attempt failed",
                );
            }
        }
    }
}

async fn reconcile(
    wiki: &Wiki,
    missing_streaks: &mut MissingStreaks,
) -> WikiResult<ReconcileStats> {
    let root = wiki.root().to_path_buf();
    // Walk all per-project subdirectories: <ws_uuid>/<proj_uuid>/
    let project_dirs = tokio::task::spawn_blocking(move || walk_project_dirs(&root))
        .await
        .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;

    let tombstones_enabled = wiki.reconcile_tombstones_deleted_pages();
    let mut stats = ReconcileStats::default();
    for (ws, proj, proj_root) in project_dirs {
        // The directory name parses as a valid UUID pair, but that does not
        // mean the store knows the project. An orphan directory (e.g. a shell
        // the OKF migration seeded an index.md into, or a leftover from older
        // history) can never reconcile: every page in it fails scope
        // resolution identically on every pass. Check the scope once per
        // directory and skip the whole thing at debug, instead of warning per
        // page indefinitely. If the row later appears (project recreated), the
        // check passes and the directory indexes normally on the next pass.
        // Rows only: reconcile runs outside the mutation guard, so it must
        // not write a `_meta.md` into a directory a concurrent project move
        // may be renaming away. These directories already have their
        // manifests — written with their first page, or by the backfill.
        if let Err(e) = wiki.ensure_project_scope_rows(ws, proj).await {
            debug!(
                workspace = %ws,
                project = %proj,
                error = %e,
                "skipping reconcile for a project directory with no store row",
            );
            stats.skipped_orphans += 1;
            continue;
        }

        // Design item #2: snapshot the store's `is_latest = 1` pages for this
        // scope BEFORE the walk runs, not after — see `reconcile_missing_pages`.
        // Filtered to paths the walk could ever return, so reserved/pending
        // rows are never candidates (design item #1). A snapshot failure
        // disables the safety net for this scope this pass only; it never
        // blocks the ordinary reindex below.
        let pre_walk_snapshot = if tombstones_enabled {
            match wiki.latest_page_ids(ws, proj).await {
                Ok(rows) => rows
                    .into_iter()
                    .filter(|(_, p)| could_have_been_walked(p))
                    .collect::<Vec<_>>(),
                Err(e) => {
                    warn!(
                        workspace = %ws,
                        project = %proj,
                        error = %e,
                        "reconcile-delete: candidate snapshot failed; skipping this scope's \
                         missing-page check this pass",
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let (pages, walk_partial) =
            tokio::task::spawn_blocking(move || walk_markdown_partial(&proj_root))
                .await
                .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;
        // Once per directory, not once per page: a project accumulates one
        // tombstone per purge, and the set is only consulted for the
        // `sessions/<id>.md` paths that a session purge could have left behind.
        let purged = wiki.purged_sessions(ws, proj).await?;
        let mut walked: HashSet<PagePath> = HashSet::with_capacity(pages.len());
        for path in pages {
            walked.insert(path.clone());
            if crate::wiki::is_purged_session_page(&path, &purged) {
                debug!(
                    path = %path,
                    "skipping reconcile of a purged (tombstoned) session page",
                );
                stats.skipped_purged_sessions += 1;
                continue;
            }
            if let Err(e) = wiki.reindex_page(ws, proj, path.clone()).await {
                warn!(path = %path, error = %e, "reconcile reindex failed");
            } else {
                stats.indexed += 1;
            }
        }

        if tombstones_enabled {
            reconcile_missing_pages(
                wiki,
                ws,
                proj,
                &pre_walk_snapshot,
                &walked,
                walk_partial,
                missing_streaks,
                &mut stats,
            )
            .await;
        }
    }
    // debug!, not info!: this fires every RECONCILE_INTERVAL regardless of
    // activity, so at info it is ~half the default server log (#894). Its
    // failure signals stay loud — the per-page `warn!` above, the
    // `watcher_degraded` `error!`, and the `info!` recovery transition.
    debug!(
        indexed = stats.indexed,
        skipped_orphans = stats.skipped_orphans,
        skipped_purged_sessions = stats.skipped_purged_sessions,
        tombstoned_missing = stats.tombstoned_missing,
        circuit_broken_scopes = stats.circuit_broken_scopes,
        "reconciliation pass complete",
    );
    Ok(stats)
}

/// Walk `<wiki_root>` and return all `(WorkspaceId, ProjectId, proj_root)` tuples
/// whose first two path segments parse as valid UUIDs.
pub(crate) fn walk_project_dirs(
    wiki_root: &Path,
) -> WikiResult<Vec<(WorkspaceId, ProjectId, std::path::PathBuf)>> {
    let mut out = Vec::new();
    let ws_read = match std::fs::read_dir(wiki_root) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(WikiError::Io(e)),
    };
    for ws_entry in ws_read {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_name = ws_entry.file_name();
        let Some(ws_str) = ws_name.to_str() else {
            continue;
        };
        let Ok(ws_id) = WorkspaceId::from_str(ws_str) else {
            continue;
        };
        let proj_read = match std::fs::read_dir(ws_entry.path()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for proj_entry in proj_read {
            let proj_entry = proj_entry?;
            if !proj_entry.file_type()?.is_dir() {
                continue;
            }
            let proj_name = proj_entry.file_name();
            let Some(proj_str) = proj_name.to_str() else {
                continue;
            };
            let Ok(proj_id) = ProjectId::from_str(proj_str) else {
                continue;
            };
            out.push((ws_id, proj_id, proj_entry.path()));
        }
    }
    Ok(out)
}

/// Parse `(WorkspaceId, ProjectId, PagePath)` from a filesystem event path.
///
/// Expects the path to have the structure:
/// `<wiki_root>/<ws_uuid>/<proj_uuid>/<page-path...>`
///
/// Returns `None` when:
/// - The path does not start with `wiki_root`.
/// - The first segment is not a valid UUID (`WorkspaceId`).
/// - The second segment is not a valid UUID (`ProjectId`).
/// - There are no remaining segments (the page path would be empty).
pub(crate) fn extract_project_ids(
    wiki_root: &Path,
    event_path: &Path,
) -> Option<(WorkspaceId, ProjectId, PagePath)> {
    let rel = event_path.strip_prefix(wiki_root).ok()?;
    let mut components = rel.components();

    let ws_seg = components.next()?.as_os_str().to_str()?;
    let ws_id = WorkspaceId::from_str(ws_seg).ok()?;

    let proj_seg = components.next()?.as_os_str().to_str()?;
    let proj_id = ProjectId::from_str(proj_seg).ok()?;

    // Rejoin remaining segments as the page path.
    let page_rel: std::path::PathBuf = components.collect();
    let page_str = crate::git::slash_path(&page_rel);
    if page_str.is_empty() {
        return None;
    }
    let page_path = PagePath::new(page_str).ok()?;
    Some((ws_id, proj_id, page_path))
}

fn extract_project_dir_ids(
    wiki_root: &Path,
    event_path: &Path,
) -> Option<(WorkspaceId, ProjectId, std::path::PathBuf)> {
    let rel = event_path.strip_prefix(wiki_root).ok()?;
    let mut components = rel.components();

    let ws_seg = components.next()?.as_os_str().to_str()?;
    let ws_id = WorkspaceId::from_str(ws_seg).ok()?;

    let proj_seg = components.next()?.as_os_str().to_str()?;
    let proj_id = ProjectId::from_str(proj_seg).ok()?;

    Some((ws_id, proj_id, wiki_root.join(ws_seg).join(proj_seg)))
}

pub(crate) fn walk_markdown(root: &Path) -> WikiResult<Vec<PagePath>> {
    walk_markdown_partial(root).map(|(pages, _partial)| pages)
}

/// Like [`walk_markdown`], but also reports whether any directory in the
/// tree — including `root` itself — could not be read because it no longer
/// exists.
///
/// A `true` second element means the walk is **partial**: some branch
/// vanished mid-walk (a git checkout, a bind-mount hiccup, a concurrent
/// project move) rather than every page under it having actually been
/// deleted one at a time. The reconcile-delete safety net (#929 design item
/// #3) relies on this to tell those two situations apart — treating a
/// partial walk's absent pages as deletion evidence would tombstone pages
/// that are still there, just temporarily unreadable.
pub(crate) fn walk_markdown_partial(root: &Path) -> WikiResult<(Vec<PagePath>, bool)> {
    let mut out = Vec::new();
    let mut partial = false;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                partial = true;
                continue;
            }
            Err(e) => return Err(WikiError::Io(e)),
        };
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            // Skip symlinks entirely. An attacker with write access to
            // the wiki/ dir could otherwise plant a symlink to /etc/hosts,
            // /home/user/.ssh/id_ed25519 etc. and have the watcher
            // index the target's content. The sanitiser would still
            // scrub credentials, but we'd be reading files we
            // shouldn't be reading. (Audit critical #3.)
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|rel| rel.components().next())
                    .and_then(|c| c.as_os_str().to_str())
                    .is_some_and(|segment| segment == "_pending")
                {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file()
                && is_markdown(&path)
                && !is_tempfile(&path)
                && let Some(pp) = page_path_relative_to(root, &path)
                && !is_pending_path(&pp)
                && !is_reserved_page_file(&path, &pp)
            {
                out.push(pp);
            }
        }
    }
    Ok((out, partial))
}

pub(crate) fn is_pending_path(page_path: &PagePath) -> bool {
    page_path.as_str() == "_pending" || page_path.as_str().starts_with("_pending/")
}

fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "md")
}

fn is_tempfile(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".ai-memory-tmp."))
}

/// `_meta.md` is the per-scope manifest the engine writes (workspace/project
/// name + repo_path) so the wiki tree is self-describing. It describes the
/// scope, it is never a wiki page.
fn is_manifest_filename(page_path: &PagePath) -> bool {
    page_path
        .as_str()
        .rsplit('/')
        .next()
        .is_some_and(|name| name == "_meta.md")
}

/// Returns `true` for markdown files that are NOT wiki pages and must be
/// skipped by the indexer:
/// - `_meta.md` (the self-describing scope manifest) and `bootstrap.md` —
///   always; and
/// - the raw event ledger (`log.md` / exact `log-YYYY-MM.md`) — skipping which
///   avoids supersession loops, since every `append_event` write triggers a
///   watcher event. A reserved-looking filename is skipped only when its
///   first body line is a raw hook log entry; ordinary markdown pages with
///   those names are indexed, frontmatter or not.
fn is_reserved_page_file(abs: &Path, page_path: &PagePath) -> bool {
    if is_manifest_filename(page_path) || page_path.as_str() == "bootstrap.md" {
        return true;
    }
    crate::ledger::is_log_ledger_filename(page_path) && crate::ledger::opens_with_log_ledger(abs)
}

fn page_path_relative_to(root: &Path, abs: &Path) -> Option<PagePath> {
    let rel: &Path = abs.strip_prefix(root).ok()?;
    PagePath::new(crate::git::slash_path(rel)).ok()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use ai_memory_store::Store;
    use std::sync::Arc;
    use tempfile::TempDir;

    #[cfg(windows)]
    fn create_test_symlink_file(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(1314) => {
                eprintln!("skipping symlink assertion: Windows symlink privilege unavailable");
                false
            }
            Err(e) => panic!("failed to create symlink {}: {e}", link.display()),
        }
    }

    #[cfg(unix)]
    fn create_test_symlink_file(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    async fn setup() -> (TempDir, Store, Wiki, WorkspaceId, ProjectId) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        // Every production wiki the watcher actually clones carries a store
        // reader (`serve.rs` attaches one before the embedder or the
        // watcher) — attached here too so tests exercise the same
        // reader-dependent path `reindex_page` uses to decide whether a
        // reindexed page is eligible for embedding (#929), not the fail-closed
        // no-reader fallback.
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        (tmp, store, wiki, ws, proj)
    }

    /// A purge whose page-file cleanup failed must not be undone by the next
    /// tick (#701).
    ///
    /// The guard #696 added closes the window where a reindex interleaves
    /// between the database delete and the file cleanup. It cannot cover the
    /// case where the file *outlives* the purge: a cleanup failure is a
    /// reported, already-tested outcome, and after one the rows are gone while
    /// the markdown is still on disk. Reverting the tombstone gate fails this.
    #[tokio::test]
    async fn purged_session_page_is_not_resurrected_by_a_reconcile_tick() {
        let (tmp, store, wiki, ws, proj) = setup().await;
        let sid = ai_memory_core::SessionId::new();
        store
            .writer
            .begin_session(ai_memory_core::NewSession {
                occurred_at: None,
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: ai_memory_core::AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();

        let sessions_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string())
            .join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let abs = sessions_dir.join(format!("{sid}.md"));
        std::fs::write(&abs, "---\ntitle: Session\n---\n\nzimbabwe pineapple\n").unwrap();
        let path = PagePath::new(format!("sessions/{sid}.md")).unwrap();
        let page_id = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        store.writer.end_session(sid, Some(page_id)).await.unwrap();
        assert_eq!(
            store
                .reader
                .search_pages("zimbabwe".into(), 10)
                .await
                .unwrap()
                .len(),
            1,
            "precondition: the session page is indexed",
        );

        // Make the unlink fail the way a read-only mount or a sharing
        // violation does — the markdown itself stays intact and readable.
        #[cfg(unix)]
        let original = std::fs::metadata(&sessions_dir).unwrap().permissions();
        #[cfg(unix)]
        {
            let mut locked = original.clone();
            locked.set_readonly(true);
            std::fs::set_permissions(&sessions_dir, locked).unwrap();
        }
        #[cfg(windows)]
        let _file_lock = {
            use std::os::windows::fs::OpenOptionsExt;

            // Windows checks the file handle's share mode when unlinking;
            // a readonly directory does not prevent deletion there.
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0x0000_0001 | 0x0000_0002) // FILE_SHARE_READ | FILE_SHARE_WRITE
                .open(&abs)
                .unwrap()
        };
        let outcome = wiki
            .purge_session(ws, proj, sid, None, ai_memory_store::Compaction::Skip)
            .await
            .unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&sessions_dir, original).unwrap();

        // Preconditions: this is the reported `files_failed` state.
        assert_eq!(
            outcome.files_failed,
            vec![path.clone()],
            "precondition: the unlink must have failed",
        );
        assert!(abs.exists(), "precondition: the page file survived");
        assert!(
            store
                .reader
                .search_pages("zimbabwe".into(), 10)
                .await
                .unwrap()
                .is_empty(),
            "precondition: the purge made the page unsearchable",
        );

        let stats = reconcile(&wiki, &mut MissingStreaks::new()).await.unwrap();
        assert_eq!(
            stats.skipped_orphans, 0,
            "a session purge leaves the project row, so the orphan guard passes",
        );
        assert_eq!(
            stats.skipped_purged_sessions, 1,
            "the leftover page is skipped, and the pass says so",
        );

        let back = store
            .reader
            .search_pages("zimbabwe".into(), 10)
            .await
            .unwrap();
        assert!(
            back.is_empty(),
            "a purged session's page must not be resurrected from a leftover file: {back:?}",
        );
    }

    /// `extract_project_ids` must parse a valid `<ws>/<proj>/<path>` triplet.
    #[test]
    fn extract_project_ids_valid_path() {
        let wiki_root = Path::new("/data/wiki");
        let ws_id = WorkspaceId::new();
        let proj_id = ProjectId::new();
        let event_path =
            std::path::PathBuf::from(format!("/data/wiki/{}/{}/decisions/foo.md", ws_id, proj_id));
        let result = extract_project_ids(wiki_root, &event_path);
        assert!(
            result.is_some(),
            "must extract IDs from valid namespaced path"
        );
        let (ws, proj, pp) = result.unwrap();
        assert_eq!(ws, ws_id);
        assert_eq!(proj, proj_id);
        assert_eq!(pp.as_str(), "decisions/foo.md");
    }

    /// `extract_project_ids` must return `None` when the first segment is not a UUID.
    #[test]
    fn extract_project_ids_garbage_first_segment() {
        let wiki_root = Path::new("/data/wiki");
        let event_path = Path::new("/data/wiki/not-a-uuid/some-proj/foo.md");
        assert!(
            extract_project_ids(wiki_root, event_path).is_none(),
            "garbage first segment must return None"
        );
    }

    /// `extract_project_ids` must return `None` for flat (non-namespaced) paths.
    #[test]
    fn extract_project_ids_flat_path_returns_none() {
        let wiki_root = Path::new("/data/wiki");
        let event_path = Path::new("/data/wiki/foo.md");
        assert!(
            extract_project_ids(wiki_root, event_path).is_none(),
            "flat path with no namespace must return None"
        );
    }

    /// `extract_project_ids` must return `None` when the second segment is not a valid UUID.
    #[test]
    fn extract_rejects_garbage_in_project_segment() {
        let wiki_root = Path::new("/tmp/wiki");
        let ws = WorkspaceId::new().to_string();
        let event_path =
            std::path::PathBuf::from(format!("/tmp/wiki/{ws}/not-a-uuid/decisions/foo.md"));
        assert!(
            extract_project_ids(wiki_root, &event_path).is_none(),
            "garbage project segment must return None"
        );
    }

    /// `extract_project_ids` must return `None` when there is no page path
    /// after the two UUID segments (would produce an empty `PagePath`).
    #[test]
    fn extract_rejects_empty_page_path() {
        let wiki_root = Path::new("/tmp/wiki");
        let ws = WorkspaceId::new().to_string();
        let proj = ProjectId::new().to_string();
        // Just the project dir itself with no page path beneath.
        let event_path = std::path::PathBuf::from(format!("/tmp/wiki/{ws}/{proj}"));
        assert!(
            extract_project_ids(wiki_root, &event_path).is_none(),
            "missing page path must return None"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn picks_up_externally_created_file() {
        let (tmp, store, wiki, ws, proj) = setup().await;

        // Create the project directory BEFORE starting the watcher so
        // the inotify backend adds a watch for it immediately. If we
        // created it after, there is a race between the new-dir event
        // and the file-write event that can cause the watcher to miss
        // the file on slower Linux inotify instances.
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();

        let handle = WatcherHandle::start(wiki.clone()).unwrap();
        // FSEvents can report readiness before the recursive watch is fully
        // settled. Give the backend one debounce window before creating the
        // file so this test checks event delivery, not watcher-start races.
        tokio::time::sleep(DEBOUNCE_WINDOW + Duration::from_millis(200)).await;

        // Drop a file inside the per-project directory, bypassing the wiki write API
        // (simulating an external editor).
        let target = proj_dir.join("external.md");
        std::fs::write(&target, "Hello from outside the wiki API.\n").unwrap();

        // Poll for the row to land. Watcher debounces at 300ms; extra
        // margin for slow CI environments.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut hits = Vec::new();
        while std::time::Instant::now() < deadline {
            hits = store
                .reader
                .search_pages("outside".into(), 5)
                .await
                .unwrap();
            if !hits.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!hits.is_empty(), "watcher did not pick up external write");
        assert_eq!(hits[0].path.as_str(), "external.md");
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn directory_event_reindexes_project_markdown() {
        let (tmp, store, wiki, ws, proj) = setup().await;
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            proj_dir.join("external.md"),
            "Directory event should reindex this page.\n",
        )
        .unwrap();

        let event = notify_debouncer_full::DebouncedEvent::new(
            notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                .add_path(proj_dir),
            std::time::Instant::now(),
        );
        handle_event(&wiki, event).await;

        let hits = store
            .reader
            .search_pages("reindex".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "external.md");
    }

    /// The watcher reports what the next auto-commit must stage: removals,
    /// and files the indexer skips; never the wiki's own git directory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_report_their_paths_for_the_next_commit() {
        let (_tmp, _store, wiki, ws, proj) = setup().await;
        let proj_dir = wiki.root().join(ws.to_string()).join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();
        let ledger = proj_dir.join("events.jsonl");
        std::fs::write(&ledger, "{}\n").unwrap();
        let git_log = wiki.root().join(".git/logs/HEAD");
        std::fs::create_dir_all(git_log.parent().unwrap()).unwrap();
        std::fs::write(&git_log, "ref\n").unwrap();
        let nested_git = proj_dir.join(".git/config");
        std::fs::create_dir_all(nested_git.parent().unwrap()).unwrap();
        std::fs::write(&nested_git, "config\n").unwrap();
        let gone = proj_dir.join("gone.md");

        for (kind, path) in [
            (EventKind::Create(notify::event::CreateKind::File), &ledger),
            (EventKind::Modify(notify::event::ModifyKind::Any), &git_log),
            (
                EventKind::Create(notify::event::CreateKind::File),
                &nested_git,
            ),
            (EventKind::Remove(notify::event::RemoveKind::File), &gone),
            (EventKind::Remove(notify::event::RemoveKind::File), &git_log),
            (
                EventKind::Remove(notify::event::RemoveKind::File),
                &nested_git,
            ),
        ] {
            let event = notify_debouncer_full::DebouncedEvent::new(
                notify::Event::new(kind).add_path(path.clone()),
                std::time::Instant::now(),
            );
            handle_event(&wiki, event).await;
        }

        let reported = wiki.git().written_paths();
        let rel = |p: &Path| p.strip_prefix(wiki.root()).unwrap().to_path_buf();
        assert!(reported.contains(&rel(&ledger)), "{reported:?}");
        assert!(reported.contains(&rel(&gone)), "{reported:?}");
        assert!(
            !reported
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == ".git")),
            "{reported:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_picks_up_file_added_while_watcher_offline() {
        let (tmp, store, wiki, ws, proj) = setup().await;

        // Write a file BEFORE starting the watcher — directly in the project dir.
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();
        let target = proj_dir.join("preexisting.md");
        std::fs::write(&target, "I existed first.\n").unwrap();

        let handle = WatcherHandle::start(wiki.clone()).unwrap();
        // Hit reconcile manually instead of waiting 30s.
        reconcile(&wiki, &mut MissingStreaks::new()).await.unwrap();

        let hits = store
            .reader
            .search_pages("existed".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "preexisting.md");
        handle.shutdown().await;
    }

    /// #613: a project directory the store has no row for must be skipped
    /// wholesale, not retried page-by-page on every pass. Regression: the OKF
    /// migration seeded `index.md` into orphan directories, and the watcher
    /// then logged a scope-resolution failure for each such file every 30s,
    /// forever, burying real warnings.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconcile_skips_project_dirs_with_no_store_row() {
        let (tmp, store, wiki, ws, proj) = setup().await;
        let wiki_root = tmp.path().join("wiki");

        // A resolvable project (row created by setup) with a real page.
        let valid_dir = wiki_root.join(ws.to_string()).join(proj.to_string());
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::write(valid_dir.join("kept.md"), "validtoken content\n").unwrap();

        // An orphan directory: a well-formed UUID pair the store knows nothing
        // about, shaped like a migration-seeded shell (an `index.md` plus a
        // stale page). Nothing should index it, and it must not warn per page.
        let orphan = ProjectId::new();
        let orphan_dir = wiki_root.join(ws.to_string()).join(orphan.to_string());
        std::fs::create_dir_all(&orphan_dir).unwrap();
        std::fs::write(orphan_dir.join("index.md"), "seeded shell\n").unwrap();
        std::fs::write(orphan_dir.join("stale.md"), "orphantoken content\n").unwrap();

        let stats = reconcile(&wiki, &mut MissingStreaks::new()).await.unwrap();

        assert_eq!(
            stats.skipped_orphans, 1,
            "the rowless directory must be skipped as an orphan"
        );
        assert!(
            stats.indexed >= 1,
            "the resolvable project's page must still index, got {}",
            stats.indexed
        );

        let kept = store
            .reader
            .search_pages("validtoken".into(), 5)
            .await
            .unwrap();
        assert_eq!(kept.len(), 1, "the valid project's page must be indexed");

        let stranded = store
            .reader
            .search_pages("orphantoken".into(), 5)
            .await
            .unwrap();
        assert!(
            stranded.is_empty(),
            "an orphan directory's page must not be indexed"
        );
    }

    /// The sibling of `reconcile_skips_project_dirs_with_no_store_row` (#613):
    /// a directory event reaches the indexer through `reindex_project_dir`,
    /// which had no orphan guard. Rarer than the 30s pass, same defect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn directory_events_skip_project_dirs_with_no_store_row() {
        let (tmp, store, wiki, ws, _proj) = setup().await;
        let wiki_root = tmp.path().join("wiki");

        let orphan = ProjectId::new();
        let orphan_dir = wiki_root.join(ws.to_string()).join(orphan.to_string());
        std::fs::create_dir_all(&orphan_dir).unwrap();
        std::fs::write(orphan_dir.join("index.md"), "seeded shell\n").unwrap();
        std::fs::write(orphan_dir.join("stale.md"), "eventtoken content\n").unwrap();

        // Exactly what a Create/Modify event on the directory triggers.
        let indexed = reindex_project_dir(&wiki, ws, orphan, orphan_dir).await;
        assert!(
            !indexed,
            "a rowless directory must be skipped before the walk, not walked \
             and failed page by page"
        );

        let stranded = store
            .reader
            .search_pages("eventtoken".into(), 5)
            .await
            .unwrap();
        assert!(
            stranded.is_empty(),
            "a rowless directory must not index through a directory event, got {}",
            stranded.len()
        );
    }

    /// The directory-event orphan guard (#616 added it beside `reconcile`'s)
    /// is the watcher's second caller that runs BEFORE `reindex_page` takes
    /// the mutation lock, so it must stay rows-only for the same reason: a
    /// `_meta.md` written from an unguarded path could land in a directory a
    /// concurrent project move is renaming away. Pages found in the walk are
    /// a different matter — `reindex_page` writes the manifest under the
    /// guard, which is safe.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn directory_events_do_not_write_scope_manifests() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("acme").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "webapp", None)
            .await
            .unwrap();
        // The reader is what lets a manifest be written at all; without it
        // attached this would pass for the wrong reason.
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());

        let ws_dir = tmp.path().join("wiki").join(ws.to_string());
        let proj_dir = ws_dir.join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();

        assert!(
            reindex_project_dir(&wiki, ws, proj, proj_dir.clone()).await,
            "a scope the store knows is not an orphan"
        );

        assert!(
            !ws_dir.join("_meta.md").exists(),
            "the unguarded directory-event pre-check must not write files"
        );
        assert!(!proj_dir.join("_meta.md").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ignores_own_atomic_tempfiles() {
        // Quick unit test: tempfile prefix detection.
        let p = Path::new("/some/dir/.ai-memory-tmp.abc.md");
        assert!(is_tempfile(p));
        let q = Path::new("/some/dir/normal.md");
        assert!(!is_tempfile(q));
    }

    /// `walk_markdown` must not return `log.md` or `bootstrap.md`
    /// (reserved per-project files that must not become wiki pages).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_markdown_skips_reserved_filenames() {
        let (tmp, store, wiki, ws, proj) = setup().await;

        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();

        // Write a legitimate page plus the reserved files (legacy
        // `log.md`, the rotated `log-YYYY-MM.md`, and `bootstrap.md`).
        // Single-word unique tokens so FTS5 (which parses hyphens as
        // operators) can match them.
        std::fs::write(proj_dir.join("real.md"), "real content\n").unwrap();
        std::fs::write(
            proj_dir.join("log.md"),
            "## [2026-06-08T12:34:56Z] session-start | logtoken unique\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.join("log-2026-05.md"),
            "## [2026-05-01T00:00:00Z] user-prompt | rotatedlogtoken unique\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.join("log-summary.md"),
            "ordinary markdown summaries regularlogtoken unique\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.join("bootstrap.md"),
            "bootstrapmanifest boottoken unique\n",
        )
        .unwrap();

        let handle = WatcherHandle::start(wiki.clone()).unwrap();
        // Trigger the reconciliation pass directly.
        reconcile(&wiki, &mut MissingStreaks::new()).await.unwrap();

        // Only `real.md` should land in the index.
        let hits = store
            .reader
            .search_pages("real content".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "only the real page should be indexed");
        assert_eq!(hits[0].path.as_str(), "real.md");

        // Neither reserved file should be searchable.
        let log_hits = store
            .reader
            .search_pages("logtoken".into(), 5)
            .await
            .unwrap();
        assert!(log_hits.is_empty(), "log.md must not be indexed");

        let rotated_hits = store
            .reader
            .search_pages("rotatedlogtoken".into(), 5)
            .await
            .unwrap();
        assert!(
            rotated_hits.is_empty(),
            "log-YYYY-MM.md (rotated) must not be indexed"
        );

        let regular_hits = store
            .reader
            .search_pages("regularlogtoken".into(), 5)
            .await
            .unwrap();
        assert_eq!(
            regular_hits.len(),
            1,
            "ordinary log-looking markdown must still be indexed"
        );
        assert_eq!(regular_hits[0].path.as_str(), "log-summary.md");

        let boot_hits = store
            .reader
            .search_pages("boottoken".into(), 5)
            .await
            .unwrap();
        assert!(boot_hits.is_empty(), "bootstrap.md must not be indexed");

        handle.shutdown().await;
        drop(store);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_markdown_skips_pending_auto_improve_sidecars() {
        let tmp = TempDir::new().unwrap();
        let proj_root = tmp.path().join("proj");
        std::fs::create_dir_all(proj_root.join("_pending/auto-improve")).unwrap();
        std::fs::write(proj_root.join("real.md"), "real content\n").unwrap();
        std::fs::write(
            proj_root.join("_pending/auto-improve/proposal.md"),
            "pending sidecar token\n",
        )
        .unwrap();

        let found = walk_markdown(&proj_root).unwrap();
        let names: Vec<_> = found.iter().map(|p| p.as_str().to_string()).collect();
        assert_eq!(names, vec!["real.md".to_string()]);
    }

    /// A page that *collides* with a reserved ledger name (`log.md`) but
    /// carries YAML frontmatter is a real page and MUST be indexed — not
    /// silently dropped. Regression for a prod data anomaly (a page lived at
    /// `log.md`) that a filename-only skip would lose on every reindex. The
    /// `_meta.md` manifest, by contrast, must NEVER be indexed even though it
    /// also has frontmatter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_page_by_content_not_filename() {
        let (tmp, store, wiki, ws, proj) = setup().await;
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();

        // A genuine page that happens to live at `log.md` (has frontmatter).
        std::fs::write(
            proj_dir.join("log.md"),
            "---\ntitle: Collides With Ledger\n---\nframmaticpage uniquetoken\n",
        )
        .unwrap();
        // The self-describing manifest — never a page, even with frontmatter.
        std::fs::write(
            proj_dir.join("_meta.md"),
            "---\nworkspace: default\nproject: scratch\n---\nmanifesttoken here\n",
        )
        .unwrap();
        // A raw ledger (no frontmatter) — still skipped.
        std::fs::write(
            proj_dir.join("log-2026-06.md"),
            "## [t] evt | x\nrawledgertoken\n",
        )
        .unwrap();
        // An OKF-conformed ledger: the migration stamps frontmatter on
        // every .md, ledgers included. Still a ledger, still skipped.
        std::fs::write(
            proj_dir.join("log-2026-07.md"),
            "---\ntype: Note\ngenerated:\n  by: process:ai-memory/2.0.0\n---\n\
             ## [t] evt | x\nstampedledgertoken\n",
        )
        .unwrap();

        let handle = WatcherHandle::start(wiki.clone()).unwrap();
        reconcile(&wiki, &mut MissingStreaks::new()).await.unwrap();

        let page_hits = store
            .reader
            .search_pages("uniquetoken".into(), 5)
            .await
            .unwrap();
        assert_eq!(
            page_hits.len(),
            1,
            "frontmatter page named log.md must be indexed"
        );
        assert_eq!(page_hits[0].path.as_str(), "log.md");

        let meta_hits = store
            .reader
            .search_pages("manifesttoken".into(), 5)
            .await
            .unwrap();
        assert!(
            meta_hits.is_empty(),
            "_meta.md manifest must not be indexed"
        );

        let ledger_hits = store
            .reader
            .search_pages("rawledgertoken".into(), 5)
            .await
            .unwrap();
        assert!(
            ledger_hits.is_empty(),
            "raw ledger (no frontmatter) must not be indexed"
        );

        // Regression: before this check looked past the frontmatter fence,
        // an OKF-migrated ledger was indexed as a page. Every hook
        // `append_event` then superseded it, writing the whole (ever
        // growing) ledger body as a new `pages` row — a store that grew
        // into the gigabytes within days.
        let stamped_hits = store
            .reader
            .search_pages("stampedledgertoken".into(), 5)
            .await
            .unwrap();
        assert!(
            stamped_hits.is_empty(),
            "OKF-conformed ledger (frontmatter + log entries) must not be indexed"
        );

        handle.shutdown().await;
        drop(store);
    }

    /// Defence: an attacker who can write to wiki/ shouldn't be able
    /// to make the watcher index arbitrary files via symlinks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn walk_markdown_skips_symlinks() {
        let tmp = TempDir::new().unwrap();
        let proj_root = tmp.path().join("proj");
        std::fs::create_dir_all(&proj_root).unwrap();

        // A real file (should be picked up).
        std::fs::write(proj_root.join("real.md"), "real content\n").unwrap();

        // A "secret" file outside the project root.
        let secret = tmp.path().join("secret.md");
        std::fs::write(&secret, "this is sensitive\n").unwrap();

        // Plant a symlink inside proj/ pointing at the outside file.
        if !create_test_symlink_file(&secret, &proj_root.join("symlinked.md")) {
            return;
        }

        let found = walk_markdown(&proj_root).unwrap();
        let names: Vec<_> = found.iter().map(|p| p.as_str().to_string()).collect();
        assert!(names.contains(&"real.md".to_string()), "real file present");
        assert!(
            !names.contains(&"symlinked.md".to_string()),
            "symlink to outside file must be skipped; got: {names:?}"
        );
    }

    /// Direct notify events must use the same symlink guard as full-tree walks;
    /// otherwise a symlinked markdown file can be opened before reconciliation
    /// gets a chance to skip it.
    #[cfg(any(unix, windows))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_file_event_skips_symlink() {
        let (tmp, store, wiki, ws, proj) = setup().await;
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();

        let secret = tmp.path().join("outside-secret.md");
        std::fs::write(&secret, "directsymlinksecret should not index\n").unwrap();

        let symlink = proj_dir.join("symlinked.md");
        if !create_test_symlink_file(&secret, &symlink) {
            return;
        }

        let event = notify_debouncer_full::DebouncedEvent::new(
            notify::Event::new(EventKind::Create(notify::event::CreateKind::File))
                .add_path(symlink),
            std::time::Instant::now(),
        );
        handle_event(&wiki, event).await;

        let hits = store
            .reader
            .search_pages("directsymlinksecret".into(), 5)
            .await
            .unwrap();
        assert!(hits.is_empty(), "direct symlink event must not be indexed");
    }

    // --- #929: watcher-driven rewrites get embedded ---

    /// A page rewritten through the watcher's `reindex_page` path (an
    /// external editor overwriting an already-imported file) must get its
    /// new version embedded the same way a brand-new file does — without a
    /// manual `ai-memory embed`. Before the fix, only `Wiki::write_page`
    /// (the API path) embedded; `reindex_page_locked` upserted the new
    /// version and stopped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_embeds_a_rewritten_page_without_manual_embed() {
        let (tmp, store, wiki, ws, proj) = setup().await;
        let embedder: Arc<dyn ai_memory_llm::Embedder> =
            Arc::new(ai_memory_llm::SyntheticEmbedder::new(32));
        let wiki = wiki.with_embedder(embedder);

        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();
        let target = proj_dir.join("rewrite.md");
        std::fs::write(&target, "alpha bravo original\n").unwrap();

        let path = PagePath::new("rewrite.md").unwrap();
        let id1 = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        let embedded = store
            .reader
            .embedded_page_ids(ws, proj, "synthetic".into(), "bag-of-words-v1".into(), 32)
            .await
            .unwrap();
        assert!(
            embedded.contains(&id1),
            "a new file indexed by the watcher must be embedded"
        );

        // Rewrite: same path, new body. `upsert_page` supersedes (mints a
        // new id) rather than short-circuiting, because the body changed.
        std::fs::write(&target, "charlie delta rewritten\n").unwrap();
        let id2 = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        assert_ne!(id1, id2, "a body change must supersede, not short-circuit");

        let embedded = store
            .reader
            .embedded_page_ids(ws, proj, "synthetic".into(), "bag-of-words-v1".into(), 32)
            .await
            .unwrap();
        assert!(
            embedded.contains(&id2),
            "the rewritten version must be embedded without a manual `ai-memory embed`"
        );
    }

    /// A no-op reindex (unchanged content, which is what every 30s
    /// reconcile pass does for a stable tree) must NOT re-embed. Otherwise
    /// every configured embedder would pay for the whole wiki's embedding
    /// cost every `RECONCILE_INTERVAL`, forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_does_not_reembed_an_unchanged_page() {
        let (tmp, _store, wiki, ws, proj) = setup().await;
        let embedder: Arc<dyn ai_memory_llm::Embedder> =
            Arc::new(ai_memory_llm::SyntheticEmbedder::new(32));
        let wiki = wiki.with_embedder(embedder);

        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();
        let target = proj_dir.join("stable.md");
        std::fs::write(&target, "stabletoken unchanged\n").unwrap();

        let path = PagePath::new("stable.md").unwrap();
        let id1 = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        let id2 = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        assert_eq!(
            id1, id2,
            "unchanged content must short-circuit to the same id"
        );
    }

    // --- #929: reconcile-delete safety net (opt-in tombstone-on-missing-file) ---

    fn proj_dir(tmp: &TempDir, ws: WorkspaceId, proj: ProjectId) -> std::path::PathBuf {
        tmp.path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string())
    }

    /// `setup()` with the reconcile-delete safety net opted in. Kept separate
    /// from `setup()` so every other watcher test keeps exercising the
    /// default-off path unchanged.
    async fn setup_reconcile_delete() -> (TempDir, Store, Wiki, WorkspaceId, ProjectId) {
        let (tmp, store, wiki, ws, proj) = setup().await;
        (
            tmp,
            store,
            wiki.with_reconcile_tombstones_deleted_pages(true),
            ws,
            proj,
        )
    }

    /// Write `body` at `path` under the project directory and index it,
    /// returning the resulting `PageId`.
    async fn write_and_index(
        wiki: &Wiki,
        tmp: &TempDir,
        ws: WorkspaceId,
        proj: ProjectId,
        path: &str,
        body: &str,
    ) -> PageId {
        let dir = proj_dir(tmp, ws, proj);
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, body).unwrap();
        wiki.reindex_page(ws, proj, PagePath::new(path).unwrap())
            .await
            .unwrap()
    }

    /// With the feature at its default (off), a page whose file disappears is
    /// never tombstoned, no matter how many reconcile passes run — reconcile's
    /// behavior is unchanged from before this feature existed (#929).
    #[tokio::test]
    async fn reconcile_delete_disabled_by_default_never_tombstones_a_missing_page() {
        let (tmp, store, wiki, ws, proj) = setup().await;
        let path = PagePath::new("gone.md").unwrap();
        let id = write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "here for now").await;
        std::fs::remove_file(proj_dir(&tmp, ws, proj).join(path.as_str())).unwrap();

        let mut streaks = MissingStreaks::new();
        for _ in 0..5 {
            let stats = reconcile(&wiki, &mut streaks).await.unwrap();
            assert_eq!(stats.tombstoned_missing, 0);
        }
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(id),
            "deletions still require `ai-memory delete-page` when the feature is off",
        );
    }

    /// The core happy path, and the two-pass bite-check: the first pass a
    /// page's file is found missing must NOT tombstone it (streak = 1); only
    /// the second CONSECUTIVE pass does (design item #5).
    #[tokio::test]
    async fn reconcile_delete_tombstones_only_after_two_consecutive_missing_passes() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let path = PagePath::new("gone.md").unwrap();
        let id = write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "here for now").await;
        // A control page that stays present the whole test, so the scope's
        // walk is never literally empty (S3, #929 review: an empty walk with
        // a non-empty snapshot is its own, unconditional breaker trip).
        write_and_index(&wiki, &tmp, ws, proj, "control.md", "always here").await;

        let mut streaks = MissingStreaks::new();
        let baseline = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(baseline.tombstoned_missing, 0);

        std::fs::remove_file(proj_dir(&tmp, ws, proj).join(path.as_str())).unwrap();

        let first_miss = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(
            first_miss.tombstoned_missing, 0,
            "one missing pass must not be enough"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(id),
            "still latest after only one missing pass",
        );

        let second_miss = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(
            second_miss.tombstoned_missing, 1,
            "two consecutive missing passes must tombstone"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            None,
        );
    }

    /// `could_have_been_walked` (design item #1, plus the B2 sessions
    /// exclusion) is the path-only filter applied to the DB-side candidate
    /// snapshot, mirroring the walk's own skip rules. `_pending/` sidecars
    /// can never even reach it as a candidate — `reindex_page` itself
    /// refuses to index one (see `refusing to index pending proposal
    /// sidecar` in `wiki.rs`) — so this unit test is the only way to
    /// exercise that branch directly.
    #[test]
    fn could_have_been_walked_excludes_every_reserved_shape() {
        let session_path = format!("sessions/{}.md", ai_memory_core::SessionId::new());
        for excluded in [
            "_pending",
            "_pending/draft.md",
            "_pending/review-bot/notes.md",
            "bootstrap.md",
            "_meta.md",
            "notes/_meta.md",
            "log.md",
            "log-2024-01.md",
            session_path.as_str(),
        ] {
            assert!(
                !could_have_been_walked(&PagePath::new(excluded).unwrap()),
                "{excluded} must never be a reconcile-delete candidate",
            );
        }
        // "sessions/abc.md" does NOT parse as a `SessionId` (not a UUID), so
        // it is an ordinary page shape, not a session summary — proving the
        // exclusion is keyed on `session_id_for_page`, not merely the
        // `sessions/` prefix.
        for included in ["notes/plan.md", "sessions/abc.md", "log-review.md"] {
            assert!(
                could_have_been_walked(&PagePath::new(included).unwrap()),
                "{included} is an ordinary page and must remain a candidate",
            );
        }
    }

    /// Reserved/indexed-but-unwalked paths (design item #1) are never
    /// candidates, even across many consecutive passes with their files gone:
    /// `bootstrap.md` and a legacy ledger row (`log.md`-shaped). Unlike the
    /// walk, `reindex_page` does not itself refuse these two paths, so they
    /// can be indexed rows exactly the way an older store or a direct API
    /// write could leave them.
    #[tokio::test]
    async fn reconcile_delete_reserved_paths_are_never_candidates() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let paths = ["bootstrap.md", "log.md"];
        let mut ids = Vec::new();
        for p in paths {
            let id = write_and_index(&wiki, &tmp, ws, proj, p, "reserved-shaped content").await;
            ids.push((PagePath::new(p).unwrap(), id));
            std::fs::remove_file(proj_dir(&tmp, ws, proj).join(p)).unwrap();
        }

        let mut streaks = MissingStreaks::new();
        for _ in 0..5 {
            let stats = reconcile(&wiki, &mut streaks).await.unwrap();
            assert_eq!(stats.tombstoned_missing, 0);
        }
        for (path, id) in ids {
            assert_eq!(
                store
                    .reader
                    .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                    .await
                    .unwrap(),
                Some(id),
                "{path} must never be treated as deletion evidence",
            );
        }
    }

    /// An atomic-save pattern (delete-then-recreate) must survive: if the
    /// file is back by the very next pass, the streak clears and nothing is
    /// ever tombstoned — the two-pass requirement plus this reappearance
    /// check is what makes that safe beyond the existing debounce window.
    #[tokio::test]
    async fn reconcile_delete_atomic_save_pattern_survives() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let path = PagePath::new("flaky.md").unwrap();
        let id1 = write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "v1").await;
        // Control page (S3, #929 review): without it, the scope's walk goes
        // fully empty the moment `flaky.md` is missing, and the survival this
        // test checks for would be explained by the empty-walk breaker
        // tripping rather than by the streak/reappearance mechanism it's
        // actually meant to exercise.
        write_and_index(&wiki, &tmp, ws, proj, "control.md", "always here").await;

        let mut streaks = MissingStreaks::new();
        reconcile(&wiki, &mut streaks).await.unwrap();

        // Pass N: file momentarily missing (streak = 1).
        std::fs::remove_file(proj_dir(&tmp, ws, proj).join(path.as_str())).unwrap();
        let miss = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(miss.tombstoned_missing, 0);

        // Recreated before the next pass — the "atomic save" completing.
        let id2 = write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "v2").await;
        assert_ne!(id1, id2, "a content change mints a new version");
        let recovered = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(recovered.tombstoned_missing, 0);

        // One more pass with the file still present must not retroactively
        // tombstone anything either — the streak was cleared, not merely paused.
        let stable = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(stable.tombstoned_missing, 0);
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(id2),
        );
    }

    /// S4 (#929 review): the test above recreates the file with DIFFERENT
    /// content, so it never actually proves the streak was CLEARED by the
    /// SAME page reappearing — a new `PageId` alone (via the `streak.0 !=
    /// *id` restart in `reconcile_missing_pages`) would make that test pass
    /// even if the "found in `walked`" clearing loop were deleted entirely.
    /// This test forces the identical-content path: missing (streak = 1) ->
    /// present again with the SAME body (so `upsert_page`'s content
    /// short-circuit returns the SAME id, exercising the presence-clears-
    /// streak branch specifically) -> missing again (must restart at
    /// streak = 1, not continue to 2) -> not tombstoned; a further pass
    /// still missing then reaches streak = 2 and IS tombstoned — proving two
    /// FRESH consecutive misses were required, not two misses ever observed.
    #[tokio::test]
    async fn reconcile_delete_streak_clears_on_identical_content_reappearance() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let path = PagePath::new("flaky-identical.md").unwrap();
        let id = write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "same body always").await;
        // Control page (S3, #929 review — see the other test's comment).
        write_and_index(&wiki, &tmp, ws, proj, "control.md", "always here").await;

        let mut streaks = MissingStreaks::new();
        reconcile(&wiki, &mut streaks).await.unwrap();

        // Miss #1.
        std::fs::remove_file(proj_dir(&tmp, ws, proj).join(path.as_str())).unwrap();
        let first_miss = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(first_miss.tombstoned_missing, 0, "streak = 1, not enough");

        // Reappears with IDENTICAL content: `upsert_page`'s sha256
        // short-circuit returns the SAME id, so this exercises "found in
        // `walked`" clearing a streak, not the `streak.0 != *id` restart.
        let same_id =
            write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "same body always").await;
        assert_eq!(
            same_id, id,
            "precondition: identical content must not mint a new id"
        );
        let recovered = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(recovered.tombstoned_missing, 0);

        // Missing again: this must be a FRESH streak (= 1), not a
        // continuation (which would already be 2 and tombstone here).
        std::fs::remove_file(proj_dir(&tmp, ws, proj).join(path.as_str())).unwrap();
        let second_streak_first_miss = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(
            second_streak_first_miss.tombstoned_missing, 0,
            "the streak must have restarted at 1, not continued to 2"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(id),
            "still latest after only one fresh missing pass"
        );

        // Still missing, second FRESH consecutive pass: now it tombstones.
        let second_streak_second_miss = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(second_streak_second_miss.tombstoned_missing, 1);
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            None,
        );
    }

    /// Circuit breaker (design item #4): when most of a scope's candidates
    /// look missing in one pass, nothing is tombstoned and a `warn` fires —
    /// that shape is far more likely a walk/mount problem than genuine mass
    /// deletion. Bite-check: with the breaker removed, this test fails
    /// (verified manually while writing it, then restored).
    #[tokio::test]
    async fn reconcile_delete_circuit_breaker_blocks_when_most_candidates_vanish() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let mut ids = Vec::new();
        for i in 0..10 {
            let p = format!("bulk/{i}.md");
            let id = write_and_index(&wiki, &tmp, ws, proj, &p, "bulk content").await;
            ids.push((PagePath::new(p).unwrap(), id));
        }

        let mut streaks = MissingStreaks::new();
        reconcile(&wiki, &mut streaks).await.unwrap();

        // 60% vanish in one pass.
        for (path, _) in ids.iter().take(6) {
            std::fs::remove_file(proj_dir(&tmp, ws, proj).join(path.as_str())).unwrap();
        }
        for pass in 0..3 {
            let stats = reconcile(&wiki, &mut streaks).await.unwrap();
            assert_eq!(
                stats.tombstoned_missing, 0,
                "pass {pass}: the breaker must block every one of the 6 missing pages"
            );
            assert_eq!(stats.circuit_broken_scopes, 1, "pass {pass}");
        }
        for (path, id) in &ids {
            assert_eq!(
                store
                    .reader
                    .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                    .await
                    .unwrap(),
                Some(*id),
                "{path} must survive a tripped breaker",
            );
        }
    }

    /// Control for the breaker test: a small fraction missing (well under
    /// `max(3, 50%)`) proceeds normally and tombstones after two consecutive
    /// passes, exactly like the non-bulk case.
    #[tokio::test]
    async fn reconcile_delete_circuit_breaker_does_not_block_a_small_fraction() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let mut ids = Vec::new();
        for i in 0..10 {
            let p = format!("bulk/{i}.md");
            let id = write_and_index(&wiki, &tmp, ws, proj, &p, "bulk content").await;
            ids.push((PagePath::new(p).unwrap(), id));
        }

        let mut streaks = MissingStreaks::new();
        reconcile(&wiki, &mut streaks).await.unwrap();

        // 10% vanish: well under the breaker threshold (max(3, 50%) = 5 of 10).
        let (missing_path, _missing_id) = ids[0].clone();
        std::fs::remove_file(proj_dir(&tmp, ws, proj).join(missing_path.as_str())).unwrap();

        let first = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(first.circuit_broken_scopes, 0);
        assert_eq!(first.tombstoned_missing, 0, "first missing pass");

        let second = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(second.circuit_broken_scopes, 0);
        assert_eq!(
            second.tombstoned_missing, 1,
            "second consecutive missing pass"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, missing_path.as_str().to_string())
                .await
                .unwrap(),
            None,
        );
        for (path, id) in ids.iter().skip(1) {
            assert_eq!(
                store
                    .reader
                    .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                    .await
                    .unwrap(),
                Some(*id),
                "{path} was never missing",
            );
        }
    }

    /// S3 (#929 review): an empty (but NOT partial/`NotFound`) walk is always
    /// suspect, even for a scope too small to ever trip the `max(3, 50%)`
    /// math — 2 candidates, both missing, is only 100% but the absolute
    /// floor of 3 never trips. This is the "directory exists but came back
    /// empty" mount-failure signature (an unmounted volume, a bind-mount
    /// briefly resolving to an empty stub): the walk succeeds (no
    /// `NotFound`, so `walk_partial` is false) but finds nothing at all.
    #[tokio::test]
    async fn reconcile_delete_circuit_breaker_trips_on_empty_walk_even_for_a_small_scope() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let mut ids = Vec::new();
        for i in 0..2 {
            let p = format!("small/{i}.md");
            let id = write_and_index(&wiki, &tmp, ws, proj, &p, "small scope content").await;
            ids.push((PagePath::new(p).unwrap(), id));
        }

        let mut streaks = MissingStreaks::new();
        reconcile(&wiki, &mut streaks).await.unwrap();

        // Both files vanish, but the directory itself stays put — an
        // ordinary (non-partial) walk that simply finds nothing.
        for (path, _) in &ids {
            std::fs::remove_file(proj_dir(&tmp, ws, proj).join(path.as_str())).unwrap();
        }
        for pass in 0..3 {
            let stats = reconcile(&wiki, &mut streaks).await.unwrap();
            assert_eq!(
                stats.tombstoned_missing, 0,
                "pass {pass}: an empty walk must never be treated as evidence, however small \
                 the scope"
            );
            assert_eq!(stats.circuit_broken_scopes, 1, "pass {pass}");
        }
        for (path, id) in &ids {
            assert_eq!(
                store
                    .reader
                    .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                    .await
                    .unwrap(),
                Some(*id),
                "{path} must survive an empty-walk breaker trip",
            );
        }
    }

    /// Partial walk (design item #3): the whole project directory vanishing
    /// mid-pass (a git checkout, a bind-mount hiccup) must not be treated as
    /// every page under it having been deleted. Restoring the directory lets
    /// the very next pass proceed normally — the reappear-after-restore case.
    #[tokio::test]
    async fn reconcile_delete_partial_walk_skips_a_scope_whose_project_directory_vanished() {
        let (tmp, store, wiki, ws, proj) = setup_reconcile_delete().await;
        let path = PagePath::new("survivor.md").unwrap();
        let id = write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "still around").await;

        let dir = proj_dir(&tmp, ws, proj);
        let mut streaks = MissingStreaks::new();
        reconcile(&wiki, &mut streaks).await.unwrap();

        // Simulate the whole project directory vanishing mid-walk (walk_partial
        // becomes true because `walk_markdown_partial`'s own root read fails).
        std::fs::remove_dir_all(&dir).unwrap();
        for pass in 0..3 {
            let stats = reconcile(&wiki, &mut streaks).await.unwrap();
            assert_eq!(
                stats.tombstoned_missing, 0,
                "pass {pass}: a partial walk must never produce deletion evidence"
            );
        }
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(id),
            "the page must survive many passes of a vanished project directory",
        );

        // Restore: the directory and file come back, and reconcile resumes
        // ordinary behavior instead of carrying over any partial-pass state.
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(path.as_str()), "still around").unwrap();
        let restored = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(restored.tombstoned_missing, 0);
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(id),
        );
    }

    /// S1 (#929 review): a partial walk's ABSENCE evidence is suspect, but
    /// its PRESENCE evidence is not — a page the walk actually found, even
    /// during a partial pass, must still clear an earlier streak. Otherwise
    /// a page that legitimately reappeared could be tombstoned later purely
    /// because an unrelated directory's `NotFound` earlier suppressed the
    /// clearing that presence should have triggered.
    ///
    /// Calls `reconcile_missing_pages` directly rather than orchestrating a
    /// real partial walk through the filesystem: a subdirectory deleted
    /// *before* `reconcile()` runs is never even listed by `read_dir`, so it
    /// produces no `NotFound` at all (that's only reachable via a genuine
    /// race — the directory vanishing *during* the walk, mid-traversal,
    /// which a single-threaded test cannot deterministically force). Driving
    /// the function directly exercises the exact contract instead.
    #[tokio::test]
    async fn reconcile_delete_partial_walk_still_clears_streaks_for_pages_it_did_find() {
        let (tmp, _store, wiki, ws, proj) = setup_reconcile_delete().await;
        let path = PagePath::new("flaky.md").unwrap();
        let id = write_and_index(&wiki, &tmp, ws, proj, path.as_str(), "here").await;

        // Seed a pre-existing streak of 1, as if an earlier complete pass had
        // already observed this exact page missing once.
        let mut streaks = MissingStreaks::new();
        streaks.insert((ws, proj, path.clone()), (id, 1));

        let snapshot = vec![(id, path.clone())];
        let mut walked: HashSet<PagePath> = HashSet::new();
        walked.insert(path.clone());
        let mut stats = ReconcileStats::default();

        // This pass is partial (some unrelated directory vanished elsewhere
        // in the scope), but it DID find this exact page present.
        reconcile_missing_pages(
            &wiki,
            ws,
            proj,
            &snapshot,
            &walked,
            true,
            &mut streaks,
            &mut stats,
        )
        .await;

        assert!(
            !streaks.contains_key(&(ws, proj, path.clone())),
            "presence evidence from a partial walk must still clear the streak"
        );
        assert_eq!(stats.tombstoned_missing, 0);

        // Bite-check performed manually while writing this test: hoisting
        // the clearing loop back inside the `if walk_partial { return; }`
        // branch (the pre-S1 shape) makes this assertion fail, since the
        // seeded streak survives untouched.
    }

    /// Cross-project isolation (the same adversarial shape #929's rejected
    /// prior attempt was tested against): two projects with a page at the
    /// same relative path, only one of which loses its file, must not let
    /// one project's missing-page streak or tombstone affect the other's.
    #[tokio::test]
    async fn reconcile_delete_cross_project_isolation() {
        let (tmp, store, wiki, ws, proj_a) = setup_reconcile_delete().await;
        let proj_b = store
            .writer
            .get_or_create_project(ws, "scratch-b", None)
            .await
            .unwrap();
        let path = PagePath::new("shared/name.md").unwrap();
        let _id_a = write_and_index(&wiki, &tmp, ws, proj_a, path.as_str(), "project a").await;
        let id_b = write_and_index(&wiki, &tmp, ws, proj_b, path.as_str(), "project b").await;
        // Control page in proj_a (S3, #929 review): keeps that scope's walk
        // from being literally empty once `shared/name.md` goes missing,
        // which would otherwise trip the unconditional empty-walk breaker.
        write_and_index(&wiki, &tmp, ws, proj_a, "control.md", "always here").await;

        let mut streaks = MissingStreaks::new();
        reconcile(&wiki, &mut streaks).await.unwrap();
        std::fs::remove_file(proj_dir(&tmp, ws, proj_a).join(path.as_str())).unwrap();

        reconcile(&wiki, &mut streaks).await.unwrap();
        let final_stats = reconcile(&wiki, &mut streaks).await.unwrap();
        assert_eq!(final_stats.tombstoned_missing, 1);

        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj_a, path.as_str().to_string())
                .await
                .unwrap(),
            None,
            "project A's page was tombstoned",
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj_b, path.as_str().to_string())
                .await
                .unwrap(),
            Some(id_b),
            "project B's identically-pathed page must survive untouched",
        );
    }

    /// B2 (#929 review): a same-workspace `move-session` re-home can leave a
    /// session page's DB row live in the destination project with no file
    /// there. Reproduced end to end through the real bug, not a synthetic
    /// row edit: the session's own record already says it lives in
    /// `proj_b` (so `Wiki::move_session_page(from=(ws,proj_b),
    /// to=(ws,proj_b), ...)` sees `from == to` and skips the file step
    /// entirely — see its doc comment), while its `sessions/<id>.md` page
    /// is a straggler still sitting in `proj_a`. `ops::move_session`'s
    /// page re-stamp is keyed purely on path + "not already in the target
    /// scope", not on `from`, so it sweeps that straggler page into
    /// `proj_b` regardless — leaving a live, correct row at `proj_b` with
    /// no file there, ever. This is a separate, pre-existing bug in
    /// `move-session`'s file relocation (not fixed here); this test only
    /// proves the reconcile-delete safety net's `sessions/*.md` exclusion
    /// keeps it from destroying that page.
    #[tokio::test]
    async fn reconcile_delete_never_tombstones_a_session_page_orphaned_by_move_session() {
        let (tmp, store, wiki, ws, proj_a) = setup_reconcile_delete().await;
        let proj_b = store
            .writer
            .get_or_create_project(ws, "scratch-b", None)
            .await
            .unwrap();
        let sid = ai_memory_core::SessionId::new();
        // The session's own record already lives in proj_b.
        store
            .writer
            .begin_session(ai_memory_core::NewSession {
                occurred_at: None,
                id: sid,
                workspace_id: ws,
                project_id: proj_b,
                agent_kind: ai_memory_core::AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        // Its page is a straggler, still in proj_a.
        let page_path = format!("sessions/{sid}.md");
        let page_id = write_and_index(&wiki, &tmp, ws, proj_a, &page_path, "session summary").await;
        // A control page that actually lives in proj_b, so proj_b's on-disk
        // directory exists and its walk is complete (not partial) — without
        // this, an absent proj_b directory would make the walk itself hit
        // `NotFound` and get skipped by the PARTIAL-walk guard (design item
        // #3) instead of by the sessions exclusion this test means to
        // isolate. Confirmed by disabling the sessions exclusion locally
        // while writing this test: without a proj_b control page the test
        // still passed for the wrong reason (partial walk), and only
        // failed as expected once this control page made the walk complete.
        write_and_index(&wiki, &tmp, ws, proj_b, "control.md", "always here").await;

        // The from==to re-home: the wiki sees no scope change and skips the
        // file step, but the store still sweeps the straggler page into
        // proj_b (see `ops::move_session`'s `rehome` branch).
        let outcome = wiki
            .move_session_page(
                sid,
                (ws, proj_b),
                (ws, proj_b),
                ai_memory_store::PagesMode::Move,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.summary.page_versions_moved, 1,
            "precondition: the page re-stamp must have actually run"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj_b, page_path.as_str().to_string())
                .await
                .unwrap(),
            Some(page_id),
            "precondition: the page row now lives in proj_b",
        );
        assert!(
            !proj_dir(&tmp, ws, proj_b).join(&page_path).exists(),
            "precondition: proj_b has no file for it — the pre-existing move-session bug",
        );

        let mut streaks = MissingStreaks::new();
        for pass in 0..3 {
            let stats = reconcile(&wiki, &mut streaks).await.unwrap();
            assert_eq!(
                stats.tombstoned_missing, 0,
                "pass {pass}: a session page must never be reconcile-delete candidate"
            );
        }
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj_b, page_path.as_str().to_string())
                .await
                .unwrap(),
            Some(page_id),
            "the orphaned-by-move-session page must survive untouched",
        );
    }
}
