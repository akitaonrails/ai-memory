//! Atomic file writes.
//!
//! Every file the wiki owns is written via a tmp + rename + fsync dance.
//! Two payoffs: a crash mid-write never produces a torn file, and the
//! upcoming watcher (M1-D) can ignore "own writes" by inode tracking.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::error::{WikiError, WikiResult};

/// Atomically replace the file at `path` with `bytes`.
///
/// Steps: write to a `tempfile` in the same directory, `sync_data` the
/// tempfile, `persist` it over the destination, then best-effort `sync_all`
/// the parent directory so the rename hits stable storage.
///
/// Returns the inode number of the persisted file (used by the watcher to
/// skip its own writes).
///
/// # Errors
/// Propagates I/O and [`tempfile::PersistError`] failures.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> WikiResult<u64> {
    let parent = path
        .parent()
        .ok_or_else(|| WikiError::Io(std::io::Error::other("path has no parent")))?;
    std::fs::create_dir_all(parent)?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".ai-memory-tmp.")
        .tempfile_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_data()?;

    let persisted: File = tmp.persist(path)?;
    persisted.sync_data()?;

    // Best-effort: fsync the parent so the rename is durable too.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(inode_of(path).unwrap_or(0))
}

#[cfg(unix)]
fn inode_of(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.ino())
}

// The NTFS file index is the closest stable analog to a Unix inode and is
// what the watcher compares to skip its own writes. Unlike Unix, it isn't
// exposed via `Metadata`; it requires an open handle (`GetFileInformationByHandle`),
// hence the extra `File::open`. Non-NTFS volumes (FAT) report 0 — harmless:
// the caller's `.unwrap_or(0)` path already treats 0 as "no stable id".
#[cfg(windows)]
fn inode_of(path: &Path) -> std::io::Result<u64> {
    let file = std::fs::File::open(path)?;
    let info = winapi_util::file::information(&file)?;
    Ok(info.file_index())
}

#[cfg(not(any(unix, windows)))]
fn inode_of(_path: &Path) -> std::io::Result<u64> {
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_atomically_and_creates_parents() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nested/dir/page.md");
        let ino = write_atomic(&target, b"hello").unwrap();
        assert!(target.is_file());
        assert_eq!(std::fs::read(&target).unwrap(), b"hello");
        if cfg!(unix) || cfg!(windows) {
            assert_ne!(ino, 0);
        }
    }

    #[test]
    fn overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("page.md");
        write_atomic(&target, b"first").unwrap();
        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
    }

    #[test]
    fn does_not_leave_tmp_files_on_success() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("page.md");
        write_atomic(&target, b"x").unwrap();
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .any(|n| n.to_string_lossy().starts_with(".ai-memory-tmp."));
        assert!(!leftover, "tempfile leaked");
    }
}
