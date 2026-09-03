//! Pre-migration backup of the entire data directory (docs/okf.md).
//!
//! The OKF migration's first step — and its gate: no verified archive,
//! no migration. The archive lands OUTSIDE the data dir, in the user's
//! home (`AI_MEMORY_BACKUP_DIR` overrides the destination for machines
//! where home is small), and a receipt is recorded at
//! `<data_dir>/pre-migration-backup.json` so the wiki homepage can show
//! where it is until the user deletes it.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{WikiError, WikiResult};

/// File name of the receipt inside the data dir.
pub const BACKUP_RECEIPT_FILE: &str = "pre-migration-backup.json";

/// What the backup step produced; persisted as the receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupReceipt {
    /// Absolute path of the archive.
    pub archive_path: PathBuf,
    /// Archive size in bytes.
    pub size_bytes: u64,
    /// Entries in the archive (files archived).
    pub entries: usize,
    /// ISO-8601 creation instant.
    pub created_at: String,
    /// What the backup was taken for (e.g. "okf-v0.2-migration").
    pub label: String,
}

impl BackupReceipt {
    /// Read the receipt from `data_dir`, if one exists and parses.
    #[must_use]
    pub fn load(data_dir: &Path) -> Option<Self> {
        let raw = std::fs::read(data_dir.join(BACKUP_RECEIPT_FILE)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// Whether the archive the receipt points at still exists on disk.
    #[must_use]
    pub fn archive_present(&self) -> bool {
        self.archive_path.exists()
    }
}

/// Whether this process runs inside a container. Same signals as the
/// serve handler: the official image sets `AI_MEMORY_IN_CONTAINER`;
/// `/.dockerenv` (Docker) and `/run/.containerenv` (Podman) cover
/// images that do not.
#[must_use]
pub fn running_in_container() -> bool {
    if std::env::var("AI_MEMORY_IN_CONTAINER").is_ok_and(|v| !v.trim().is_empty()) {
        return true;
    }
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

/// Destination directory for archives.
///
/// - the explicit override wins (callers read `AI_MEMORY_BACKUP_DIR`);
/// - **in a container, the user's home is ephemeral** — it lives in the
///   container layer and is destroyed on the next recreation, which
///   would silently lose the safety archive. The only storage
///   guaranteed to persist is the data-dir volume, so the default there
///   is `<data_dir>/backups/` (excluded from the archive itself);
/// - otherwise the user's home.
///
/// The destination may live inside the data dir (the archive protects
/// against the MIGRATION corrupting content, not against disk loss, and
/// the walk excludes it), but never the data dir root itself.
fn destination_dir(
    dest_override: Option<&Path>,
    data_dir: &Path,
    in_container: bool,
) -> WikiResult<PathBuf> {
    let dest = if let Some(dir) = dest_override {
        dir.to_path_buf()
    } else if in_container {
        data_dir.join("backups")
    } else {
        dirs::home_dir().ok_or_else(|| {
            WikiError::Io(std::io::Error::other(
                "no home directory found for the pre-migration backup; \
                 set AI_MEMORY_BACKUP_DIR to a directory outside the data dir",
            ))
        })?
    };
    // Canonicalize both sides before comparing: `<data_dir>/.` or a
    // symlinked spelling must not defeat the root guard (or the walk's
    // self-exclusion, which compares against the path returned here).
    // The dest is created first so it ALWAYS canonicalizes — an
    // uncanonical spelling of a not-yet-existing dest defeated the
    // self-exclusion on the very first backup.
    std::fs::create_dir_all(&dest)?;
    let canonical_data = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    let canonical_dest = dest.canonicalize().unwrap_or_else(|_| dest.clone());
    if canonical_dest == canonical_data {
        return Err(WikiError::Io(std::io::Error::other(
            "the backup destination must not be the data dir root itself; \
             use a subdirectory (e.g. <data_dir>/backups) or a path outside it",
        )));
    }
    Ok(canonical_dest)
}

/// Compress `data_dir` into a timestamped tar.gz in the destination dir,
/// verify the archive is readable and complete, write the receipt, and
/// return it. Any failure aborts (the caller must not migrate).
pub fn create_pre_migration_backup(
    data_dir: &Path,
    label: &str,
    dest_override: Option<&Path>,
) -> WikiResult<BackupReceipt> {
    create_pre_migration_backup_inner(data_dir, label, dest_override, running_in_container())
}

fn create_pre_migration_backup_inner(
    data_dir: &Path,
    label: &str,
    dest_override: Option<&Path>,
    in_container: bool,
) -> WikiResult<BackupReceipt> {
    // One spelling for every comparison below: `destination_dir` returns a
    // canonical path, so the walk must produce canonical paths too or the
    // self-exclusion guards (`path == dest_dir`, `path == archive_path`)
    // silently miss under a symlinked data-dir spelling (macOS /var ->
    // /private/var; observed tarring the half-written archive itself).
    let data_dir = &data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    let dest_dir = destination_dir(dest_override, data_dir, in_container)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S").to_string();
    let archive_path = dest_dir.join(format!("ai-memory-backup-{label}-{stamp}.tar.gz"));

    // The migration runs at server startup before any traffic, so the
    // SQLite files (db + WAL/SHM) are quiescent and archived as a set.
    let mut expected = 0usize;
    {
        let file = BufWriter::new(File::create(&archive_path)?);
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.follow_symlinks(false);
        let mut stack = vec![data_dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let rel = path
                    .strip_prefix(data_dir)
                    .map_err(|e| WikiError::Io(std::io::Error::other(e)))?;
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    // Never archive the destination itself (a container
                    // default of <data_dir>/backups, or an override
                    // pointed inside the data dir) — self-inclusion
                    // would tar the half-written archive.
                    if path == dest_dir {
                        continue;
                    }
                    stack.push(path);
                } else if ft.is_file() {
                    let file_name = path.file_name().and_then(|n| n.to_str());
                    if path == archive_path
                        || file_name == Some("pre-migration-backup.json.tmp")
                        // The single-instance serve lock and its holder-info
                        // sidecar. On Windows the exclusive LockFileEx is
                        // mandatory, so the same serve process that owns the
                        // migration also holds `.serve.lock` and cannot read
                        // it back from the walk (os error 33). Keep in sync
                        // with crates/ai-memory-cli/src/commands/serve.rs
                        // `SERVE_LOCK_FILE`.
                        || file_name.is_some_and(|n| n.starts_with(".serve.lock"))
                    {
                        continue;
                    }
                    tar.append_path_with_name(&path, rel)?;
                    expected += 1;
                }
                // Symlinks are skipped: nothing in a data dir should be
                // one, and following one out of the tree must not happen.
            }
        }
        tar.into_inner()?.finish()?;
    }

    // Verify: re-open and count entries; a torn or unreadable archive
    // fails here and the caller aborts the migration.
    let mut counted = 0usize;
    {
        let file = BufReader::new(File::open(&archive_path)?);
        let dec = flate2::read::GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        for entry in ar.entries()? {
            entry?;
            counted += 1;
        }
    }
    let size_bytes = std::fs::metadata(&archive_path)?.len();
    if counted != expected || size_bytes == 0 {
        let _ = std::fs::remove_file(&archive_path);
        return Err(WikiError::Io(std::io::Error::other(format!(
            "backup verification failed: archived {expected} files but the \
             archive lists {counted} (size {size_bytes}); refusing to migrate"
        ))));
    }

    let receipt = BackupReceipt {
        archive_path,
        size_bytes,
        entries: counted,
        created_at: jiff::Timestamp::now()
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        label: label.to_string(),
    };
    let tmp = data_dir.join(format!("{BACKUP_RECEIPT_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&receipt)?)?;
    std::fs::rename(&tmp, data_dir.join(BACKUP_RECEIPT_FILE))?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn data_dir_with_content() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("wiki/ws/proj/notes")).unwrap();
        std::fs::write(tmp.path().join("wiki/ws/proj/notes/a.md"), "---\n---\nbody").unwrap();
        std::fs::write(tmp.path().join("db.sqlite"), b"not really a db").unwrap();
        tmp
    }

    #[test]
    fn a_backup_archives_everything_and_writes_a_receipt() {
        let data = data_dir_with_content();
        let dest = TempDir::new().unwrap();
        let receipt = create_pre_migration_backup(data.path(), "test", Some(dest.path())).unwrap();
        // TempDir spellings differ from canonical on macOS (/var vs
        // /private/var); the backup works in canonical space.
        assert!(
            receipt
                .archive_path
                .starts_with(dest.path().canonicalize().unwrap())
        );
        assert_eq!(receipt.entries, 2);
        assert!(receipt.size_bytes > 0);
        assert!(receipt.archive_present());
        let loaded = BackupReceipt::load(data.path()).unwrap();
        assert_eq!(loaded.archive_path, receipt.archive_path);
    }

    #[test]
    fn an_unwritable_destination_aborts_instead_of_falling_back() {
        let data = data_dir_with_content();
        let dest = TempDir::new().unwrap();
        let blocked = dest.path().join("blocked-file");
        std::fs::write(&blocked, b"a file, not a dir").unwrap();
        let err = create_pre_migration_backup(data.path(), "test", Some(&blocked));
        assert!(err.is_err(), "backup must fail when it cannot write");
        assert!(
            BackupReceipt::load(data.path()).is_none(),
            "no receipt without an archive"
        );
    }

    /// In a container the home dir is ephemeral (destroyed on the next
    /// recreation), so the default destination is the data-dir volume's
    /// backups/ subdir — and the archive must not swallow itself.
    #[test]
    fn in_a_container_the_archive_lands_on_the_data_volume() {
        let data = data_dir_with_content();
        let receipt = create_pre_migration_backup_inner(data.path(), "test", None, true).unwrap();
        assert!(
            receipt
                .archive_path
                .starts_with(data.path().canonicalize().unwrap().join("backups"))
        );
        // The two data files, not the archive or receipt.
        assert_eq!(receipt.entries, 2);
        assert!(receipt.archive_present());
    }

    /// A destination inside the data dir (container default or an
    /// override) is excluded from the walk: entry counts match the
    /// verification pass and a second backup does not tar the first.
    #[test]
    fn a_destination_inside_the_data_dir_is_never_self_included() {
        let data = data_dir_with_content();
        let dest = data.path().join("backups");
        let first = create_pre_migration_backup(data.path(), "test", Some(&dest)).unwrap();
        assert_eq!(first.entries, 2);
        // Receipt now exists in the data dir; a second run must archive
        // it (a real file) but never the prior archive in backups/.
        let second = create_pre_migration_backup(data.path(), "test", Some(&dest)).unwrap();
        assert_eq!(second.entries, 3, "data files + first receipt");
    }

    /// Reproduces the macOS matrix failure on any Unix: hand the backup a
    /// symlinked spelling of the data dir (as macOS's /var -> /private/var
    /// does implicitly) and prove the self-exclusion still holds — a second
    /// run must not descend into backups/ or tar the first archive.
    #[cfg(unix)]
    #[test]
    fn self_exclusion_survives_a_symlinked_data_dir_spelling() {
        let real = data_dir_with_content();
        let outer = TempDir::new().unwrap();
        let alias = outer.path().join("alias");
        std::os::unix::fs::symlink(real.path(), &alias).unwrap();
        let dest = alias.join("backups");
        let first = create_pre_migration_backup(&alias, "test", Some(&dest)).unwrap();
        assert_eq!(first.entries, 2);
        let second = create_pre_migration_backup(&alias, "test", Some(&dest)).unwrap();
        assert_eq!(
            second.entries, 3,
            "data files + first receipt, never the archive"
        );
    }

    #[test]
    fn the_data_dir_root_is_refused_as_a_destination() {
        let data = data_dir_with_content();
        let err = create_pre_migration_backup(data.path(), "test", Some(data.path()));
        assert!(err.is_err(), "data dir root must be refused");
    }

    #[test]
    fn a_deleted_archive_is_reported_absent() {
        let data = data_dir_with_content();
        let dest = TempDir::new().unwrap();
        let receipt = create_pre_migration_backup(data.path(), "test", Some(dest.path())).unwrap();
        std::fs::remove_file(&receipt.archive_path).unwrap();
        assert!(!receipt.archive_present());
    }

    /// The pre-migration backup runs in the same process as `serve`, which
    /// holds `.serve.lock` under an exclusive LockFileEx. On Windows that
    /// lock is mandatory, so the walk cannot read the file back. The walk
    /// must skip `.serve.lock` and its `.serve.lock.holder` sidecar by name,
    /// exactly as it already skips the archive and the receipt tmp. Simulated
    /// here on all platforms because the skip is by name, independent of OS
    /// lock semantics; a Windows CI running the real lock would hit the same
    /// path after this guard.
    #[test]
    fn the_backup_skips_the_serve_lock_and_its_holder_sidecar() {
        let data = data_dir_with_content();
        std::fs::write(data.path().join(".serve.lock"), b"").unwrap();
        std::fs::write(data.path().join(".serve.lock.holder"), b"pid=1").unwrap();
        let dest = TempDir::new().unwrap();
        let receipt = create_pre_migration_backup(data.path(), "test", Some(dest.path())).unwrap();
        assert_eq!(
            receipt.entries, 2,
            "only the two data files; serve lock and holder never archived"
        );
        let file = std::fs::File::open(&receipt.archive_path).unwrap();
        let dec = flate2::read::GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        for entry in ar.entries().unwrap() {
            let name = entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert!(
                !name.starts_with(".serve.lock"),
                "archive must not contain serve lock entry: {name:?}"
            );
        }
    }
}
