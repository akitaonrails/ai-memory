//! Git versioning for the wiki tree.
//!
//! On `Wiki::new`, we lazily `git init` the wiki root if it isn't already
//! a repo. Auto-commits fire from the hook router on `SessionEnd` and
//! from the M7 consolidator. Author/email are fixed so the wiki history
//! can't accidentally leak the maintainer's git identity.

use std::path::{Path, PathBuf};

use git2::{ErrorCode, IndexAddOption, ObjectType, Repository, Signature};
use tracing::{debug, warn};

use crate::error::{WikiError, WikiResult};

/// Author identity used for ai-memory's own commits. The user can
/// rewrite history with their own identity later if they care.
pub const COMMIT_AUTHOR_NAME: &str = "ai-memory";
/// Author email used for ai-memory's own commits.
pub const COMMIT_AUTHOR_EMAIL: &str = "ai-memory@local";

/// Thin handle over the wiki repo. Cheap to clone — internally a `PathBuf`.
#[derive(Clone)]
pub struct GitAdapter {
    root: PathBuf,
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
        })
    }

    /// Path of the wiki root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stage *everything* in the wiki root, then commit with `message`.
    /// Returns `Ok(None)` if there were no changes to commit (working
    /// tree clean), or `Ok(Some(commit_oid))` on a successful commit.
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
        let repo = Repository::open(&self.root).map_err(CommitGit2Error::Open)?;

        // Stage everything (including deletions). Clear the index first so
        // every entry is re-hashed from the working tree: libgit2 keeps a
        // stat cache and will skip re-reading a file whose size/mtime look
        // unchanged, trusting the cached blob OID. When that cached OID is
        // stale or its blob is absent from the object database — which a
        // store carried across libgit2/git versions or an interrupted
        // earlier operation can leave behind — `write_tree` below aborts
        // with "invalid object specified … class=Tree" and, since the wiki
        // migration commits through this path, the server crash-loops and
        // never starts (#594). Clearing drops the stat cache, so `add_all`
        // re-hashes each working-tree file into the ODB and every entry the
        // tree references is guaranteed present.
        let mut index = repo.index().map_err(CommitGit2Error::Other)?;
        index.clear().map_err(CommitGit2Error::Other)?;
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .map_err(CommitGit2Error::Other)?;
        index.write().map_err(CommitGit2Error::Other)?;

        // If the index matches HEAD, there is nothing to commit.
        let tree_oid = index.write_tree().map_err(CommitGit2Error::Other)?;
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
    let rel = path.to_string_lossy().replace('\\', "/");
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

    /// #594 hardening: the commit re-hashes every file from the working
    /// tree rather than trusting cached index blob OIDs. A store carried
    /// across libgit2/git versions (or an interrupted operation) can leave
    /// the index's stat cache pointing at a blob absent from the object
    /// database; `write_tree` then aborts with "invalid object specified"
    /// and the migration crash-loops. The fix clears the index before
    /// `add_all`. We can't fabricate a missing-blob entry through git2's
    /// safe API (it validates the OID on `add`), so we verify the
    /// behaviour the clear guarantees: the *current* working-tree content
    /// is what gets committed, even after the same path was committed
    /// before — i.e. staging always reflects disk, never a cache.
    #[test]
    fn commit_all_commits_current_working_tree_content() {
        let tmp = tempdir();
        let root = tmp.path().join("wiki");
        let adapter = GitAdapter::open_or_init(&root).unwrap();

        std::fs::write(root.join("log-2026-07.md"), "v1").unwrap();
        adapter.commit_all("v1").unwrap();

        // Overwrite in place and commit again: the committed blob must be v2.
        std::fs::write(root.join("log-2026-07.md"), "v2 rewritten").unwrap();
        let oid = adapter.commit_all("v2").unwrap();
        assert!(oid.is_some(), "a content change must produce a commit");

        let committed = adapter
            .file_at_rev("HEAD", Path::new("log-2026-07.md"))
            .unwrap();
        assert_eq!(
            committed, b"v2 rewritten",
            "commit must reflect the working-tree file, re-hashed from disk"
        );
        assert_eq!(adapter.commit_count(), 2);
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
