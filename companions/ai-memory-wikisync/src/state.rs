//! Clone-local export state: one JSON file under the destination.
//!
//! The state file is the only thing wikisync writes besides page bodies. It
//! records, per exported page, the hash of the exact bytes last written and
//! the server ETag observed at that write, so later runs can revalidate with
//! `If-None-Match` and detect local edits with a three-way comparison. No
//! token, server credential, or page frontmatter ever lands in it.
//!
//! The state is per clone: a `.gitignore` of `*` beside it keeps the whole
//! state directory out of the repository the export lands in, so the
//! `git add` of the destination never commits it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::STATE_DIR;

const STATE_FILE: &str = "state.json";
const IGNORE_FILE: &str = ".gitignore";
const LOCK_FILE: &str = "write.lock";
/// Ignores the state directory, this file included. Two clones that both
/// commit their state would conflict on every merge and replay each other's
/// baselines.
const IGNORE_ALL: &[u8] = b"# Written by ai-memory-wikisync: export state is per clone.\n*\n";
const STATE_VERSION: u32 = 1;
const TMP_SUFFIX: &str = ".tmp";

/// SHA-256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = out.write_fmt(format_args!("{byte:02x}"));
    }
    out
}

/// Per-page export state: what this tool last wrote, and the server
/// revision (ETag) it came from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageState {
    /// SHA-256 of the exact bytes last written to the destination file.
    pub hash: String,
    /// Server ETag captured at that write, for `If-None-Match` revalidation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Server version id (`/api/v1` `id`) at that write. Absent in state
    /// written before conditional writes existed, which still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
}

/// Which server scope a baseline was recorded from (#1708).
///
/// The page map alone cannot say that a destination belongs to one project:
/// pointing the same `--dest` at another project made every page of the first
/// one look "deleted on the server", and `--propagate-deletes --apply` then
/// removed those files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncScope {
    /// Server origin the baseline was exported from.
    pub server: String,
    /// Workspace the baseline was exported from.
    pub workspace: String,
    /// Project the baseline was exported from.
    pub project: String,
}

impl SyncScope {
    /// Derive the scope of a run. The server origin is normalised the way the
    /// client normalises it, so `http://host:49374/` and `http://host:49374`
    /// are one scope.
    #[must_use]
    pub fn of(server: &str, workspace: &str, project: &str) -> Self {
        Self {
            server: server.trim_end_matches('/').to_owned(),
            workspace: workspace.to_owned(),
            project: project.to_owned(),
        }
    }
}

/// What a baseline says about the run asking to use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeCheck {
    /// The baseline belongs to this run's scope.
    Match,
    /// The baseline predates scope recording (or is empty) and is taken over
    /// by this run, which stamps its scope on the next save.
    Adopted,
    /// The baseline belongs to another scope. Reusing it would read that
    /// scope's pages as deletions here.
    Mismatch { recorded: SyncScope },
}

/// Compare a baseline with the run's scope.
#[must_use]
pub fn check_scope(state: &SyncState, scope: &SyncScope) -> ScopeCheck {
    match &state.scope {
        Some(recorded) if recorded != scope => ScopeCheck::Mismatch {
            recorded: recorded.clone(),
        },
        Some(_) => ScopeCheck::Match,
        // A file written before scopes existed carries no origin. With no
        // pages it is empty and cannot mislead; with pages, the origin is
        // unknown and taking it over is the behaviour every existing export
        // already had, so it is adopted with a warning rather than refused.
        None => ScopeCheck::Adopted,
    }
}

/// The whole state file. `version` guards against silently misreading a
/// future format: an unknown version fails closed instead of resetting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncState {
    pub version: u32,
    /// Server scope this baseline was recorded from. Absent in state written
    /// before #1708, which still loads and is adopted by the next save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<SyncScope>,
    pub pages: BTreeMap<String, PageState>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            scope: None,
            pages: BTreeMap::new(),
        }
    }
}

/// Path of the single state file inside a destination root.
pub fn state_path(dest_root: &Path) -> PathBuf {
    dest_root.join(STATE_DIR).join(STATE_FILE)
}

/// Exclusive ownership of one destination's read/plan/write lifecycle.
/// Keeping the file handle alive keeps the OS lock alive; process exit also
/// releases it, so a crashed sync never leaves a stale lease to clean up.
#[derive(Debug)]
pub struct WriteLock {
    _file: fs::File,
}

/// Linear backoff, 10 ms + 20 ms + … + 100 ms: about half a second.
const LOCK_RETRY_ATTEMPTS: u32 = 10;
const LOCK_RETRY_BASE_DELAY: Duration = Duration::from_millis(10);

/// Fail when another mutating run owns this destination.
///
/// The lock covers the whole operation, not only [`save`]: two plans derived
/// from one baseline must not both apply.
///
/// A held lock is retried for about half a second first. On Unix a child
/// forked by another thread inherits the `flock` until it calls `exec`, so a
/// lock released a moment ago can still read as held. A real concurrent run
/// outlasts the budget and is refused.
pub fn acquire_write_lock(dest_root: &Path) -> Result<WriteLock> {
    acquire_write_lock_impl(
        dest_root,
        LOCK_RETRY_ATTEMPTS,
        LOCK_RETRY_BASE_DELAY,
        fs2::FileExt::try_lock_exclusive,
    )
}

/// [`acquire_write_lock`] with the budget and the lock call injected for tests.
fn acquire_write_lock_impl(
    dest_root: &Path,
    attempts: u32,
    base_delay: Duration,
    try_lock: impl Fn(&fs::File) -> std::io::Result<()>,
) -> Result<WriteLock> {
    let dir = dest_root.join(STATE_DIR);
    ensure_real_state_dir(&dir)?;
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    ensure_real_state_dir(&dir)?;
    ignore_state_dir(&dir)?;

    let path = dir.join(LOCK_FILE);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        bail!("write lock is a symlink: {}", path.display());
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .with_context(|| format!("cannot open write lock {}", path.display()))?;
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        bail!("write lock became a symlink: {}", path.display());
    }
    let contended = fs2::lock_contended_error().raw_os_error();
    let mut attempt = 0u32;
    loop {
        match try_lock(&file) {
            Ok(()) => return Ok(WriteLock { _file: file }),
            Err(err) if attempt < attempts && err.raw_os_error() == contended => {
                attempt += 1;
                std::thread::sleep(base_delay * attempt);
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "destination {} is already being modified by another ai-memory-wikisync run",
                        dest_root.display()
                    )
                });
            }
        }
    }
}

/// The state directory must be a real directory, never a symlink: a symlink
/// planted on `.ai-memory-wikisync` would move state (and lock the writer
/// into trusting it) outside the destination.
fn ensure_real_state_dir(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("state directory is a symlink: {}", dir.display());
            }
            if !meta.is_dir() {
                bail!("state path is not a directory: {}", dir.display());
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e))
            .context(format!("cannot inspect state directory {}", dir.display())),
    }
}

/// Load the state for a destination root. A missing file is an empty state
/// (first export). A corrupt or future-version file fails closed: guessing
/// would risk overwriting local edits.
pub fn load(dest_root: &Path) -> Result<SyncState> {
    let path = state_path(dest_root);
    ensure_real_state_dir(path.parent().unwrap_or(dest_root))?;
    if !path.exists() {
        return Ok(SyncState::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("cannot read state file {}", path.display()))?;
    let state: SyncState = serde_json::from_str(&raw)
        .with_context(|| format!("state file {} is corrupt", path.display()))?;
    if state.version != STATE_VERSION {
        bail!(
            "state file {} has version {}; this build understands version {STATE_VERSION}; \
             resolve it manually before exporting again",
            path.display(),
            state.version
        );
    }
    Ok(state)
}

/// Atomically persist the state: tmp file in the same directory, fsync,
/// rename over the old file. Called once after each successful write batch,
/// so a crash mid-batch leaves the previous state intact and the next run
/// re-classifies through the three-way comparison instead of trusting it.
pub fn save(dest_root: &Path, state: &SyncState) -> Result<()> {
    let path = state_path(dest_root);
    let dir = path.parent().unwrap_or(dest_root);
    ensure_real_state_dir(dir)?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    if fs::symlink_metadata(dir)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
    {
        bail!("state directory became a symlink: {}", dir.display());
    }
    ignore_state_dir(dir)?;
    let json = serde_json::to_vec_pretty(state).context("cannot serialize state")?;
    let tmp = PathBuf::from(format!("{}{TMP_SUFFIX}", path.display()));
    // Leftover tmp from a crashed run is ours by name; clear it.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .context(format!("cannot clear stale {}", tmp.display()));
        }
    }
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot restrict permissions on {}", tmp.display()))?;
        }
        file.write_all(&json)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot sync {}", tmp.display()))?;
    }
    fs::rename(&tmp, &path)
        .with_context(|| format!("cannot move {} over {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Keep the state directory out of git. An existing `.gitignore` is left as
/// it is: the operator may have written it, and `create_new` never follows a
/// symlink planted under that name.
fn ignore_state_dir(dir: &Path) -> Result<()> {
    let path = dir.join(IGNORE_FILE);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(IGNORE_ALL)
                .with_context(|| format!("cannot write {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("cannot sync {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow::Error::new(e)).context(format!("cannot create {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(hash: &str, etag: Option<&str>) -> PageState {
        PageState {
            hash: hash.to_string(),
            etag: etag.map(str::to_owned),
            page_id: None,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut state = SyncState::default();
        state.pages.insert(
            "notes/a.md".to_string(),
            page(&"a".repeat(64), Some("\"v1\"")),
        );
        save(&root, &state).unwrap();
        assert_eq!(load(&root).unwrap(), state);
        // No tmp file is left behind.
        let dir = fs::read_dir(root.join(STATE_DIR)).unwrap();
        let mut names: Vec<String> = dir
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![".gitignore".to_string(), "state.json".to_string()]
        );
    }

    #[test]
    fn state_without_page_ids_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        fs::write(
            state_path(&root),
            r#"{"version":1,"pages":{"notes/a.md":{"hash":"h","etag":"\"e\""}}}"#,
        )
        .unwrap();
        let state = load(&root).unwrap();
        assert_eq!(state.pages["notes/a.md"].page_id, None);

        let mut with_id = state.clone();
        with_id.pages.get_mut("notes/a.md").unwrap().page_id = Some("v1".to_string());
        save(&root, &with_id).unwrap();
        assert_eq!(load(&root).unwrap(), with_id);
    }

    #[test]
    fn state_file_is_owner_only_on_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        save(&root, &SyncState::default()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(state_path(&root))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn write_lock_is_exclusive_and_released_with_its_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let first = acquire_write_lock(&root).unwrap();
        let err = acquire_write_lock(&root).unwrap_err().to_string();
        assert!(err.contains("already being modified"), "{err}");

        drop(first);
        acquire_write_lock(&root).expect("closing the first handle releases the OS lock");
    }

    /// A hold that clears is taken, one that never clears is refused after
    /// the budget, any other error is reported at once.
    #[test]
    fn a_briefly_contended_lock_is_retried_within_its_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let contended = || fs2::lock_contended_error();

        let calls = std::cell::Cell::new(0u32);
        let lock = acquire_write_lock_impl(&root, 5, Duration::ZERO, |file| {
            calls.set(calls.get() + 1);
            if calls.get() <= 3 {
                Err(contended())
            } else {
                fs2::FileExt::try_lock_exclusive(file)
            }
        })
        .expect("a hold that clears is absorbed");
        assert_eq!(calls.get(), 4);
        drop(lock);

        let calls = std::cell::Cell::new(0u32);
        let err = acquire_write_lock_impl(&root, 5, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            Err(contended())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("already being modified"), "{err}");
        assert_eq!(
            calls.get(),
            6,
            "initial attempt plus five retries, then stop"
        );

        let calls = std::cell::Cell::new(0u32);
        let err = acquire_write_lock_impl(&root, 5, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            Err(std::io::Error::other("disk on fire"))
        })
        .unwrap_err();
        assert_eq!(calls.get(), 1, "a non-contention error is not retried");
        assert!(format!("{err:#}").contains("disk on fire"), "{err:#}");
    }

    #[cfg(unix)]
    #[test]
    fn write_lock_refuses_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dir = root.join(STATE_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(root.join("elsewhere"), b"").unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), dir.join(LOCK_FILE)).unwrap();

        let err = acquire_write_lock(&root).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }

    #[test]
    fn missing_state_is_empty_and_corrupt_state_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        assert_eq!(load(&root).unwrap(), SyncState::default());

        fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        fs::write(state_path(&root), "{ not json").unwrap();
        let err = load(&root).unwrap_err().to_string();
        assert!(err.contains("corrupt"), "{err}");

        let future = SyncState {
            version: 99,
            ..SyncState::default()
        };
        fs::write(state_path(&root), serde_json::to_string(&future).unwrap()).unwrap();
        let err = load(&root).unwrap_err().to_string();
        assert!(err.contains("version"), "{err}");
    }

    #[test]
    fn a_baseline_records_its_scope_and_refuses_another_one() {
        let scope = SyncScope::of("http://host:49374", "demo", "a");
        let mut state = SyncState::default();
        assert_eq!(check_scope(&state, &scope), ScopeCheck::Adopted);

        state.scope = Some(scope.clone());
        state.pages.insert("notes/a.md".into(), page("hash", None));
        assert_eq!(check_scope(&state, &scope), ScopeCheck::Match);
        // A trailing slash is the same server.
        assert_eq!(
            check_scope(&state, &SyncScope::of("http://host:49374/", "demo", "a")),
            ScopeCheck::Match
        );

        // Another project on the same server is a different scope (#1708).
        let other = SyncScope::of("http://host:49374", "demo", "b");
        assert_eq!(
            check_scope(&state, &other),
            ScopeCheck::Mismatch {
                recorded: scope.clone()
            }
        );
        // So is another server with the same names.
        let elsewhere = SyncScope::of("http://other:49374", "demo", "a");
        assert!(matches!(
            check_scope(&state, &elsewhere),
            ScopeCheck::Mismatch { .. }
        ));
    }

    /// A state file written before scopes existed still loads, and the next
    /// save stamps the scope so the window closes after one run.
    #[test]
    fn legacy_state_loads_and_is_stamped_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        fs::write(
            state_path(&root),
            r#"{"version":1,"pages":{"notes/a.md":{"hash":"h"}}}"#,
        )
        .unwrap();

        let mut state = load(&root).unwrap();
        assert!(state.scope.is_none());
        let scope = SyncScope::of("http://host:49374", "demo", "a");
        assert_eq!(check_scope(&state, &scope), ScopeCheck::Adopted);

        state.scope = Some(scope.clone());
        save(&root, &state).unwrap();
        let reloaded = load(&root).unwrap();
        assert_eq!(reloaded.scope.as_ref(), Some(&scope));
        assert_eq!(check_scope(&reloaded, &scope), ScopeCheck::Match);
    }

    #[test]
    fn refuses_symlinked_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&elsewhere, root.join(STATE_DIR)).unwrap();
            let err = load(&root).unwrap_err().to_string();
            assert!(err.contains("symlink"), "{err}");
            let err = save(&root, &SyncState::default()).unwrap_err().to_string();
            assert!(err.contains("symlink"), "{err}");
        }
    }

    #[test]
    fn save_survives_a_stale_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        fs::write(root.join(STATE_DIR).join("state.json.tmp"), b"junk").unwrap();
        save(&root, &SyncState::default()).unwrap();
        assert!(load(&root).unwrap() == SyncState::default());
    }

    #[test]
    fn state_dir_ignores_itself_and_keeps_an_existing_ignore_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        save(&root, &SyncState::default()).unwrap();
        let ignore = root.join(STATE_DIR).join(IGNORE_FILE);
        assert_eq!(fs::read(&ignore).unwrap(), IGNORE_ALL);

        fs::write(&ignore, b"state.json\n").unwrap();
        save(&root, &SyncState::default()).unwrap();
        assert_eq!(fs::read(&ignore).unwrap(), b"state.json\n");
    }

    /// The state must not reach a commit: `git add` of the destination, the
    /// command the export prints, stages the pages and nothing under the
    /// state directory.
    #[test]
    fn git_add_of_the_destination_skips_the_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
        };
        let Ok(init) = git(&["init", "-q"]) else {
            eprintln!("git is not installed; skipping");
            return;
        };
        assert!(init.status.success());
        let dest = repo.join("docs/wiki");
        fs::create_dir_all(dest.join("decisions")).unwrap();
        fs::write(dest.join("decisions/a.md"), b"# A\n").unwrap();
        save(&dest, &SyncState::default()).unwrap();

        let added = git(&["add", "--dry-run", "--", "docs/wiki"]).unwrap();
        assert!(added.status.success());
        let added = String::from_utf8(added.stdout).unwrap();
        assert!(added.contains("docs/wiki/decisions/a.md"), "{added}");
        assert!(!added.contains(STATE_DIR), "{added}");
    }
}
