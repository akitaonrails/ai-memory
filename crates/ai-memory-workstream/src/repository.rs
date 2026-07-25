//! Repository/worktree identity and non-mutating boundary checkpoints.

use std::path::{Path, PathBuf};
use std::process::Command;

use ai_memory_core::WorkstreamCheckpoint;
use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};

/// Stable identity used to select a workstream without relying only on CWD.
#[derive(Debug, Clone)]
pub struct RepositoryIdentity {
    /// Canonical host cwd.
    pub cwd: PathBuf,
    /// Repository identity hash shared by linked worktrees.
    pub repo_fingerprint: String,
    /// Worktree-specific identity hash.
    pub worktree_fingerprint: String,
    /// Current non-mutating Git checkpoint.
    pub checkpoint: WorkstreamCheckpoint,
}

/// Inspect the current checkout without committing, stashing, resetting, or
/// otherwise changing it.
pub fn inspect_repository(cwd: &Path) -> Result<RepositoryIdentity> {
    let canonical = cwd
        .canonicalize()
        .with_context(|| format!("canonicalizing managed run cwd {}", cwd.display()))?;
    let git_root = git(&canonical, &["rev-parse", "--show-toplevel"]);
    let common_dir = git(
        &canonical,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    let git_dir = git(
        &canonical,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    );
    let remotes = git(&canonical, &["remote", "get-url", "--all", "origin"]);

    let repo_seed = match (&git_root, &common_dir, &remotes) {
        (_, _, Some(remotes)) if !remotes.trim().is_empty() => format!("git-remotes\n{remotes}"),
        (_, Some(common), _) => format!("git-common\n{common}"),
        (Some(root), _, _) => format!("git-root\n{root}"),
        _ => format!("directory\n{}", canonical.display()),
    };
    let worktree_seed = match (&git_dir, &git_root) {
        (Some(git_dir), _) => format!("{}\ngit-dir\n{git_dir}", sha256(&repo_seed)),
        (_, Some(root)) => format!("{}\nroot\n{root}", sha256(&repo_seed)),
        _ => format!("{}\ncwd\n{}", sha256(&repo_seed), canonical.display()),
    };
    Ok(RepositoryIdentity {
        cwd: canonical.clone(),
        repo_fingerprint: sha256(&repo_seed),
        worktree_fingerprint: sha256(&worktree_seed),
        checkpoint: checkpoint(&canonical),
    })
}

fn checkpoint(cwd: &Path) -> WorkstreamCheckpoint {
    let status = git_bytes(cwd, &["status", "--porcelain=v1", "-z"]);
    let changed_paths = status
        .as_deref()
        .map(parse_status_paths)
        .unwrap_or_default();
    WorkstreamCheckpoint {
        head: git(cwd, &["rev-parse", "HEAD"]),
        branch: git(cwd, &["branch", "--show-current"]).map(|branch| {
            if branch.is_empty() {
                "(detached HEAD)".to_string()
            } else {
                branch
            }
        }),
        dirty_hash: status
            .as_deref()
            .and_then(|bytes| (!bytes.is_empty()).then(|| format!("{:x}", Sha256::digest(bytes)))),
        changed_paths,
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = git_bytes(cwd, args)?;
    String::from_utf8(output)
        .ok()
        .map(|value| value.trim().to_string())
}

fn git_bytes(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn parse_status_paths(status: &[u8]) -> Vec<String> {
    let mut entries = status.split(|byte| *byte == 0).peekable();
    let mut paths = Vec::new();
    while let Some(entry) = entries.next() {
        if entry.len() <= 3 {
            continue;
        }
        if let Ok(path) = String::from_utf8(entry[3..].to_vec()) {
            paths.push(path);
        }
        if entry[..2]
            .iter()
            .any(|status| matches!(*status, b'R' | b'C'))
            && let Some(old_path) = entries.next()
            && let Ok(path) = String::from_utf8(old_path.to_vec())
        {
            paths.push(path);
        }
        if paths.len() >= 256 {
            break;
        }
    }
    paths.truncate(256);
    paths
}

fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_git_directory_has_stable_distinct_fingerprints() {
        let temp = tempfile::tempdir().unwrap();
        let first = inspect_repository(temp.path()).unwrap();
        let second = inspect_repository(temp.path()).unwrap();
        assert_eq!(first.repo_fingerprint, second.repo_fingerprint);
        assert_eq!(first.worktree_fingerprint, second.worktree_fingerprint);
        assert_eq!(first.repo_fingerprint.len(), 64);
    }

    #[test]
    fn status_parser_handles_spaces_without_shell_splitting() {
        let paths = parse_status_paths(b" M path with spaces.rs\0?? new.txt\0");
        assert_eq!(paths, ["path with spaces.rs", "new.txt"]);
    }

    #[test]
    fn status_parser_preserves_both_rename_paths() {
        let paths = parse_status_paths(b"R  new name.rs\0old name.rs\0");
        assert_eq!(paths, ["new name.rs", "old name.rs"]);
    }
}
