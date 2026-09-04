//! One-shot migration of an existing store to native OKF v0.2
//! (docs/okf.md). Order is fixed and each step gates the next:
//!
//! 1. **verified backup of the whole data dir, or no migration at all**;
//! 2. git checkpoint of the wiki tree;
//! 3. DB pass: conform every latest page row IN PLACE (same id, same
//!    version row, `updated_at` untouched);
//! 4. file pass: conform every wiki `.md` in place (body untouched),
//!    `generated.at` from the row's `updated_at` when the DB pass
//!    rewrote that row, else the file's mtime (already-conformant rows
//!    are skipped by the DB pass, so their files fall back to mtime —
//!    the strip-comparison keeps row and file in agreement either
//!    way); scope `_meta.md` manifests get
//!    their `type` only; each project bundle root gains an `index.md`
//!    declaring `okf_version` when absent;
//! 5. single "okf-migration" git commit.
//!
//! Idempotent: on a conformant store both passes find nothing and the
//! backup gate never engages (fresh installs never create archives).

use std::path::{Path, PathBuf};

use ai_memory_store::WriterHandle;

use super::WikiMigration;
use crate::error::{WikiError, WikiResult};
use crate::markdown::{Markdown, emit, parse};

pub struct OkfConformance {
    /// Where the backup archive lands; `None` = the user's home. The
    /// registry fills it from `AI_MEMORY_BACKUP_DIR`; tests inject a
    /// tempdir directly.
    pub dest_override: Option<PathBuf>,
}

impl OkfConformance {
    /// Construct with the destination taken from `AI_MEMORY_BACKUP_DIR`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            dest_override: std::env::var("AI_MEMORY_BACKUP_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from),
        }
    }
}

const BACKUP_LABEL: &str = "okf-v0.2";

/// Take the pre-migration safety archive **before** the store's DB schema is
/// migrated, so it captures the true pre-2.0 state a 1.x binary can reopen
/// (#633). Call this from the boot path *before* `Store::open`, which advances
/// the SQLite schema — otherwise the archive's `db/` is already at 2.x and the
/// documented 1.x rollback is impossible.
///
/// Gated to the real 1.x→2.0 upgrade and idempotent:
/// - if a usable backup already exists (`pre-migration-backup.json` pointing at
///   a present archive), reuse it — never overwrite a good pre-migration snapshot;
/// - if the wiki has no pre-OKF files, this is a fresh install or an already
///   conformant store — nothing to protect, take nothing (unchanged behaviour);
/// - otherwise archive the whole data dir now.
///
/// `dest_override` is the destination directory (the serve path passes
/// `AI_MEMORY_BACKUP_DIR`); `None` uses the default resolution. A `None`
/// *return* means "no backup was needed/taken here".
///
/// # Errors
/// [`WikiError`] if the wiki scan or the archive write fails.
pub fn snapshot_before_db_migration(
    data_dir: &Path,
    dest_override: Option<&Path>,
) -> WikiResult<Option<crate::backup::BackupReceipt>> {
    if crate::backup::BackupReceipt::load(data_dir).is_some_and(|r| r.archive_present()) {
        // A prior boot in this same upgrade already captured the pre-migration
        // state; do not overwrite it with a now-partially-migrated snapshot.
        return Ok(None);
    }
    let wiki_root = data_dir.join("wiki");
    if nonconformant_files(&wiki_root)?.is_empty() {
        // No pre-OKF wiki files → not a 1.x store being upgraded.
        return Ok(None);
    }
    let receipt =
        crate::backup::create_pre_migration_backup(data_dir, BACKUP_LABEL, dest_override)?;
    tracing::info!(
        archive = %receipt.archive_path.display(),
        size_bytes = receipt.size_bytes,
        dest_free_bytes = ?receipt.dest_free_bytes,
        "pre-migration backup taken before the DB schema migration (#633: 1.x-restorable)"
    );
    Ok(Some(receipt))
}

#[async_trait::async_trait]
impl WikiMigration for OkfConformance {
    fn name(&self) -> &'static str {
        "2026_09_01T18_00_okf_v02_conformance"
    }

    fn description(&self) -> &'static str {
        "conform wiki pages and index rows to OKF v0.2 (backup-gated, in place)"
    }

    async fn up(&self, writer: &WriterHandle, wiki_root: &Path) -> WikiResult<()> {
        let data_dir = wiki_root.parent().ok_or_else(|| {
            WikiError::Io(std::io::Error::other("wiki root has no parent data dir"))
        })?;

        let db_pending = writer.okf_nonconformant_count().await?;
        let file_pending = nonconformant_files(wiki_root)?;
        if db_pending == 0 && file_pending.is_empty() {
            // Fresh install or already conformant: nothing to protect,
            // nothing to rewrite, no archive.
            return Ok(());
        }
        tracing::info!(
            db_rows = db_pending,
            files = file_pending.len(),
            "OKF migration: taking the pre-migration backup first"
        );

        // 1. Backup gate. The boot path takes this archive BEFORE `Store::open`
        //    migrates the DB schema (#633), so a usable receipt normally already
        //    exists here — reuse it rather than archive the now-DB-migrated tree
        //    a second time. Only archive here for the rare path where the DB was
        //    already at 2.x but the wiki was not yet conformant (no pre-open
        //    snapshot was taken).
        match crate::backup::BackupReceipt::load(data_dir)
            .filter(crate::backup::BackupReceipt::archive_present)
        {
            Some(existing) => {
                tracing::info!(
                    archive = %existing.archive_path.display(),
                    "reusing the pre-open pre-migration backup (#633)"
                );
            }
            None => {
                let receipt = crate::backup::create_pre_migration_backup(
                    data_dir,
                    BACKUP_LABEL,
                    self.dest_override.as_deref(),
                )?;
                tracing::info!(
                    archive = %receipt.archive_path.display(),
                    size_bytes = receipt.size_bytes,
                    entries = receipt.entries,
                    dest_free_bytes = ?receipt.dest_free_bytes,
                    "pre-migration backup verified"
                );
            }
        }

        // 2. Checkpoint whatever the tree holds before touching it.
        let git = crate::git::GitAdapter::open_or_init(wiki_root)?;
        git.commit_all("pre-okf-migration checkpoint")?;

        // 3. DB pass — in place, through the single writer.
        let migrated = writer.okf_migrate_latest_pages().await?;
        let at_by_page: std::collections::HashMap<(String, String, String), String> = migrated
            .iter()
            .map(|m| {
                (
                    (
                        m.workspace_id.to_string(),
                        m.project_id.to_string(),
                        m.path.clone(),
                    ),
                    m.generated_at.clone(),
                )
            })
            .collect();

        // 4. File pass.
        for file in file_pending {
            conform_file(wiki_root, &file, &at_by_page)?;
        }
        ensure_bundle_indexes(wiki_root)?;

        // 5. One commit for the whole rewrite.
        git.commit_all("okf-migration: conform wiki to OKF v0.2")?;
        tracing::info!(
            rows = migrated.len(),
            "OKF migration complete; homepage will show the backup notice \
             until the archive is deleted"
        );
        Ok(())
    }
}

/// Wiki-relative paths of `.md` files that still need the file pass.
fn nonconformant_files(wiki_root: &Path) -> WikiResult<Vec<PathBuf>> {
    let mut pending = Vec::new();
    let mut stack = vec![wiki_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file()
                && path.extension().is_some_and(|e| e == "md")
                && path.file_name().is_none_or(|n| n != "index.md")
            {
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let fm = parse(&raw).map(|m| m.frontmatter).unwrap_or_default();
                let conformant = if is_meta_manifest(&path) {
                    ai_memory_core::okf::is_conformant(&fm)
                } else {
                    ai_memory_core::okf::is_conformant(&fm)
                        && ai_memory_core::okf::generated_at(&fm).is_some()
                };
                if !conformant {
                    pending.push(path.strip_prefix(wiki_root).unwrap_or(&path).to_path_buf());
                }
            }
        }
    }
    pending.sort();
    Ok(pending)
}

fn is_meta_manifest(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "_meta.md")
}

/// Conform one file in place: same body, frontmatter filled. Manifests
/// (`_meta.md`) get their `type` only — they are identity records, not
/// concept pages with provenance.
fn conform_file(
    wiki_root: &Path,
    rel: &Path,
    at_by_page: &std::collections::HashMap<(String, String, String), String>,
) -> WikiResult<()> {
    let abs = wiki_root.join(rel);
    let raw = std::fs::read_to_string(&abs)?;
    let mut md = parse(&raw).unwrap_or_else(|_| Markdown {
        frontmatter: serde_json::Value::Object(serde_json::Map::new()),
        body: raw.clone(),
    });

    if is_meta_manifest(&abs) {
        if let Some(map) = md.frontmatter.as_object_mut() {
            map.entry("type".to_string())
                .or_insert(serde_json::Value::String("Scope Manifest".into()));
        }
    } else {
        // Page-relative path (strip `<ws>/<proj>/`) drives type derivation
        // and the row lookup.
        let components: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let (page_rel, row_key) = if components.len() >= 3 {
            (
                components[2..].join("/"),
                Some((
                    components[0].clone(),
                    components[1].clone(),
                    components[2..].join("/"),
                )),
            )
        } else {
            (components.join("/"), None)
        };
        ai_memory_core::okf::conform_frontmatter(&page_rel, &mut md.frontmatter);
        if ai_memory_core::okf::generated_at(&md.frontmatter).is_none() {
            let at = row_key
                .and_then(|k| at_by_page.get(&k).cloned())
                .unwrap_or_else(|| mtime_iso(&abs));
            ai_memory_core::okf::stamp_generated_at(&mut md.frontmatter, &at);
        }
    }

    let emitted = emit(&md)?;
    if emitted != raw {
        let tmp = tempfile::NamedTempFile::new_in(abs.parent().unwrap_or(wiki_root))?;
        std::fs::write(tmp.path(), emitted.as_bytes())?;
        crate::atomic::persist_with_retry(tmp, &abs)?;
    }
    Ok(())
}

fn mtime_iso(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| jiff::Timestamp::from_second(d.as_secs() as i64).ok())
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Each project directory is one OKF bundle: give it an `index.md`
/// declaring `okf_version` when absent (the only place index.md may
/// carry frontmatter, per spec).
fn ensure_bundle_indexes(wiki_root: &Path) -> WikiResult<()> {
    let Ok(workspaces) = std::fs::read_dir(wiki_root) else {
        return Ok(());
    };
    for ws in workspaces.flatten() {
        if !ws.file_type().map(|t| t.is_dir()).unwrap_or(false) || ws.file_name() == ".git" {
            continue;
        }
        let Ok(projects) = std::fs::read_dir(ws.path()) else {
            continue;
        };
        for proj in projects.flatten() {
            if !proj.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let index = proj.path().join("index.md");
            if index.exists() {
                continue;
            }
            let mut families: Vec<String> = std::fs::read_dir(proj.path())
                .map(|it| {
                    it.flatten()
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            families.sort();
            let listing = families
                .iter()
                .map(|f| format!("- [{f}/]({f}/)\n"))
                .collect::<String>();
            let body =
                format!("# Bundle index\n\nConcept files live in these directories:\n\n{listing}");
            let content = format!("---\nokf_version: \"0.2\"\n---\n\n{body}");
            std::fs::write(&index, content)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ai_memory_core::{PagePath, Tier};
    use ai_memory_store::Store;
    use tempfile::TempDir;

    use super::*;
    use crate::wiki::{Wiki, WritePageRequest};

    /// Build a data dir that looks like a 1.x store: pages written
    /// through the current stack, then stripped of every OKF key in
    /// both the row and the file — exactly what a pre-2.0 install holds.
    async fn legacy_store() -> (
        TempDir,
        Store,
        ai_memory_core::WorkspaceId,
        ai_memory_core::ProjectId,
    ) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        for (path, body) in [
            ("gotchas/linker.md", "mind the linker"),
            ("notes/setup.md", "how it was set up"),
        ] {
            wiki.write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new(path).unwrap(),
                frontmatter: serde_json::json!({"title": path}),
                body: body.into(),
                tier: Tier::Semantic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ai_memory_core::ActorContext::anonymous(),
            })
            .await
            .unwrap();
        }

        // Strip OKF keys from rows (second connection; the writer is idle).
        let db = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
        let rows: Vec<(Vec<u8>, String)> = db
            .prepare("SELECT id, frontmatter_json FROM pages")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (id, fm_str) in rows {
            let mut fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap();
            if let Some(map) = fm.as_object_mut() {
                for key in ["type", "generated", "sources", "stale_after", "description"] {
                    map.remove(key);
                }
            }
            db.execute(
                "UPDATE pages SET frontmatter_json = ?1 WHERE id = ?2",
                rusqlite::params![serde_json::to_string(&fm).unwrap(), id],
            )
            .unwrap();
        }
        drop(db);

        // Strip the same keys from the files.
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        for rel in ["gotchas/linker.md", "notes/setup.md"] {
            let abs = proj_dir.join(rel);
            let mut md = parse(&std::fs::read_to_string(&abs).unwrap()).unwrap();
            if let Some(map) = md.frontmatter.as_object_mut() {
                for key in ["type", "generated", "sources", "stale_after", "description"] {
                    map.remove(key);
                }
            }
            std::fs::write(&abs, emit(&md).unwrap()).unwrap();
        }
        (tmp, store, ws, proj)
    }

    fn page_rows(data_dir: &Path) -> Vec<(Vec<u8>, String, i64, String)> {
        let db = rusqlite::Connection::open(data_dir.join("db").join("memory.sqlite")).unwrap();
        db.prepare(
            "SELECT id, path, updated_at, frontmatter_json FROM pages \
             WHERE is_latest = 1 ORDER BY path",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    #[tokio::test]
    async fn a_legacy_store_is_migrated_in_place_with_a_verified_backup() {
        let (tmp, store, ws, proj) = legacy_store().await;
        let before = page_rows(tmp.path());
        let dest = TempDir::new().unwrap();
        let migration = OkfConformance {
            dest_override: Some(dest.path().to_path_buf()),
        };
        migration
            .up(&store.writer, &tmp.path().join("wiki"))
            .await
            .unwrap();

        // No-churn: same ids, same updated_at, one row per page.
        let after = page_rows(tmp.path());
        assert_eq!(before.len(), after.len());
        for (b, a) in before.iter().zip(after.iter()) {
            assert_eq!(b.0, a.0, "page id changed");
            assert_eq!(b.2, a.2, "updated_at changed");
            let fm: serde_json::Value = serde_json::from_str(&a.3).unwrap();
            assert!(ai_memory_core::okf::is_conformant(&fm), "{}", a.1);
            assert!(ai_memory_core::okf::generated_at(&fm).is_some());
        }

        // Files conformant, bundle index declared.
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        let fm = parse(&std::fs::read_to_string(proj_dir.join("gotchas/linker.md")).unwrap())
            .unwrap()
            .frontmatter;
        assert_eq!(fm["type"], "Gotcha");
        assert!(ai_memory_core::okf::generated_at(&fm).is_some());
        let index = std::fs::read_to_string(proj_dir.join("index.md")).unwrap();
        assert!(index.contains("okf_version: \"0.2\""));

        // Backup exists, receipt recorded, archive present.
        let receipt = crate::backup::BackupReceipt::load(tmp.path()).unwrap();
        assert!(receipt.archive_present());
        assert!(receipt.entries > 0);
    }

    #[tokio::test]
    async fn a_second_run_migrates_nothing_and_takes_no_second_backup() {
        let (tmp, store, _ws, _proj) = legacy_store().await;
        let dest = TempDir::new().unwrap();
        let migration = OkfConformance {
            dest_override: Some(dest.path().to_path_buf()),
        };
        migration
            .up(&store.writer, &tmp.path().join("wiki"))
            .await
            .unwrap();
        let archives_after_first = std::fs::read_dir(dest.path()).unwrap().count();
        let rows_after_first = page_rows(tmp.path());

        migration
            .up(&store.writer, &tmp.path().join("wiki"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_dir(dest.path()).unwrap().count(),
            archives_after_first,
            "idempotent re-run took another backup"
        );
        assert_eq!(rows_after_first, page_rows(tmp.path()));
    }

    #[tokio::test]
    async fn a_fresh_store_skips_the_backup_entirely() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        std::fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        let dest = TempDir::new().unwrap();
        let migration = OkfConformance {
            dest_override: Some(dest.path().to_path_buf()),
        };
        migration
            .up(&store.writer, &tmp.path().join("wiki"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_dir(dest.path()).unwrap().count(),
            0,
            "fresh install created a backup archive"
        );
        assert!(crate::backup::BackupReceipt::load(tmp.path()).is_none());
    }

    // ---- #633: the safety archive must be taken BEFORE the DB migration ----

    /// The core assertion for #633: the pre-open snapshot captures the DB as it
    /// was at snapshot time, so a mutation applied afterwards — which is exactly
    /// what `Store::open`'s schema migration is — is NOT in the archive. Prove
    /// it end to end: write PRE, snapshot, overwrite to POST, and the extracted
    /// archive must still read PRE. If it read POST, the archive would be
    /// post-migration and a 1.x binary could not use it.
    #[test]
    fn snapshot_archive_captures_the_db_before_a_later_mutation() {
        let tmp = TempDir::new().unwrap();
        // A non-OKF wiki file makes this look like a 1.x store to the gate.
        let proj_dir = tmp.path().join("wiki").join("w").join("p");
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(proj_dir.join("note.md"), "# Note\n\nlegacy body\n").unwrap();
        // A DB carrying a sentinel row.
        let db_path = tmp.path().join("db").join("memory.sqlite");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshot_marker(v TEXT); INSERT INTO snapshot_marker VALUES('PRE');",
            )
            .unwrap();
        }

        let dest = TempDir::new().unwrap();
        let receipt = snapshot_before_db_migration(tmp.path(), Some(dest.path()))
            .unwrap()
            .expect("a legacy store must be snapshotted");

        // Mutate the on-disk DB AFTER the snapshot, as a schema migration would.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("UPDATE snapshot_marker SET v = 'POST'", [])
                .unwrap();
        }

        // Extract the archive and read its DB: it must still say PRE.
        let extract = TempDir::new().unwrap();
        {
            let file = std::fs::File::open(&receipt.archive_path).unwrap();
            let dec = flate2::read::GzDecoder::new(file);
            tar::Archive::new(dec).unpack(extract.path()).unwrap();
        }
        let archived_db = extract.path().join("db").join("memory.sqlite");
        let conn = rusqlite::Connection::open(&archived_db).unwrap();
        let marker: String = conn
            .query_row("SELECT v FROM snapshot_marker", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            marker, "PRE",
            "the archive captured the DB after a later mutation — the backup is not pre-migration"
        );
    }

    #[tokio::test]
    async fn snapshot_backs_up_a_legacy_store() {
        let (tmp, _store, _ws, _proj) = legacy_store().await;
        let dest = TempDir::new().unwrap();
        let receipt = snapshot_before_db_migration(tmp.path(), Some(dest.path())).unwrap();
        assert!(
            receipt.is_some(),
            "a legacy store must be snapshotted pre-open"
        );
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 1);
        assert!(crate::backup::BackupReceipt::load(tmp.path()).is_some());
    }

    #[tokio::test]
    async fn snapshot_skips_a_conformant_or_fresh_store() {
        let tmp = TempDir::new().unwrap();
        let _store = Store::open(tmp.path()).unwrap();
        std::fs::create_dir_all(tmp.path().join("wiki")).unwrap();
        let dest = TempDir::new().unwrap();
        assert!(
            snapshot_before_db_migration(tmp.path(), Some(dest.path()))
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    /// The pre-open snapshot and the OKF migration must not both archive: the
    /// migration reuses the receipt the boot path already wrote.
    #[tokio::test]
    async fn okf_migration_reuses_the_pre_open_snapshot() {
        let (tmp, store, _ws, _proj) = legacy_store().await;
        let pre_dest = TempDir::new().unwrap();
        let receipt = snapshot_before_db_migration(tmp.path(), Some(pre_dest.path()))
            .unwrap()
            .expect("legacy store snapshotted pre-open");
        assert_eq!(std::fs::read_dir(pre_dest.path()).unwrap().count(), 1);

        // A different dest: if the migration re-archived, an artifact would land
        // here. It must reuse the pre-open one instead.
        let okf_dest = TempDir::new().unwrap();
        let migration = OkfConformance {
            dest_override: Some(okf_dest.path().to_path_buf()),
        };
        migration
            .up(&store.writer, &tmp.path().join("wiki"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_dir(okf_dest.path()).unwrap().count(),
            0,
            "the OKF migration took a second backup instead of reusing the pre-open one"
        );
        assert!(
            receipt.archive_present(),
            "the reused archive must still exist"
        );
    }

    #[tokio::test]
    async fn snapshot_is_idempotent_and_never_overwrites() {
        let (tmp, _store, _ws, _proj) = legacy_store().await;
        let dest = TempDir::new().unwrap();
        assert!(
            snapshot_before_db_migration(tmp.path(), Some(dest.path()))
                .unwrap()
                .is_some()
        );
        let after_first = std::fs::read_dir(dest.path()).unwrap().count();
        assert!(
            snapshot_before_db_migration(tmp.path(), Some(dest.path()))
                .unwrap()
                .is_none(),
            "a second snapshot when a usable backup exists must be a no-op"
        );
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), after_first);
    }

    /// The gate itself: when the backup cannot be written, the migration
    /// must fail and leave every row and file untouched.
    #[tokio::test]
    async fn a_failing_backup_aborts_before_anything_is_touched() {
        let (tmp, store, ws, proj) = legacy_store().await;
        let before = page_rows(tmp.path());
        let dest = TempDir::new().unwrap();
        let blocked = dest.path().join("blocked-file");
        std::fs::write(&blocked, b"a file, not a dir").unwrap();
        let migration = OkfConformance {
            dest_override: Some(blocked),
        };
        let result = migration.up(&store.writer, &tmp.path().join("wiki")).await;
        assert!(result.is_err(), "migration must refuse without a backup");
        assert_eq!(before, page_rows(tmp.path()), "rows were touched");
        let proj_dir = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string());
        let fm = parse(&std::fs::read_to_string(proj_dir.join("gotchas/linker.md")).unwrap())
            .unwrap()
            .frontmatter;
        assert!(
            !ai_memory_core::okf::is_conformant(&fm),
            "files were touched despite the failed backup"
        );
    }
}
