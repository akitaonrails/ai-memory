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
//!    reindexes every markdown file. Catches any events the OS dropped
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
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::{WikiError, WikiResult};
use crate::wiki::Wiki;

/// Reconciliation tick interval.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Debounce window for filesystem events.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);

/// Handle representing an active watcher; drop to stop.
pub struct WatcherHandle {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
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

        let mut debouncer = new_debouncer(
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
                    Ok(()) => {
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
        if !is_markdown(raw_path) {
            continue;
        }
        if is_tempfile(raw_path) {
            continue;
        }
        let Some((ws, proj, page_path)) = extract_project_ids(wiki.root(), raw_path) else {
            continue;
        };
        if is_reserved_filename(&page_path) {
            continue;
        }
        if !raw_path.is_file() {
            // Likely a transient state (mv, atomic rename in flight).
            continue;
        }
        match wiki.reindex_page(ws, proj, page_path.clone()).await {
            Ok(_) => debug!(path = %page_path, "reindexed via watcher"),
            Err(e) => warn!(path = %page_path, error = %e, "watcher reindex failed"),
        }
    }
}

async fn reconcile(wiki: &Wiki) -> WikiResult<()> {
    let root = wiki.root().to_path_buf();
    // Walk all per-project subdirectories: <ws_uuid>/<proj_uuid>/
    let project_dirs = tokio::task::spawn_blocking(move || walk_project_dirs(&root))
        .await
        .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;

    let mut total = 0_usize;
    for (ws, proj, proj_root) in project_dirs {
        let pages = tokio::task::spawn_blocking(move || walk_markdown(&proj_root))
            .await
            .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;
        total += pages.len();
        for path in pages {
            if let Err(e) = wiki.reindex_page(ws, proj, path.clone()).await {
                warn!(path = %path, error = %e, "reconcile reindex failed");
            }
        }
    }
    info!(count = total, "reconciliation pass complete");
    Ok(())
}

/// Walk `<wiki_root>` and return all `(WorkspaceId, ProjectId, proj_root)` tuples
/// whose first two path segments parse as valid UUIDs.
fn walk_project_dirs(
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

fn walk_markdown(root: &Path) -> WikiResult<Vec<PagePath>> {
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
                stack.push(path);
            } else if ft.is_file()
                && is_markdown(&path)
                && !is_tempfile(&path)
                && let Some(pp) = page_path_relative_to(root, &path)
                && !is_reserved_filename(&pp)
            {
                out.push(pp);
            }
        }
    }
    Ok(out)
}

fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "md")
}

fn is_tempfile(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(".ai-memory-tmp."))
}

/// Returns `true` for the per-project reserved files that are NOT wiki
/// pages: `log-YYYY-MM.md` (per-month rolling event ledger — see
/// `ai-memory-hooks::log::log_filename_for`), the legacy `log.md`
/// filename for backwards compatibility, and `bootstrap.md` (manifest).
/// The watcher must skip them to avoid supersession loops — every
/// `append_event` write triggers a watcher event, triggering an
/// `upsert_page`, creating a spurious new supersession version.
fn is_reserved_filename(page_path: &PagePath) -> bool {
    let s = page_path.as_str();
    s == "log.md"
        || s == "bootstrap.md"
        // log-YYYY-MM.md — 7 chars between the dash and the dot.
        || (s.starts_with("log-") && s.ends_with(".md") && s.len() == "log-YYYY-MM.md".len())
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
    use std::path::PathBuf;
    use tempfile::TempDir;

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

    fn test_wiki_root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\data\wiki")
        } else {
            PathBuf::from("/data/wiki")
        }
    }

    /// `extract_project_ids` must parse a valid `<ws>/<proj>/<path>` triplet.
    #[test]
    fn extract_project_ids_valid_path() {
        let wiki_root = test_wiki_root();
        let ws_id = WorkspaceId::new();
        let proj_id = ProjectId::new();
        let event_path = wiki_root.join(format!("{ws_id}/{proj_id}/decisions/foo.md"));
        let result = extract_project_ids(&wiki_root, &event_path);
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
        let wiki_root = test_wiki_root();
        let event_path = wiki_root.join("not-a-uuid/some-proj/foo.md");
        assert!(
            extract_project_ids(&wiki_root, &event_path).is_none(),
            "garbage first segment must return None"
        );
    }

    /// `extract_project_ids` must return `None` for flat (non-namespaced) paths.
    #[test]
    fn extract_project_ids_flat_path_returns_none() {
        let wiki_root = test_wiki_root();
        let event_path = wiki_root.join("foo.md");
        assert!(
            extract_project_ids(&wiki_root, &event_path).is_none(),
            "flat path with no namespace must return None"
        );
    }

    /// `extract_project_ids` must return `None` when the second segment is not a valid UUID.
    #[test]
    fn extract_rejects_garbage_in_project_segment() {
        let wiki_root = test_wiki_root();
        let ws = WorkspaceId::new().to_string();
        let event_path = wiki_root.join(format!("{ws}/not-a-uuid/decisions/foo.md"));
        assert!(
            extract_project_ids(&wiki_root, &event_path).is_none(),
            "garbage project segment must return None"
        );
    }

    /// `extract_project_ids` must return `None` when there is no page path
    /// after the two UUID segments (would produce an empty `PagePath`).
    #[test]
    fn extract_rejects_empty_page_path() {
        let wiki_root = test_wiki_root();
        let ws = WorkspaceId::new().to_string();
        let proj = ProjectId::new().to_string();
        // Just the project dir itself with no page path beneath.
        let event_path = wiki_root.join(format!("{ws}/{proj}"));
        assert!(
            extract_project_ids(&wiki_root, &event_path).is_none(),
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

        // Drop a file inside the per-project directory, bypassing the wiki write API
        // (simulating an external editor).
        let target = proj_dir.join("external.md");
        std::fs::write(&target, "Hello from outside the wiki API.\n").unwrap();

        // Poll for the row to land. Watcher debounces at 300ms; extra
        // margin for slow CI environments.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
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
        std::fs::write(proj_dir.join("log.md"), "sessionstarted logtoken unique\n").unwrap();
        std::fs::write(
            proj_dir.join("log-2026-05.md"),
            "sessionstarted rotatedlogtoken unique\n",
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

        let boot_hits = store
            .reader
            .search_pages("boottoken".into(), 5)
            .await
            .unwrap();
        assert!(boot_hits.is_empty(), "bootstrap.md must not be indexed");

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
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, proj_root.join("symlinked.md")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&secret, proj_root.join("symlinked.md")).unwrap();

        let found = walk_markdown(&proj_root).unwrap();
        let names: Vec<_> = found.iter().map(|p| p.as_str().to_string()).collect();
        assert!(names.contains(&"real.md".to_string()), "real file present");
        assert!(
            !names.contains(&"symlinked.md".to_string()),
            "symlink to outside file must be skipped; got: {names:?}"
        );
    }
}
