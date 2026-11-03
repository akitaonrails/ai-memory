//! Atomic file replacement with unpredictable, exclusively created
//! staging files (GHSA-jr85-7366-xrm2).
//!
//! Every wikisync write used to stage bytes at a name an attacker could
//! compute in advance — `.wikisync-tmp-<page>`, `state.json.tmp`,
//! `.post-merge.wikisync-tmp` — opened with `File::create`, which follows
//! symlinks, or after a remove-then-create race. In a destination shared
//! with others, a planted symlink at that name could turn an apply into an
//! overwrite of a file of the planter's choosing.
//!
//! Staging now mirrors `crates/ai-memory-wiki/src/atomic.rs`: a random
//! suffix under this tool's prefix, created with `O_EXCL`/`CREATE_NEW`, so
//! there is no name to plant and nothing to follow, and the final rename
//! retries the transient Windows sharing violation a scanner can cause.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

/// Prefix of every staging file, so a crash's leftover is visibly this
/// tool's rather than an ambiguous artifact next to page files.
const TMP_PREFIX: &str = ".wikisync-tmp.";

/// How many times a persist is retried when the failure is a transient
/// Windows sharing violation, and the base of the linear backoff between
/// attempts. Worst case adds 10+20+30+40+50 = 150 ms before the real error
/// propagates — the same budget the wiki crate's atomic writes use.
const PERSIST_RETRY_ATTEMPTS: u32 = 5;
const PERSIST_RETRY_BASE_DELAY: Duration = Duration::from_millis(10);

/// The Unix mode a replaced file keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeepMode {
    /// Created as `0o666 & !umask` — exactly what `File::create`, which
    /// the page writer always used, produces on every filesystem.
    UmaskDefault,
    /// `chmod`ed to exactly this mode before the rename, as the state
    /// (`0o600`) and hook (`0o755`) writers always did, so a restrictive
    /// umask cannot change the outcome. Ignored where Unix modes do not
    /// exist.
    Exact(u32),
}

/// Replace `path` with `bytes` atomically.
///
/// Steps: stage the bytes in a `tempfile` under [`TMP_PREFIX`] plus an
/// unpredictable random part, created exclusively in the same directory;
/// `sync_all`; `persist` (rename) over the destination, retrying transient
/// Windows sharing violations; then best-effort fsync the parent so the
/// rename is durable too. The parent directory must already exist — each
/// caller creates it behind its own guards (destination confinement,
/// state-dir checks, hooks-dir creation).
///
/// Because the staging name is unpredictable and the file is created
/// exclusively, a symlink planted in the destination is neither followed
/// nor raced: there is no staging name to plant at.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: KeepMode) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(TMP_PREFIX);
    if mode == KeepMode::UmaskDefault {
        // Pages keep the umask-default mode `File::create` gave them;
        // tempfile alone would pin them to 0o600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o666));
        }
    }
    let mut tmp = builder
        .tempfile_in(parent)
        .with_context(|| format!("cannot create a staging file in {}", parent.display()))?;
    if let KeepMode::Exact(mode) = mode {
        // The staging name is unpredictable and exclusively ours, so the
        // chmod through it carries no plantable race. Set before the
        // bytes, the order the state and hook writers always used.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))
                .with_context(|| format!("cannot set permissions on {}", tmp.path().display()))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
    tmp.write_all(bytes)
        .with_context(|| format!("cannot write {}", tmp.path().display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("cannot sync {}", tmp.path().display()))?;

    if let Err(err) = persist_with_retry(tmp, path) {
        // Dropping the PersistError drops the untouched staging file.
        return Err(anyhow::Error::new(err.error)).context(format!(
            "cannot move the staging file over {}",
            path.display()
        ));
    }

    // Best-effort: fsync the parent so the rename is durable too.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// `NamedTempFile::persist`, retrying transient sharing violations within
/// the budget.
fn persist_with_retry(
    tmp: tempfile::NamedTempFile,
    path: &Path,
) -> Result<fs::File, tempfile::PersistError> {
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
/// are public) and supply their own transience predicate and zero delay.
fn persist_with_retry_impl<F, P>(
    mut tmp: tempfile::NamedTempFile,
    attempts: u32,
    base_delay: Duration,
    transient: P,
    mut attempt_fn: F,
    path: &Path,
) -> Result<fs::File, tempfile::PersistError>
where
    F: FnMut(tempfile::NamedTempFile, &Path) -> Result<fs::File, tempfile::PersistError>,
    P: Fn(&std::io::Error) -> bool,
{
    let mut attempt = 0u32;
    loop {
        match attempt_fn(tmp, path) {
            Ok(file) => return Ok(file),
            Err(err) if attempt < attempts && transient(&err.error) => {
                attempt += 1;
                std::thread::sleep(base_delay * attempt);
                // `PersistError` hands the tempfile back, which is what
                // makes retrying possible at all: the staged bytes are
                // still intact.
                tmp = err.file;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Windows: sharing (32) and lock (33) violations are transient third-party
/// holds, most often an antivirus or indexer scanning the just-created
/// file. Everything else — including `ACCESS_DENIED`, usually a real
/// permissions problem — propagates immediately. Elsewhere nothing is
/// transient: raw code 32 on Unix is `EPIPE`, and retrying a rename on it
/// would be noise on top of a real failure.
fn is_transient_sharing_violation(err: &std::io::Error) -> bool {
    if cfg!(windows) {
        matches!(err.raw_os_error(), Some(32) | Some(33))
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
        assert_eq!(fs::read(&target).unwrap(), b"staged bytes");
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
    /// only delays it and can mask its cause.
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
    /// unchanged by the retry. On Windows, 32 and 33 retry.
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
    fn replaces_and_overwrites_without_leaving_staging_files() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("page.md");
        write_atomic(&target, b"v1", KeepMode::UmaskDefault).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"v1");
        write_atomic(&target, b"v2", KeepMode::UmaskDefault).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"v2");
        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .any(|name| name.to_string_lossy().starts_with(TMP_PREFIX));
        assert!(!leftover, "staging file leaked");
    }

    /// A page replacement lands with exactly the mode `File::create` would
    /// have given it, compared differentially against a control file so the
    /// test holds under any umask.
    #[cfg(unix)]
    #[test]
    fn page_replacements_keep_the_mode_file_create_always_gave() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("page.md");
        write_atomic(&target, b"x", KeepMode::UmaskDefault).unwrap();
        let control = dir.path().join("control");
        fs::File::create(&control).unwrap();
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            fs::metadata(&control).unwrap().permissions().mode() & 0o777,
        );
    }

    /// The state and hook modes are set verbatim: 0o600 and 0o755 whatever
    /// the umask, as both writers always did.
    #[cfg(unix)]
    #[test]
    fn exact_modes_are_set_verbatim() {
        let dir = TempDir::new().unwrap();
        for (name, mode) in [("state.json", 0o600u32), ("post-merge", 0o755)] {
            let target = dir.path().join(name);
            write_atomic(&target, b"x", KeepMode::Exact(mode)).unwrap();
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                mode,
                "{name}"
            );
        }
    }

    /// A persist that keeps failing surfaces the underlying error and
    /// leaves no staging file behind: the dropped `PersistError` deletes
    /// the untouched staging file.
    #[test]
    fn a_failed_persist_reports_and_cleans_up() {
        let dir = TempDir::new().unwrap();
        // A directory squatting on the destination makes the rename fail.
        let target = dir.path().join("page.md");
        fs::create_dir_all(&target).unwrap();
        let err = write_atomic(&target, b"x", KeepMode::UmaskDefault).unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot move the staging file over"),
            "{err:#}"
        );
        let leftover = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .any(|name| name.to_string_lossy().starts_with(TMP_PREFIX));
        assert!(!leftover, "staging file leaked on failure");
    }
}
