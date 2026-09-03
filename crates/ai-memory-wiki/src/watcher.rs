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
//! The watcher never *writes* to disk — that loop would be unbounded.
//! External writes drive store updates; internal writes drive disk +
//! store updates via [`Wiki::write_page`].

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use ai_memory_core::{PagePath, ProjectId, WorkspaceId};
use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer_opt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

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
                match reconcile(&wiki).await {
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

async fn handle_event(wiki: &Wiki, event: notify_debouncer_full::DebouncedEvent) {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Other
    ) {
        return;
    }
    for raw_path in &event.paths {
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
        if !ft.is_file() {
            continue;
        }
        if !is_markdown(raw_path) {
            continue;
        }
        if is_tempfile(raw_path) {
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
        match wiki.reindex_page(ws, proj, page_path.clone()).await {
            Ok(_) => debug!(path = %page_path, "reindexed via watcher"),
            Err(e) => warn!(path = %page_path, error = %e, "watcher reindex failed"),
        }
    }
}

async fn reindex_project_dir(
    wiki: &Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
    proj_root: std::path::PathBuf,
) {
    let pages = match tokio::task::spawn_blocking(move || walk_markdown(&proj_root)).await {
        Ok(Ok(pages)) => pages,
        Ok(Err(e)) => {
            warn!(error = %e, "watcher directory walk failed");
            return;
        }
        Err(e) => {
            warn!(error = %e, "watcher directory walk task failed");
            return;
        }
    };

    for path in pages {
        match wiki.reindex_page(ws, proj, path.clone()).await {
            Ok(_) => debug!(path = %path, "reindexed via watcher directory event"),
            Err(e) => warn!(path = %path, error = %e, "watcher directory reindex failed"),
        }
    }
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
}

async fn reconcile(wiki: &Wiki) -> WikiResult<ReconcileStats> {
    let root = wiki.root().to_path_buf();
    // Walk all per-project subdirectories: <ws_uuid>/<proj_uuid>/
    let project_dirs = tokio::task::spawn_blocking(move || walk_project_dirs(&root))
        .await
        .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;

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
        if let Err(e) = wiki.ensure_project_workspace(ws, proj).await {
            debug!(
                workspace = %ws,
                project = %proj,
                error = %e,
                "skipping reconcile for a project directory with no store row",
            );
            stats.skipped_orphans += 1;
            continue;
        }
        let pages = tokio::task::spawn_blocking(move || walk_markdown(&proj_root))
            .await
            .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;
        for path in pages {
            if let Err(e) = wiki.reindex_page(ws, proj, path.clone()).await {
                warn!(path = %path, error = %e, "reconcile reindex failed");
            } else {
                stats.indexed += 1;
            }
        }
    }
    info!(
        indexed = stats.indexed,
        skipped_orphans = stats.skipped_orphans,
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
    let page_str = page_rel.to_string_lossy().replace('\\', "/");
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
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
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
    Ok(out)
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

/// `log.md` / `log-YYYY-MM.md` are the raw per-project event ledger the hooks
/// append to (see `ai-memory-hooks::log::log_filename_for`): `## [ts] ...`
/// entries, never YAML frontmatter.
fn is_log_ledger_filename(page_path: &PagePath) -> bool {
    let s = page_path.as_str();
    s == "log.md" || is_rotated_log_filename(s)
}

fn is_rotated_log_filename(s: &str) -> bool {
    let Some(stem) = s.strip_prefix("log-").and_then(|v| v.strip_suffix(".md")) else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == "YYYY-MM".len()
        && bytes[4] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..].iter().all(|b| b.is_ascii_digit())
}

/// Cheap peek: does the file open with a `---` YAML frontmatter fence?
/// Used to tell a real page apart from the raw event ledger.
fn opens_with_frontmatter(abs: &Path) -> bool {
    use std::io::{BufRead, BufReader};
    let Ok(file) = std::fs::File::open(abs) else {
        return false;
    };
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).is_ok() && line.trim_end() == "---"
}

/// Cheap check for the raw hook event ledger shape. Real page markdown can be
/// frontmatter-free; a reserved-looking filename is only a ledger when the
/// content starts with the hook log prefix.
fn opens_with_log_ledger(abs: &Path) -> bool {
    use std::io::{BufRead, BufReader};
    let Ok(file) = std::fs::File::open(abs) else {
        return false;
    };
    let mut line = String::new();
    BufReader::new(file)
        .read_line(&mut line)
        .is_ok_and(|_| line.starts_with("## ["))
}

/// Returns `true` for markdown files that are NOT wiki pages and must be
/// skipped by the indexer:
/// - `_meta.md` (the self-describing scope manifest) and `bootstrap.md` —
///   always; and
/// - the raw event ledger (`log.md` / exact `log-YYYY-MM.md`) — skipping which
///   avoids supersession loops, since every `append_event` write triggers a
///   watcher event. A reserved-looking filename is skipped only when its
///   content opens with the raw hook log prefix; ordinary markdown pages with
///   those names are indexed.
fn is_reserved_page_file(abs: &Path, page_path: &PagePath) -> bool {
    if is_manifest_filename(page_path) || page_path.as_str() == "bootstrap.md" {
        return true;
    }
    is_log_ledger_filename(page_path) && !opens_with_frontmatter(abs) && opens_with_log_ledger(abs)
}

fn page_path_relative_to(root: &Path, abs: &Path) -> Option<PagePath> {
    let rel: &Path = abs.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    PagePath::new(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_store::Store;
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
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        (tmp, store, wiki, ws, proj)
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
        reconcile(&wiki).await.unwrap();

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

        let stats = reconcile(&wiki).await.unwrap();

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
        reconcile(&wiki).await.unwrap();

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

        let handle = WatcherHandle::start(wiki.clone()).unwrap();
        reconcile(&wiki).await.unwrap();

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
}
