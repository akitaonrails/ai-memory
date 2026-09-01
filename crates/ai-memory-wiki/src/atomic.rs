//! Atomic file writes.
//!
//! Every file the wiki owns is written via a tmp + rename + fsync dance.
//! Two payoffs: a crash mid-write never produces a torn file, and the
//! upcoming watcher (M1-D) can ignore "own writes" by inode tracking.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::error::{WikiError, WikiResult};

/// How many times a persist is retried when the failure is a transient
/// sharing violation, and the base of the linear backoff between attempts.
/// Worst case adds 10+20+30+40+50 = 150ms before the real error propagates.
const PERSIST_RETRY_ATTEMPTS: u32 = 5;
const PERSIST_RETRY_BASE_DELAY: Duration = Duration::from_millis(10);

/// Persist `tmp` over `path`, absorbing transient Windows sharing violations.
///
/// On Windows the rename can fail with `ERROR_SHARING_VIOLATION` (32) or
/// `ERROR_LOCK_VIOLATION` (33) when another process briefly holds the
/// just-created tempfile or the destination — most commonly an antivirus or
/// indexer scanning the new file, which is endemic on CI runners. The hold is
/// transient by nature, so a short bounded retry absorbs it; anything that
/// outlasts the budget is a real error and propagates unchanged. On
/// non-Windows platforms nothing is classified transient, so behaviour is
/// byte-for-byte what it was.
pub(crate) fn persist_with_retry(
    tmp: tempfile::NamedTempFile,
    path: &Path,
) -> Result<File, tempfile::PersistError> {
    persist_with_retry_impl(
        tmp,
        PERSIST_RETRY_ATTEMPTS,
        PERSIST_RETRY_BASE_DELAY,
        is_transient_sharing_violation,
        |tmp, path| tmp.persist(path),
        path,
    )
}

/// The retry loop with its collaborators injected, so the mechanics are
/// testable on every platform: tests fabricate `PersistError`s (its fields
/// are public) and supply their own transience predicate and zero delay,
/// while production supplies the Windows classifier and the real `persist`.
fn persist_with_retry_impl<F, P>(
    mut tmp: tempfile::NamedTempFile,
    attempts: u32,
    base_delay: Duration,
    transient: P,
    mut attempt_fn: F,
    path: &Path,
) -> Result<File, tempfile::PersistError>
where
    F: FnMut(tempfile::NamedTempFile, &Path) -> Result<File, tempfile::PersistError>,
    P: Fn(&std::io::Error) -> bool,
{
    let mut attempt = 0u32;
    loop {
        match attempt_fn(tmp, path) {
            Ok(file) => return Ok(file),
            Err(err) if attempt < attempts && transient(&err.error) => {
                attempt += 1;
                std::thread::sleep(base_delay * attempt);
                // `PersistError` hands the tempfile back, which is what makes
                // retrying possible at all: the staged bytes are still intact.
                tmp = err.file;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Windows: sharing (32) and lock (33) violations are transient third-party
/// holds. Everything else — including `ACCESS_DENIED`, which is usually a
/// real permissions problem — propagates immediately. Elsewhere: nothing is
/// transient; raw code 32 on Unix is `EPIPE` and retrying a rename on it
/// would be noise on top of a real failure.
fn is_transient_sharing_violation(err: &std::io::Error) -> bool {
    if cfg!(windows) {
        matches!(err.raw_os_error(), Some(32) | Some(33))
    } else {
        false
    }
}

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

    let persisted: File = persist_with_retry(tmp, path)?;
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

    fn staged(dir: &TempDir) -> tempfile::NamedTempFile {
        let mut tmp = tempfile::Builder::new().tempfile_in(dir.path()).unwrap();
        tmp.write_all(b"staged bytes").unwrap();
        tmp
    }

    fn sharing_violation(tmp: tempfile::NamedTempFile) -> tempfile::PersistError {
        tempfile::PersistError {
            error: std::io::Error::from_raw_os_error(32),
            file: tmp,
        }
    }

    /// A transient hold that clears must not surface: the retry absorbs the
    /// first failures and the bytes still land. This is the Windows-CI
    /// antivirus scenario, exercised on every platform by injecting the
    /// failure instead of hoping to reproduce a scanner's timing.
    #[test]
    fn a_transient_hold_is_absorbed_and_the_write_lands() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("page.md");
        let mut failures_left = 3;
        let result = persist_with_retry_impl(
            staged(&dir),
            5,
            Duration::ZERO,
            |_| true,
            |tmp, path| {
                if failures_left > 0 {
                    failures_left -= 1;
                    Err(sharing_violation(tmp))
                } else {
                    tmp.persist(path)
                }
            },
            &target,
        );
        assert!(result.is_ok(), "three transient failures must be absorbed");
        assert_eq!(std::fs::read(&target).unwrap(), b"staged bytes");
    }

    /// A hold that never clears is a real error: the budget bounds the wait
    /// and the final error propagates rather than looping forever.
    #[test]
    fn a_persistent_hold_exhausts_the_budget_and_propagates() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("page.md");
        let mut calls = 0u32;
        let result = persist_with_retry_impl(
            staged(&dir),
            5,
            Duration::ZERO,
            |_| true,
            |tmp, _| {
                calls += 1;
                Err(sharing_violation(tmp))
            },
            &target,
        );
        assert!(result.is_err());
        assert_eq!(calls, 6, "initial attempt plus five retries, then stop");
    }

    /// A non-transient error must NOT be retried: retrying a real failure
    /// only delays it and can mask its cause. The control for the whole
    /// mechanism — if the loop treated everything as transient, this fails.
    #[test]
    fn a_non_transient_error_propagates_immediately() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("page.md");
        let mut calls = 0u32;
        let result = persist_with_retry_impl(
            staged(&dir),
            5,
            Duration::ZERO,
            |_| false,
            |tmp, _| {
                calls += 1;
                Err(sharing_violation(tmp))
            },
            &target,
        );
        assert!(result.is_err());
        assert_eq!(calls, 1, "no retry when the predicate says non-transient");
    }

    /// Pins the production classifier per platform: on Unix nothing is
    /// transient (raw 32 is EPIPE there), so Unix persist behaviour is
    /// byte-for-byte unchanged by this fix. On Windows, 32 and 33 retry.
    #[test]
    fn the_classifier_matches_only_windows_sharing_codes() {
        let sharing = std::io::Error::from_raw_os_error(32);
        let lock = std::io::Error::from_raw_os_error(33);
        let denied = std::io::Error::from_raw_os_error(5);
        if cfg!(windows) {
            assert!(is_transient_sharing_violation(&sharing));
            assert!(is_transient_sharing_violation(&lock));
            assert!(
                !is_transient_sharing_violation(&denied),
                "ACCESS_DENIED is a real permissions problem, not a transient hold"
            );
        } else {
            assert!(
                !is_transient_sharing_violation(&sharing),
                "nothing is transient off Windows; unix behaviour is unchanged"
            );
        }
    }

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
