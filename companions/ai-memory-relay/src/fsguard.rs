//! Filesystem guards for the queue directory.
//!
//! Event bodies reach this directory before server sanitization. Unix paths
//! must be private; existing permissions are checked without changing them.
//!
//! Windows: only symlink/reparse-point rejection is implemented. No ACL
//! hardening is performed, so a queue directory there is exactly as private as
//! the operator made it.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

/// SQLite sidecars that must be checked alongside the database itself.
pub const SIDECARS: &[&str] = &[
    "relay.sqlite-wal",
    "relay.sqlite-shm",
    "relay.sqlite-journal",
];
/// The queue database file name.
pub const DB_FILE: &str = "relay.sqlite";

/// Reject a path that is a symlink or a Windows reparse point.
///
/// A missing path is fine: it is about to be created inside a directory that
/// was itself checked.
pub fn reject_symlink(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
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
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
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
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))
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
    std::fs::create_dir(dir).with_context(|| format!("create {}", dir.display()))
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
