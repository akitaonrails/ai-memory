//! Outl content backend for ai-memory.
//!
//! Implements [`crate::ContentBackend`] on top of an embedded
//! Outl workspace: page content is persisted as ops in the workspace's
//! append-only CRDT log (source of truth), and the SQLite index row is
//! written right after — search, recency, graph, embeddings, and decay
//! keep reading the index exactly as in sqlite mode.
//!
//! ## Consistency model
//!
//! Unlike the filesystem backend (which rolls the markdown file back
//! when the index write fails), the op log is append-only: a failed
//! index write leaves the SoT ahead of the index until the
//! [`reconcile`] task heals it. That task also absorbs edits the user
//! makes inside the Outl app: each ai-memory page carries an
//! `ai-memory-sha` property stamped with the sha256 of its projected
//! content, so "our own projection" and "externally edited" are
//! distinguishable without false positives.
//!
//! ## What intentionally does NOT go through Outl
//!
//! Sessions, observations, users, handoffs, embeddings, audit, and
//! schedulers are operational data and stay SQLite-only. Git
//! checkpoints/restore of pages are a wiki-backend feature with no Outl
//! equivalent yet (declared limitation).

use std::path::PathBuf;
use std::sync::Arc;

use crate::{
    ContentBackend, ContentError, ContentResult, MoveSummary, PageContent, ReaderPool, WriterHandle,
};
use ai_memory_core::{NewPage, PageId, PagePath, ProjectId, UserId, WorkspaceId};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

mod actor;
mod factory;
pub mod project;
pub mod reconcile;
mod shadow;
pub mod slug;

pub use actor::{OpenInfo, OutlHandle, PROJECTED_SHA_PROP};
pub use factory::{OutlAdapterFactory, OutlAdapterSettings, OutlMode};
pub use shadow::ShadowContentBackend;

/// Content backend persisting pages into an embedded Outl workspace.
pub struct OutlContentBackend {
    handle: OutlHandle,
    writer: WriterHandle,
    reader: ReaderPool,
    slug_prefix: String,
}

impl OutlContentBackend {
    /// Open the Outl workspace at `workspace_dir` (spawning the writer
    /// actor thread) and build the backend. Fails when the directory is
    /// not an initialised Outl workspace — run `outl init <dir>` first.
    pub fn open(
        workspace_dir: PathBuf,
        slug_prefix: impl Into<String>,
        writer: WriterHandle,
        reader: ReaderPool,
    ) -> Result<(Arc<Self>, OpenInfo), ContentError> {
        let (handle, info) = OutlHandle::open(workspace_dir).map_err(ContentError::Backend)?;
        Ok((
            Arc::new(Self {
                handle,
                writer,
                reader,
                slug_prefix: slug_prefix.into(),
            }),
            info,
        ))
    }

    /// Handle to the workspace actor (used by the reconciler and by
    /// migration tooling/tests).
    #[must_use]
    pub fn handle(&self) -> &OutlHandle {
        &self.handle
    }

    /// Mirror a page into the Outl workspace WITHOUT touching the
    /// SQLite index. Used by shadow mode, where the fs backend stays
    /// primary and owns the index.
    pub async fn mirror_page(&self, page: &NewPage) -> Result<(), ContentError> {
        self.write_to_outl(page).await
    }

    /// Trash the Outl copy of a page WITHOUT touching the index
    /// (shadow-mode delete mirror).
    pub async fn mirror_delete(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> Result<(), ContentError> {
        let page_slug = slug::encode(&self.slug_prefix, workspace_id, project_id, path);
        self.handle
            .delete_page(page_slug)
            .await
            .map_err(ContentError::Backend)?;
        Ok(())
    }

    /// Best-effort: drop the sha stamps so the reconciler re-indexes
    /// these pages instead of trusting stale index rows. Failures are
    /// logged — the SoT copy is durable either way and a later engine
    /// write restamps.
    async fn mark_dirty(&self, slugs: &[String]) {
        for page_slug in slugs {
            if let Err(e) = self.handle.clear_stamp(page_slug.clone()).await {
                tracing::warn!(slug = %page_slug, error = %e, "outl: failed to mark page dirty after index write failure");
            }
        }
    }

    async fn write_to_outl(&self, page: &NewPage) -> Result<(), ContentError> {
        let slug = slug::encode(
            &self.slug_prefix,
            page.workspace_id,
            page.project_id,
            &page.path,
        );
        let specs = project::body_to_specs(&page.body);
        let projected_sha = projected_sha(&specs);
        self.handle
            .upsert_page(
                slug,
                page.title.clone(),
                page.tier.as_str().to_string(),
                specs,
                projected_sha,
            )
            .await
            .map_err(ContentError::Backend)
    }
}

/// sha256 (hex) of the body the projection will read back — the marker
/// that lets the reconciler skip pages only we have touched.
#[must_use]
pub fn projected_sha(specs: &[outl_actions::BlockTreeSpec]) -> String {
    let texts: Vec<String> = specs.iter().map(|s| s.text.clone()).collect();
    sha256_hex(&project::texts_to_body(&texts))
}

/// sha256 hex digest of a string.
#[must_use]
pub fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[async_trait]
impl ContentBackend for OutlContentBackend {
    async fn persist_page(&self, page: NewPage, _rendered: String) -> ContentResult<PageId> {
        // SoT first (append-only op log), index second. A failure
        // between the two leaves the index behind; dropping the sha
        // stamp marks the page diverged so the reconciler re-indexes it
        // on its next pass (see crate docs — this is the documented
        // contract change vs the fs backend's rollback).
        self.write_to_outl(&page).await?;
        let page_slug = slug::encode(
            &self.slug_prefix,
            page.workspace_id,
            page.project_id,
            &page.path,
        );
        match self.writer.upsert_page(page).await {
            Ok(id) => Ok(id),
            Err(e) => {
                self.mark_dirty(std::slice::from_ref(&page_slug)).await;
                Err(e.into())
            }
        }
    }

    async fn persist_pages_batch(
        &self,
        pages: Vec<(NewPage, String)>,
    ) -> ContentResult<Vec<PageId>> {
        let mut batch = Vec::with_capacity(pages.len());
        let mut slugs = Vec::with_capacity(pages.len());
        for (page, _rendered) in pages {
            self.write_to_outl(&page).await?;
            slugs.push(slug::encode(
                &self.slug_prefix,
                page.workspace_id,
                page.project_id,
                &page.path,
            ));
            batch.push(page);
        }
        match self.writer.upsert_pages_batch(batch).await {
            Ok(ids) => Ok(ids),
            Err(e) => {
                self.mark_dirty(&slugs).await;
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
        let slug = slug::encode(&self.slug_prefix, workspace_id, project_id, &path);
        self.handle
            .delete_page(slug)
            .await
            .map_err(ContentError::Backend)?;
        self.writer
            .delete_page(workspace_id, project_id, path, author_id)
            .await?;
        Ok(())
    }

    async fn remove_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> ContentResult<()> {
        let prefix = slug::project_prefix(&self.slug_prefix, workspace_id, project_id);
        let removed = self
            .handle
            .delete_by_prefix(prefix)
            .await
            .map_err(ContentError::Backend)?;
        tracing::debug!(removed, %project_id, "outl: project pages trashed");
        Ok(())
    }

    async fn remove_workspace(&self, workspace_id: WorkspaceId) -> ContentResult<()> {
        let prefix = slug::workspace_prefix(&self.slug_prefix, workspace_id);
        let removed = self
            .handle
            .delete_by_prefix(prefix)
            .await
            .map_err(ContentError::Backend)?;
        tracing::debug!(removed, %workspace_id, "outl: workspace pages trashed");
        Ok(())
    }

    async fn move_project(
        &self,
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
    ) -> ContentResult<MoveSummary> {
        // Slugs embed the workspace id, so a true move re-slugs every
        // page of the project in the SoT: read → recreate → trash old.
        let old_prefix = slug::project_prefix(&self.slug_prefix, from_workspace, project_id);
        let slugs = self
            .handle
            .list_owned(old_prefix)
            .await
            .map_err(ContentError::Backend)?;
        for old_slug in slugs {
            let Some((_, _, path)) = slug::decode(&self.slug_prefix, &old_slug) else {
                continue;
            };
            let Some(content) = self
                .handle
                .read_owned(old_slug.clone())
                .await
                .map_err(ContentError::Backend)?
            else {
                continue;
            };
            let new_slug = slug::encode(&self.slug_prefix, to_workspace, project_id, &path);
            let specs = project::body_to_specs(&content.body);
            let sha = projected_sha(&specs);
            // Carry the tier prop across the re-slug; a page we wrote
            // always has one, but fall back to the default tier rather
            // than stamping an empty string.
            let tier = content.tier.unwrap_or_else(|| "semantic".into());
            self.handle
                .upsert_page(new_slug, content.title, tier, specs, sha)
                .await
                .map_err(ContentError::Backend)?;
            self.handle
                .delete_page(old_slug)
                .await
                .map_err(ContentError::Backend)?;
        }
        Ok(self
            .writer
            .move_project_workspace(project_id, from_workspace, to_workspace)
            .await?)
    }

    async fn read_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> ContentResult<PageContent> {
        // Reads serve the index (Design A): the reconciler keeps it in
        // sync with the SoT, and the index preserves the ORIGINAL
        // document body (the Outl side stores a projection).
        let stored = self
            .reader
            .page_body_by_ids(workspace_id, project_id, path.as_str())
            .await?
            .ok_or_else(|| {
                ContentError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("page not found: {}", path.as_str()),
                ))
            })?;
        let frontmatter =
            serde_json::from_str(&stored.frontmatter_json).unwrap_or(serde_json::Value::Null);
        Ok(PageContent {
            frontmatter,
            body: stored.body,
        })
    }

    fn fs_root(&self) -> Option<PathBuf> {
        None
    }
}
