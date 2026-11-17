//! Filesystem confinement for the namespaced wiki tree.

use std::path::{Component, Path, PathBuf};

use ai_memory_core::{ProjectId, WorkspaceId};

use crate::error::{WikiError, WikiResult};

#[derive(Clone, Copy)]
pub(crate) enum Prepare {
    Inspect,
    Parents,
    Directory,
}

pub(crate) fn initialize_root(root: &Path) -> WikiResult<()> {
    // lgtm [rust/path-injection] — these are the confinement probes themselves
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => require_directory(root, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(root)?; // lgtm [rust/path-injection]
            inspect_created_directory(root)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn project_root(
    root: &Path,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    prepare: Prepare,
) -> WikiResult<PathBuf> {
    tree_path(
        root,
        &PathBuf::from(workspace_id.to_string()).join(project_id.to_string()),
        prepare,
    )
}

pub(crate) fn project_path(
    root: &Path,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    relative: &Path,
    prepare: Prepare,
) -> WikiResult<PathBuf> {
    tree_path(
        root,
        &PathBuf::from(workspace_id.to_string())
            .join(project_id.to_string())
            .join(relative),
        prepare,
    )
}

pub(crate) fn workspace_path(
    root: &Path,
    workspace_id: WorkspaceId,
    prepare: Prepare,
) -> WikiResult<PathBuf> {
    tree_path(root, Path::new(&workspace_id.to_string()), prepare)
}

pub(crate) fn tree_path(root: &Path, relative: &Path, prepare: Prepare) -> WikiResult<PathBuf> {
    validate_relative(relative)?;
    inspect_root(root)?;
    let components: Vec<_> = relative.components().collect();
    let ancestor_count = match prepare {
        Prepare::Inspect | Prepare::Parents => components.len().saturating_sub(1),
        Prepare::Directory => components.len(),
    };
    let create = !matches!(prepare, Prepare::Inspect);
    let mut current = root.to_path_buf();
    let mut missing = false;
    for component in components.iter().take(ancestor_count) {
        current.push(component.as_os_str());
        // lgtm [rust/path-injection]
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => require_directory(&current, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing = true;
                if create {
                    std::fs::create_dir(&current)?; // lgtm [rust/path-injection]
                    inspect_created_directory(&current)?;
                }
            }
            Err(error) => return Err(error.into()),
        }
        if missing && !create {
            break;
        }
    }

    let target = root.join(relative);
    if !matches!(prepare, Prepare::Directory) && !missing {
        inspect_final(&target)?;
    }
    Ok(target)
}

/// Validate the complete wiki tree and its in-root repository metadata.
///
/// # Errors
/// Returns [`WikiError::Confinement`] for symbolic links, reparse points,
/// redirected repository metadata, or a working tree outside `root`.
pub fn validate_wiki_tree(root: &Path) -> WikiResult<()> {
    let git_dir = inspect_git_directory(root)?;
    inspect_tree_except(root, &[git_dir.as_path()])
}

pub(crate) fn inspect_tree(root: &Path) -> WikiResult<()> {
    inspect_tree_except(root, &[])
}

pub(crate) fn inspect_git_directory(root: &Path) -> WikiResult<PathBuf> {
    inspect_root(root)?;
    let git_dir = root.join(".git");
    // lgtm [rust/path-injection]
    let metadata = std::fs::symlink_metadata(&git_dir).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WikiError::Confinement {
                path: git_dir.clone(),
                reason: "wiki repository metadata directory is missing",
            }
        } else {
            error.into()
        }
    })?;
    if is_link_like(&metadata) || !metadata.is_dir() {
        return Err(WikiError::Confinement {
            path: git_dir,
            reason: "wiki repository metadata must be an in-root ordinary directory",
        });
    }
    for relative in [
        "commondir",
        "objects/info/alternates",
        "objects/info/http-alternates",
    ] {
        let redirect = git_dir.join(relative);
        // lgtm [rust/path-injection]
        match std::fs::symlink_metadata(&redirect) {
            Ok(_) => {
                return Err(WikiError::Confinement {
                    path: redirect,
                    reason: "wiki repository metadata must not redirect outside its in-root .git directory",
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    inspect_tree(&git_dir)?;
    inspect_configured_worktree(root, &git_dir)?;
    Ok(git_dir)
}

fn inspect_configured_worktree(root: &Path, git_dir: &Path) -> WikiResult<()> {
    for config_path in [git_dir.join("config"), git_dir.join("config.worktree")] {
        if !config_path.is_file() {
            continue;
        }
        let config = git2::Config::open(&config_path).map_err(|error| {
            WikiError::Io(std::io::Error::other(format!(
                "could not inspect wiki repository config {}: {error}",
                config_path.display()
            )))
        })?;
        let Ok(configured) = config.get_string("core.worktree") else {
            continue;
        };
        let configured = PathBuf::from(configured);
        let worktree = if configured.is_absolute() {
            configured
        } else {
            git_dir.join(configured)
        };
        if worktree.canonicalize()? != root.canonicalize()? {
            return Err(WikiError::Confinement {
                path: config_path,
                reason: "wiki repository working directory escapes the wiki root",
            });
        }
    }
    Ok(())
}

pub(crate) fn inspect_tree_if_present(root: &Path) -> WikiResult<()> {
    // lgtm [rust/path-injection]
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => inspect_tree(root),
    }
}

/// Confine only `paths` (relative to `root`) and their ancestor directories
/// under `root`, without walking unrelated subtrees.
pub(crate) fn inspect_scoped_paths(
    root: &Path,
    paths: &std::collections::BTreeSet<PathBuf>,
) -> WikiResult<()> {
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => require_directory(root, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let mut checked_ancestors = std::collections::HashSet::new();
    for rel in paths {
        let mut cursor = root.to_path_buf();
        let mut components = rel.components().peekable();
        while let Some(comp) = components.next() {
            cursor.push(comp);
            if components.peek().is_some() {
                if !checked_ancestors.insert(cursor.clone()) {
                    continue;
                }
                match std::fs::symlink_metadata(&cursor) {
                    Ok(metadata) => require_directory(&cursor, &metadata)?,
                    Err(error) if is_transient_churn(&error) => break,
                    Err(error) => return Err(error.into()),
                }
            } else {
                match std::fs::symlink_metadata(&cursor) {
                    Ok(metadata) if metadata.is_dir() => inspect_tree_if_present(&cursor)?,
                    Ok(metadata) if is_link_like(&metadata) => return Err(confined(&cursor)),
                    Ok(_) => {}
                    Err(error) if is_transient_churn(&error) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn inspect_tree_except(root: &Path, excluded_roots: &[&Path]) -> WikiResult<()> {
    // lgtm [rust/path-injection]
    match std::fs::symlink_metadata(root) {
        Ok(metadata) => require_directory(root, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        // lgtm [rust/path-injection]
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            // The directory vanished after the walk observed it (concurrent
            // commits churn lock and temp files); an absent directory hides
            // nothing that was observed. Same tolerance as the absent root.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                // The directory went away mid-listing; the rest of it is
                // as unobserved as the part already read. On Windows the
                // same churn surfaces as sharing violations or
                // delete-pending handles instead of NotFound.
                Err(error) if is_transient_churn(&error) => break,
                Err(error) => return Err(error.into()),
            };
            let path = entry.path();
            // Vanish-tolerant, never link-tolerant: an entry unstatable by
            // the time it is probed — deleted outright, or held and
            // rename-replaced by a concurrent commit under Windows sharing
            // semantics — was not observed, so there is nothing to refuse;
            // every entry that IS stat-able must pass the link-like check
            // below.
            // lgtm [rust/path-injection]
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if is_transient_churn(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            if is_link_like(&metadata) {
                return Err(confined(&path));
            }
            if excluded_roots.iter().any(|excluded| path == *excluded) {
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

/// Whether metadata identifies a symbolic link or Windows reparse point.
#[must_use]
pub(crate) fn is_link_like(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Whether a walk-probe error is concurrent-writer churn rather than a
/// verdict about the tree.
///
/// Windows rename semantics give the vanish race a second shape: the
/// atomic writer rename-replaces files and a concurrent commit holds
/// `.git` lock and temp files, so a stat on an already-listed entry can
/// fail with a sharing violation (`PermissionDenied`) or
/// `ERROR_DELETE_PENDING` for a path that exists again — or does not —
/// an instant later. Like NotFound, neither says anything stable enough
/// to act on, so the walk skips the entry; any entry that does stat
/// still faces the link-like refusal.
fn is_transient_churn(error: &std::io::Error) -> bool {
    // ERROR_DELETE_PENDING: std maps sharing violations to
    // PermissionDenied but leaves this one an unmapped raw code.
    const ERROR_DELETE_PENDING: i32 = 303;
    error.kind() == std::io::ErrorKind::NotFound
        || error.kind() == std::io::ErrorKind::PermissionDenied
        || error.raw_os_error() == Some(ERROR_DELETE_PENDING)
}

fn inspect_root(root: &Path) -> WikiResult<()> {
    let metadata = std::fs::symlink_metadata(root)?; // lgtm [rust/path-injection]
    require_directory(root, &metadata)
}

fn inspect_final(path: &Path) -> WikiResult<()> {
    // lgtm [rust/path-injection]
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_link_like(&metadata) => Err(confined(path)),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn inspect_created_directory(path: &Path) -> WikiResult<()> {
    let metadata = std::fs::symlink_metadata(path)?; // lgtm [rust/path-injection]
    require_directory(path, &metadata)
}

fn require_directory(path: &Path, metadata: &std::fs::Metadata) -> WikiResult<()> {
    if is_link_like(metadata) {
        return Err(confined(path));
    }
    if !metadata.is_dir() {
        return Err(WikiError::Io(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("wiki path component is not a directory: {}", path.display()),
        )));
    }
    Ok(())
}

fn validate_relative(relative: &Path) -> WikiResult<()> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(WikiError::Confinement {
            path: relative.to_path_buf(),
            reason: "path is not a normalized relative wiki path",
        });
    }
    Ok(())
}

fn confined(path: &Path) -> WikiError {
    WikiError::Confinement {
        path: path.to_path_buf(),
        reason: "symbolic links and reparse points are not allowed in the wiki project tree",
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn link_metadata_helper_accepts_regular_entries() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(!is_link_like(&std::fs::symlink_metadata(file).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn link_metadata_helper_rejects_dangling_links() {
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(temp.path().join("missing"), &link).unwrap();
        assert!(is_link_like(&std::fs::symlink_metadata(link).unwrap()));
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
                eprintln!("skipping link assertion: Windows privilege unavailable");
                false
            }
            Err(error) => panic!("failed to create directory link: {error}"),
        }
    }

    /// A walk beside create/delete churn (a concurrent commit's `.git`
    /// lock and temp files, an atomic writer's temp renames) must not
    /// fail on an entry that vanishes between read_dir and stat — while
    /// a link the walk does observe is still refused, and the walk runs
    /// clean once the churn stops.
    #[test]
    fn tree_walks_skip_entries_that_vanish_but_still_refuse_observed_links() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("wiki");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("page.md"), b"x").unwrap();
        let scratch = root.join("scratch");
        let stop = Arc::new(AtomicBool::new(false));
        let racer = {
            let scratch = scratch.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::fs::write(&scratch, b"churn");
                    let _ = std::fs::remove_file(&scratch);
                }
            })
        };
        for _ in 0..200 {
            inspect_tree_except(&root, &[]).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        racer.join().unwrap();
        let _ = std::fs::remove_file(&scratch);

        // Churn settled: a plain walk over the now-stable tree must
        // succeed, proving the skip tolerance above leaves no lingering
        // failure behind on the next walk.
        inspect_tree_except(&root, &[]).unwrap();

        #[cfg(any(unix, windows))]
        {
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            if link_directory(&outside, &root.join("escape")) {
                assert!(matches!(
                    inspect_tree_except(&root, &[]),
                    Err(WikiError::Confinement { .. })
                ));
            }
        }
    }

    /// Vanish tolerance is for read_dir race entries only: a direct
    /// target's own semantics are unchanged — absent stays acceptable
    /// (nothing to confine), an observed link stays refused.
    #[test]
    fn direct_targets_keep_their_own_absence_and_link_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("wiki");
        std::fs::create_dir_all(&root).unwrap();
        let absent = root.join("absent.md");
        assert!(inspect_final(&absent).is_ok());

        std::fs::write(&absent, b"x").unwrap();
        assert!(inspect_final(&absent).is_ok());

        #[cfg(any(unix, windows))]
        {
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            let linked = root.join("linked.md");
            if link_directory(&outside, &linked) {
                assert!(matches!(
                    inspect_final(&linked),
                    Err(WikiError::Confinement { .. })
                ));
            }
        }
    }

    #[test]
    fn scoped_path_confinement_skips_unrelated_subtrees() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("wiki");
        let target_dir = root.join("ws").join("proj").join("sessions");
        let unrelated_dir = root.join("other-ws").join("huge-proj");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::create_dir_all(&unrelated_dir).unwrap();
        std::fs::write(target_dir.join("note.md"), b"ok").unwrap();

        #[cfg(any(unix, windows))]
        {
            let outside = temp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            // A link inside an unrelated subtree does not cause a scoped
            // check for `ws/proj/sessions/note.md` to walk `other-ws`,
            // whereas a link on the scoped path itself is still refused.
            if link_directory(&outside, &unrelated_dir.join("escape")) {
                let mut paths = std::collections::BTreeSet::new();
                paths.insert(PathBuf::from("ws/proj/sessions/note.md"));
                assert!(inspect_scoped_paths(&root, &paths).is_ok());
            }
            if link_directory(&outside, &target_dir.join("linked.md")) {
                let mut paths = std::collections::BTreeSet::new();
                paths.insert(PathBuf::from("ws/proj/sessions/linked.md"));
                assert!(matches!(
                    inspect_scoped_paths(&root, &paths),
                    Err(WikiError::Confinement { .. })
                ));
            }
        }
    }
}
