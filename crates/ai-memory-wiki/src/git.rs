//! Git versioning for the wiki tree.
//!
//! On `Wiki::new`, we lazily `git init` the wiki root if it isn't already
//! a repo. Auto-commits fire from the hook router on `SessionEnd` and
//! from the M7 consolidator. Author/email are fixed so the wiki history
//! can't accidentally leak the maintainer's git identity.
//!
//! A commit stages the paths reported through `mark_written` since the
//! last one; the adapter's own write methods report for their caller and
//! the crate's `clippy.toml` refuses the raw calls. The full walk is the
//! safety net, decided in `take_staging`. The repository stays open
//! between commits: reopening it dropped libgit2's index tree cache, so
//! every commit rebuilt every directory tree.
//!
//! The raw hook event ledgers (`log.md`, `log-YYYY-MM.md`) are never staged.
//! They are append-only logs that reach hundreds of megabytes, git stores each
//! new version as a whole blob, and nothing reads them back from git; see
//! [`is_ledger_file`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use git2::{ConfigLevel, ErrorCode, IndexAddOption, ObjectType, Repository, Signature};
use tracing::{debug, warn};

use crate::error::{WikiError, WikiResult};

/// Author identity used for ai-memory's own commits. The user can
/// rewrite history with their own identity later if they care.
pub const COMMIT_AUTHOR_NAME: &str = "ai-memory";
/// Author email used for ai-memory's own commits.
pub const COMMIT_AUTHOR_EMAIL: &str = "ai-memory@local";

/// Thin handle over the wiki repo. Cheap to clone — a `PathBuf` and a
/// shared lock.
#[derive(Clone)]
pub struct GitAdapter {
    root: PathBuf,
    /// One commit at a time per repository: libgit2 fails a concurrent
    /// commit with "the index is locked" instead of waiting. Also holds
    /// the repository kept open between commits. Clones share it; a
    /// second adapter opened on the same root does not.
    commit_lock: Arc<Mutex<Option<Open>>>,
    /// Paths written since the last commit; shared by clones like the lock.
    written: Arc<Mutex<Written>>,
}

/// The repository kept open between commits, so libgit2's cached index
/// (tree cache included) survives them. `Index` itself is not `Send`.
struct Open {
    repo: Repository,
    /// HEAD as this adapter last left it; moved by another writer, the
    /// kept index is re-read and the next commit walks.
    head: Option<git2::Oid>,
}

#[derive(Debug, Default)]
struct Written {
    /// Relative to the root; a directory covers its subtree.
    paths: BTreeSet<PathBuf>,
    walk_needed: bool,
    /// `None` before the first commit, which walks.
    last_walk: Option<Instant>,
    /// Everything reported since the last walk, so the walk can name
    /// what nobody reported.
    since_walk: BTreeSet<PathBuf>,
    unreported_writes: u64,
    last_unreported: Vec<PathBuf>,
}

/// A path-scoped commit this long after the last walk walks instead.
const SWEEP_INTERVAL: Duration = Duration::from_secs(600);

const UNREPORTED_SAMPLE: usize = 5;

/// Retries for a commit whose staging read a file mid-write.
const RACY_READ_ATTEMPTS: u32 = 4;
const RACY_READ_BACKOFF: Duration = Duration::from_millis(25);

/// A staging read that raced a writer outside the commit lock: libgit2's
/// "file changed before we could read it" (mid-write), or a file that was
/// listed by the walk but gone by the time libgit2 streamed it in (an `Os`
/// "failed to read file into stream" — e.g. an atomic writer's temp file
/// renamed away between the scan and the read). Both settle within
/// milliseconds; a rescan no longer sees a vanished file.
fn is_racy_read(e: &git2::Error) -> bool {
    match e.class() {
        git2::ErrorClass::Filesystem => e.message().contains("changed before"),
        git2::ErrorClass::Os => e.message().contains("failed to read file into stream"),
        _ => false,
    }
}

/// Writes that reached the tree without a report: a writer bypassed the wiki.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepSnapshot {
    /// Over the adapter's life.
    pub unreported_writes: u64,
    /// Up to [`UNREPORTED_SAMPLE`] of them from the last walk.
    pub last_unreported: Vec<PathBuf>,
}

enum Staging {
    Everything,
    Paths(BTreeSet<PathBuf>),
}

/// One git checkpoint in the wiki repository.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// Full commit OID.
    pub oid: String,
    /// Commit summary (first line of the commit message).
    pub summary: String,
    /// Author timestamp, seconds since Unix epoch.
    pub time: i64,
}

enum CommitGit2Error {
    Open(git2::Error),
    Other(git2::Error),
    Wiki(WikiError),
}

enum OpenRepositoryError {
    Git(git2::Error),
    Wiki(WikiError),
}

fn open_validated_repository(root: &Path) -> Result<Repository, OpenRepositoryError> {
    let expected_git_dir =
        crate::confinement::inspect_git_directory(root).map_err(OpenRepositoryError::Wiki)?;
    let repo = Repository::open(root).map_err(OpenRepositoryError::Git)?;
    let workdir = repo.workdir().ok_or_else(|| {
        OpenRepositoryError::Wiki(WikiError::Confinement {
            path: root.to_path_buf(),
            reason: "wiki repository must have a working tree",
        })
    })?;
    let canonical_root = root
        .canonicalize()
        .map_err(WikiError::from)
        .map_err(OpenRepositoryError::Wiki)?;
    let canonical_workdir = workdir
        .canonicalize()
        .map_err(WikiError::from)
        .map_err(OpenRepositoryError::Wiki)?;
    let canonical_git_dir = repo
        .path()
        .canonicalize()
        .map_err(WikiError::from)
        .map_err(OpenRepositoryError::Wiki)?;
    let canonical_common_dir = repo
        .commondir()
        .canonicalize()
        .map_err(WikiError::from)
        .map_err(OpenRepositoryError::Wiki)?;
    let canonical_expected_git_dir = expected_git_dir
        .canonicalize()
        .map_err(WikiError::from)
        .map_err(OpenRepositoryError::Wiki)?;
    if canonical_workdir != canonical_root {
        return Err(OpenRepositoryError::Wiki(WikiError::Confinement {
            path: workdir.to_path_buf(),
            reason: "wiki repository working directory escapes the wiki root",
        }));
    }
    if canonical_git_dir != canonical_expected_git_dir
        || canonical_common_dir != canonical_expected_git_dir
    {
        return Err(OpenRepositoryError::Wiki(WikiError::Confinement {
            path: repo.path().to_path_buf(),
            reason: "wiki repository metadata escapes the in-root .git directory",
        }));
    }
    ensure_gitdir_fsync(&repo).map_err(OpenRepositoryError::Git)?;
    Ok(repo)
}

/// libgit2 fsyncs loose objects and refs only when `core.fsyncObjectFiles` is
/// set; without it a crash can leave a ref pointing at a zero-length object.
/// git2 0.21 does not wrap the process-wide `GIT_OPT_ENABLE_FSYNC_GITDIR`, and
/// this crate forbids `unsafe`, so the repo-scoped key is set instead. It is
/// written on every open of a repo that lacks it, so pre-existing wikis are
/// covered too.
fn ensure_gitdir_fsync(repo: &Repository) -> Result<(), git2::Error> {
    if repo
        .config()?
        .get_bool("core.fsyncObjectFiles")
        .unwrap_or(false)
    {
        return Ok(());
    }
    repo.config()?
        .open_level(ConfigLevel::Local)?
        .set_bool("core.fsyncObjectFiles", true)
}

fn map_open_error(error: OpenRepositoryError) -> WikiError {
    match error {
        OpenRepositoryError::Git(error) => map_git_err(error),
        OpenRepositoryError::Wiki(error) => error,
    }
}

fn validate_repository_or_allow_cli_fallback(root: &Path) -> WikiResult<()> {
    match open_validated_repository(root) {
        Ok(_) => Ok(()),
        Err(OpenRepositoryError::Git(error))
            if cfg!(windows) && should_try_commit_cli_fallback(&error) =>
        {
            validate_cli_fallback_root(root)
        }
        Err(error) => Err(map_open_error(error)),
    }
}

fn validate_cli_fallback_root(root: &Path) -> WikiResult<()> {
    let git_dir = crate::confinement::inspect_git_directory(root)?;
    crate::confinement::inspect_tree_except(root, &[git_dir.as_path()])?;
    let worktree = git_resolved_path(root, "--show-toplevel")?;
    let resolved_git_dir = git_resolved_path(root, "--absolute-git-dir")?;
    let common_dir = git_resolved_path(root, "--git-common-dir")?;
    validate_resolved_repository_paths(root, &git_dir, &worktree, &resolved_git_dir, &common_dir)
}

fn git_resolved_path(root: &Path, argument: &str) -> WikiResult<PathBuf> {
    let output = git_command(root)
        .args(["rev-parse", "--path-format=absolute", argument])
        .output()?;
    if !output.status.success() {
        return Err(WikiError::Io(std::io::Error::other(format!(
            "git repository validation exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let path = PathBuf::from(raw.trim());
    if path.as_os_str().is_empty() {
        return Err(WikiError::Confinement {
            path: root.to_path_buf(),
            reason: "git CLI returned an empty repository path",
        });
    }
    if !path.is_absolute() {
        return Err(WikiError::Confinement {
            path,
            reason: "git CLI returned a non-absolute repository path",
        });
    }
    Ok(path)
}

fn validate_resolved_repository_paths(
    root: &Path,
    expected_git_dir: &Path,
    worktree: &Path,
    git_dir: &Path,
    common_dir: &Path,
) -> WikiResult<()> {
    let canonical_root = root.canonicalize()?;
    let canonical_expected_git_dir = expected_git_dir.canonicalize()?;
    let canonical_worktree = worktree.canonicalize()?;
    let canonical_git_dir = git_dir.canonicalize()?;
    let canonical_common_dir = common_dir.canonicalize()?;
    if canonical_worktree != canonical_root {
        return Err(WikiError::Confinement {
            path: worktree.to_path_buf(),
            reason: "git CLI resolved a working directory outside the wiki root",
        });
    }
    if canonical_git_dir != canonical_expected_git_dir
        || canonical_common_dir != canonical_expected_git_dir
    {
        return Err(WikiError::Confinement {
            path: git_dir.to_path_buf(),
            reason: "git CLI resolved metadata outside the in-root .git directory",
        });
    }
    Ok(())
}

fn git_command(root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .arg("-C")
        .arg(root); // lgtm [rust/command-line-injection]
    command
}

impl GitAdapter {
    /// Open or initialise the repo at `root`. Idempotent: if the
    /// directory is already a git repo, leaves it alone.
    ///
    /// # Errors
    /// Propagates any underlying libgit2 error.
    pub fn open_or_init(root: &Path) -> WikiResult<Self> {
        crate::confinement::initialize_root(root)?;
        let git_dir = root.join(".git");
        match std::fs::symlink_metadata(&git_dir) {
            Ok(_) => {
                validate_repository_or_allow_cli_fallback(root)?;
                debug!(root = %root.display(), "wiki repo already initialised");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                debug!(root = %root.display(), "initialising wiki repo");
                init_repo(root)?;
                validate_repository_or_allow_cli_fallback(root)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            root: root.to_path_buf(),
            commit_lock: Arc::new(Mutex::new(None)),
            written: Arc::new(Mutex::new(Written::default())),
        })
    }

    fn written(&self) -> MutexGuard<'_, Written> {
        self.written.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Report a path written, moved or removed (absolute or relative to
    /// the root); the next commit stages it. A path outside the root is
    /// a caller's bug: logged, and the next commit walks.
    pub fn mark_written(&self, path: &Path) {
        let rel = if path.is_absolute() {
            path.strip_prefix(&self.root).ok()
        } else {
            Some(path)
        };
        // The watcher reports git's own writes too; never stage them.
        // Also ignore any path that contains a git-reserved component.
        if rel.is_some_and(|rel| {
            rel.components().any(|c| {
                ai_memory_core::is_git_reserved_component(&c.as_os_str().to_string_lossy())
            })
        }) {
            return;
        }
        let mut written = self.written();
        match rel {
            Some(rel) if !rel.as_os_str().is_empty() => {
                written.paths.insert(rel.to_path_buf());
                written.since_walk.insert(rel.to_path_buf());
            }
            _ => {
                warn!(path = %path.display(), "reported path is not under the wiki root");
                written.walk_needed = true;
            }
        }
    }

    /// Drop the kept repository, as a restart would.
    #[cfg(test)]
    pub(crate) fn close(&self) {
        *self.commit_lock.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Make the next path-scoped commit walk, as if the sweep were due.
    #[cfg(test)]
    pub(crate) fn age_last_walk(&self) {
        let mut written = self.written();
        if let Some(aged) = Instant::now().checked_sub(SWEEP_INTERVAL) {
            written.last_walk = Some(aged);
        } else {
            // A fresh Windows runner may not have ten minutes of clock history.
            // Preserve the previous walk so the sweep still counts missed writes.
            written.walk_needed = true;
        }
    }

    /// What the sweeps found; see [`SweepSnapshot`].
    #[must_use]
    pub fn sweep_snapshot(&self) -> SweepSnapshot {
        let written = self.written();
        SweepSnapshot {
            unreported_writes: written.unreported_writes,
            last_unreported: written.last_unreported.clone(),
        }
    }
    #[cfg(test)]
    pub(crate) fn written_paths(&self) -> Vec<PathBuf> {
        self.written().paths.iter().cloned().collect()
    }

    /// Path of the wiki root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stage the reported paths (or walk the tree; see `take_staging`)
    /// and commit with `message`. Returns `Ok(None)` when nothing
    /// changed, `Ok(Some(commit_oid))` on a commit.
    ///
    /// # Errors
    /// Propagates any underlying libgit2 error.
    pub fn commit_all(&self, message: &str) -> WikiResult<Option<git2::Oid>> {
        self.commit_all_with_fallback(message, commit_all_fallback)
    }

    fn commit_all_with_fallback<F>(
        &self,
        message: &str,
        fallback: F,
    ) -> WikiResult<Option<git2::Oid>>
    where
        F: FnOnce(&Path, &str, git2::Error) -> WikiResult<Option<git2::Oid>>,
    {
        // One hold of the commit lock spans the confinement walk and the
        // commit. Our own commit activity churns `.git` lock and temp
        // files and atomic temp names in the tree; a walk beside it saw
        // entries that vanished between read_dir and stat. Writers we do
        // not serialize (the git CLI, a second adapter) remain covered by
        // the walk's own vanish tolerance.
        let mut slot = self.writing();
        let git_dir = crate::confinement::inspect_git_directory(&self.root)?;
        crate::confinement::inspect_tree_except(&self.root, &[git_dir.as_path()])?;
        match self.commit_all_git2(&mut slot, message) {
            Ok(result) => Ok(result),
            Err(CommitGit2Error::Open(error)) => commit_open_error_with_fallback_on(
                cfg!(windows),
                &self.root,
                message,
                error,
                fallback,
            ),
            Err(CommitGit2Error::Other(error)) => Err(map_git_err(error)),
            Err(CommitGit2Error::Wiki(error)) => Err(error),
        }
    }

    /// Called with the commit lock already held by
    /// [`commit_all_with_fallback`]; `slot` is that lock's payload.
    fn commit_all_git2(
        &self,
        slot: &mut Option<Open>,
        message: &str,
    ) -> Result<Option<git2::Oid>, CommitGit2Error> {
        if slot.is_none() {
            let repo = open_validated_repository(&self.root).map_err(|error| match error {
                OpenRepositoryError::Git(error) => CommitGit2Error::Open(error),
                OpenRepositoryError::Wiki(error) => CommitGit2Error::Wiki(error),
            })?;
            *slot = Some(Open { repo, head: None });
        }
        crate::confinement::inspect_git_directory(&self.root).map_err(CommitGit2Error::Wiki)?;
        let open = slot.as_mut().expect("opened above");
        let head_now = open.repo.head().ok().and_then(|h| h.target());
        if open.head.is_some() && open.head != head_now {
            warn!("HEAD moved under the kept index; re-reading it and walking");
            open.repo
                .index()
                .and_then(|mut index| index.read(true))
                .map_err(CommitGit2Error::Other)?;
            self.written().walk_needed = true;
        }
        let staging = self.take_staging();
        let mut attempt = 0u32;
        let result = loop {
            attempt += 1;
            match self.commit_staged(open, message, &staging) {
                // A writer outside the lock was mid-write; it settles in
                // milliseconds.
                Err(CommitGit2Error::Other(e))
                    if is_racy_read(&e) && attempt < RACY_READ_ATTEMPTS =>
                {
                    debug!(attempt, "a file changed under the commit; retrying");
                    std::thread::sleep(RACY_READ_BACKOFF);
                }
                other => break other,
            }
        };
        if let Ok(oid) = &result {
            open.head = oid.or(head_now);
        }
        if let Err(e) = &result {
            *slot = None;
            let racy = matches!(e, CommitGit2Error::Other(e) if is_racy_read(e));
            self.keep_pending(staging, racy);
        }
        result
    }

    /// After a failed commit: the paths stay reported, and unless the
    /// failure was a racy read the next commit walks.
    fn keep_pending(&self, staging: Staging, racy: bool) {
        let mut written = self.written();
        if let Staging::Paths(paths) = staging {
            written.since_walk.extend(paths.iter().cloned());
            written.paths.extend(paths);
        }
        written.walk_needed |= !racy;
    }
    /// Walk when told to, when nothing was reported, or when the sweep is
    /// due; otherwise stage the reported paths. Called under the commit lock.
    fn take_staging(&self) -> Staging {
        let mut written = self.written();
        let sweep_due = written
            .last_walk
            .is_none_or(|at| at.elapsed() >= SWEEP_INTERVAL);
        if written.walk_needed || written.paths.is_empty() || sweep_due {
            written.walk_needed = false;
            written.paths.clear();
            Staging::Everything
        } else {
            Staging::Paths(std::mem::take(&mut written.paths))
        }
    }

    /// After a walk: name what nobody reported since the last one and
    /// start the next interval. The first walk has nothing to compare
    /// against.
    fn record_walk(&self, walked: &[PathBuf]) {
        let mut written = self.written();
        if written.last_walk.is_some() {
            // A report covers its subtree.
            let mut unreported: Vec<PathBuf> = walked
                .iter()
                .filter(|path| !path.ancestors().any(|a| written.since_walk.contains(a)))
                .cloned()
                .collect();
            if !unreported.is_empty() {
                warn!(
                    count = unreported.len(),
                    first = ?unreported.iter().take(UNREPORTED_SAMPLE).collect::<Vec<_>>(),
                    "the sweep staged writes nobody reported: a writer bypassed the wiki"
                );
            }
            written.unreported_writes += unreported.len() as u64;
            unreported.truncate(UNREPORTED_SAMPLE);
            written.last_unreported = unreported;
        }
        written.since_walk.clear();
        written.last_walk = Some(Instant::now());
    }

    fn commit_staged(
        &self,
        open: &Open,
        message: &str,
        staging: &Staging,
    ) -> Result<Option<git2::Oid>, CommitGit2Error> {
        let repo = &open.repo;
        let mut index = repo.index().map_err(CommitGit2Error::Other)?;
        // Stage through libgit2's stat cache: an entry whose size and mtime
        // are unchanged keeps its cached blob OID and is not re-read, so a
        // commit costs what changed rather than the whole tree. The cache can
        // name a blob that is gone from the object database (a store carried
        // across libgit2/git versions, or an interrupted operation); then
        // `write_tree` fails, and through the wiki migration that crash-looped
        // the server at boot (#594). That is the recovery path: drop the index
        // and hash every file again, once.
        let touched = match staging {
            Staging::Everything => {
                let walked =
                    stage_working_tree(&self.root, &mut index).map_err(CommitGit2Error::Other)?;
                // The walk only skips ledgers it would add or update; one a
                // past version already tracks, and that has not changed, is
                // not offered to it at all.
                untrack_ledgers(&self.root, &mut index, None).map_err(CommitGit2Error::Other)?;
                self.record_walk(&walked);
                walked.len()
            }
            Staging::Paths(paths) => {
                stage_paths(&self.root, &mut index, paths).map_err(CommitGit2Error::Other)?
            }
        };
        debug!(
            touched,
            walked = matches!(staging, Staging::Everything),
            "staged"
        );
        let tree_oid = match index.write_tree() {
            Ok(oid) => oid,
            Err(e) => {
                warn!(
                    error = %e,
                    "index could not be written as a tree; re-hashing the working tree (#594)"
                );
                index.clear().map_err(CommitGit2Error::Other)?;
                stage_working_tree(&self.root, &mut index).map_err(CommitGit2Error::Other)?;
                index.write().map_err(CommitGit2Error::Other)?;
                index.write_tree().map_err(CommitGit2Error::Other)?
            }
        };
        // Every commit, including the one with nothing to commit, leaves the
        // index file matching the tree: another process (the git CLI, an
        // editor, a backup) reads the file, and a stale one shows the
        // checkpointed pages as both staged and unstaged (#983). Written
        // after `write_tree`, it also carries the tree cache.
        index.write().map_err(CommitGit2Error::Other)?;

        // If the index matches HEAD, there is nothing to commit.
        if let Ok(head) = repo.head()
            && let Some(target) = head.target()
            && let Ok(parent_commit) = repo.find_commit(target)
            && parent_commit.tree_id() == tree_oid
        {
            debug!("working tree clean; no commit");
            return Ok(None);
        }
        // Fresh repo with no HEAD yet: still skip the commit if there
        // is nothing staged. Otherwise we'd produce an "initial" commit
        // pointing at the empty tree, which surprises both `git log`
        // and our own callers.
        if repo.head().is_err() && index.is_empty() {
            debug!("fresh repo, empty index; no commit");
            return Ok(None);
        }
        let tree = repo.find_tree(tree_oid).map_err(CommitGit2Error::Other)?;
        let sig = Signature::now(COMMIT_AUTHOR_NAME, COMMIT_AUTHOR_EMAIL)
            .map_err(CommitGit2Error::Other)?;

        let parents: Vec<git2::Commit<'_>> = match repo.head() {
            Ok(head) => match head.target() {
                Some(oid) => vec![repo.find_commit(oid).map_err(CommitGit2Error::Other)?],
                None => Vec::new(),
            },
            Err(_) => Vec::new(),
        };
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .map_err(CommitGit2Error::Other)?;
        debug!(oid = %oid, "wiki commit");
        Ok(Some(oid))
    }

    /// Count commits reachable from HEAD. Returns 0 for an empty or unreadable repo.
    /// Useful for the test suite + for `ai-memory status`.
    #[must_use]
    pub fn commit_count(&self) -> usize {
        self.commit_count_checked().unwrap_or(0)
    }

    pub(crate) fn commit_count_checked(&self) -> WikiResult<usize> {
        let repo = match open_validated_repository(&self.root) {
            Ok(repo) => repo,
            Err(OpenRepositoryError::Git(error)) => {
                #[cfg(windows)]
                if should_try_commit_cli_fallback(&error) {
                    return commit_count_fallback_checked(&self.root);
                }
                return Err(map_git_err(error));
            }
            Err(OpenRepositoryError::Wiki(error)) => return Err(error),
        };
        let Ok(mut walk) = repo.revwalk() else {
            return Ok(0);
        };
        if walk.push_head().is_err() {
            return Ok(0);
        }
        Ok(walk.count())
    }

    /// Return the most recent commits reachable from HEAD.
    ///
    /// Empty repositories return an empty list.
    ///
    /// # Errors
    /// Propagates any underlying libgit2 error.
    pub fn recent_checkpoints(&self, limit: usize) -> WikiResult<Vec<Checkpoint>> {
        let repo = match open_validated_repository(&self.root) {
            Ok(repo) => repo,
            Err(OpenRepositoryError::Git(error)) => {
                #[cfg(windows)]
                if should_try_commit_cli_fallback(&error) {
                    return recent_checkpoints_fallback(&self.root, limit, error);
                }
                return Err(map_git_err(error));
            }
            Err(OpenRepositoryError::Wiki(error)) => return Err(error),
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut walk = repo.revwalk().map_err(map_git_err)?;
        if walk.push_head().is_err() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(limit.min(100));
        for oid in walk.take(limit) {
            let oid = oid.map_err(map_git_err)?;
            let commit = repo.find_commit(oid).map_err(map_git_err)?;
            out.push(Checkpoint {
                oid: oid.to_string(),
                summary: commit
                    .summary()
                    .ok()
                    .flatten()
                    .unwrap_or("(no summary)")
                    .to_string(),
                time: commit.time().seconds(),
            });
        }
        Ok(out)
    }

    /// Return up to `limit` commits, newest first, that changed the file at
    /// `path` (relative to the repo root), including the commit that deleted
    /// it. At most `max_walk` commits are inspected, so a page untouched for a
    /// long history costs a bounded walk rather than the whole repository.
    ///
    /// # Errors
    /// Propagates any underlying libgit2 error.
    pub fn path_checkpoints(
        &self,
        path: &Path,
        limit: usize,
        max_walk: usize,
    ) -> WikiResult<Vec<Checkpoint>> {
        let repo = open_validated_repository(&self.root).map_err(|error| match error {
            OpenRepositoryError::Git(error) => map_git_err(error),
            OpenRepositoryError::Wiki(error) => error,
        })?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut walk = repo.revwalk().map_err(map_git_err)?;
        if walk.push_head().is_err() {
            return Ok(Vec::new());
        }
        let blob_at = |commit: &git2::Commit<'_>| {
            commit
                .tree()
                .ok()
                .and_then(|tree| tree.get_path(path).ok())
                .map(|entry| entry.id())
        };
        let mut out = Vec::new();
        for oid in walk.take(max_walk) {
            let oid = oid.map_err(map_git_err)?;
            let commit = repo.find_commit(oid).map_err(map_git_err)?;
            let parent_blob = commit.parent(0).ok().and_then(|p| blob_at(&p));
            if blob_at(&commit) == parent_blob {
                continue;
            }
            out.push(Checkpoint {
                oid: oid.to_string(),
                summary: commit
                    .summary()
                    .ok()
                    .flatten()
                    .unwrap_or("(no summary)")
                    .to_string(),
                time: commit.time().seconds(),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Read `path` as it existed at `rev`.
    ///
    /// `path` is relative to the wiki repo root. The returned bytes are the
    /// blob contents exactly as stored in git.
    ///
    /// # Errors
    /// Returns [`WikiError`] when the revision, path, or blob cannot be read.
    pub fn file_at_rev(&self, rev: &str, path: &Path) -> WikiResult<Vec<u8>> {
        let repo = match open_validated_repository(&self.root) {
            Ok(repo) => repo,
            Err(OpenRepositoryError::Git(error)) => {
                #[cfg(windows)]
                if should_try_commit_cli_fallback(&error) {
                    return file_at_rev_fallback(&self.root, rev, path, map_git_err(error));
                }
                return Err(map_git_err(error));
            }
            Err(OpenRepositoryError::Wiki(error)) => return Err(error),
        };
        let object = repo.revparse_single(rev).map_err(map_git_err)?;
        let commit = object.peel_to_commit().map_err(map_git_err)?;
        let tree = commit.tree().map_err(map_git_err)?;
        let entry = tree.get_path(path).map_err(map_git_err)?;
        let blob = entry
            .to_object(&repo)
            .map_err(map_git_err)?
            .peel(ObjectType::Blob)
            .map_err(map_git_err)?;
        let blob = blob.as_blob().ok_or_else(|| {
            WikiError::Io(std::io::Error::other(format!(
                "{} at {rev} is not a file",
                path.display()
            )))
        })?;
        Ok(blob.content().to_vec())
    }
}

/// The writes into the tree, each reporting its path; `clippy.toml`
/// refuses the raw calls. Each holds the commit lock for the write, so a
/// commit never reads a file mid-write ("file changed before we could
/// read it" would drop the commit).
#[allow(clippy::disallowed_methods)]
impl GitAdapter {
    fn confined(&self, path: &Path, prepare: crate::confinement::Prepare) -> WikiResult<PathBuf> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| WikiError::Confinement {
                path: path.to_path_buf(),
                reason: "path is outside the wiki root",
            })?;
        crate::confinement::tree_path(&self.root, relative, prepare)
    }

    fn writing(&self) -> MutexGuard<'_, Option<Open>> {
        self.commit_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `bytes` to `path` atomically (tmp, rename, fsync).
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn write_atomic(&self, path: &Path, bytes: &[u8]) -> WikiResult<()> {
        let _writing = self.writing();
        let path = self.confined(path, crate::confinement::Prepare::Parents)?;
        crate::atomic::write_atomic(&path, bytes)?;
        self.mark_written(&path);
        Ok(())
    }

    /// Persist a temp file over `path` and return the persisted file.
    pub(crate) fn persist(
        &self,
        tmp: tempfile::NamedTempFile,
        path: &Path,
    ) -> WikiResult<std::fs::File> {
        let _writing = self.writing();
        let path = self.confined(path, crate::confinement::Prepare::Parents)?;
        let file = crate::atomic::persist_with_retry(tmp, &path)?;
        self.mark_written(&path);
        Ok(file)
    }

    /// Append `bytes` to `path`, creating it and its parent as needed.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn append(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.append_checked(path, bytes)
            .map_err(crate::error::into_io_error)
    }

    pub(crate) fn append_checked(&self, path: &Path, bytes: &[u8]) -> WikiResult<()> {
        let _writing = self.writing();
        use std::io::Write as _;
        let path = self.confined(path, crate::confinement::Prepare::Parents)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?; // lgtm [rust/path-injection]
        file.write_all(bytes)?;
        file.sync_data()?;
        self.mark_written(&path);
        Ok(())
    }

    /// Move `from` to `to`.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.rename_checked(from, to)
            .map_err(crate::error::into_io_error)
    }

    pub(crate) fn rename_checked(&self, from: &Path, to: &Path) -> WikiResult<()> {
        let _writing = self.writing();
        let from = self.confined(from, crate::confinement::Prepare::Inspect)?;
        let to = self.confined(to, crate::confinement::Prepare::Parents)?;
        std::fs::rename(&from, &to)?;
        self.mark_written(&from);
        self.mark_written(&to);
        Ok(())
    }

    /// Remove one file.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.remove_file_checked(path)
            .map_err(crate::error::into_io_error)
    }

    pub(crate) fn remove_file_checked(&self, path: &Path) -> WikiResult<()> {
        let _writing = self.writing();
        let path = self.confined(path, crate::confinement::Prepare::Inspect)?;
        std::fs::remove_file(&path)?; // lgtm [rust/path-injection]
        self.mark_written(&path);
        Ok(())
    }

    /// Remove a directory and everything under it.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.remove_dir_all_checked(path)
            .map_err(crate::error::into_io_error)
    }

    pub(crate) fn remove_dir_all_checked(&self, path: &Path) -> WikiResult<()> {
        let _writing = self.writing();
        let path = self.confined(path, crate::confinement::Prepare::Inspect)?;
        std::fs::remove_dir_all(&path)?;
        self.mark_written(&path);
        Ok(())
    }
}

/// Forward slashes whatever the platform, for pathspecs and page paths.
pub(crate) fn slash_path(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Bring the index in line with the working tree: refresh modified
/// entries, drop deleted ones, add untracked files. libgit2 does all three
/// from one index-to-workdir diff. Returns the paths touched; does not
/// write the index file.
///
/// Ledgers are skipped: a positive return from the callback leaves that path
/// alone, neither added nor refreshed.
fn stage_working_tree(root: &Path, index: &mut git2::Index) -> Result<Vec<PathBuf>, git2::Error> {
    let mut touched = Vec::new();
    index.add_all(
        ["*"].iter(),
        IndexAddOption::DEFAULT,
        Some(&mut |path: &Path, _: &[u8]| {
            if is_ledger_file(root, path) {
                return 1;
            }
            touched.push(path.to_path_buf());
            0
        }),
    )?;
    Ok(touched)
}

/// True when `rel` (relative to the wiki root) is a raw hook event ledger:
/// a `log.md` / `log-YYYY-MM.md` file whose content also opens with a hook log
/// entry.
///
/// Both halves are needed, as everywhere else a ledger is recognized
/// ([`crate::ledger`]): an ordinary page may be named `log-2026-09.md`, and it
/// must stay versioned. The content half reads at most
/// `GATE_PREFIX_BYTES` from the file, so a ledger of any size costs one small
/// read per commit.
///
/// Why this is not a `.gitignore` or `info/exclude` entry: those match names,
/// so one entry would also hide an ordinary page that wears a ledger's name,
/// and the entry would outlive the file's being a ledger.
fn is_ledger_file(root: &Path, rel: &Path) -> bool {
    rel.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(ai_memory_core::log_ledger::is_log_ledger_path)
        && crate::ledger::opens_with_log_ledger(&root.join(rel))
}

/// Drop ledgers that earlier versions committed from the index, under the
/// directory `under` (a `/`-separated path) or everywhere when it is `None`.
/// The files stay on disk: only the index entry goes, so the next commit's tree
/// no longer names them and they stop growing the repository. Their old blobs
/// remain in history. Returns how many entries were dropped.
fn untrack_ledgers(
    root: &Path,
    index: &mut git2::Index,
    under: Option<&str>,
) -> Result<usize, git2::Error> {
    let prefix = under.map(|dir| format!("{}/", dir.trim_end_matches('/')));
    let tracked: Vec<PathBuf> = index
        .iter()
        .filter_map(|entry| String::from_utf8(entry.path).ok())
        .filter(|path| prefix.as_ref().is_none_or(|p| path.starts_with(p)))
        .filter(|path| {
            // The shape test is a string compare; only a shape match pays for
            // the file read.
            path.rsplit('/')
                .next()
                .is_some_and(ai_memory_core::log_ledger::is_log_ledger_path)
        })
        .map(PathBuf::from)
        .filter(|path| is_ledger_file(root, path))
        .collect();
    for path in &tracked {
        index.remove_path(path)?;
    }
    if !tracked.is_empty() {
        debug!(
            count = tracked.len(),
            "hook event ledgers dropped from the index; the files stay on disk"
        );
    }
    Ok(tracked.len())
}
/// Stage only `paths`: a file by path, a directory with its subtree, a
/// gone path by removing its entries. Does not write the index file.
fn stage_paths(
    root: &Path,
    index: &mut git2::Index,
    paths: &BTreeSet<PathBuf>,
) -> Result<usize, git2::Error> {
    for rel in paths {
        if rel
            .components()
            .any(|c| ai_memory_core::is_git_reserved_component(&c.as_os_str().to_string_lossy()))
        {
            warn!(path = %rel.display(), "skipping invalid git path with git-reserved component");
            continue;
        }
        let abs = root.join(rel);
        if abs.is_dir() {
            let spec = slash_path(rel);
            index.add_all(
                [spec.as_str()].iter(),
                IndexAddOption::DISABLE_PATHSPEC_MATCH,
                Some(&mut |path: &Path, _: &[u8]| i32::from(is_ledger_file(root, path))),
            )?;
            untrack_ledgers(root, index, Some(&spec))?;
        } else if abs.is_file() {
            if is_ledger_file(root, rel) {
                if index.get_path(rel, 0).is_some() {
                    index.remove_path(rel)?;
                }
            } else {
                index.add_path(rel)?;
            }
        } else if index.get_path(rel, 0).is_some() {
            index.remove_path(rel)?;
        } else {
            // Gone and not a file in the index: a directory, or nothing.
            index.remove_dir(rel, 0)?;
        }
    }
    Ok(paths.len())
}

fn commit_open_error_with_fallback_on<F>(
    windows: bool,
    root: &Path,
    message: &str,
    error: git2::Error,
    fallback: F,
) -> WikiResult<Option<git2::Oid>>
where
    F: FnOnce(&Path, &str, git2::Error) -> WikiResult<Option<git2::Oid>>,
{
    if should_try_commit_cli_fallback_on(windows, &error) {
        validate_cli_fallback_root(root)?;
        fallback(root, message, error)
    } else {
        Err(map_git_err(error))
    }
}

fn should_try_commit_cli_fallback(error: &git2::Error) -> bool {
    should_try_commit_cli_fallback_on(cfg!(windows), error)
}

fn should_try_commit_cli_fallback_on(windows: bool, error: &git2::Error) -> bool {
    !matches!(error.code(), ErrorCode::Owner)
        && (matches!(error.code(), ErrorCode::NotFound)
            || windows
                && matches!(error.class(), git2::ErrorClass::Os)
                && error.message().contains("failed to resolve path"))
}

#[cfg(windows)]
fn commit_all_fallback(
    root: &Path,
    message: &str,
    original: git2::Error,
) -> WikiResult<Option<git2::Oid>> {
    validate_cli_fallback_root(root)?;
    warn!(error = %original, root = %root.display(), "libgit2 commit failed; trying git CLI fallback");
    stage_all_except_ledgers_cli(root)?;
    let diff = git_command(root)
        .args(["diff", "--cached", "--quiet", "--exit-code"])
        .status()
        .map_err(|error| {
            WikiError::Io(std::io::Error::other(format!(
                "{original}; git diff fallback failed to start: {error}"
            )))
        })?;
    if diff.success() {
        return Ok(None);
    }
    run_git(
        root,
        [
            "-c",
            "user.name=ai-memory",
            "-c",
            "user.email=ai-memory@local",
            "commit",
            "-q",
            "-m",
            message,
        ],
    )?;
    let out = git_output(root, ["rev-parse", "HEAD"])?;
    let oid = String::from_utf8_lossy(&out.stdout);
    git2::Oid::from_str(oid.trim())
        .map(Some)
        .map_err(map_git_err)
}

#[cfg(not(windows))]
fn commit_all_fallback(
    _root: &Path,
    _message: &str,
    original: git2::Error,
) -> WikiResult<Option<git2::Oid>> {
    Err(map_git_err(original))
}

#[cfg(windows)]
fn commit_count_fallback_checked(root: &Path) -> WikiResult<usize> {
    validate_cli_fallback_root(root)?;
    let out = git_output(root, ["rev-list", "--count", "HEAD"])?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|error| {
            WikiError::Io(std::io::Error::other(format!(
                "git fallback returned an invalid commit count: {error}"
            )))
        })
}

#[cfg(windows)]
fn recent_checkpoints_fallback(
    root: &Path,
    limit: usize,
    original: git2::Error,
) -> WikiResult<Vec<Checkpoint>> {
    validate_cli_fallback_root(root)?;
    warn!(error = %original, root = %root.display(), "libgit2 log failed; trying git CLI fallback");
    let limit = limit.to_string();
    let out = git_output(root, ["log", "-n", &limit, "--format=%H%x1f%s%x1f%ct"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut checkpoints = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\x1f');
        let (Some(oid), Some(summary), Some(time)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        checkpoints.push(Checkpoint {
            oid: oid.to_string(),
            summary: if summary.is_empty() {
                "(no summary)".to_string()
            } else {
                summary.to_string()
            },
            time: time.parse().unwrap_or_default(),
        });
    }
    Ok(checkpoints)
}

#[cfg(windows)]
fn file_at_rev_fallback(
    root: &Path,
    rev: &str,
    path: &Path,
    original: WikiError,
) -> WikiResult<Vec<u8>> {
    validate_cli_fallback_root(root)?;
    warn!(error = %original, root = %root.display(), "libgit2 show failed; trying git CLI fallback");
    let rel = slash_path(path);
    let spec = format!("{rev}:{rel}");
    let out = git_output(root, ["show", &spec])?;
    Ok(out.stdout)
}

fn init_repo(root: &Path) -> WikiResult<()> {
    match Repository::init(root) {
        Ok(_) => Ok(()),
        Err(error) => init_error_with_fallback_on(cfg!(windows), root, error, init_repo_fallback),
    }
}

fn init_error_with_fallback_on<F>(
    windows: bool,
    root: &Path,
    error: git2::Error,
    fallback: F,
) -> WikiResult<()>
where
    F: FnOnce(&Path, git2::Error) -> WikiResult<()>,
{
    if !windows {
        return Err(map_git_err(error));
    }
    validate_init_fallback_root(root)?;
    fallback(root, error)
}

fn validate_init_fallback_root(root: &Path) -> WikiResult<()> {
    crate::confinement::initialize_root(root)?;
    match std::fs::symlink_metadata(root.join(".git")) {
        Ok(_) => validate_cli_fallback_root(root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn init_repo_fallback(root: &Path, original: git2::Error) -> WikiResult<()> {
    validate_init_fallback_root(root)?;
    warn!(error = %original, root = %root.display(), "libgit2 init failed; trying git CLI fallback");
    run_git(root, ["init", "-q"])?;
    validate_cli_fallback_root(root)
}

#[cfg(not(windows))]
fn init_repo_fallback(_root: &Path, original: git2::Error) -> WikiResult<()> {
    Err(map_git_err(original))
}

#[cfg(windows)]
fn run_git<const N: usize>(root: &Path, args: [&str; N]) -> WikiResult<()> {
    let status = git_command(root)
        .args(args)
        .status()
        .map_err(|error| WikiError::Io(std::io::Error::other(error.to_string())))?;
    if status.success() {
        Ok(())
    } else {
        Err(WikiError::Io(std::io::Error::other(format!(
            "git fallback exited with status {status}"
        ))))
    }
}

/// `git add -A` for the CLI fallback, leaving the hook event ledgers out the
/// way [`stage_working_tree`] does for libgit2.
///
/// Ledgers a past version committed are dropped from the index first, then
/// everything else is added with the ledgers as `exclude` pathspecs, so git
/// never reads a ledger into a blob. Pathspecs go through stdin because a
/// project with a ledger per month can name more files than a Windows command
/// line holds.
#[cfg(windows)]
fn stage_all_except_ledgers_cli(root: &Path) -> WikiResult<()> {
    let mut ledgers = Vec::new();
    collect_ledger_files(root, root, &mut ledgers)?;
    if ledgers.is_empty() {
        return run_git(root, ["add", "-A"]);
    }
    let pathspecs = |prefix: &str, first: &str| -> Vec<u8> {
        let mut input = first.as_bytes().to_vec();
        for ledger in &ledgers {
            input.extend_from_slice(prefix.as_bytes());
            input.extend_from_slice(slash_path(ledger).as_bytes());
            input.push(0);
        }
        input
    };
    run_git_with_stdin(
        root,
        [
            "rm",
            "--cached",
            // Without `--force` git refuses an entry whose staged content
            // matches neither HEAD nor the file (an old version staged an
            // append and never committed it), and that refusal would abort
            // the whole checkpoint. Only paths already validated as ledgers
            // are listed, and only their index entries go.
            "--force",
            "-q",
            "--ignore-unmatch",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ],
        &pathspecs(":(literal)", ""),
    )?;
    run_git_with_stdin(
        root,
        ["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"],
        &pathspecs(":(exclude,literal)", ".\0"),
    )
}

/// Every ledger file under `dir`, relative to `root`, skipping `.git` and
/// symlinks.
#[cfg(windows)]
fn collect_ledger_files(root: &Path, dir: &Path, found: &mut Vec<PathBuf>) -> WikiResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if entry.file_name() != ".git" {
                collect_ledger_files(root, &path, found)?;
            }
        } else if file_type.is_file()
            && let Ok(rel) = path.strip_prefix(root)
            && is_ledger_file(root, rel)
        {
            found.push(rel.to_path_buf());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn run_git_with_stdin<const N: usize>(
    root: &Path,
    args: [&str; N],
    input: &[u8],
) -> WikiResult<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = git_command(root)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| WikiError::Io(std::io::Error::other(error.to_string())))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(WikiError::Io(std::io::Error::other(format!(
            "git fallback exited with status {status}"
        ))))
    }
}

#[cfg(windows)]
fn git_output<const N: usize>(root: &Path, args: [&str; N]) -> WikiResult<std::process::Output> {
    let output = git_command(root)
        .args(args)
        .output()
        .map_err(|error| WikiError::Io(std::io::Error::other(error.to_string())))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(WikiError::Io(std::io::Error::other(format!(
            "git fallback exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))))
    }
}

fn map_git_err(e: git2::Error) -> WikiError {
    // An owner-check failure (`code=Owner`, CVE-2022-24765 dubious-ownership
    // guard) is kept as a distinct variant so startup can log it at ERROR with
    // an actionable fix instead of burying it in a generic WARN. Everything
    // else stays an opaque I/O error, matching prior behaviour.
    if matches!(e.code(), ErrorCode::Owner) {
        warn!(error = %e, "libgit2 wiki owner-validation error");
        return WikiError::GitOwner(e.to_string());
    }
    warn!(error = %e, "libgit2 error");
    WikiError::Io(std::io::Error::other(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tempdir() -> TempDir {
        tempfile::Builder::new()
            .prefix("ai-memory-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn commit_count_public_api_remains_a_plain_usize() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let count: usize = adapter.commit_count();
        assert_eq!(count, 0);
    }

    #[test]
    fn windows_commit_fallback_accepts_established_eligible_errors() {
        let eligible = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "failed to resolve path 'C:\\wiki\\.git'",
        );
        assert!(should_try_commit_cli_fallback_on(true, &eligible));
        assert!(!should_try_commit_cli_fallback_on(false, &eligible));

        let wrong_class = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Repository,
            "failed to resolve path 'C:\\wiki\\.git'",
        );
        assert!(!should_try_commit_cli_fallback_on(true, &wrong_class));
        let not_found = git2::Error::new(
            ErrorCode::NotFound,
            git2::ErrorClass::Repository,
            "not found",
        );
        assert!(should_try_commit_cli_fallback_on(true, &not_found));
        assert!(should_try_commit_cli_fallback_on(false, &not_found));

        let owner = git2::Error::new(
            ErrorCode::Owner,
            git2::ErrorClass::Os,
            "failed to resolve path 'C:\\wiki\\.git'",
        );
        assert!(!should_try_commit_cli_fallback_on(true, &owner));
        let wrong_message = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "repository is corrupt",
        );
        assert!(!should_try_commit_cli_fallback_on(true, &wrong_message));
    }

    #[test]
    fn public_filesystem_wrappers_preserve_io_signatures_and_non_not_found_confinement() {
        fn assert_signatures(
            append: fn(&GitAdapter, &Path, &[u8]) -> std::io::Result<()>,
            rename: fn(&GitAdapter, &Path, &Path) -> std::io::Result<()>,
            remove_file: fn(&GitAdapter, &Path) -> std::io::Result<()>,
            remove_dir_all: fn(&GitAdapter, &Path) -> std::io::Result<()>,
        ) {
            let _ = (append, rename, remove_file, remove_dir_all);
        }
        assert_signatures(
            GitAdapter::append,
            GitAdapter::rename,
            GitAdapter::remove_file,
            GitAdapter::remove_dir_all,
        );

        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let linked = root.join("linked");
        if !link_directory(&outside, &linked) {
            return;
        }
        let error = adapter.append(&linked.join("log.md"), b"x").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn windows_init_error_dispatches_fallback_after_root_preflight() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        std::fs::create_dir(&root).unwrap();
        let called = AtomicBool::new(false);
        let error = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "failed to initialize repository",
        );

        init_error_with_fallback_on(true, &root, error, |validated, _| {
            assert_eq!(validated, root);
            called.store(true, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        assert!(called.load(Ordering::Relaxed));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn invalid_root_never_reaches_init_fallback() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempdir();
        let outside = tmp.path().join("outside");
        let root = tmp.path().join("wiki");
        std::fs::create_dir(&outside).unwrap();
        if !link_directory(&outside, &root) {
            return;
        }
        let called = AtomicBool::new(false);
        let error = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "failed to initialize repository",
        );

        assert!(matches!(
            init_error_with_fallback_on(true, &root, error, |_, _| {
                called.store(true, Ordering::Relaxed);
                Ok(())
            }),
            Err(WikiError::Confinement { .. })
        ));
        assert!(!called.load(Ordering::Relaxed));
    }

    #[test]
    fn init_is_idempotent_and_creates_dotgit() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        assert!(root.join(".git").is_dir());
        let adapter2 = GitAdapter::open_or_init(&root).unwrap();
        assert_eq!(adapter.commit_count(), 0);
        assert!(adapter2.recent_checkpoints(1).unwrap().is_empty());
    }

    #[cfg(unix)]
    fn link_directory(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[cfg(windows)]
    fn link_directory(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_dir(target, link) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(1314) => {
                eprintln!("skipping .git reparse assertion: Windows privilege unavailable");
                false
            }
            Err(error) => panic!("failed to create .git directory link: {error}"),
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn git_metadata_link_is_refused_without_touching_external_repository() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        std::fs::create_dir(&root).unwrap();
        let external = tmp.path().join("external-git");
        Repository::init_bare(&external).unwrap();
        let before = std::fs::read(external.join("HEAD")).unwrap();
        if !link_directory(&external, &root.join(".git")) {
            return;
        }

        assert!(matches!(
            GitAdapter::open_or_init(&root),
            Err(WikiError::Confinement { .. })
        ));
        assert_eq!(std::fs::read(external.join("HEAD")).unwrap(), before);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn eligible_windows_open_error_dispatches_the_fallback_after_validation() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let called = AtomicBool::new(false);
        let error = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "failed to resolve path 'C:\\wiki\\.git'",
        );

        assert!(
            commit_open_error_with_fallback_on(
                true,
                &adapter.root,
                "fallback",
                error,
                |root, message, _| {
                    assert_eq!(root, adapter.root);
                    assert_eq!(message, "fallback");
                    called.store(true, Ordering::Relaxed);
                    Ok(None)
                },
            )
            .unwrap()
            .is_none()
        );
        assert!(called.load(Ordering::Relaxed));
    }

    fn assert_eligible_fallback_is_refused(root: &Path) {
        use std::sync::atomic::{AtomicBool, Ordering};

        let called = AtomicBool::new(false);
        let error = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "failed to resolve path 'C:\\wiki\\.git'",
        );
        assert!(
            commit_open_error_with_fallback_on(true, root, "refused", error, |_, _, _| {
                called.store(true, Ordering::Relaxed);
                Ok(None)
            },)
            .is_err()
        );
        assert!(!called.load(Ordering::Relaxed));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn linked_git_metadata_never_reaches_commit_fallback() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        adapter.close();
        let original = root.join(".git-original");
        std::fs::rename(root.join(".git"), &original).unwrap();
        if !link_directory(&original, &root.join(".git")) {
            return;
        }

        assert_eligible_fallback_is_refused(&adapter.root);
    }

    #[test]
    fn gitfile_metadata_never_reaches_commit_fallback() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        adapter.close();
        let original = root.join(".git-original");
        std::fs::rename(root.join(".git"), &original).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", original.display()),
        )
        .unwrap();

        assert_eligible_fallback_is_refused(&adapter.root);
    }

    #[test]
    fn out_of_root_worktree_never_reaches_commit_fallback() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        Repository::open(&root)
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.worktree", outside.to_str().unwrap())
            .unwrap();

        assert_eligible_fallback_is_refused(&adapter.root);
    }

    #[test]
    fn fallback_preflight_refuses_invalid_git_metadata() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        std::fs::write(root.join(".git/commondir"), "../outside\n").unwrap();

        assert!(matches!(
            validate_cli_fallback_root(&adapter.root),
            Err(WikiError::Confinement { .. })
        ));
    }

    #[test]
    fn fallback_resolved_paths_must_match_the_validated_repository() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let git_dir = root.join(".git");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();

        assert!(
            validate_resolved_repository_paths(&root, &git_dir, &root, &git_dir, &git_dir,).is_ok()
        );
        for (worktree, resolved_git_dir, common_dir) in [
            (outside.as_path(), git_dir.as_path(), git_dir.as_path()),
            (root.as_path(), outside.as_path(), git_dir.as_path()),
            (root.as_path(), git_dir.as_path(), outside.as_path()),
        ] {
            assert!(matches!(
                validate_resolved_repository_paths(
                    &root,
                    &git_dir,
                    worktree,
                    resolved_git_dir,
                    common_dir,
                ),
                Err(WikiError::Confinement { .. })
            ));
        }
        assert_eq!(adapter.root(), root);
    }

    #[test]
    fn gitfile_redirect_outside_is_refused_without_touching_target() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        std::fs::create_dir(&root).unwrap();
        let external = tmp.path().join("external-git");
        Repository::init_bare(&external).unwrap();
        let before = std::fs::read(external.join("HEAD")).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", external.display()),
        )
        .unwrap();

        assert!(matches!(
            GitAdapter::open_or_init(&root),
            Err(WikiError::Confinement { .. })
        ));
        assert_eq!(std::fs::read(external.join("HEAD")).unwrap(), before);
    }

    #[test]
    fn git_metadata_redirect_files_are_refused_before_repository_open() {
        for relative in [
            "commondir",
            "objects/info/alternates",
            "objects/info/http-alternates",
        ] {
            let tmp = tempdir();
            let root = tmp.path().join("wiki");
            let adapter = GitAdapter::open_or_init(&root).unwrap();
            let redirect = root.join(".git").join(relative);
            std::fs::create_dir_all(redirect.parent().unwrap()).unwrap();
            std::fs::write(&redirect, "../outside\n").unwrap();

            assert!(matches!(
                adapter.recent_checkpoints(0),
                Err(WikiError::Confinement { .. })
            ));
        }
    }

    #[test]
    fn wiki_gitdir_enables_fsync_on_init_and_on_pre_existing_repos() {
        let tmp = tempdir();
        let fresh = tmp.path().join("fresh");
        GitAdapter::open_or_init(&fresh).unwrap();
        let fsync = |root: &Path| {
            Repository::open(root)
                .unwrap()
                .config()
                .unwrap()
                .open_level(ConfigLevel::Local)
                .unwrap()
                .get_bool("core.fsyncObjectFiles")
                .unwrap_or(false)
        };
        assert!(fsync(&fresh));

        let existing = tmp.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        Repository::init(&existing).unwrap();
        assert!(!fsync(&existing));
        GitAdapter::open_or_init(&existing).unwrap();
        assert!(fsync(&existing));
    }

    #[test]
    fn configured_worktree_outside_the_wiki_root_is_refused() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let external = tmp.path().join("external-worktree");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("canary"), "outside canary").unwrap();
        let repo = Repository::init(&root).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.worktree", external.to_str().unwrap())
            .unwrap();
        drop(repo);

        assert!(matches!(
            GitAdapter::open_or_init(&root),
            Err(WikiError::Confinement { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(external.join("canary")).unwrap(),
            "outside canary"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn checkpoint_and_revision_reads_refuse_a_replaced_git_directory() {
        let (_tmp, root, adapter) = committed(&[("a.md", "one")]);
        adapter.close();
        let original = root.join(".git-original");
        std::fs::rename(root.join(".git"), &original).unwrap();
        let external = root.parent().unwrap().join("external-git");
        Repository::init_bare(&external).unwrap();
        let before = std::fs::read(external.join("HEAD")).unwrap();
        if !link_directory(&external, &root.join(".git")) {
            return;
        }

        assert!(matches!(
            adapter.commit_all("refused"),
            Err(WikiError::Confinement { .. })
        ));
        assert!(matches!(
            adapter.recent_checkpoints(1),
            Err(WikiError::Confinement { .. })
        ));
        assert!(matches!(
            adapter.file_at_rev("HEAD", Path::new("a.md")),
            Err(WikiError::Confinement { .. })
        ));
        assert!(matches!(
            adapter.commit_count_checked(),
            Err(WikiError::Confinement { .. })
        ));
        assert_eq!(adapter.commit_count(), 0);
        assert_eq!(std::fs::read(external.join("HEAD")).unwrap(), before);
    }

    #[test]
    fn path_checkpoints_lists_changes_and_deletion_and_stops_at_the_walk_cap() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        std::fs::write(root.join("page.md"), "v1").unwrap();
        adapter.commit_all("write v1").unwrap();
        std::fs::write(root.join("page.md"), "v2").unwrap();
        adapter.commit_all("write v2").unwrap();
        std::fs::remove_file(root.join("page.md")).unwrap();
        adapter.commit_all("delete").unwrap();
        for i in 0..3 {
            std::fs::write(root.join("other.md"), format!("o{i}")).unwrap();
            adapter.commit_all("unrelated").unwrap();
        }
        let summaries = |hits: Vec<Checkpoint>| -> Vec<String> {
            hits.into_iter().map(|c| c.summary).collect()
        };

        let all = adapter
            .path_checkpoints(Path::new("page.md"), 10, 100)
            .unwrap();
        assert_eq!(summaries(all), ["delete", "write v2", "write v1"]);
        let limited = adapter
            .path_checkpoints(Path::new("page.md"), 1, 100)
            .unwrap();
        assert_eq!(summaries(limited), ["delete"]);
        // Three unrelated commits sit on top: a walk of four sees only the
        // delete, a walk of three sees nothing.
        let capped = adapter
            .path_checkpoints(Path::new("page.md"), 10, 4)
            .unwrap();
        assert_eq!(summaries(capped), ["delete"]);
        assert!(
            adapter
                .path_checkpoints(Path::new("page.md"), 10, 3)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn commit_all_returns_none_when_clean_some_when_dirty() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        // No changes: returns None.
        assert!(adapter.commit_all("initial").unwrap().is_none());

        // Add a file -> commit -> Some(oid).
        std::fs::write(root.join("foo.md"), "hello").unwrap();
        let oid = adapter.commit_all("add foo").unwrap();
        assert!(oid.is_some());

        // Re-commit with no changes -> None again.
        assert!(adapter.commit_all("no changes").unwrap().is_none());
        assert_eq!(adapter.commit_count(), 1);
    }

    /// #594 as it happens: the index trusts a cached blob OID whose object
    /// is gone from the store. Stat-cache staging cannot see that, so the
    /// tree write refuses it and the commit must recover by re-hashing.
    #[test]
    fn commit_all_recovers_when_the_index_names_a_missing_blob() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        std::fs::write(root.join("cached.md"), "content the index remembers").unwrap();
        adapter.commit_all("first").unwrap();

        // Remove the blob the index entry points at. The file on disk is
        // untouched, so its size and mtime still match the cached entry.
        let blob = {
            let repo = Repository::open(&root).unwrap();
            let index = repo.index().unwrap();
            index.get_path(Path::new("cached.md"), 0).unwrap().id
        };
        let hex = blob.to_string();
        let object = root
            .join(".git")
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        std::fs::remove_file(&object).expect("a fresh repo stores the blob loose");

        std::fs::write(root.join("other.md"), "a second file").unwrap();
        let oid = adapter
            .commit_all("second")
            .expect("the commit recovers by re-hashing the tree");
        assert!(oid.is_some());
        assert!(object.exists(), "the re-hash wrote the blob back");
        assert_eq!(
            adapter.file_at_rev("HEAD", Path::new("cached.md")).unwrap(),
            b"content the index remembers"
        );
        assert_eq!(adapter.commit_count(), 2);
    }

    /// The cost of a commit is what changed, not the size of the wiki: after
    /// a hundred files are committed, changing one must stage one path.
    #[test]
    fn commit_stages_only_what_changed_since_the_last_one() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        for i in 0..100 {
            std::fs::write(root.join(format!("page-{i}.md")), format!("page {i}")).unwrap();
        }
        adapter.commit_all("hundred pages").unwrap();

        let staged = |root: &Path| {
            let repo = Repository::open(root).unwrap();
            let mut index = repo.index().unwrap();
            let touched = stage_working_tree(root, &mut index).unwrap().len();
            index.write().unwrap();
            touched
        };
        assert_eq!(staged(&root), 0, "a clean tree stages nothing");
        std::fs::write(root.join("page-7.md"), "page 7, revised").unwrap();
        assert_eq!(staged(&root), 1, "one changed file stages one path");
        std::fs::remove_file(root.join("page-8.md")).unwrap();
        std::fs::write(root.join("page-100.md"), "new page").unwrap();
        assert_eq!(staged(&root), 2, "a delete and an add stage two paths");
        assert!(adapter.commit_all("edits").unwrap().is_some());
        assert_eq!(adapter.commit_count(), 2);
    }

    fn head_blob(adapter: &GitAdapter, rel: &str) -> Option<String> {
        adapter
            .file_at_rev("HEAD", Path::new(rel))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// `files` committed by the fixture's own walk.
    fn committed(files: &[(&str, &str)]) -> (TempDir, PathBuf, GitAdapter) {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        for (rel, body) in files {
            write(&root, rel, body);
        }
        adapter.commit_all("initial").unwrap();
        (tmp, root, adapter)
    }

    #[test]
    fn a_commit_stages_the_reported_paths_only() {
        let (_tmp, root, adapter) = committed(&[("page-7.md", "page 7"), ("page-8.md", "page 8")]);
        write(&root, "page-7.md", "seven, revised");
        write(&root, "page-8.md", "eight, revised");
        adapter.mark_written(&root.join("page-7.md"));
        assert_eq!(adapter.written_paths(), vec![PathBuf::from("page-7.md")]);
        assert!(adapter.commit_all("seven").unwrap().is_some());
        assert_eq!(
            head_blob(&adapter, "page-7.md").as_deref(),
            Some("seven, revised")
        );
        assert_eq!(
            head_blob(&adapter, "page-8.md").as_deref(),
            Some("page 8"),
            "the unreported edit is not in this commit"
        );
        assert!(adapter.written_paths().is_empty(), "the report is consumed");

        // Nothing reported: the tree is walked and the edit lands.
        assert!(adapter.commit_all("sweep").unwrap().is_some());
        assert_eq!(
            head_blob(&adapter, "page-8.md").as_deref(),
            Some("eight, revised")
        );
    }

    #[test]
    fn reported_deletions_and_directories_are_staged() {
        let (_tmp, root, adapter) =
            committed(&[("ws/proj/sessions/a.md", "a"), ("gone.md", "gone")]);
        std::fs::remove_file(root.join("gone.md")).unwrap();
        write(&root, "ws/proj/sessions/b.md", "b");
        adapter.mark_written(Path::new("gone.md"));
        adapter.mark_written(&root.join("ws/proj/sessions"));
        assert!(adapter.commit_all("delete and add").unwrap().is_some());
        assert!(head_blob(&adapter, "gone.md").is_none());
        assert_eq!(
            head_blob(&adapter, "ws/proj/sessions/b.md").as_deref(),
            Some("b")
        );
    }

    #[test]
    fn the_repository_directory_is_never_reported() {
        let (_tmp, root, adapter) = committed(&[("a.md", "a")]);
        adapter.mark_written(Path::new(".git/logs/HEAD"));
        adapter.mark_written(&root.join(".git/index"));
        adapter.mark_written(Path::new(".git"));
        adapter.mark_written(Path::new("ws/proj/.git/config"));
        adapter.mark_written(&root.join("ws/proj/.git/hooks/pre-commit"));
        adapter.mark_written(Path::new("ws/proj/git~1/config"));
        assert!(adapter.written_paths().is_empty());
    }

    const LEDGER_BODY: &str = "## [2026-09-05T00:00:00Z] tool Bash: cargo test\n";
    const PROSE_BODY: &str = "# Log\n\nAn ordinary page named like a ledger.\n";

    /// Whether the index names `rel`, from a fresh read of the index file.
    fn indexed(root: &Path, rel: &str) -> bool {
        let repo = Repository::open(root).unwrap();
        let index = repo.index().unwrap();
        index.get_path(Path::new(rel), 0).is_some()
    }

    /// Path-scoped staging (the commit after the first walk): a ledger a past
    /// version tracked is dropped from the index when it is reported, by file
    /// or through its directory, and the file stays on disk. Ordinary files in
    /// the same call, including a page wearing a ledger's name, are staged.
    #[test]
    fn path_staging_untracks_a_ledger_and_keeps_a_page_wearing_its_name() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        write(&root, "p/log-2026-09.md", LEDGER_BODY);
        write(&root, "q/log-2026-09.md", LEDGER_BODY);
        write(&root, "r/log-2026-09.md", PROSE_BODY);
        let repo = Repository::open(&root).unwrap();
        let mut index = repo.index().unwrap();
        for rel in ["p/log-2026-09.md", "q/log-2026-09.md", "r/log-2026-09.md"] {
            index.add_path(Path::new(rel)).unwrap();
        }
        index.write().unwrap();
        drop(adapter);

        write(
            &root,
            "p/log-2026-09.md",
            &format!("{LEDGER_BODY}{LEDGER_BODY}"),
        );
        write(&root, "r/log-2026-09.md", &format!("{PROSE_BODY}edited\n"));
        write(&root, "q/other.md", "page\n");
        // Never tracked, reported through its directory: it must not even be
        // read into a blob.
        let fresh = format!("{LEDGER_BODY}## [2026-09-06T00:00:00Z] stop\n");
        write(&root, "s/log-2026-08.md", &fresh);
        let mut index = repo.index().unwrap();
        // `p` is reported by file, `q` and `s` through their directories.
        let paths: BTreeSet<PathBuf> = ["p/log-2026-09.md", "q", "r/log-2026-09.md", "s"]
            .iter()
            .map(PathBuf::from)
            .collect();
        stage_paths(&root, &mut index, &paths).unwrap();
        index.write().unwrap();

        assert!(!indexed(&root, "p/log-2026-09.md"), "file report");
        assert!(!indexed(&root, "q/log-2026-09.md"), "directory report");
        assert!(
            !indexed(&root, "s/log-2026-08.md"),
            "untracked, by directory"
        );
        assert!(
            !repo
                .odb()
                .unwrap()
                .exists(git2::Oid::hash_object(ObjectType::Blob, fresh.as_bytes()).unwrap()),
            "a ledger under a reported directory was hashed into git"
        );
        assert!(indexed(&root, "q/other.md"), "control: the page beside it");
        assert!(indexed(&root, "r/log-2026-09.md"), "control: a page");
        assert_eq!(
            std::fs::read_to_string(root.join("p/log-2026-09.md")).unwrap(),
            format!("{LEDGER_BODY}{LEDGER_BODY}"),
            "the file on disk is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("q/log-2026-09.md")).unwrap(),
            LEDGER_BODY
        );
    }

    /// The CLI fallback (Windows only, when libgit2 cannot open the repo)
    /// must leave ledgers out as well, and must never read one into a blob.
    #[cfg(windows)]
    #[test]
    fn cli_fallback_commit_leaves_ledgers_out_and_untracks_a_tracked_one() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        // A repo-local setting beats a contributor's global `commit.gpgsign`.
        Repository::open(&root)
            .unwrap()
            .config()
            .unwrap()
            .set_bool("commit.gpgsign", false)
            .unwrap();
        write(&root, "p/log-2026-07.md", LEDGER_BODY);
        run_git(&root, ["add", "p/log-2026-07.md"]).unwrap();
        run_git(
            &root,
            [
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@local",
                "commit",
                "-q",
                "--no-gpg-sign",
                "-m",
                "fixture: tracked by an earlier version",
            ],
        )
        .unwrap();
        // Content no other file or commit holds, so a blob for it can only come
        // from staging this file.
        let new_ledger = format!("{LEDGER_BODY}## [2026-09-06T00:00:00Z] stop\n");
        write(&root, "p/log-2026-09.md", &new_ledger);
        write(&root, "p/log.md", PROSE_BODY);
        write(&root, "p/page.md", "page\n");

        let oid = commit_all_fallback(&root, "fallback", git2::Error::from_str("forced"))
            .unwrap()
            .expect("the page is new");

        let repo = Repository::open(&root).unwrap();
        let tree = repo.find_commit(oid).unwrap().tree().unwrap();
        assert!(tree.get_path(Path::new("p/page.md")).is_ok());
        assert!(tree.get_path(Path::new("p/log.md")).is_ok(), "a page");
        assert!(tree.get_path(Path::new("p/log-2026-09.md")).is_err());
        assert!(
            tree.get_path(Path::new("p/log-2026-07.md")).is_err(),
            "the tracked ledger was untracked"
        );
        assert!(root.join("p/log-2026-07.md").is_file(), "and kept on disk");
        let unseen = git2::Oid::hash_object(ObjectType::Blob, new_ledger.as_bytes()).unwrap();
        assert!(
            !repo.odb().unwrap().exists(unseen),
            "git read the new ledger into a blob"
        );
        drop(adapter);
    }

    /// The state an older version could leave behind: it staged an append to a
    /// ledger and never committed it, and the hook appended again after that.
    /// HEAD, the index and the file then hold three different contents, which
    /// plain `git rm --cached` refuses; that refusal must not abort the
    /// checkpoint, which still has ordinary pages to save.
    #[cfg(windows)]
    #[test]
    fn cli_fallback_untracks_a_ledger_whose_index_matches_neither_head_nor_disk() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        Repository::open(&root)
            .unwrap()
            .config()
            .unwrap()
            .set_bool("commit.gpgsign", false)
            .unwrap();
        let ledger = "p/log-2026-09.md";
        let version_a = LEDGER_BODY.to_string();
        let version_b = format!(
            "{version_a}## [2026-09-06T00:00:00Z] stop
"
        );
        let version_c = format!(
            "{version_b}## [2026-09-07T00:00:00Z] start
"
        );

        write(&root, ledger, &version_a);
        run_git(&root, ["add", ledger]).unwrap();
        run_git(
            &root,
            [
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@local",
                "commit",
                "-q",
                "--no-gpg-sign",
                "-m",
                "fixture: HEAD holds version A",
            ],
        )
        .unwrap();
        write(&root, ledger, &version_b);
        run_git(&root, ["add", ledger]).unwrap();
        write(&root, ledger, &version_c);
        write(
            &root,
            "p/page.md",
            "page
",
        );

        let oid = commit_all_fallback(&root, "fallback", git2::Error::from_str("forced"))
            .expect("the unforced removal aborted the whole checkpoint")
            .expect("the page is new");

        let repo = Repository::open(&root).unwrap();
        let tree = repo.find_commit(oid).unwrap().tree().unwrap();
        assert!(
            tree.get_path(Path::new("p/page.md")).is_ok(),
            "the ordinary page was not committed"
        );
        assert!(
            tree.get_path(Path::new(ledger)).is_err(),
            "the ledger is still in the commit"
        );
        assert!(!indexed(&root, ledger), "the ledger is still in the index");
        assert_eq!(
            std::fs::read_to_string(root.join(ledger)).unwrap(),
            version_c,
            "the file on disk was changed"
        );
        drop(adapter);
    }

    #[test]
    fn the_adapters_writes_report_their_paths() {
        let (_tmp, root, adapter) = committed(&[("a.md", "a")]);
        adapter.write_atomic(&root.join("b.md"), b"b").unwrap();
        adapter.append(&root.join("log.md"), b"line\n").unwrap();
        adapter
            .rename(&root.join("a.md"), &root.join("c.md"))
            .unwrap();
        adapter.remove_file(&root.join("b.md")).unwrap();
        let reported: Vec<PathBuf> = adapter.written_paths();
        assert_eq!(
            reported,
            ["a.md", "b.md", "c.md", "log.md"]
                .map(PathBuf::from)
                .to_vec()
        );
        adapter.commit_all("writes").unwrap();
        assert!(head_blob(&adapter, "a.md").is_none());
        assert!(head_blob(&adapter, "b.md").is_none());
        assert_eq!(head_blob(&adapter, "c.md").as_deref(), Some("a"));
        assert_eq!(head_blob(&adapter, "log.md").as_deref(), Some("line\n"));
    }

    #[test]
    fn the_tree_is_swept_on_the_interval_and_when_a_report_is_untrusted() {
        let (tmp, root, adapter) = committed(&[("tracked.md", "0"), ("bypassed.md", "old")]);
        write(&root, "bypassed.md", "edited behind the wiki");
        write(&root, "tracked.md", "1");
        adapter.mark_written(Path::new("tracked.md"));
        adapter.commit_all("session 1").unwrap();
        assert_eq!(
            head_blob(&adapter, "bypassed.md").as_deref(),
            Some("old"),
            "a path-scoped commit within the interval leaves the bypassed edit"
        );

        adapter.age_last_walk();
        write(&root, "tracked.md", "2");
        adapter.mark_written(Path::new("tracked.md"));
        adapter.commit_all("session 2").unwrap();
        assert_eq!(
            head_blob(&adapter, "bypassed.md").as_deref(),
            Some("edited behind the wiki"),
            "the sweep walks the tree"
        );

        write(&root, "bypassed.md", "edited again");
        write(&root, "tracked.md", "x");
        adapter.mark_written(Path::new("tracked.md"));
        adapter.mark_written(tmp.path().join("elsewhere.md").as_path());
        adapter.commit_all("untrusted report").unwrap();
        assert_eq!(
            head_blob(&adapter, "bypassed.md").as_deref(),
            Some("edited again"),
            "a report naming a path outside the root walks the tree"
        );
    }

    /// A directory report covers its subtree.
    #[test]
    fn the_sweep_names_the_writes_nobody_reported() {
        let (_tmp, root, adapter) = committed(&[("ws/proj/a.md", "a")]);
        assert_eq!(adapter.sweep_snapshot(), SweepSnapshot::default());

        write(&root, "ws/proj/a.md", "a2");
        write(&root, "ws/proj/b.md", "nobody reported this");
        adapter.mark_written(Path::new("ws/proj/a.md"));
        adapter.age_last_walk();
        adapter.commit_all("sweep").unwrap();
        assert_eq!(
            adapter.sweep_snapshot(),
            SweepSnapshot {
                unreported_writes: 1,
                last_unreported: vec![PathBuf::from("ws/proj/b.md")],
            }
        );

        write(&root, "ws/proj/c.md", "c");
        adapter.mark_written(Path::new("ws/proj"));
        adapter.age_last_walk();
        adapter.commit_all("sweep again").unwrap();
        let snapshot = adapter.sweep_snapshot();
        assert_eq!(snapshot.unreported_writes, 1, "the count is cumulative");
        assert!(snapshot.last_unreported.is_empty());
    }

    #[test]
    fn concurrent_appends_do_not_fail_a_commit() {
        let (_tmp, root, adapter) = committed(&[("ws/proj/log.md", "")]);
        let ledger = root.join("ws/proj/log.md");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers: Vec<_> = (0..3)
            .map(|_| {
                let adapter = adapter.clone();
                let ledger = ledger.clone();
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        adapter
                            .append(&ledger, b"2026-09-08T00:00:00Z stop x\n")
                            .unwrap();
                    }
                })
            })
            .collect();
        for i in 0..30 {
            std::thread::sleep(Duration::from_millis(2));
            adapter.mark_written(&ledger);
            adapter
                .commit_all(&format!("session {i}"))
                .unwrap_or_else(|e| panic!("commit {i} failed: {e}"));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in writers {
            w.join().unwrap();
        }
        assert!(adapter.commit_count() >= 2);
    }

    /// Not a test: run by hand with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_commit_cost_on_a_large_tree() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let projects = 120usize;
        let per_project = 150usize;
        for p in 0..projects {
            let dir = root.join(format!("ws/proj-{p:04}/sessions"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..per_project {
                std::fs::write(dir.join(format!("s-{f:04}.md")), "# session\n\nbody\n").unwrap();
            }
        }
        let t = Instant::now();
        adapter.commit_all("initial walk").unwrap();
        println!(
            "files {}: initial walk {:?}",
            projects * per_project,
            t.elapsed()
        );

        let t = Instant::now();
        adapter.commit_all("clean walk").unwrap();
        println!("clean walk (nothing reported) {:?}", t.elapsed());

        let mut scoped = Vec::new();
        for i in 0..10 {
            let rel = format!("ws/proj-{:04}/sessions/new-{i}.md", i % projects);
            std::fs::write(root.join(&rel), "# new\n\nbody\n").unwrap();
            adapter.mark_written(Path::new(&rel));
            let t = Instant::now();
            adapter.commit_all(&format!("scoped {i}")).unwrap();
            scoped.push(t.elapsed());
        }
        println!("path-scoped commits: {scoped:?}");
    }
    /// A later adapter's first commit walks from the stale index file
    /// and still commits everything.
    #[test]
    fn a_stale_index_file_costs_a_later_adapter_only_a_walk() {
        let (_tmp, root, adapter) = committed(&[("a.md", "a")]);
        for i in 0..3 {
            write(&root, "a.md", &format!("a{i}"));
            adapter.mark_written(Path::new("a.md"));
            adapter.commit_all(&format!("scoped {i}")).unwrap();
        }
        assert_eq!(head_blob(&adapter, "a.md").as_deref(), Some("a2"));
        write(&root, "b.md", "b");
        let later = GitAdapter::open_or_init(&root).unwrap();
        assert!(later.commit_all("from a fresh adapter").unwrap().is_some());
        assert_eq!(head_blob(&later, "a.md").as_deref(), Some("a2"));
        assert_eq!(head_blob(&later, "b.md").as_deref(), Some("b"));
        // The first adapter's kept index predates that commit.
        write(&root, "a.md", "a3");
        adapter.mark_written(Path::new("a.md"));
        adapter.commit_all("after the other adapter").unwrap();
        assert_eq!(head_blob(&adapter, "a.md").as_deref(), Some("a3"));
        assert_eq!(head_blob(&adapter, "b.md").as_deref(), Some("b"));
    }

    #[test]
    fn a_racy_read_is_the_filesystem_error_that_names_it() {
        let racy = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Filesystem,
            "file changed before we could read it",
        );
        assert!(is_racy_read(&racy));
        let other_class = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "file changed before we could read it",
        );
        assert!(!is_racy_read(&other_class));
        let other_message = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Filesystem,
            "failed to open",
        );
        assert!(!is_racy_read(&other_message));
    }

    #[test]
    fn owner_error_is_surfaced_distinctly() {
        let owner = git2::Error::new(
            ErrorCode::Owner,
            git2::ErrorClass::Config,
            "repository path is not owned by current user",
        );
        assert!(matches!(map_git_err(owner), WikiError::GitOwner(_)));
        let generic = git2::Error::new(
            ErrorCode::GenericError,
            git2::ErrorClass::Os,
            "some other failure",
        );
        assert!(matches!(map_git_err(generic), WikiError::Io(_)));
    }

    /// A racy read keeps the commit path-scoped; any other failure walks.
    #[test]
    fn a_racy_read_failure_does_not_escalate_to_a_walk() {
        let (_tmp, _root, adapter) = committed(&[("a.md", "a")]);
        adapter.mark_written(Path::new("a.md"));
        let staging = adapter.take_staging();
        assert!(matches!(staging, Staging::Paths(_)));
        adapter.keep_pending(staging, true);
        assert_eq!(adapter.written_paths(), vec![PathBuf::from("a.md")]);
        let staging = adapter.take_staging();
        assert!(matches!(&staging, Staging::Paths(p) if p.len() == 1));

        adapter.keep_pending(staging, false);
        assert_eq!(adapter.written_paths(), vec![PathBuf::from("a.md")]);
        assert!(matches!(adapter.take_staging(), Staging::Everything));
    }

    #[test]
    fn persist_and_remove_dir_all_report_their_paths() {
        let (_tmp, root, adapter) = committed(&[("d/x.md", "x"), ("d/y.md", "y")]);
        let mut tmp = tempfile::NamedTempFile::new_in(&root).unwrap();
        std::io::Write::write_all(&mut tmp, b"p").unwrap();
        adapter.persist(tmp, &root.join("p.md")).unwrap();
        adapter.remove_dir_all(&root.join("d")).unwrap();
        assert_eq!(
            adapter.written_paths(),
            ["d", "p.md"].map(PathBuf::from).to_vec()
        );
        assert!(adapter.commit_all("persist and remove").unwrap().is_some());
        assert_eq!(head_blob(&adapter, "p.md").as_deref(), Some("p"));
        assert!(head_blob(&adapter, "d/x.md").is_none());
        assert!(head_blob(&adapter, "d/y.md").is_none());
    }

    /// What another process sees: a repository opened fresh reads the
    /// index file, not the adapter's kept index.
    fn assert_clean_from_outside(root: &Path, context: &str) {
        let repo = Repository::open(root).unwrap();
        let dirty: Vec<(String, git2::Status)> = repo
            .statuses(None)
            .unwrap()
            .iter()
            .map(|entry| (entry.path().unwrap_or_default().to_owned(), entry.status()))
            .collect();
        assert!(dirty.is_empty(), "{context}: {dirty:?}");
        // The git CLI too, where there is one.
        if let Ok(out) = std::process::Command::new("git")
            .args(["status", "--porcelain=v2"])
            .current_dir(root)
            .output()
            && out.status.success()
        {
            let porcelain = String::from_utf8_lossy(&out.stdout);
            assert!(porcelain.is_empty(), "{context}: git status: {porcelain}");
        }
    }

    #[test]
    fn every_commit_leaves_the_index_file_matching_head() {
        let (_tmp, root, adapter) = committed(&[("a.md", "a"), ("b.md", "b")]);
        assert_clean_from_outside(&root, "after the walk");
        for i in 1..=3 {
            write(&root, "a.md", &format!("a{i}"));
            adapter.mark_written(Path::new("a.md"));
            assert!(
                adapter
                    .commit_all(&format!("scoped {i}"))
                    .unwrap()
                    .is_some()
            );
            assert_clean_from_outside(&root, &format!("after scoped commit {i}"));
        }
        assert_eq!(head_blob(&adapter, "a.md").as_deref(), Some("a3"));
    }

    #[test]
    fn a_commit_with_nothing_to_commit_still_writes_the_index_file() {
        let (_tmp, root, adapter) = committed(&[("a.md", "a")]);
        let index_file = root.join(".git/index");
        let stale = std::fs::read(&index_file).unwrap();
        write(&root, "a.md", "a2");
        adapter.mark_written(Path::new("a.md"));
        assert!(adapter.commit_all("a2").unwrap().is_some());
        // The index file falls behind HEAD, as a missed write left it.
        std::fs::write(&index_file, &stale).unwrap();
        adapter.mark_written(Path::new("a.md"));
        assert!(adapter.commit_all("unchanged").unwrap().is_none());
        assert_clean_from_outside(&root, "after the no-op commit");
    }

    #[test]
    fn a_failed_commit_keeps_the_reported_paths() {
        let (_tmp, root, adapter) = committed(&[("a.md", "a")]);
        write(&root, "a.md", "a2");
        adapter.mark_written(Path::new("a.md"));
        // Break the repository; the kept one is dropped first, as a restart would.
        adapter.close();
        let git_dir = root.join(".git");
        std::fs::rename(&git_dir, root.join(".git-parked")).unwrap();
        assert!(adapter.commit_all("broken").is_err());
        std::fs::rename(root.join(".git-parked"), &git_dir).unwrap();
        assert_eq!(adapter.written_paths(), vec![PathBuf::from("a.md")]);
        assert!(adapter.commit_all("retry").unwrap().is_some());
        assert_eq!(head_blob(&adapter, "a.md").as_deref(), Some("a2"));
    }
    /// The stat cache's blind spot: a file rewritten with the same size and
    /// an mtime no newer than the index looks unchanged by stat alone. git
    /// treats an entry whose mtime is not older than the index as "racy"
    /// and re-reads it; the commit must carry the new content, or a page
    /// saved right after a session end would be snapshotted stale. The
    /// rewritten file is given the index file's own mtime so the case is
    /// forced rather than left to the clock.
    #[test]
    fn a_same_size_rewrite_with_an_unchanged_mtime_is_still_committed() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let file = root.join("racy.md");
        std::fs::write(&file, "version A").unwrap();
        adapter.commit_all("A").unwrap();

        std::fs::write(&file, "version B").unwrap();
        let index_written = std::fs::metadata(root.join(".git").join("index"))
            .unwrap()
            .modified()
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(index_written)
            .unwrap();

        adapter.commit_all("B").unwrap();
        assert_eq!(
            adapter.file_at_rev("HEAD", Path::new("racy.md")).unwrap(),
            b"version B"
        );
        assert_eq!(adapter.commit_count(), 2);
    }

    /// Two session ends at once used to collide on libgit2's index lock and
    /// one of them lost its snapshot; now they queue.
    /// The CI flake behind `concurrent_commits_queue_instead_of_failing`: a
    /// walk listed a file that was gone when libgit2 read it, and libgit2
    /// reports that as an `Os`-class "failed to read file into stream" — not
    /// the "changed before" the retry recognized — so the commit failed
    /// instead of retrying. Unrelated errors must still fail fast.
    #[test]
    fn a_file_vanishing_mid_walk_is_a_racy_read_but_other_errors_are_not() {
        let err =
            |class, message: &str| git2::Error::new(git2::ErrorCode::GenericError, class, message);
        assert!(is_racy_read(&err(
            git2::ErrorClass::Os,
            "failed to read file into stream: "
        )));
        assert!(is_racy_read(&err(
            git2::ErrorClass::Filesystem,
            "file changed before we could read it"
        )));
        assert!(!is_racy_read(&err(
            git2::ErrorClass::Os,
            "failed to open file"
        )));
        assert!(!is_racy_read(&err(
            git2::ErrorClass::Index,
            "failed to read file into stream: "
        )));
    }

    #[test]
    fn concurrent_commits_queue_instead_of_failing() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let adapter = adapter.clone();
                let root = root.clone();
                std::thread::spawn(move || {
                    std::fs::write(root.join(format!("s{i}.md")), format!("session {i}")).unwrap();
                    adapter.commit_all(&format!("session {i}")).unwrap()
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // Every file is in HEAD whether its own commit ran or a later one
        // swept it up; nothing was dropped.
        for i in 0..8 {
            assert_eq!(
                adapter
                    .file_at_rev("HEAD", Path::new(&format!("s{i}.md")))
                    .unwrap(),
                format!("session {i}").as_bytes()
            );
        }
    }

    #[test]
    fn commit_all_captures_deletes_too() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();
        std::fs::write(root.join("a.md"), "first").unwrap();
        adapter.commit_all("first").unwrap();
        std::fs::remove_file(root.join("a.md")).unwrap();
        let oid = adapter.commit_all("remove a").unwrap();
        assert!(oid.is_some());
        assert_eq!(adapter.commit_count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn commit_all_handles_windows_dot_prefixed_temp_roots() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();

        std::fs::write(root.join("foo.md"), "hello").unwrap();
        assert!(adapter.commit_all("add foo").unwrap().is_some());
        assert_eq!(adapter.commit_count(), 1);
    }

    #[test]
    fn recent_checkpoints_returns_newest_first() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();

        std::fs::write(root.join("a.md"), "one").unwrap();
        let first = adapter.commit_all("first checkpoint").unwrap().unwrap();
        std::fs::write(root.join("a.md"), "two").unwrap();
        let second = adapter.commit_all("second checkpoint").unwrap().unwrap();

        let checkpoints = adapter.recent_checkpoints(10).unwrap();
        assert_eq!(checkpoints.len(), 2);
        assert_eq!(checkpoints[0].oid, second.to_string());
        assert_eq!(checkpoints[0].summary, "second checkpoint");
        assert_eq!(checkpoints[1].oid, first.to_string());
    }

    #[test]
    fn file_at_rev_reads_historical_blob() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();

        std::fs::write(root.join("a.md"), "one").unwrap();
        let first = adapter.commit_all("first").unwrap().unwrap();
        std::fs::write(root.join("a.md"), "two").unwrap();
        adapter.commit_all("second").unwrap();

        let bytes = adapter
            .file_at_rev(&first.to_string(), Path::new("a.md"))
            .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "one");
    }
}
