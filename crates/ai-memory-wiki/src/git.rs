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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use git2::{ErrorCode, IndexAddOption, ObjectType, Repository, Signature};
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
    commits_since_index_write: u32,
    /// HEAD as this adapter last left it; moved by another writer, the
    /// kept index is re-read and the next commit walks.
    head: Option<git2::Oid>,
}

/// The index file is written after a walk and every this many
/// path-scoped commits.
const INDEX_WRITE_EVERY: u32 = 50;

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

/// libgit2's "file changed before we could read it": a writer was mid-write.
fn is_racy_read(e: &git2::Error) -> bool {
    e.class() == git2::ErrorClass::Filesystem && e.message().contains("changed before")
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
}

impl GitAdapter {
    /// Open or initialise the repo at `root`. Idempotent: if the
    /// directory is already a git repo, leaves it alone.
    ///
    /// # Errors
    /// Propagates any underlying libgit2 error.
    pub fn open_or_init(root: &Path) -> WikiResult<Self> {
        std::fs::create_dir_all(root)?;
        match Repository::open(root) {
            Ok(_) => debug!(root = %root.display(), "wiki repo already initialised"),
            Err(_) => {
                debug!(root = %root.display(), "initialising wiki repo");
                init_repo(root)?;
            }
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
        if rel.is_some_and(|rel| rel.starts_with(".git")) {
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
        self.written().last_walk = Instant::now().checked_sub(SWEEP_INTERVAL);
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
        match self.commit_all_git2(message) {
            Ok(result) => Ok(result),
            Err(CommitGit2Error::Open(e)) if should_try_commit_cli_fallback(&e) => {
                commit_all_fallback(&self.root, message, e)
            }
            Err(CommitGit2Error::Open(e) | CommitGit2Error::Other(e)) => Err(map_git_err(e)),
        }
    }

    fn commit_all_git2(&self, message: &str) -> Result<Option<git2::Oid>, CommitGit2Error> {
        let mut slot = self.commit_lock.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let repo = Repository::open(&self.root).map_err(CommitGit2Error::Open)?;
            *slot = Some(Open {
                repo,
                commits_since_index_write: 0,
                head: None,
            });
        }
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
        open: &mut Open,
        message: &str,
        staging: &Staging,
    ) -> Result<Option<git2::Oid>, CommitGit2Error> {
        let Open {
            repo,
            commits_since_index_write,
            ..
        } = open;
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
                let walked = stage_working_tree(&mut index).map_err(CommitGit2Error::Other)?;
                self.record_walk(&walked);
                walked.len()
            }
            Staging::Paths(paths) => {
                stage_paths(&self.root, &mut index, paths).map_err(CommitGit2Error::Other)?
            }
        };
        // The index file is the stat cache across restarts; serializing it
        // costs the size of the tree, so not per commit.
        let write_index = match staging {
            Staging::Everything => true,
            Staging::Paths(_) => {
                *commits_since_index_write += 1;
                *commits_since_index_write >= INDEX_WRITE_EVERY
            }
        };
        if write_index {
            index.write().map_err(CommitGit2Error::Other)?;
            *commits_since_index_write = 0;
        }
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
                stage_working_tree(&mut index).map_err(CommitGit2Error::Other)?;
                index.write().map_err(CommitGit2Error::Other)?;
                index.write_tree().map_err(CommitGit2Error::Other)?
            }
        };

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

    /// Count commits reachable from HEAD. Returns 0 for an empty repo.
    /// Useful for the test suite + for `ai-memory status`.
    #[must_use]
    pub fn commit_count(&self) -> usize {
        let Ok(repo) = Repository::open(&self.root) else {
            return commit_count_fallback(&self.root);
        };
        let Ok(mut walk) = repo.revwalk() else {
            return 0;
        };
        if walk.push_head().is_err() {
            return 0;
        }
        walk.count()
    }

    /// Return the most recent commits reachable from HEAD.
    ///
    /// Empty repositories return an empty list.
    ///
    /// # Errors
    /// Propagates any underlying libgit2 error.
    pub fn recent_checkpoints(&self, limit: usize) -> WikiResult<Vec<Checkpoint>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let repo = match Repository::open(&self.root) {
            Ok(repo) => repo,
            Err(e) => return recent_checkpoints_fallback(&self.root, limit, map_git_err(e)),
        };
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

    /// Read `path` as it existed at `rev`.
    ///
    /// `path` is relative to the wiki repo root. The returned bytes are the
    /// blob contents exactly as stored in git.
    ///
    /// # Errors
    /// Returns [`WikiError`] when the revision, path, or blob cannot be read.
    pub fn file_at_rev(&self, rev: &str, path: &Path) -> WikiResult<Vec<u8>> {
        let repo = match Repository::open(&self.root) {
            Ok(repo) => repo,
            Err(e) => return file_at_rev_fallback(&self.root, rev, path, map_git_err(e)),
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
    fn writing(&self) -> MutexGuard<'_, Option<Open>> {
        self.commit_lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `bytes` to `path` atomically (tmp, rename, fsync).
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn write_atomic(&self, path: &Path, bytes: &[u8]) -> WikiResult<()> {
        let _writing = self.writing();
        crate::atomic::write_atomic(path, bytes)?;
        self.mark_written(path);
        Ok(())
    }

    /// Persist a temp file over `path` and return the persisted file.
    pub(crate) fn persist(
        &self,
        tmp: tempfile::NamedTempFile,
        path: &Path,
    ) -> Result<std::fs::File, tempfile::PersistError> {
        let _writing = self.writing();
        let file = crate::atomic::persist_with_retry(tmp, path)?;
        self.mark_written(path);
        Ok(file)
    }

    /// Append `bytes` to `path`, creating it and its parent as needed.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn append(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let _writing = self.writing();
        use std::io::Write as _;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_data()?;
        self.mark_written(path);
        Ok(())
    }

    /// Move `from` to `to`.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        let _writing = self.writing();
        std::fs::rename(from, to)?;
        self.mark_written(from);
        self.mark_written(to);
        Ok(())
    }

    /// Remove one file.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        let _writing = self.writing();
        std::fs::remove_file(path)?;
        self.mark_written(path);
        Ok(())
    }

    /// Remove a directory and everything under it.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        let _writing = self.writing();
        std::fs::remove_dir_all(path)?;
        self.mark_written(path);
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
fn stage_working_tree(index: &mut git2::Index) -> Result<Vec<PathBuf>, git2::Error> {
    let mut touched = Vec::new();
    index.add_all(
        ["*"].iter(),
        IndexAddOption::DEFAULT,
        Some(&mut |path: &Path, _: &[u8]| {
            touched.push(path.to_path_buf());
            0
        }),
    )?;
    Ok(touched)
}
/// Stage only `paths`: a file by path, a directory with its subtree, a
/// gone path by removing its entries. Does not write the index file.
fn stage_paths(
    root: &Path,
    index: &mut git2::Index,
    paths: &BTreeSet<PathBuf>,
) -> Result<usize, git2::Error> {
    for rel in paths {
        let abs = root.join(rel);
        if abs.is_dir() {
            let spec = slash_path(rel);
            index.add_all(
                [spec.as_str()].iter(),
                IndexAddOption::DISABLE_PATHSPEC_MATCH,
                None,
            )?;
        } else if abs.is_file() {
            index.add_path(rel)?;
        } else if index.get_path(rel, 0).is_some() {
            index.remove_path(rel)?;
        } else {
            // Gone and not a file in the index: a directory, or nothing.
            index.remove_dir(rel, 0)?;
        }
    }
    Ok(paths.len())
}

#[cfg(windows)]
fn commit_all_fallback(
    root: &Path,
    message: &str,
    original: git2::Error,
) -> WikiResult<Option<git2::Oid>> {
    warn!(error = %original, root = %root.display(), "libgit2 commit failed; trying git CLI fallback");
    run_git(root, ["add", "-A"])?;
    let diff = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--cached", "--quiet", "--exit-code"])
        .status()
        .map_err(|e| {
            WikiError::Io(std::io::Error::other(format!(
                "{original}; git diff fallback failed to start: {e}"
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

fn should_try_commit_cli_fallback(error: &git2::Error) -> bool {
    if matches!(error.code(), ErrorCode::NotFound) {
        return true;
    }

    #[cfg(windows)]
    {
        // On native Windows, libgit2 can fail to reopen a freshly initialised
        // wiki repo under dot-prefixed temp dirs with an OS path-resolution
        // error. The fallback still runs only for Repository::open failures;
        // real permission or repo corruption errors must also pass through the
        // Git CLI before they are treated as recoverable.
        if matches!(error.class(), git2::ErrorClass::Os)
            && error.message().contains("failed to resolve path")
        {
            return true;
        }
    }

    false
}

#[cfg(windows)]
fn commit_count_fallback(root: &Path) -> usize {
    let Ok(out) = git_output(root, ["rev-list", "--count", "HEAD"]) else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

#[cfg(not(windows))]
fn commit_count_fallback(_root: &Path) -> usize {
    0
}

#[cfg(windows)]
fn recent_checkpoints_fallback(
    root: &Path,
    limit: usize,
    original: WikiError,
) -> WikiResult<Vec<Checkpoint>> {
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

#[cfg(not(windows))]
fn recent_checkpoints_fallback(
    _root: &Path,
    _limit: usize,
    original: WikiError,
) -> WikiResult<Vec<Checkpoint>> {
    Err(original)
}

#[cfg(windows)]
fn file_at_rev_fallback(
    root: &Path,
    rev: &str,
    path: &Path,
    original: WikiError,
) -> WikiResult<Vec<u8>> {
    warn!(error = %original, root = %root.display(), "libgit2 show failed; trying git CLI fallback");
    let rel = slash_path(path);
    let spec = format!("{rev}:{rel}");
    let out = git_output(root, ["show", &spec])?;
    Ok(out.stdout)
}

#[cfg(not(windows))]
fn file_at_rev_fallback(
    _root: &Path,
    _rev: &str,
    _path: &Path,
    original: WikiError,
) -> WikiResult<Vec<u8>> {
    Err(original)
}

fn init_repo(root: &Path) -> WikiResult<()> {
    match Repository::init(root) {
        Ok(_) => Ok(()),
        Err(e) => init_repo_fallback(root, e),
    }
}

#[cfg(windows)]
fn init_repo_fallback(root: &Path, original: git2::Error) -> WikiResult<()> {
    warn!(error = %original, root = %root.display(), "libgit2 init failed; trying git CLI fallback");
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .arg(root)
        .status()
        .map_err(|io| {
            WikiError::Io(std::io::Error::other(format!(
                "{original}; git init fallback failed to start: {io}"
            )))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(map_git_err(original))
    }
}

#[cfg(not(windows))]
fn init_repo_fallback(_root: &Path, original: git2::Error) -> WikiResult<()> {
    Err(map_git_err(original))
}

#[cfg(windows)]
fn run_git<const N: usize>(root: &Path, args: [&str; N]) -> WikiResult<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))?;
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(WikiError::Io(std::io::Error::other(format!(
            "git fallback exited with status {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))))
    }
}

fn map_git_err(e: git2::Error) -> WikiError {
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
    fn init_is_idempotent_and_creates_dotgit() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let _adapter = GitAdapter::open_or_init(&root).unwrap();
        assert!(root.join(".git").is_dir());
        // Second open is a no-op.
        let _adapter2 = GitAdapter::open_or_init(&root).unwrap();
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
            let touched = stage_working_tree(&mut index).unwrap().len();
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
        assert!(adapter.written_paths().is_empty());
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

    #[test]
    fn the_index_file_is_written_after_a_walk_and_every_fifty_commits() {
        let (_tmp, root, adapter) = committed(&[("a.md", "a")]);
        let index_file = root.join(".git/index");
        let after_walk = std::fs::read(&index_file).unwrap();
        for i in 1..=INDEX_WRITE_EVERY {
            let rel = format!("n-{i}.md");
            write(&root, &rel, "n");
            adapter.mark_written(Path::new(&rel));
            assert!(adapter.commit_all(&rel).unwrap().is_some());
            let now = std::fs::read(&index_file).unwrap();
            if i < INDEX_WRITE_EVERY {
                assert_eq!(now, after_walk, "commit {i} wrote the index file");
            } else {
                assert_ne!(now, after_walk, "commit {i} did not write the index file");
            }
        }
        let before_walk = std::fs::read(&index_file).unwrap();
        write(&root, "b.md", "b");
        adapter.mark_written(Path::new("b.md"));
        adapter.age_last_walk();
        assert!(adapter.commit_all("sweep").unwrap().is_some());
        assert_ne!(std::fs::read(&index_file).unwrap(), before_walk);
        assert_eq!(head_blob(&adapter, "b.md").as_deref(), Some("b"));
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
