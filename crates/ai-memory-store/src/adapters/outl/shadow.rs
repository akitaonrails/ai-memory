//! Shadow mode: the filesystem backend stays the primary content SoT
//! (and owner of the index); every successful mutation is mirrored
//! into the Outl workspace best-effort. Mirror failures are logged and
//! never fail the caller — this is the migration-validation mode, not
//! a durability guarantee.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{ContentBackend, ContentResult, MoveSummary, PageContent};
use ai_memory_core::{NewPage, PageId, PagePath, ProjectId, UserId, WorkspaceId};
use async_trait::async_trait;

use super::{OutlContentBackend, slug};

/// Primary backend + best-effort Outl mirror.
pub struct ShadowContentBackend {
    primary: Arc<dyn ContentBackend>,
    mirror: Arc<OutlContentBackend>,
}

impl ShadowContentBackend {
    /// Wrap `primary` so every successful mutation is mirrored to Outl.
    pub fn new(primary: Arc<dyn ContentBackend>, mirror: Arc<OutlContentBackend>) -> Self {
        Self { primary, mirror }
    }
}

fn log_mirror_failure(op: &str, error: &dyn std::fmt::Display) {
    tracing::warn!(op, %error, "outl shadow mirror failed (primary write already durable)");
}

#[async_trait]
impl ContentBackend for ShadowContentBackend {
    async fn persist_page(&self, page: NewPage, rendered: String) -> ContentResult<PageId> {
        let mirror_copy = page.clone();
        let id = self.primary.persist_page(page, rendered).await?;
        if let Err(e) = self.mirror.mirror_page(&mirror_copy).await {
            log_mirror_failure("persist_page", &e);
        }
        Ok(id)
    }

    async fn persist_pages_batch(
        &self,
        pages: Vec<(NewPage, String)>,
    ) -> ContentResult<Vec<PageId>> {
        let mirror_copies: Vec<NewPage> = pages.iter().map(|(p, _)| p.clone()).collect();
        let ids = self.primary.persist_pages_batch(pages).await?;
        for page in &mirror_copies {
            if let Err(e) = self.mirror.mirror_page(page).await {
                log_mirror_failure("persist_pages_batch", &e);
            }
        }
        Ok(ids)
    }

    async fn delete_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        author_id: Option<UserId>,
    ) -> ContentResult<()> {
        self.primary
            .delete_page(workspace_id, project_id, path.clone(), author_id)
            .await?;
        if let Err(e) = self
            .mirror
            .mirror_delete(workspace_id, project_id, &path)
            .await
        {
            log_mirror_failure("delete_page", &e);
        }
        Ok(())
    }

    async fn remove_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> ContentResult<()> {
        self.primary
            .remove_project(workspace_id, project_id)
            .await?;
        let prefix =
            slug::project_prefix(self.mirror.slug_prefix.as_str(), workspace_id, project_id);
        if let Err(e) = self.mirror.handle().delete_by_prefix(prefix).await {
            log_mirror_failure("remove_project", &e);
        }
        Ok(())
    }

    async fn remove_workspace(&self, workspace_id: WorkspaceId) -> ContentResult<()> {
        self.primary.remove_workspace(workspace_id).await?;
        let prefix = slug::workspace_prefix(self.mirror.slug_prefix.as_str(), workspace_id);
        if let Err(e) = self.mirror.handle().delete_by_prefix(prefix).await {
            log_mirror_failure("remove_workspace", &e);
        }
        Ok(())
    }

    async fn move_project(
        &self,
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
    ) -> ContentResult<MoveSummary> {
        // Primary owns the index re-stamp; the mirror just drops the
        // old-workspace copies (next writes recreate them under the new
        // slug — cheaper and safer than duplicating the re-slug logic
        // best-effort).
        let summary = self
            .primary
            .move_project(project_id, from_workspace, to_workspace)
            .await?;
        let prefix =
            slug::project_prefix(self.mirror.slug_prefix.as_str(), from_workspace, project_id);
        if let Err(e) = self.mirror.handle().delete_by_prefix(prefix).await {
            log_mirror_failure("move_project", &e);
        }
        Ok(summary)
    }

    async fn read_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> ContentResult<PageContent> {
        self.primary.read_page(workspace_id, project_id, path).await
    }

    fn fs_root(&self) -> Option<PathBuf> {
        self.primary.fs_root()
    }
}
