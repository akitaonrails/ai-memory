//! Filesystem [`ContentBackend`]: markdown files under the wiki root as
//! the content source of truth, write-through to the SQLite index with
//! file-level rollback when the index write fails.
//!
//! This is the historical (and default) persistence path, extracted
//! verbatim from `Wiki::write_page` / `Wiki::apply_batch` /
//! `Wiki::delete_page` so alternative backends (Outl) can slot in
//! behind the same trait. Callers hold the engine's mutation lock.

use std::path::{Path, PathBuf};

use ai_memory_core::{NewPage, PageId, PagePath, ProjectId, UserId, WorkspaceId};
use ai_memory_store::{
    ContentBackend, ContentError, ContentResult, MoveSummary, PageContent, WriterHandle,
};
use async_trait::async_trait;

use crate::markdown::parse;

/// Markdown-on-disk content backend (`<data_dir>/wiki/`).
#[derive(Clone)]
pub struct FsContentBackend {
    root: PathBuf,
    writer: WriterHandle,
}

impl FsContentBackend {
    /// Backend rooted at the wiki directory (already created by the
    /// engine), writing index rows through the given writer handle.
    pub fn new(root: PathBuf, writer: WriterHandle) -> Self {
        Self { root, writer }
    }

    fn project_root(&self, workspace_id: WorkspaceId, project_id: ProjectId) -> PathBuf {
        self.root
            .join(workspace_id.to_string())
            .join(project_id.to_string())
    }

    fn abs_path(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> PathBuf {
        self.project_root(workspace_id, project_id)
            .join(path.as_str())
    }
}

#[async_trait]
impl ContentBackend for FsContentBackend {
    async fn persist_page(&self, page: NewPage, rendered: String) -> ContentResult<PageId> {
        let abs = self.abs_path(page.workspace_id, page.project_id, &page.path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let installed = replace_file_with_rollback_snapshot(&abs, rendered.as_bytes())?;
        match self.writer.upsert_page(page).await {
            Ok(id) => Ok(id),
            Err(e) => {
                rollback_or_inconsistent(std::slice::from_ref(&installed), &e)?;
                Err(e.into())
            }
        }
    }

    async fn persist_pages_batch(
        &self,
        pages: Vec<(NewPage, String)>,
    ) -> ContentResult<Vec<PageId>> {
        // Stage every rendered document as a tempfile next to its target
        // first, so a failure before install leaves the wiki untouched.
        let mut staged: Vec<(NewPage, tempfile::NamedTempFile, PathBuf)> =
            Vec::with_capacity(pages.len());
        for (page, rendered) in pages {
            let abs = self.abs_path(page.workspace_id, page.project_id, &page.path);
            let parent = abs.parent().ok_or_else(|| {
                ContentError::Backend("page path has no parent (cannot stage tempfile)".into())
            })?;
            std::fs::create_dir_all(parent)?;
            let mut tmp = tempfile::Builder::new()
                .prefix(".ai-memory-tmp.")
                .tempfile_in(parent)?;
            use std::io::Write as _;
            tmp.write_all(rendered.as_bytes())?;
            tmp.as_file().sync_data()?;
            staged.push((page, tmp, abs));
        }

        // Install files first so the DB is never ahead of markdown. If the
        // SQL batch fails below, rollback restores the prior disk state;
        // if the process crashes in this window, startup/reindex repairs
        // the derived DB from the markdown source of truth.
        let mut installed = Vec::with_capacity(staged.len());
        let mut batch = Vec::with_capacity(staged.len());
        for (page, tmp, abs) in staged {
            let install = match persist_tmp_with_rollback_snapshot(tmp, &abs) {
                Ok(install) => install,
                Err(e) => {
                    let msg = e.to_string();
                    rollback_or_inconsistent(&installed, &msg)?;
                    return Err(e);
                }
            };
            installed.push(install);
            batch.push(page);
        }

        match self.writer.upsert_pages_batch(batch).await {
            Ok(ids) => Ok(ids),
            Err(e) => {
                rollback_or_inconsistent(&installed, &e)?;
                Err(e.into())
            }
        }
    }

    async fn delete_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        author_id: Option<UserId>,
    ) -> ContentResult<()> {
        let abs = self.abs_path(workspace_id, project_id, &path);
        let quarantined = match quarantine_file(&abs) {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(ContentError::Io(e)),
        };

        let delete_result = self
            .writer
            .delete_page(workspace_id, project_id, path.clone(), author_id)
            .await;
        if let Err(e) = delete_result {
            if let Some(quarantine) = &quarantined
                && let Err(restore_err) = std::fs::rename(quarantine, &abs)
            {
                tracing::error!(
                    path = %path.as_str(),
                    quarantine = %quarantine.display(),
                    error = %restore_err,
                    "delete_page: DB delete failed and restoring quarantined file also failed"
                );
            }
            return Err(e.into());
        }

        if let Some(quarantine) = quarantined {
            std::fs::remove_file(&quarantine)?;
        }
        Ok(())
    }

    async fn remove_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> ContentResult<()> {
        let root = self.project_root(workspace_id, project_id);
        match std::fs::remove_dir_all(&root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ContentError::Io(e)),
        }
    }

    async fn remove_workspace(&self, workspace_id: WorkspaceId) -> ContentResult<()> {
        let root = self.root.join(workspace_id.to_string());
        match std::fs::remove_dir_all(&root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ContentError::Io(e)),
        }
    }

    async fn move_project(
        &self,
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
    ) -> ContentResult<MoveSummary> {
        let src = self.project_root(from_workspace, project_id);
        let dst = self.project_root(to_workspace, project_id);

        if dst.exists() {
            return Err(ContentError::DestinationExists(dst.display().to_string()));
        }

        let renamed = if src.exists() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&src, &dst)?;
            true
        } else {
            // Nothing on disk to move (a project with zero written pages).
            false
        };

        match self
            .writer
            .move_project_workspace(project_id, from_workspace, to_workspace)
            .await
        {
            Ok(summary) => Ok(summary),
            Err(e) => {
                if renamed {
                    if let Some(parent) = src.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    if let Err(rollback_err) = std::fs::rename(&dst, &src) {
                        return Err(ContentError::Io(std::io::Error::other(format!(
                            "INCONSISTENT STATE: files moved but DB re-stamp failed ({e}) and dir rename-back also failed ({rollback_err}); manually move {} -> {} or finish the re-stamp",
                            dst.display(),
                            src.display()
                        ))));
                    }
                }
                Err(e.into())
            }
        }
    }

    async fn read_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> ContentResult<PageContent> {
        let abs = self.abs_path(workspace_id, project_id, path);
        let raw = std::fs::read_to_string(&abs)?;
        let md = parse(&raw).map_err(|e| ContentError::Parse(e.to_string()))?;
        Ok(PageContent {
            frontmatter: md.frontmatter,
            body: md.body,
        })
    }

    fn fs_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
}

/// A file installed on disk together with the bytes it replaced, so a
/// failed index write can roll the wiki back to its prior state.
#[derive(Debug)]
pub(crate) struct InstalledFile {
    path: PathBuf,
    previous: Option<Vec<u8>>,
}

fn snapshot_existing_file(path: &Path) -> ContentResult<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ContentError::Io(e)),
    }
}

fn sync_parent_best_effort(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

fn persist_tmp_with_rollback_snapshot(
    tmp: tempfile::NamedTempFile,
    path: &Path,
) -> ContentResult<InstalledFile> {
    let previous = snapshot_existing_file(path)?;
    let persisted = tmp.persist(path).map_err(|e| ContentError::Io(e.error))?;
    persisted.sync_data()?;
    sync_parent_best_effort(path);
    Ok(InstalledFile {
        path: path.to_path_buf(),
        previous,
    })
}

pub(crate) fn replace_file_with_rollback_snapshot(
    path: &Path,
    bytes: &[u8],
) -> ContentResult<InstalledFile> {
    let previous = snapshot_existing_file(path)?;
    crate::atomic::write_atomic(path, bytes)?;
    Ok(InstalledFile {
        path: path.to_path_buf(),
        previous,
    })
}

fn rollback_installed_files(installed: &[InstalledFile]) -> ContentResult<()> {
    for file in installed.iter().rev() {
        match &file.previous {
            Some(bytes) => {
                crate::atomic::write_atomic(&file.path, bytes)?;
            }
            None => match std::fs::remove_file(&file.path) {
                Ok(()) => sync_parent_best_effort(&file.path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ContentError::Io(e)),
            },
        }
    }
    Ok(())
}

pub(crate) fn rollback_or_inconsistent<E: std::fmt::Display>(
    installed: &[InstalledFile],
    cause: &E,
) -> ContentResult<()> {
    if let Err(rollback_err) = rollback_installed_files(installed) {
        return Err(ContentError::Io(std::io::Error::other(format!(
            "INCONSISTENT STATE: wiki files changed but store write failed ({cause}) and rollback failed ({rollback_err})"
        ))));
    }
    Ok(())
}

fn quarantine_file(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other(
            "page path has no parent (cannot quarantine delete)",
        ));
    };
    let tmp = tempfile::Builder::new()
        .prefix(".ai-memory-delete.")
        .tempfile_in(parent)?;
    let (_file, quarantine) = tmp.keep().map_err(|e| e.error)?;
    std::fs::remove_file(&quarantine)?;
    match std::fs::rename(path, &quarantine) {
        Ok(()) => Ok(Some(quarantine)),
        Err(e) => {
            let _ = std::fs::remove_file(&quarantine);
            Err(e)
        }
    }
}
