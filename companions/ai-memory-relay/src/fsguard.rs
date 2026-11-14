//! Filesystem guards for the queue directory.
//!
//! Event bodies reach this directory before server sanitization. Unix paths
//! must be private; existing permissions are checked without changing them.
//!
//! Windows: only symlink/reparse-point rejection is implemented. No ACL
//! hardening is performed, so a queue directory there is exactly as private as
//! the operator made it.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// SQLite sidecars that must be checked alongside the database itself.
pub const SIDECARS: &[&str] = &[
    "relay.sqlite-wal",
    "relay.sqlite-shm",
    "relay.sqlite-journal",
];
/// The queue database file name.
pub const DB_FILE: &str = "relay.sqlite";

/// Retries for a queue file that answers "access denied" on Windows.
const DELETE_PENDING_RETRIES: u32 = 5;
const DELETE_PENDING_PAUSE: Duration = Duration::from_millis(20);

/// Read a queue path's metadata, `None` when it does not exist.
///
/// On Windows a file another process is deleting answers "access denied"
/// until the delete completes, and SQLite deletes its rollback journal at
/// every commit. Two relays opening one queue would otherwise fail at random
/// on the other's journal. The read is retried briefly (at most about 100 ms)
/// and then reported, so a file that stays unreadable is still refused.
fn queue_path_metadata(path: &Path) -> Result<Option<std::fs::Metadata>> {
    read_settled(path, cfg!(windows), || std::fs::symlink_metadata(path))
}

fn read_settled<T>(
    path: &Path,
    retry_denied: bool,
    mut read: impl FnMut() -> std::io::Result<T>,
) -> Result<Option<T>> {
    let mut retries = 0;
    loop {
        match read() {
            Ok(value) => return Ok(Some(value)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e)
                if retry_denied
                    && e.kind() == std::io::ErrorKind::PermissionDenied
                    && retries < DELETE_PENDING_RETRIES =>
            {
                retries += 1;
                std::thread::sleep(DELETE_PENDING_PAUSE);
            }
            Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
        }
    }
}

/// Reject a path that is a symlink or a Windows reparse point.
///
/// A missing path is fine: it is about to be created inside a directory that
/// was itself checked.
pub fn reject_symlink(path: &Path) -> Result<()> {
    let Some(meta) = queue_path_metadata(path)? else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        bail!(
            "{} is a symlink; the relay refuses to follow one into a queue directory",
            path.display()
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!(
                "{} is a reparse point; the relay refuses to follow one into a queue directory",
                path.display()
            );
        }
    }
    Ok(())
}

/// Check one queue file: not a symlink/reparse point, a regular file, not
/// hardlinked elsewhere, and owner-only on Unix. A missing file passes.
pub fn check_queue_file(path: &Path) -> Result<()> {
    reject_symlink(path)?;
    let Some(meta) = queue_path_metadata(path)? else {
        return Ok(());
    };
    if !meta.is_file() {
        bail!(
            "{} exists but is not a regular file; refusing to use it as queue state",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        if meta.nlink() > 1 {
            bail!(
                "{} has {} hard links; refusing to write queue state through a shared inode",
                path.display(),
                meta.nlink()
            );
        }
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} is mode {:o}; queue state must be owner-only (chmod 600) \
                 (existing permissions are unchanged)",
                path.display(),
                mode & 0o7777
            );
        }
    }
    Ok(())
}

/// Prepare `--queue-dir`.
///
/// A directory the relay creates is created owner-only. An existing directory
/// must already be private and must be either empty or an existing relay queue;
/// anything else is refused untouched, so pointing the relay at a shared
/// directory can never re-permission it.
pub fn prepare_queue_dir(dir: &Path) -> Result<PathBuf> {
    if dir.components().any(|c| c == Component::ParentDir) {
        bail!(
            "--queue-dir must not contain a `..` component: {}",
            dir.display()
        );
    }
    reject_symlink(dir)?;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if !meta.is_dir() => bail!("--queue-dir {} is not a directory", dir.display()),
        Ok(_) => check_existing_dir(dir)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_private_dir(dir)?,
        Err(e) => return Err(e).with_context(|| format!("inspect {}", dir.display())),
    }
    for name in std::iter::once(DB_FILE).chain(SIDECARS.iter().copied()) {
        check_queue_file(&dir.join(name))?;
    }
    std::fs::canonicalize(dir).with_context(|| format!("resolve {}", dir.display()))
}

/// An existing directory must be private and dedicated to this queue.
fn check_existing_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!(
                "--queue-dir {} is mode {:o}; it must be owner-only (chmod 700) before the relay \
                 will store event bodies in it. The relay does not re-permission a directory it \
                 did not create",
                dir.display(),
                mode & 0o7777
            );
        }
    }
    let mut foreign = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let name = entry?.file_name();
        let name = name.to_string_lossy().to_string();
        let known = name == DB_FILE || name == "flush.lock" || SIDECARS.contains(&name.as_str());
        if !known {
            foreign.push(name);
        }
    }
    if !foreign.is_empty() {
        foreign.sort();
        foreign.truncate(5);
        bail!(
            "--queue-dir {} already holds unrelated files ({}); point the relay at an empty or \
             existing queue directory instead of sharing one",
            dir.display(),
            foreign.join(", ")
        );
    }
    Ok(())
}

fn finish_directory_creation(dir: &Path, created: std::io::Result<()>) -> Result<()> {
    match created {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another initializer may win after our missing-path inspection.
            // Its path must pass the same checks as any pre-existing queue.
            reject_symlink(dir)?;
            if !std::fs::symlink_metadata(dir)?.is_dir() {
                bail!("--queue-dir {} is not a directory", dir.display());
            }
            check_existing_dir(dir)
        }
        Err(error) => Err(error).with_context(|| format!("create {}", dir.display())),
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!(
            "parent of --queue-dir {} does not exist; create it deliberately first",
            dir.display()
        );
    }
    finish_directory_creation(dir, std::fs::DirBuilder::new().mode(0o700).create(dir))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!(
            "parent of --queue-dir {} does not exist; create it deliberately first",
            dir.display()
        );
    }
    finish_directory_creation(dir, std::fs::create_dir(dir))
}

/// Create a queue file owner-only, or leave an existing one alone.
///
/// SQLite copies the database mode onto its sidecars. Set 0600 at creation so
/// WAL and SHM files are private from their first write.
#[cfg(unix)]
pub fn create_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        // Another process won the race and made it; its own guard applies.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    }
}

/// No ACL hardening on non-Unix targets; documented in the README.
#[cfg(not(unix))]
pub fn create_private_file(path: &Path) -> Result<()> {
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn denied() -> Error {
        Error::from(ErrorKind::PermissionDenied)
    }

    #[test]
    fn a_file_mid_delete_settles_after_a_brief_retry() {
        let mut calls = 0;
        let read = read_settled(Path::new("relay.sqlite-journal"), true, || {
            calls += 1;
            if calls < 3 { Err(denied()) } else { Ok(calls) }
        });
        assert_eq!(read.unwrap(), Some(3));
    }

    #[test]
    fn a_file_that_stays_unreadable_is_still_refused() {
        let mut calls = 0;
        let read = read_settled(Path::new("relay.sqlite-journal"), true, || {
            calls += 1;
            Err::<(), _>(denied())
        });
        let message = format!("{:#}", read.unwrap_err());
        assert!(
            message.contains("inspect relay.sqlite-journal"),
            "{message}"
        );
        assert_eq!(calls, DELETE_PENDING_RETRIES + 1);
    }

    #[test]
    fn without_the_windows_retry_a_denied_read_fails_at_once_and_a_missing_file_is_absent() {
        let mut calls = 0;
        let read = read_settled(Path::new("relay.sqlite"), false, || {
            calls += 1;
            Err::<(), _>(denied())
        });
        assert!(read.is_err());
        assert_eq!(calls, 1);
        let missing = read_settled(Path::new("relay.sqlite"), true, || {
            Err::<(), _>(Error::from(ErrorKind::NotFound))
        });
        assert_eq!(missing.unwrap(), None);
    }

    #[test]
    fn a_concurrent_private_directory_creator_is_recognized() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("queue");
        create_private_dir(&dir).unwrap();

        // The first creator won after the other process inspected a missing path.
        create_private_dir(&dir).expect("an identical creator must be recognized");
        assert_eq!(
            prepare_queue_dir(&dir).unwrap(),
            dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn a_concurrent_file_creator_is_refused_without_changing_its_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("queue");
        std::fs::write(&dir, b"not a queue directory").unwrap();

        assert!(create_private_dir(&dir).is_err());
        assert_eq!(std::fs::read(&dir).unwrap(), b"not a queue directory");
    }

    #[test]
    fn a_concurrent_directory_holding_unrelated_files_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("queue");
        create_private_dir(&dir).unwrap();
        let foreign = dir.join("unrelated.txt");
        std::fs::write(&foreign, b"keep this file").unwrap();

        assert!(create_private_dir(&dir).is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"keep this file");
    }

    #[cfg(unix)]
    #[test]
    fn a_concurrent_shared_directory_creator_is_refused_without_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("queue");
        create_private_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(create_private_dir(&dir).is_err());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_concurrent_symlink_creator_is_refused_without_following_it() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let dir = root.path().join("queue");
        create_private_dir(&target).unwrap();
        symlink(&target, &dir).unwrap();

        assert!(create_private_dir(&dir).is_err());
        assert!(dir.is_symlink());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
}
