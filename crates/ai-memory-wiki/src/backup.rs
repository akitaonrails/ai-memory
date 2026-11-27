//! Pre-migration backup of the entire data directory (docs/okf.md).
//!
//! The OKF migration's first step — and its gate: no verified archive,
//! no migration. The archive lands OUTSIDE the data dir, in the user's
//! home (`AI_MEMORY_BACKUP_DIR` overrides the destination for machines
//! where home is small), and a receipt is recorded at
//! `<data_dir>/pre-migration-backup.json` so the wiki homepage can show
//! where it is until the user deletes it.
// Backups are written outside the wiki tree and are not part of its history.
#![allow(clippy::disallowed_methods)]

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
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
    /// Free space on the destination filesystem right after the archive was
    /// written, in bytes. `None` when it could not be read (e.g. an
    /// unsupported filesystem) — never fails the backup on its own. A
    /// verified archive next to a nearly-full disk is the exact shape of
    /// the outage that motivated this field: the WAL had nowhere left to
    /// extend minutes after a backup that "succeeded".
    pub dest_free_bytes: Option<u64>,
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
    crate::confinement::inspect_tree_if_present(&data_dir.join("wiki"))?;
    // One spelling for every comparison below: `destination_dir` returns a
    // canonical path, so the walk must produce canonical paths too or the
    // self-exclusion guards (`path == dest_dir`, `path == archive_path`)
    // silently miss under a symlinked data-dir spelling (macOS /var ->
    // /private/var; observed tarring the half-written archive itself).
    let data_dir = &data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    crate::confinement::inspect_tree(data_dir)?;
    let dest_dir = destination_dir(dest_override, data_dir, in_container)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%d-%H%M%S").to_string();
    let (archive_path, archive_file) = reserve_archive(&dest_dir, label, &stamp)?;

    // The migration runs at server startup before any traffic, so the
    // SQLite files (db + WAL/SHM) are quiescent and archived as a set.
    let mut expected = 0usize;
    {
        let file = BufWriter::new(archive_file);
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
                let metadata = std::fs::symlink_metadata(&path)?;
                if crate::confinement::is_link_like(&metadata) {
                    return Err(WikiError::Confinement {
                        path,
                        reason: "symbolic links and reparse points are not allowed in backups",
                    });
                }
                if metadata.is_dir() {
                    // Never archive the destination itself (a container
                    // default of <data_dir>/backups, or an override
                    // pointed inside the data dir) — self-inclusion
                    // would tar the half-written archive.
                    if path == dest_dir {
                        continue;
                    }
                    stack.push(path);
                } else if metadata.is_file() {
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
            }
        }
        // The verify pass below re-reads the page cache, so it cannot tell an
        // archive on stable storage from one that is not. This archive is the
        // gate for a destructive in-place migration, so it has to survive a
        // crash: fsync the file, then its directory entry. The admin backup
        // endpoints sync after `finish` for the same reason.
        finish_archive(tar)?.sync_all()?;
        File::open(&dest_dir)
            .and_then(|dir| dir.sync_all())
            .unwrap_or_else(|e| {
                tracing::debug!(
                    dest = %dest_dir.display(),
                    error = %e,
                    "could not fsync the backup destination directory"
                );
            });
    }

    // Verify: re-open and count entries; a torn or unreadable archive
    // fails here and the caller aborts the migration.
    let counted = verify_archive_entries(&archive_path)?;
    let size_bytes = std::fs::metadata(&archive_path)?.len();
    if counted != expected || size_bytes == 0 {
        let _ = std::fs::remove_file(&archive_path);
        return Err(WikiError::Io(std::io::Error::other(format!(
            "backup verification failed: archived {expected} files but the \
             archive lists {counted} (size {size_bytes}); refusing to migrate"
        ))));
    }

    // A signal only: the backup itself already succeeded and verified, so a
    // free-space read that fails (unsupported filesystem, transient error)
    // must not turn a good archive into a failed migration.
    let dest_free_bytes = match fs2::available_space(&dest_dir) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            tracing::debug!(
                dest = %dest_dir.display(),
                error = %e,
                "could not read destination free space"
            );
            None
        }
    };

    let receipt = BackupReceipt {
        archive_path,
        size_bytes,
        entries: counted,
        created_at: jiff::Timestamp::now()
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
        label: label.to_string(),
        dest_free_bytes,
    };
    let tmp = data_dir.join(format!("{BACKUP_RECEIPT_FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&receipt)?)?;
    std::fs::rename(&tmp, data_dir.join(BACKUP_RECEIPT_FILE))?;
    Ok(receipt)
}

/// Different data dirs can migrate in the same second and share a destination
/// (the default is the user's home). Reserve a new path atomically so neither
/// archive can replace the other's only pre-migration snapshot.
fn reserve_archive(dest: &Path, label: &str, stamp: &str) -> WikiResult<(PathBuf, File)> {
    for suffix in 0_u32..=u32::MAX {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("-{suffix}")
        };
        let path = dest.join(format!("ai-memory-backup-{label}-{stamp}{suffix}.tar.gz"));
        match File::options().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(WikiError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve a unique pre-migration archive path",
    )))
}

/// End the gzip stream and flush the buffer, returning the underlying writer.
///
/// `GzEncoder::finish` hands back the `BufWriter`; dropping it would write the
/// buffered tail from `Drop`, which discards the error, and a backup on a full
/// disk would then reach the verify pass as if it had been written.
fn finish_archive<W: Write>(
    tar: tar::Builder<flate2::write::GzEncoder<BufWriter<W>>>,
) -> std::io::Result<W> {
    tar.into_inner()?
        .finish()?
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)
}

fn verify_archive_entries(path: &Path) -> std::io::Result<usize> {
    let file = BufReader::new(File::open(path)?);
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut counted = 0;
    for entry in archive.entries()? {
        entry?;
        counted += 1;
    }
    // Tar's end marker can arrive before gzip checks its checksum and size.
    std::io::copy(&mut archive.into_inner(), &mut std::io::sink())?;
    Ok(counted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A destination that takes no bytes, like a full disk.
    struct FullDisk;

    impl Write for FullDisk {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::StorageFull.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The whole small archive sits in the `BufWriter` until `finish`, so the
    /// write that fails is the one flushing that tail; it must reach the
    /// caller instead of being dropped with the buffer.
    #[test]
    fn a_failed_final_flush_fails_the_archive() {
        let enc =
            flate2::write::GzEncoder::new(BufWriter::new(FullDisk), flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        let body = b"page body";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_cksum();
        tar.append_data(&mut header, "wiki/page.md", &body[..])
            .unwrap();

        let error = finish_archive(tar).err().expect("a full disk must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::StorageFull);
    }

    fn data_dir_with_content() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("wiki/ws/proj/notes")).unwrap();
        std::fs::write(tmp.path().join("wiki/ws/proj/notes/a.md"), "---\n---\nbody").unwrap();
        std::fs::write(tmp.path().join("db.sqlite"), b"not really a db").unwrap();
        tmp
    }

    #[cfg(unix)]
    #[test]
    fn a_pre_migration_backup_refuses_a_linked_wiki_tree_without_an_archive() {
        let data = data_dir_with_content();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("canary.md"), "outside canary").unwrap();
        std::os::unix::fs::symlink(outside.path(), data.path().join("wiki/linked")).unwrap();
        let dest = TempDir::new().unwrap();

        assert!(matches!(
            create_pre_migration_backup(data.path(), "test", Some(dest.path())),
            Err(WikiError::Confinement { .. })
        ));
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_to_string(outside.path().join("canary.md")).unwrap(),
            "outside canary"
        );
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
    fn verification_requires_an_intact_gzip_trailer() {
        let data = data_dir_with_content();
        let dest = TempDir::new().unwrap();
        let receipt = create_pre_migration_backup(data.path(), "test", Some(dest.path())).unwrap();
        let path = &receipt.archive_path;
        assert_eq!(verify_archive_entries(path).unwrap(), receipt.entries);

        let archive = std::fs::read(path).unwrap();
        let trailer_start = archive.len() - 8;
        let mut damaged_checksum = archive.clone();
        damaged_checksum[trailer_start] ^= 1;

        for bytes in [&archive[..trailer_start], damaged_checksum.as_slice()] {
            std::fs::write(path, bytes).unwrap();
            assert!(verify_archive_entries(path).is_err());
        }
    }

    #[test]
    fn separate_installs_keep_their_own_pre_migration_archives() {
        let first_data = data_dir_with_content();
        let second_data = data_dir_with_content();
        std::fs::write(second_data.path().join("db.sqlite"), b"another install").unwrap();
        let dest = TempDir::new().unwrap();
        let first =
            create_pre_migration_backup(first_data.path(), "test", Some(dest.path())).unwrap();
        let original = std::fs::read(&first.archive_path).unwrap();
        let second =
            create_pre_migration_backup(second_data.path(), "test", Some(dest.path())).unwrap();

        assert_ne!(first.archive_path, second.archive_path);
        assert_eq!(std::fs::read(&first.archive_path).unwrap(), original);
        assert_eq!(
            BackupReceipt::load(first_data.path()).unwrap().archive_path,
            first.archive_path
        );
        assert_eq!(
            BackupReceipt::load(second_data.path())
                .unwrap()
                .archive_path,
            second.archive_path
        );
    }

    #[test]
    fn archive_reservations_at_the_same_timestamp_preserve_existing_files() {
        let dest = TempDir::new().unwrap();
        let stamp = "20261028-120000";
        let first = dest
            .path()
            .join(format!("ai-memory-backup-test-{stamp}.tar.gz"));
        let second = dest
            .path()
            .join(format!("ai-memory-backup-test-{stamp}-1.tar.gz"));
        std::fs::write(&first, b"first snapshot").unwrap();
        std::fs::write(&second, b"second snapshot").unwrap();

        let (third, mut file) = reserve_archive(dest.path(), "test", stamp).unwrap();
        file.write_all(b"third snapshot").unwrap();
        drop(file);

        assert_eq!(std::fs::read(&first).unwrap(), b"first snapshot");
        assert_eq!(std::fs::read(&second).unwrap(), b"second snapshot");
        assert_eq!(std::fs::read(&third).unwrap(), b"third snapshot");
        assert_eq!(
            third.file_name().unwrap(),
            format!("ai-memory-backup-test-{stamp}-2.tar.gz").as_str()
        );
    }

    /// The signal this field exists for: a verified archive next to a
    /// nearly-full disk. A temp dir always reports some free space, and the
    /// receipt (round-tripped through the JSON file) must carry it.
    #[test]
    fn the_receipt_carries_destination_free_space() {
        let data = data_dir_with_content();
        let dest = TempDir::new().unwrap();
        let receipt = create_pre_migration_backup(data.path(), "test", Some(dest.path())).unwrap();
        let free = receipt
            .dest_free_bytes
            .expect("free space should be readable for a real temp dir");
        assert!(free > 0);
        let loaded = BackupReceipt::load(data.path()).unwrap();
        assert_eq!(loaded.dest_free_bytes, receipt.dest_free_bytes);
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
