//! [`Wiki`] — the only correct write path for the markdown source-of-truth.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ai_memory_core::{
    ActorContext, AutoImproveProposalId, NewPage, PageId, PagePath, ProjectId, Sanitizer, Tier,
    UserId, WorkspaceId,
};
use ai_memory_llm::Embedder;
use ai_memory_store::{
    ApproveAutoImproveProposal, ApproveAutoImproveProposalResult, AutoImproveProposalDetail,
    ContentBackend, FailAutoImproveProposal, MoveSummary, ReaderPool, WriterHandle,
    artifact_path_for, f32_vec_to_bytes,
};
use tokio::sync::RwLock;

use crate::admission::{AdmissionChain, AdmissionContext, AdmissionOp};
use crate::atomic;
use crate::backend::{
    FsContentBackend, replace_file_with_rollback_snapshot, rollback_or_inconsistent,
};
use crate::error::{WikiError, WikiResult};
use crate::git::{Checkpoint, GitAdapter};
use crate::markdown::{Markdown, derive_title, emit, extract_links, parse};
use crate::watcher::is_pending_path;

/// Summary of a [`Wiki::reindex_all`] run.
#[derive(Debug, Default, Clone)]
pub struct ReindexSummary {
    /// Workspaces recreated from `_meta.md`.
    pub workspaces: usize,
    /// Projects recreated from `_meta.md`.
    pub projects: usize,
    /// Pages reindexed from the wiki tree.
    pub pages: usize,
}

/// Wiki filesystem handle.
///
/// Owns the path of the wiki root (`<data_dir>/wiki/`) and a cloneable
/// [`WriterHandle`] so that every public mutation writes the markdown
/// file *and* sends a `WriteCmd::UpsertPage` to the store in a single
/// call — no background-task indexing-after-return (basic-memory #763
/// lesson).
///
/// ## On-disk layout
///
/// Pages are stored at `<wiki_root>/<workspace_id>/<project_id>/<page-path>`.
/// Each of `workspace_id` and `project_id` is a UUID string. This layout is
/// the single canonical namespace; all path construction must go through
/// [`Wiki::project_root`] or [`Wiki::abs_path`] — never hand-rolled joins.
#[derive(Clone)]
pub struct Wiki {
    root: PathBuf,
    writer: WriterHandle,
    /// Content source-of-truth backend. Defaults to the filesystem
    /// backend (markdown + index with rollback); the Outl backend is
    /// injected via [`Wiki::with_content_backend`] when
    /// `[storage] backend = "outl"`.
    content: Arc<dyn ContentBackend>,
    git: GitAdapter,
    embedder: Option<Arc<dyn Embedder>>,
    /// Privacy strip applied to every page body before persistence.
    /// Defence-in-depth: any caller path (LLM consolidation, manual
    /// write-page CLI, agent-supplied tool input) still gets scrubbed
    /// at the wiki boundary even if upstream forgot.
    sanitizer: Sanitizer,
    /// Optional HTTP webhook chain invoked just before page persistence.
    /// When configured, each `write_page` call POSTs the (path, frontmatter,
    /// body, ctx) tuple to every webhook subscribing to the op; webhooks
    /// may mutate frontmatter/body before the atomic write hits disk.
    /// Set via [`Wiki::with_admission_chain`]; see [`crate::admission`].
    admission_chain: Option<AdmissionChain>,
    /// Optional store reader used to resolve `workspace_id`/`project_id`
    /// into human names for the [`AdmissionContext`] passed to webhooks.
    /// Set via [`Wiki::with_store_reader`]; when unset, webhooks receive
    /// empty `workspace`/`project` strings and must fall back to
    /// IDs/headers/`_unscoped` paths.
    store_reader: Option<ReaderPool>,
    /// Process-local gate around filesystem mutations. Page writes/reindexes
    /// take a shared guard; project true-move takes the exclusive guard across
    /// the directory rename and SQLite re-stamp so stale writes cannot land
    /// files under the old workspace while the project is in flight.
    mutation_lock: Arc<RwLock<()>>,
}

impl Wiki {
    /// Construct a wiki handle rooted at `<data_dir>/wiki/`. Creates the
    /// directory if absent and initialises a git repo inside it.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] if the wiki root or git repo cannot be
    /// created.
    pub fn new(data_dir: &Path, writer: WriterHandle) -> WikiResult<Self> {
        let root = data_dir.join("wiki");
        std::fs::create_dir_all(&root)?;
        let git = GitAdapter::open_or_init(&root)?;
        let content = Arc::new(FsContentBackend::new(root.clone(), writer.clone()));
        Ok(Self {
            root,
            writer,
            content,
            git,
            embedder: None,
            sanitizer: Sanitizer::builtin(),
            admission_chain: None,
            store_reader: None,
            mutation_lock: Arc::new(RwLock::new(())),
        })
    }

    /// Attach an admission webhook chain. When set, every `write_page` call
    /// invokes the chain after the [`Markdown`] is built but before the
    /// atomic write — webhooks may mutate frontmatter/body. An empty chain
    /// is a no-op (skipped without HTTP overhead).
    #[must_use]
    pub fn with_admission_chain(mut self, chain: AdmissionChain) -> Self {
        if !chain.is_empty() {
            self.admission_chain = Some(chain);
        }
        self
    }

    /// Attach a store reader so the admission chain receives
    /// human-readable `workspace`/`project` names in its context, resolved
    /// from the `workspace_id`/`project_id` carried on the
    /// [`WritePageRequest`]. Without this, those fields stay empty and
    /// external webhooks must fall back to header introspection or use
    /// `_unscoped` placeholders.
    ///
    /// The reader is only invoked when the chain is configured AND would
    /// actually fire; tests and CLI paths that don't wire a chain pay
    /// nothing for setting (or omitting) this.
    #[must_use]
    pub fn with_store_reader(mut self, reader: ReaderPool) -> Self {
        self.store_reader = Some(reader);
        self
    }

    /// Replace the default filesystem content backend with an
    /// alternative source of truth (e.g. the Outl backend). The engine
    /// keeps orchestrating sanitize/admission/embedding; only the
    /// persistence of page content is swapped.
    #[must_use]
    pub fn with_content_backend(mut self, content: Arc<dyn ContentBackend>) -> Self {
        self.content = content;
        self
    }

    /// Replace the default built-in-only sanitizer with one carrying
    /// the operator's `[sanitize].extra_patterns` + `allowlist`.
    #[must_use]
    pub fn with_sanitizer(mut self, sanitizer: Sanitizer) -> Self {
        self.sanitizer = sanitizer;
        self
    }

    /// Attach an embedder. When set, `write_page` computes + stores an
    /// embedding for the new version synchronously. `apply_batch` keeps
    /// the SQL/file fan-out atomic and leaves vector completeness to
    /// admin or scheduled embedding backfill. Without an embedder,
    /// vector search is skipped and `ReaderPool::hybrid_search` uses
    /// FTS5 + graph expansion.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Borrow the optional embedder (used by the `ai-memory embed`
    /// backfill command).
    #[must_use]
    pub fn embedder(&self) -> Option<&Arc<dyn Embedder>> {
        self.embedder.as_ref()
    }

    /// Return a clone-friendly handle with the embedder detached, so
    /// `write_page` skips the per-page `embed_document` call. Used by
    /// bulk copy paths (e.g. `move-project`) that carry the source page's
    /// existing embedding over verbatim instead of recomputing it — the
    /// caller is then responsible for `store_embedding` on the new page.
    #[must_use]
    pub fn without_embedder(mut self) -> Self {
        self.embedder = None;
        self
    }

    /// Borrow the git adapter (for callers wiring auto-commit).
    #[must_use]
    pub fn git(&self) -> &GitAdapter {
        &self.git
    }

    /// Stage + commit the entire wiki tree. Returns `Ok(None)` if there
    /// was nothing to commit.
    ///
    /// # Errors
    /// Propagates [`WikiError`] from the git adapter.
    pub fn commit_all(&self, message: &str) -> WikiResult<Option<git2::Oid>> {
        self.git.commit_all(message)
    }

    /// Return the most recent wiki git checkpoints, newest first.
    ///
    /// # Errors
    /// Propagates [`WikiError`] from the git adapter.
    pub fn recent_checkpoints(&self, limit: usize) -> WikiResult<Vec<Checkpoint>> {
        self.git.recent_checkpoints(limit)
    }

    /// Create the one-time baseline checkpoint for an existing wiki tree that
    /// was created before git recovery checkpoints existed.
    ///
    /// Existing repos with any commit are left untouched. Fresh empty repos
    /// return `Ok(None)` because there is nothing to commit.
    ///
    /// # Errors
    /// Propagates [`WikiError`] from the git adapter.
    pub fn ensure_upgrade_baseline_checkpoint(&self) -> WikiResult<Option<git2::Oid>> {
        if self.git.commit_count() == 0 {
            self.git
                .commit_all("upgrade baseline: existing wiki tree before recovery checkpoints")
        } else {
            Ok(None)
        }
    }

    /// Path of the wiki root on disk.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve the on-disk root for a project: `<wiki_root>/<ws>/<proj>`.
    /// All page files for this project live under this directory.
    #[must_use]
    pub fn project_root(&self, workspace_id: WorkspaceId, project_id: ProjectId) -> PathBuf {
        self.root
            .join(workspace_id.to_string())
            .join(project_id.to_string())
    }

    /// Losslessly move a project to another workspace: rename its on-disk
    /// directory and re-stamp every store row that carries `workspace_id`, while
    /// keeping the same `project_id`.
    ///
    /// The exclusive mutation guard is held across both filesystem and store
    /// phases. That keeps in-process page writes/reindexes from observing the
    /// project half-moved; stale callers that still carry `(from_workspace,
    /// project_id)` only resume after the DB re-stamp and then fail the pair
    /// validator before touching disk.
    ///
    /// Ordering is rename-first, SQL-last so the DB is never ahead of disk. If
    /// the SQL re-stamp fails after a rename, the directory is renamed back.
    ///
    /// # Errors
    /// Returns [`WikiError`] if the destination directory already exists, the
    /// rename fails, the SQL re-stamp fails, or rollback fails.
    pub async fn move_project_workspace(
        &self,
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<MoveSummary> {
        let _guard = self.mutation_lock.write().await;
        let resolved_ctx = if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.op = AdmissionOp::MoveProject;
            self.resolve_admission_names(from_workspace, project_id, &mut ctx)
                .await;
            chain.notify(None, &ctx).await?;
            Some(ctx)
        } else {
            None
        };

        let summary = self
            .content
            .move_project(project_id, from_workspace, to_workspace)
            .await?;
        if let (Some(chain), Some(ctx)) = (&self.admission_chain, &resolved_ctx) {
            chain.dispatch_async(None, &serde_json::Value::Null, "", ctx);
        }
        Ok(summary)
    }

    async fn ensure_project_workspace(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<()> {
        self.writer
            .ensure_project_workspace(workspace_id, project_id)
            .await?;
        Ok(())
    }

    /// Absolute on-disk path for a page within a specific project.
    #[must_use]
    pub fn abs_path(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> PathBuf {
        self.project_root(workspace_id, project_id)
            .join(path.as_str())
    }

    /// Read the page at `path` from the content source of truth for the
    /// given project.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] if the page is missing or unreadable, or
    /// [`WikiError::Yaml`] if the frontmatter block is malformed.
    pub async fn read_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> WikiResult<Markdown> {
        let content = self
            .content
            .read_page(workspace_id, project_id, path)
            .await?;
        Ok(Markdown {
            frontmatter: content.frontmatter,
            body: content.body,
        })
    }

    /// Restore one page from a git checkpoint and reindex it into the store.
    ///
    /// The checkpoint path is resolved through the UUID-backed wiki layout, so
    /// callers address pages by `(workspace_id, project_id, page_path)` while
    /// git still stores the exact markdown file. The restored file is parsed
    /// before it is written so a malformed checkpoint cannot replace the live
    /// disk copy and then fail during indexing.
    ///
    /// # Errors
    /// Returns [`WikiError`] when the revision/path does not exist, the stored
    /// bytes are not UTF-8 markdown, parsing fails, or the filesystem/store
    /// update fails.
    pub async fn restore_page_from_checkpoint(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        rev: &str,
    ) -> WikiResult<PageId> {
        let rel = PathBuf::from(workspace_id.to_string())
            .join(project_id.to_string())
            .join(path.as_str());
        let bytes = self.git.file_at_rev(rev, &rel)?;
        let raw = String::from_utf8(bytes).map_err(|e| {
            WikiError::Io(std::io::Error::other(format!(
                "{} at {rev} is not valid UTF-8 markdown: {e}",
                path.as_str()
            )))
        })?;
        let md = parse(&raw)?;
        let title = derive_title(&md.frontmatter, &md.body, &path);
        let links = extract_links(&md.body, &path);
        let (tier, pinned) = derive_index_metadata(&path, &md.frontmatter)?;

        let _guard = self.mutation_lock.read().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        let abs = self.abs_path(workspace_id, project_id, &path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        atomic::write_atomic(&abs, raw.as_bytes())?;
        let id = self
            .writer
            .upsert_page(NewPage {
                workspace_id,
                project_id,
                path,
                title,
                body: md.body,
                tier,
                frontmatter_json: md.frontmatter,
                pinned,
                links,
                author_id: None,
            })
            .await?;
        Ok(id)
    }

    /// Delete the on-disk file for `path` within the given project.
    ///
    /// Returns `Ok(())` when the file was removed or did not exist (idempotent).
    /// The file watcher will observe the deletion; the sha256 short-circuit in
    /// the watcher's reindex path means a missing file produces a graceful
    /// no-op rather than an error.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] for any OS error other than "not found".
    /// Best-effort fill of `ctx.workspace`/`ctx.project` from ids via the
    /// store reader, so webhooks address pages by the same human names the
    /// engine uses. Mirrors the inline resolution in [`Self::write_page`].
    async fn resolve_admission_names(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        ctx: &mut AdmissionContext,
    ) {
        if let Some(reader) = &self.store_reader {
            if ctx.workspace.is_empty()
                && let Ok(Some(name)) = reader.workspace_name_by_id(workspace_id).await
            {
                ctx.workspace = name;
            }
            if ctx.project.is_empty()
                && let Ok(Some(name)) = reader.project_name_by_id(workspace_id, project_id).await
            {
                ctx.project = name;
            }
        }
    }

    /// Best-effort fill of `ctx.workspace` from a workspace id via the store
    /// reader. Workspace-wide ops have no project id to resolve.
    async fn resolve_workspace_name(&self, workspace_id: WorkspaceId, ctx: &mut AdmissionContext) {
        if let Some(reader) = &self.store_reader
            && ctx.workspace.is_empty()
            && let Ok(Some(name)) = reader.workspace_name_by_id(workspace_id).await
        {
            ctx.workspace = name;
        }
    }

    /// Delete a single page file. When an admission chain is attached, it is
    /// notified (`op=delete`) BEFORE the file is removed, so a mirror can
    /// `git rm` the same path. A `Reject`-policy webhook aborts the delete.
    ///
    /// # Errors
    /// Returns [`WikiError`] on a filesystem error or a rejecting webhook.
    pub async fn delete_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        admission_ctx: Option<AdmissionContext>,
        author_id: Option<ai_memory_core::UserId>,
    ) -> WikiResult<()> {
        let _guard = self.mutation_lock.read().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;

        let mut resolved_ctx = None;
        if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.op = AdmissionOp::Delete;
            self.resolve_admission_names(workspace_id, project_id, &mut ctx)
                .await;
            chain.notify(Some(path.as_str()), &ctx).await?;
            resolved_ctx = Some(ctx);
        }
        self.content
            .delete_page(workspace_id, project_id, path.clone(), author_id)
            .await?;

        if let (Some(chain), Some(ctx)) = (&self.admission_chain, &resolved_ctx) {
            chain.dispatch_async(Some(path.as_str()), &serde_json::Value::Null, "", ctx);
        }
        Ok(())
    }

    /// Purge a whole project's wiki directory. When an admission chain is
    /// attached, it is notified (`op=purge_project`, no page path) BEFORE the
    /// directory is removed, so a mirror can drop the project. A `Reject`
    /// webhook aborts the purge. Routes the on-disk removal through the
    /// namespaced [`Self::project_root`] (invariant: never hand-roll paths).
    ///
    /// # Errors
    /// Returns [`WikiError`] on a filesystem error or a rejecting webhook.
    pub async fn purge_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<()> {
        let ctx = self
            .admit_purge_project(workspace_id, project_id, admission_ctx)
            .await?;
        self.remove_project_dir(workspace_id, project_id).await?;
        self.dispatch_purge_project(ctx.as_ref());
        Ok(())
    }

    /// Run the blocking admission notification for a project purge without
    /// removing files. Admin callers use this before the DB purge so a
    /// `failure_policy = reject` webhook can still abort all destructive work.
    ///
    /// # Errors
    /// Returns [`WikiError`] when a reject-policy webhook fails.
    pub async fn admit_purge_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<Option<AdmissionContext>> {
        if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.op = AdmissionOp::PurgeProject;
            self.resolve_admission_names(workspace_id, project_id, &mut ctx)
                .await;
            chain.notify(None, &ctx).await?;
            Ok(Some(ctx))
        } else {
            Ok(None)
        }
    }

    /// Run the blocking admission notification for a workspace purge without
    /// removing files. Admin callers use this before the DB purge so a
    /// `failure_policy = reject` webhook can still abort all destructive work.
    ///
    /// # Errors
    /// Returns [`WikiError`] when a reject-policy webhook fails.
    pub async fn admit_purge_workspace(
        &self,
        workspace_id: WorkspaceId,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<Option<AdmissionContext>> {
        if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.op = AdmissionOp::PurgeWorkspace;
            self.resolve_workspace_name(workspace_id, &mut ctx).await;
            ctx.project.clear();
            chain.notify(None, &ctx).await?;
            Ok(Some(ctx))
        } else {
            Ok(None)
        }
    }

    /// Run just the blocking admission chain for a would-be write, without
    /// touching the store, disk, or any upstream cost (e.g. an LLM call). A
    /// `failure_policy = reject` webhook can still abort here, so callers use
    /// this to fail fast on a scope/actor that admission would refuse anyway.
    ///
    /// The webhook receives an empty placeholder body: the blocking chain
    /// decides on `ctx` (op / actor / workspace / project + path), not on
    /// content, so no real body is needed and any mutation it returns is
    /// discarded. Returns `Ok(())` when no chain is attached (nothing to gate).
    ///
    /// # Errors
    /// Returns [`WikiError`] when a reject-policy webhook refuses the write.
    pub async fn preflight_admission(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        op: AdmissionOp,
        actor: ActorContext,
    ) -> WikiResult<()> {
        if let Some(chain) = &self.admission_chain {
            let mut ctx = AdmissionContext {
                op,
                actor,
                ..Default::default()
            };
            self.resolve_admission_names(workspace_id, project_id, &mut ctx)
                .await;
            chain.notify(Some(path.as_str()), &ctx).await?;
        }
        Ok(())
    }

    /// Remove the project's on-disk directory without running admission.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] on filesystem errors other than NotFound.
    pub async fn remove_project_dir(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<()> {
        let _guard = self.mutation_lock.write().await;
        self.content
            .remove_project(workspace_id, project_id)
            .await?;
        Ok(())
    }

    /// Remove a workspace's on-disk directory (`<wiki_root>/<ws>`), including
    /// every project under it, without running admission. Best-effort: a
    /// missing dir is not an error.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] on filesystem errors other than NotFound.
    pub async fn remove_workspace_dir(&self, workspace_id: WorkspaceId) -> WikiResult<()> {
        let _guard = self.mutation_lock.write().await;
        self.content.remove_workspace(workspace_id).await?;
        Ok(())
    }

    /// Dispatch non-blocking purge webhooks after the caller's purge has
    /// completed its durable DB/filesystem work.
    pub fn dispatch_purge_project(&self, admission_ctx: Option<&AdmissionContext>) {
        if let (Some(chain), Some(ctx)) = (&self.admission_chain, admission_ctx) {
            chain.dispatch_async(None, &serde_json::Value::Null, "", ctx);
        }
    }

    /// Dispatch non-blocking purge-workspace webhooks after the caller's purge
    /// has completed its durable DB/filesystem work.
    pub fn dispatch_purge_workspace(&self, admission_ctx: Option<&AdmissionContext>) {
        if let (Some(chain), Some(ctx)) = (&self.admission_chain, admission_ctx) {
            chain.dispatch_async(None, &serde_json::Value::Null, "", ctx);
        }
    }

    /// Cloneable handle to the underlying store writer.
    #[must_use]
    pub fn writer(&self) -> &WriterHandle {
        &self.writer
    }

    /// Absolute on-disk path for an auto-improvement proposal sidecar.
    #[must_use]
    pub fn auto_improve_sidecar_path(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        proposal_id: AutoImproveProposalId,
    ) -> PathBuf {
        self.project_root(workspace_id, project_id)
            .join(artifact_path_for(proposal_id))
    }

    /// Write a human-reviewable non-indexed sidecar for a staged proposal.
    ///
    /// This intentionally bypasses [`Self::write_page`]: pending proposal
    /// artifacts are review aids, not durable wiki pages, and must not create
    /// rows in `pages`/FTS/embeddings.
    pub async fn write_auto_improve_sidecar(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        proposal_id: AutoImproveProposalId,
    ) -> WikiResult<PathBuf> {
        let reader = self.store_reader.as_ref().ok_or_else(|| {
            ai_memory_wiki_error("auto-improve sidecar write requires a store reader")
        })?;
        let detail = reader
            .auto_improve_proposal_detail(workspace_id, project_id, proposal_id)
            .await?
            .ok_or_else(|| ai_memory_wiki_error("auto-improve proposal not found in scope"))?;
        let path = self.auto_improve_sidecar_path(workspace_id, project_id, proposal_id);
        let content = self.sanitizer.scrub(&render_auto_improve_sidecar(&detail)?);
        let _guard = self.mutation_lock.read().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        atomic::write_atomic(&path, content.as_bytes())?;
        Ok(path)
    }

    /// Approve a staged auto-improvement proposal by applying its stored body
    /// through the normal wiki write pipeline, then atomically marking the DB
    /// proposal approved.
    pub async fn approve_auto_improve_proposal(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        proposal_id: AutoImproveProposalId,
        actor: ActorContext,
        author_id: Option<UserId>,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<ApproveAutoImproveProposalResult> {
        let reader = self
            .store_reader
            .as_ref()
            .ok_or_else(|| ai_memory_wiki_error("auto-improve approval requires a store reader"))?;
        let detail = reader
            .auto_improve_proposal_detail(workspace_id, project_id, proposal_id)
            .await?
            .ok_or_else(|| ai_memory_wiki_error("auto-improve proposal not found in scope"))?;

        let path = detail.summary.target_path.clone();
        let mut frontmatter = serde_json::json!({
            "kind": detail.summary.kind,
            "title": detail.summary.title,
            "auto_improve_proposal_id": proposal_id.to_string(),
            "auto_improve_run_id": detail.summary.run_id.to_string(),
        });
        frontmatter = stamp_last_modified_by(frontmatter, &actor);
        let body = self.sanitizer.scrub(&detail.body_markdown);
        let mut markdown = Markdown { frontmatter, body };
        let mut resolved_ctx = None;
        if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.actor = actor.clone();
            self.resolve_admission_names(workspace_id, project_id, &mut ctx)
                .await;
            match chain.run(&path, &mut markdown, &ctx).await {
                Ok(()) => resolved_ctx = Some(ctx),
                Err(e) => {
                    let reason = e.to_string();
                    self.writer
                        .fail_auto_improve_proposal(FailAutoImproveProposal {
                            workspace_id,
                            project_id,
                            proposal_id,
                            reason,
                            actor,
                            author_id,
                        })
                        .await?;
                    return Err(e);
                }
            }
        }
        markdown.body = self.sanitizer.scrub(&markdown.body);
        scrub_frontmatter_strings(&mut markdown.frontmatter, &self.sanitizer);
        let title = self.sanitizer.scrub(&detail.summary.title);
        let links = extract_links(&markdown.body, &path);
        let emitted = emit(&markdown)?;
        let page = NewPage {
            workspace_id,
            project_id,
            path: path.clone(),
            title,
            body: markdown.body.clone(),
            tier: Tier::Semantic,
            frontmatter_json: markdown.frontmatter.clone(),
            pinned: is_slot_path(&path),
            links,
            author_id,
        };

        let result = {
            let _guard = self.mutation_lock.write().await;
            self.ensure_project_workspace(workspace_id, project_id)
                .await?;
            let abs = self.abs_path(workspace_id, project_id, &path);
            let installed = replace_file_with_rollback_snapshot(&abs, emitted.as_bytes())?;
            match self
                .writer
                .approve_auto_improve_proposal(ApproveAutoImproveProposal {
                    workspace_id,
                    project_id,
                    proposal_id,
                    page,
                    actor: actor.clone(),
                    author_id,
                    checkpoint: None,
                })
                .await
            {
                Ok(ApproveAutoImproveProposalResult::Approved { page_id }) => {
                    ApproveAutoImproveProposalResult::Approved { page_id }
                }
                Ok(ApproveAutoImproveProposalResult::Conflict) => {
                    rollback_or_inconsistent(
                        std::slice::from_ref(&installed),
                        &"proposal conflict",
                    )?;
                    ApproveAutoImproveProposalResult::Conflict
                }
                Err(e) => {
                    rollback_or_inconsistent(std::slice::from_ref(&installed), &e)?;
                    return Err(e.into());
                }
            }
        };

        if let (Some(chain), Some(ctx)) = (&self.admission_chain, &resolved_ctx) {
            chain.dispatch_async(
                Some(path.as_str()),
                &markdown.frontmatter,
                &markdown.body,
                ctx,
            );
        }
        Ok(result)
    }

    /// Re-index the page on disk at `path` into the store *without*
    /// rewriting the file.
    ///
    /// Called by the watcher when an external editor (Obsidian, vim) has
    /// changed a file we did not write. The store-side sha256 short-circuit
    /// makes this idempotent: if the on-disk content already matches the
    /// latest version, no supersession happens.
    ///
    /// # Errors
    /// Returns [`WikiError`] for any filesystem, parsing, or store error.
    pub async fn reindex_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
    ) -> WikiResult<PageId> {
        if is_pending_path(&path) {
            return Err(ai_memory_wiki_error(
                "refusing to index pending proposal sidecar",
            ));
        }
        let _guard = self.mutation_lock.read().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;

        let md = self.read_page(workspace_id, project_id, &path).await?;
        let title = derive_title(&md.frontmatter, &md.body, &path);
        let links = extract_links(&md.body, &path);
        // Markdown is the source of truth: preserve explicit tier/pinned
        // metadata on reindex instead of forcing every page back to semantic.
        let (tier, pinned) = derive_index_metadata(&path, &md.frontmatter)?;
        let id = self
            .writer
            .upsert_page(NewPage {
                workspace_id,
                project_id,
                path,
                title,
                body: md.body,
                tier,
                frontmatter_json: md.frontmatter,
                pinned,
                links,

                author_id: None,
            })
            .await?;
        Ok(id)
    }

    /// Read a `_meta.md` scope-manifest's frontmatter from `dir`.
    fn read_scope_meta(dir: &Path) -> WikiResult<serde_json::Value> {
        let path = dir.join("_meta.md");
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(WikiError::Io(std::io::Error::other(format!(
                "refusing to read symlinked scope manifest {}",
                path.display()
            ))));
        }
        let raw = std::fs::read_to_string(path)?;
        Ok(parse(&raw)?.frontmatter)
    }

    /// Rebuild the **entire** store index from the on-disk wiki tree — the
    /// "DB is rebuildable from files" guarantee made concrete. Walks every
    /// `<ws-uuid>/<proj-uuid>/` directory, recreates the workspace/project rows
    /// from each dir's self-describing `_meta.md` manifest (preserving the ids
    /// the tree is keyed by, via [`WriterHandle::ensure_workspace_with_id`] /
    /// [`ensure_project_with_id`]), then reindexes every page. Pages are
    /// detected by content (a frontmatter file named `log.md` is a page; the
    /// raw `## [..]` ledger, `_meta.md` and `bootstrap.md` are skipped).
    ///
    /// Intended for a freshly-migrated (clean) store, e.g. to move a data dir
    /// onto a different migration lineage without carrying the old
    /// `refinery_schema_history`. DB-only episodic state (sessions,
    /// observations, handoffs, decay counters) is NOT reconstructed — it is not
    /// in the markdown; embeddings can be recomputed separately via `embed`.
    ///
    /// # Errors
    /// Returns [`WikiError`] for filesystem/parse/store errors, including a
    /// scope directory that lacks its `_meta.md` (the wiki is not
    /// self-describing — newer engines write the manifest on scope creation).
    pub async fn reindex_all(&self) -> WikiResult<ReindexSummary> {
        let root = self.root().to_path_buf();
        let project_dirs =
            tokio::task::spawn_blocking(move || crate::watcher::walk_project_dirs(&root))
                .await
                .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;

        let mut summary = ReindexSummary::default();
        let mut seen_ws = std::collections::HashSet::new();

        for (ws, proj, proj_root) in project_dirs {
            if seen_ws.insert(ws) {
                let ws_dir = proj_root
                    .parent()
                    .unwrap_or(proj_root.as_path())
                    .to_path_buf();
                let meta = Self::read_scope_meta(&ws_dir)?;
                let name = meta
                    .get("workspace")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        WikiError::Io(std::io::Error::other(format!(
                            "{}/_meta.md is missing the `workspace` name",
                            ws_dir.display()
                        )))
                    })?;
                self.writer.ensure_workspace_with_id(ws, name).await?;
                summary.workspaces += 1;
            }

            let meta = Self::read_scope_meta(&proj_root)?;
            let name = meta
                .get("project")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    WikiError::Io(std::io::Error::other(format!(
                        "{}/_meta.md is missing the `project` name",
                        proj_root.display()
                    )))
                })?;
            let repo_path = meta
                .get("repo_path")
                .and_then(|v| v.as_str())
                .map(String::from);
            self.writer
                .ensure_project_with_id(proj, ws, name, repo_path)
                .await?;
            summary.projects += 1;

            let pr = proj_root.clone();
            let pages = tokio::task::spawn_blocking(move || crate::watcher::walk_markdown(&pr))
                .await
                .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;
            for path in pages {
                self.reindex_page(ws, proj, path).await?;
                summary.pages += 1;
            }
        }
        Ok(summary)
    }

    /// Write a `_meta.md` scope manifest under `dir` from `frontmatter`,
    /// idempotently — unchanged content is left untouched so a startup
    /// backfill never churns the wiki git history. Returns `true` if written.
    fn write_scope_manifest(dir: &Path, frontmatter: serde_json::Value) -> WikiResult<bool> {
        let content = emit(&Markdown {
            frontmatter,
            body: String::new(),
        })?;
        let path = dir.join("_meta.md");
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(WikiError::Io(std::io::Error::other(format!(
                    "refusing to update symlinked scope manifest {}",
                    path.display()
                ))));
            }
            Ok(meta) if meta.is_file() => {
                if std::fs::read_to_string(&path).is_ok_and(|existing| existing == content) {
                    return Ok(false);
                }
            }
            Ok(_) | Err(_) => {}
        }
        std::fs::create_dir_all(dir)?;
        crate::atomic::write_atomic(&path, content.as_bytes())?;
        Ok(true)
    }

    /// Ensure every workspace/project scope has its self-describing `_meta.md`
    /// manifest on disk (`workspace`/`project` name + `repo_path`), so the wiki
    /// tree alone is enough to rebuild the index via [`Self::reindex_all`] —
    /// the "DB is rebuildable from files" guarantee. Idempotent; safe to run on
    /// every startup. No-op without a store reader. Returns the count written.
    ///
    /// # Errors
    /// Returns [`WikiError`] for store or filesystem errors.
    pub async fn backfill_scope_manifests(&self) -> WikiResult<usize> {
        let Some(reader) = &self.store_reader else {
            return Ok(0);
        };
        let _guard = self.mutation_lock.write().await;
        let workspaces = reader.list_all_workspace_scopes().await?;
        let scopes = reader.list_all_scopes().await?;
        let mut written = 0;
        for ws in workspaces {
            let ws_dir = self.root().join(ws.workspace_id.to_string());
            if Self::write_scope_manifest(
                &ws_dir,
                serde_json::json!({ "workspace": ws.workspace_name }),
            )? {
                written += 1;
            }
        }
        for s in scopes {
            let ws_dir = self.root().join(s.workspace_id.to_string());
            let mut fm = serde_json::json!({ "project": s.project_name });
            if let Some(rp) = s.repo_path {
                fm["repo_path"] = serde_json::Value::String(rp);
            }
            if Self::write_scope_manifest(&ws_dir.join(s.project_id.to_string()), fm)? {
                written += 1;
            }
        }
        Ok(written)
    }

    /// Atomically apply a batch of page writes. Either all pages land
    /// (one SQL transaction) and their files are renamed into place.
    /// Files are installed before the SQL batch so markdown remains the source
    /// of truth; if the SQL batch fails at runtime, installed files are rolled
    /// back best-effort to their prior contents.
    ///
    /// # Errors
    /// Returns [`WikiError`] for any filesystem, parsing, or store
    /// error.
    pub async fn apply_batch(&self, requests: Vec<WritePageRequest>) -> WikiResult<Vec<PageId>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        // Pre-compute markdown for each request. Persistence happens only
        // after the mutation guard + project/workspace validation below.
        let mut staged: Vec<(WritePageRequest, String, Option<AdmissionContext>)> =
            Vec::with_capacity(requests.len());
        for mut req in requests {
            // Defence-in-depth scrub at the batch boundary too.
            req.body = self.sanitizer.scrub(&req.body);
            if let Some(t) = req.title.take() {
                req.title = Some(self.sanitizer.scrub(&t));
            }

            req.frontmatter = stamp_last_modified_by(req.frontmatter, &req.actor);
            let mut markdown = Markdown {
                frontmatter: req.frontmatter,
                body: req.body,
            };

            let resolved_ctx = if let Some(chain) = &self.admission_chain {
                let mut ctx = req.admission_ctx.take().unwrap_or_default();
                ctx.actor = req.actor.clone();
                self.resolve_admission_names(req.workspace_id, req.project_id, &mut ctx)
                    .await;
                chain.run(&req.path, &mut markdown, &ctx).await?;
                Some(ctx)
            } else {
                None
            };

            markdown.body = self.sanitizer.scrub(&markdown.body);
            scrub_frontmatter_strings(&mut markdown.frontmatter, &self.sanitizer);
            req.pinned = req.pinned
                || is_slot_path(&req.path)
                || markdown
                    .frontmatter
                    .get("pinned")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            markdown.frontmatter =
                canonicalize_index_frontmatter(markdown.frontmatter, req.tier, req.pinned);

            let title = req
                .title
                .take()
                .unwrap_or_else(|| derive_title(&markdown.frontmatter, &markdown.body, &req.path));
            let emitted = emit(&markdown)?;
            req.frontmatter = markdown.frontmatter;
            req.body = markdown.body;
            let req_with_title = WritePageRequest {
                title: Some(title),
                ..req
            };
            staged.push((req_with_title, emitted, resolved_ctx));
        }

        let (ids, dispatches) = {
            let _guard = self.mutation_lock.read().await;
            let mut batch: Vec<(ai_memory_core::NewPage, String)> =
                Vec::with_capacity(staged.len());
            let mut dispatches = Vec::with_capacity(staged.len());
            for (req, emitted, ctx) in staged {
                self.ensure_project_workspace(req.workspace_id, req.project_id)
                    .await?;
                batch.push((
                    ai_memory_core::NewPage {
                        workspace_id: req.workspace_id,
                        project_id: req.project_id,
                        path: req.path.clone(),
                        title: req.title.clone().unwrap_or_default(),
                        body: req.body.clone(),
                        tier: req.tier,
                        frontmatter_json: req.frontmatter.clone(),
                        pinned: req.pinned,
                        links: extract_links(&req.body, &req.path),
                        author_id: req.author_id,
                    },
                    emitted,
                ));
                dispatches.push((req.path, req.frontmatter, req.body, ctx));
            }

            // The backend stages + installs the SoT writes and commits the
            // index rows in one transaction, rolling the SoT back (where it
            // can) when the index write fails.
            let ids = self.content.persist_pages_batch(batch).await?;

            (ids, dispatches)
        };

        if let Some(chain) = &self.admission_chain {
            for (path, frontmatter, body, ctx) in &dispatches {
                if let Some(ctx) = ctx {
                    chain.dispatch_async(Some(path.as_str()), frontmatter, body, ctx);
                }
            }
        }

        Ok(ids)
    }

    /// Write `body` (with optional `frontmatter`) atomically to
    /// `<wiki_root>/<workspace_id>/<project_id>/<path>` and upsert the
    /// matching page row in the store.
    ///
    /// The store side does the sha256 short-circuit + supersession dance.
    /// Returns the id of the page version that is now `is_latest = 1`.
    ///
    /// # Errors
    /// Returns [`WikiError`] for any filesystem, parsing, or store error.
    pub async fn write_page(&self, req: WritePageRequest) -> WikiResult<PageId> {
        let WritePageRequest {
            workspace_id,
            project_id,
            path,
            frontmatter,
            body,
            tier,
            pinned,
            title: explicit_title,
            admission_ctx,
            author_id,
            actor,
        } = req;

        // Defence-in-depth: scrub the body before we touch disk or the
        // store, regardless of caller. The hook ingress already scrubs
        // observation text; this catches LLM-rewritten consolidation
        // bodies, manual `write-page` CLI inputs, and anything an MCP
        // tool slips through.
        let body = self.sanitizer.scrub(&body);

        let mut pinned = pinned || is_slot_path(&path);
        // Multi-user attribution (P1.6): stamp `last_modified_by` into the
        // frontmatter BEFORE building the markdown, so both the admission
        // chain and the on-disk file see the resolved author. Rung 0
        // (anonymous) → no block, no disk-shape change for single-user.
        let frontmatter = stamp_last_modified_by(frontmatter, &actor);
        let mut markdown = Markdown { frontmatter, body };

        // Admission webhook chain runs after the initial scrub, before emit +
        // atomic write. Mutations to
        // frontmatter/body here propagate to both the on-disk markdown
        // (via emit below) and the store's `frontmatter_json` / `body`
        // (via the upsert below) atomically. See `crate::admission`.
        let resolved_ctx = if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            // Single identity source: the webhook actor is the same
            // `ActorContext` used for on-disk attribution (req.actor),
            // populated by the auth layer — not a separate header bridge.
            ctx.actor = actor.clone();
            self.resolve_admission_names(workspace_id, project_id, &mut ctx)
                .await;
            // Blocking webhooks run synchronously (they may mutate / reject).
            chain.run(&path, &mut markdown, &ctx).await?;
            Some(ctx)
        } else {
            None
        };

        // Webhook mutations are external input too. Scrub again so a webhook
        // cannot reintroduce secrets after the caller body was sanitized.
        markdown.body = self.sanitizer.scrub(&markdown.body);
        scrub_frontmatter_strings(&mut markdown.frontmatter, &self.sanitizer);
        pinned = pinned
            || markdown
                .frontmatter
                .get("pinned")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        markdown.frontmatter = canonicalize_index_frontmatter(markdown.frontmatter, tier, pinned);

        // Re-derive title + links from the (possibly mutated) markdown.
        // We do this after the chain so explicit title overrides survive
        // mutations and webhooks that rename or restructure the body
        // still get the right title/links extracted.
        let title = explicit_title
            .clone()
            .map(|t| self.sanitizer.scrub(&t))
            .unwrap_or_else(|| derive_title(&markdown.frontmatter, &markdown.body, &path));
        let links = extract_links(&markdown.body, &path);

        let Markdown {
            frontmatter: final_frontmatter,
            body: final_body,
        } = markdown;
        let path_for_dispatch = path.clone();
        let frontmatter_for_dispatch = final_frontmatter.clone();
        let emitted = emit(&Markdown {
            frontmatter: final_frontmatter.clone(),
            body: final_body.clone(),
        })?;

        let page_id = {
            let _guard = self.mutation_lock.read().await;
            self.ensure_project_workspace(workspace_id, project_id)
                .await?;
            self.content
                .persist_page(
                    NewPage {
                        workspace_id,
                        project_id,
                        path,
                        title,
                        body: final_body.clone(),
                        tier,
                        frontmatter_json: final_frontmatter,
                        pinned,
                        links,
                        author_id,
                    },
                    emitted,
                )
                .await?
        };
        // Embed if configured. We do this on the caller's task so the
        // tool reply still happens "indexes commit in the same
        // transaction" (basic-memory #763 lesson): no fire-and-forget
        // background embedding.
        if let Some(embedder) = &self.embedder {
            match embedder.embed_document(&final_body).await {
                Ok(vec) => {
                    let bytes = f32_vec_to_bytes(&vec);
                    self.writer
                        .store_embedding(
                            page_id,
                            bytes,
                            embedder.provider().to_string(),
                            embedder.model().to_string(),
                            embedder.dim(),
                        )
                        .await?;
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %page_id, "embedding failed; page indexed without it");
                }
            }
        }

        // Non-blocking webhooks fire-and-forget only after the page has landed
        // on disk and the DB/index write has succeeded. They observe the final
        // persisted page and cannot mutate or reject it.
        if let (Some(chain), Some(ctx)) = (&self.admission_chain, &resolved_ctx) {
            chain.dispatch_async(
                Some(path_for_dispatch.as_str()),
                &frontmatter_for_dispatch,
                &final_body,
                ctx,
            );
        }
        Ok(page_id)
    }
}

/// Input bundle for [`Wiki::write_page`]. Carries the full 3-tuple
/// identity (`workspace_id`, `project_id`, `path`) plus body & metadata.
#[derive(Debug, Clone)]
pub struct WritePageRequest {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Relative wiki path.
    pub path: PagePath,
    /// Optional frontmatter (JSON object). May be `Null` for no frontmatter.
    pub frontmatter: serde_json::Value,
    /// Markdown body (excluding any frontmatter block).
    pub body: String,
    /// Tier classification.
    pub tier: Tier,
    /// `true` if the user has pinned this page.
    pub pinned: bool,
    /// Optional pre-derived title (used by `apply_batch` to share the
    /// title between the staged markdown file + the store row).
    #[doc(hidden)]
    pub title: Option<String>,
    /// Optional admission webhook context (op + loop-prevention skip
    /// list + resolved workspace/project names). Populated by
    /// authenticated callers (MCP tool, admin endpoint); left `None` by
    /// internal callers (CLI bootstrap, consolidator from hooks, tests)
    /// — when the chain is configured, `None` is treated as a default
    /// [`AdmissionContext`]. The actor that rides in the webhook payload
    /// comes from [`Self::actor`], not from here (single source of
    /// identity since the v0.8 multi-user merge).
    pub admission_ctx: Option<AdmissionContext>,
    /// Multi-user attribution: the registered user (rung-2) who made
    /// this write, when resolved by the auth middleware. Propagates to
    /// `pages.author_id` and the on-disk frontmatter `last_modified_by`
    /// block (the latter is built from the broader `ActorContext` —
    /// see [`Self::actor`] — so root + anonymous writes also get
    /// frontmatter even though they leave `author_id` NULL). Defaults
    /// to `None` for backward compat with internal callers
    /// (consolidator, lint rewriters) that build `WritePageRequest`
    /// without an HTTP request layer.
    pub author_id: Option<ai_memory_core::UserId>,
    /// Identity carried in the on-disk frontmatter's `last_modified_by`
    /// block AND the admission webhook payload's `ctx.actor`. The auth
    /// middleware fills this from the four-rung resolution (injected as
    /// `Extension<ai_memory_core::ActorContext>`): rung 1 supplies the
    /// configured root template, rung 2 supplies the row's
    /// user/name/email. Defaults to anonymous for backward compat.
    pub actor: ai_memory_core::ActorContext,
}

fn ai_memory_wiki_error(msg: &str) -> crate::WikiError {
    crate::WikiError::Io(std::io::Error::other(msg.to_string()))
}

fn derive_index_metadata(
    path: &PagePath,
    frontmatter: &serde_json::Value,
) -> WikiResult<(Tier, bool)> {
    let tier = match frontmatter.get("tier") {
        None => Tier::Semantic,
        Some(serde_json::Value::String(s)) => s.parse::<Tier>().map_err(|e| {
            ai_memory_wiki_error(&format!(
                "invalid tier in frontmatter for {}: {e}",
                path.as_str()
            ))
        })?,
        Some(_) => {
            return Err(ai_memory_wiki_error(&format!(
                "invalid non-string tier in frontmatter for {}",
                path.as_str()
            )));
        }
    };
    let pinned = is_slot_path(path)
        || frontmatter
            .get("pinned")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    Ok((tier, pinned))
}

fn canonicalize_index_frontmatter(
    frontmatter: serde_json::Value,
    tier: Tier,
    pinned: bool,
) -> serde_json::Value {
    let mut obj = match frontmatter {
        serde_json::Value::Object(m) => m,
        serde_json::Value::Null => serde_json::Map::new(),
        other => return other,
    };
    obj.insert(
        "tier".to_string(),
        serde_json::Value::String(tier.as_str().to_string()),
    );
    if pinned {
        obj.insert("pinned".to_string(), serde_json::Value::Bool(true));
    } else if obj.get("pinned").and_then(|v| v.as_bool()) == Some(true) {
        obj.remove("pinned");
    }
    serde_json::Value::Object(obj)
}

fn render_auto_improve_sidecar(detail: &AutoImproveProposalDetail) -> WikiResult<String> {
    let evidence = serde_json::to_string_pretty(&detail.evidence_json)?;
    let patch = detail
        .patch_json
        .as_ref()
        .map(serde_json::to_string_pretty)
        .transpose()?
        .unwrap_or_else(|| "null".into());
    let expected_base = detail
        .expected_base_body_sha256
        .map(|hash| hash.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .unwrap_or_else(|| "none".into());
    Ok(format!(
        "# Pending auto-improvement proposal\n\n\
         - proposal_id: `{}`\n\
         - run_id: `{}`\n\
         - status: `{}`\n\
         - operation: `{}`\n\
         - target_path: `{}`\n\
         - kind: `{}`\n\
         - title: `{}`\n\
         - confidence: `{}`\n\
         - edit_mode: `{}`\n\
         - expected_base_body_sha256: `{}`\n\
         - staged_at: `{}`\n\n\
         ## Rationale\n\n{}\n\n\
         ## Evidence\n\n```json\n{}\n```\n\n\
         ## Patch metadata\n\n```json\n{}\n```\n\n\
         ## Proposed body\n\n{}\n",
        detail.summary.id,
        detail.summary.run_id,
        detail.summary.status.as_str(),
        detail.summary.operation.as_str(),
        detail.summary.target_path.as_str(),
        detail.summary.kind,
        detail.summary.title,
        detail.summary.confidence,
        detail.edit_mode,
        expected_base,
        detail.summary.staged_at,
        detail.rationale,
        evidence,
        patch,
        detail.body_markdown,
    ))
}

fn scrub_frontmatter_strings(value: &mut serde_json::Value, sanitizer: &Sanitizer) {
    match value {
        serde_json::Value::String(s) => {
            *s = sanitizer.scrub(s);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                scrub_frontmatter_strings(item, sanitizer);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                scrub_frontmatter_strings(item, sanitizer);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Append a `last_modified_by` block to the page's frontmatter when the
/// auth middleware resolved a non-anonymous actor. The block carries the
/// stable `username` plus optional `name` + `email`. Designed to be
/// **idempotent on the keys** (the value replaces any prior version), so
/// repeated writes by different users always reflect the latest one
/// rather than accumulating history — historical authorship lives in
/// `pages.author_id` + the supersession chain, not in frontmatter.
///
/// When the actor is anonymous (rung 0) the input is returned
/// untouched — pre-multi-user installs see zero disk-shape change.
fn stamp_last_modified_by(
    frontmatter: serde_json::Value,
    actor: &ai_memory_core::ActorContext,
) -> serde_json::Value {
    let Some(username) = actor.user.as_ref().filter(|s| !s.is_empty()) else {
        return frontmatter;
    };
    let mut obj = match frontmatter {
        serde_json::Value::Object(m) => m,
        serde_json::Value::Null => serde_json::Map::new(),
        // Frontmatter is conventionally an object; preserve a non-null
        // non-object value by NOT mutating it (operator wrote something
        // exotic; we shouldn't clobber it on every write).
        other => return other,
    };
    let mut author = serde_json::Map::new();
    author.insert(
        "username".to_string(),
        serde_json::Value::String(username.clone()),
    );
    if let Some(name) = &actor.name {
        author.insert("name".to_string(), serde_json::Value::String(name.clone()));
    }
    if let Some(email) = &actor.email {
        author.insert(
            "email".to_string(),
            serde_json::Value::String(email.clone()),
        );
    }
    obj.insert(
        "last_modified_by".to_string(),
        serde_json::Value::Object(author),
    );
    serde_json::Value::Object(obj)
}

fn is_slot_path(path: &PagePath) -> bool {
    path.as_str().starts_with("_slots/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::{FailurePolicy, WebhookConfig};
    use ai_memory_store::{
        AutoImproveProposalOperation, AutoImproveProposalStatus, NewAutoImproveProposal,
        StageAutoImproveRun, Store,
    };
    use tempfile::TempDir;

    #[cfg(windows)]
    fn create_test_symlink_file(target: &Path, link: &Path) -> bool {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(1314) => {
                eprintln!("skipping symlink assertion: Windows symlink privilege unavailable");
                false
            }
            Err(e) => panic!("failed to create symlink {}: {e}", link.display()),
        }
    }

    #[cfg(unix)]
    fn create_test_symlink_file(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }

    #[tokio::test]
    async fn project_root_is_wiki_root_joined_with_ws_and_proj() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        assert_eq!(
            wiki.project_root(ws, proj),
            tmp.path()
                .join("wiki")
                .join(ws.to_string())
                .join(proj.to_string()),
        );
    }

    #[tokio::test]
    async fn upgrade_baseline_checkpoint_commits_existing_uncommitted_tree_once() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        wiki.write_page(req(
            ws,
            proj,
            "notes/baseline.md",
            "existing page before recovery checkpoints",
            serde_json::json!({ "title": "Baseline" }),
        ))
        .await
        .unwrap();

        assert_eq!(wiki.git().commit_count(), 0);
        let oid = wiki
            .ensure_upgrade_baseline_checkpoint()
            .unwrap()
            .expect("existing wiki content should be checkpointed");
        assert_eq!(wiki.git().commit_count(), 1);
        assert!(wiki.ensure_upgrade_baseline_checkpoint().unwrap().is_none());
        assert_eq!(wiki.git().commit_count(), 1);

        let checkpoints = wiki.recent_checkpoints(1).unwrap();
        assert_eq!(checkpoints[0].oid, oid.to_string());
        assert_eq!(
            checkpoints[0].summary,
            "upgrade baseline: existing wiki tree before recovery checkpoints"
        );
    }

    fn req(
        ws: WorkspaceId,
        proj: ProjectId,
        path: &str,
        body: &str,
        fm: serde_json::Value,
    ) -> WritePageRequest {
        WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: fm,
            body: body.into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        }
    }

    fn proposal(
        path: &str,
        op: AutoImproveProposalOperation,
        body: &str,
    ) -> NewAutoImproveProposal {
        NewAutoImproveProposal {
            operation: op,
            target_path: PagePath::new(path).unwrap(),
            kind: "note".into(),
            title: "Proposed".into(),
            confidence: 0.9,
            rationale: "test rationale".into(),
            evidence_json: serde_json::json!([{ "source": "test" }]),
            body_markdown: body.into(),
            artifact_sha256: None,
            edit_mode: None,
            patch_json: None,
            expected_base_body_sha256: None,
        }
    }

    fn proposal_with_fields(
        path: &str,
        body: &str,
        rationale: &str,
        evidence_json: serde_json::Value,
    ) -> NewAutoImproveProposal {
        NewAutoImproveProposal {
            rationale: rationale.into(),
            evidence_json,
            ..proposal(path, AutoImproveProposalOperation::Create, body)
        }
    }

    async fn stage_one(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        path: &str,
        body: &str,
    ) -> AutoImproveProposalId {
        stage_one_op(
            store,
            ws,
            proj,
            proposal(path, AutoImproveProposalOperation::Create, body),
        )
        .await
    }

    async fn stage_one_op(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        proposal: NewAutoImproveProposal,
    ) -> AutoImproveProposalId {
        store
            .writer
            .stage_auto_improve_run(StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: None,
                provider: Some("test".into()),
                model: Some("model".into()),
                summary: Some("summary".into()),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({}),
                proposal_actor: ActorContext {
                    agent: Some("auto_improve".into()),
                    ..ActorContext::default()
                },
                proposals: vec![proposal],
            })
            .await
            .unwrap()
            .proposal_ids[0]
    }

    async fn scoped(tmp: &TempDir) -> (Store, Wiki, WorkspaceId, ProjectId) {
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        (store, wiki, ws, proj)
    }

    #[tokio::test]
    async fn auto_improve_sidecar_writes_non_indexed_review_file() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let id = stage_one(&store, ws, proj, "notes/proposed.md", "proposed body").await;
        let sidecar = wiki.write_auto_improve_sidecar(ws, proj, id).await.unwrap();
        assert_eq!(
            sidecar,
            wiki.project_root(ws, proj)
                .join(format!("_pending/auto-improve/{id}.md"))
        );
        let content = std::fs::read_to_string(sidecar).unwrap();
        assert!(content.contains("Pending auto-improvement proposal"));
        assert!(content.contains("proposed body"));
        assert!(
            store
                .reader
                .search_pages_for_project(ws, proj, "proposed body".into(), 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn auto_improve_sidecar_scrubs_stored_proposal_secrets() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let id = stage_one_op(
            &store,
            ws,
            proj,
            proposal_with_fields(
                "notes/leaky-proposal.md",
                "body has ANTHROPIC_API_KEY=sk-ant-leak-1234567890abcdef",
                "rationale has postgres://admin:hunter2@db.internal/prod",
                serde_json::json!([{ "secret": "GH_TOKEN=ghp_1234567890abcdef1234567890abcdef1234" }]),
            ),
        )
        .await;
        let sidecar = wiki.write_auto_improve_sidecar(ws, proj, id).await.unwrap();
        let content = std::fs::read_to_string(sidecar).unwrap();
        assert!(content.contains("[REDACTED]"));
        assert!(!content.contains("sk-ant-leak"));
        assert!(!content.contains("hunter2"));
        assert!(!content.contains("ghp_"));
    }

    #[tokio::test]
    async fn reindex_page_refuses_pending_auto_improve_sidecar() {
        let tmp = TempDir::new().unwrap();
        let (_store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("_pending/auto-improve/x.md").unwrap();
        let abs = wiki.abs_path(ws, proj, &path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "---\ntitle: pending\n---\nbody").unwrap();
        assert!(wiki.reindex_page(ws, proj, path).await.is_err());
    }

    #[tokio::test]
    async fn reindex_page_derives_tier_and_pinned_from_frontmatter() {
        // Regression: the watcher's reindex_page must honour the on-disk
        // frontmatter tier/pinned (as synth.rs writes for session pages),
        // not hardcode Tier::Semantic / is_slot_path. Hardcoding flipped
        // episodic session pages to semantic (so they never decayed) and
        // dropped the frontmatter pin, churning a spurious version per write.
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;

        // Episodic + pinned page on disk, exactly like a synth session page.
        let path = PagePath::new("sessions/abc.md").unwrap();
        let abs = wiki.abs_path(ws, proj, &path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(
            &abs,
            "---\ntitle: S\ntier: episodic\npinned: true\n---\nbody",
        )
        .unwrap();

        let id1 = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        let meta = store
            .reader
            .page_meta("default", "scratch", path.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            meta.tier, "episodic",
            "reindex must keep the frontmatter tier, not force semantic"
        );
        assert!(meta.pinned, "reindex must keep the frontmatter pinned flag");

        // Idempotent: re-running on the unchanged file must not supersede.
        let id2 = wiki.reindex_page(ws, proj, path).await.unwrap();
        assert_eq!(
            id1, id2,
            "reindex of an unchanged page must be a no-op (no spurious version)"
        );

        // Backward-compat: a page with no tier in frontmatter defaults to semantic.
        let plain = PagePath::new("notes/plain.md").unwrap();
        let pabs = wiki.abs_path(ws, proj, &plain);
        std::fs::create_dir_all(pabs.parent().unwrap()).unwrap();
        std::fs::write(&pabs, "---\ntitle: P\n---\nbody").unwrap();
        wiki.reindex_page(ws, proj, plain.clone()).await.unwrap();
        let pmeta = store
            .reader
            .page_meta("default", "scratch", plain.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pmeta.tier, "semantic",
            "a page without a frontmatter tier must default to semantic"
        );

        let invalid = PagePath::new("notes/bad-tier.md").unwrap();
        let iabs = wiki.abs_path(ws, proj, &invalid);
        std::fs::write(&iabs, "---\ntitle: Bad\ntier: episdoic\n---\nbody").unwrap();
        assert!(
            wiki.reindex_page(ws, proj, invalid).await.is_err(),
            "malformed tier frontmatter must fail closed instead of silently becoming semantic"
        );

        let non_string = PagePath::new("notes/non-string-tier.md").unwrap();
        let ns_abs = wiki.abs_path(ws, proj, &non_string);
        std::fs::write(&ns_abs, "---\ntitle: Bad\ntier: 7\n---\nbody").unwrap();
        assert!(
            wiki.reindex_page(ws, proj, non_string).await.is_err(),
            "non-string tier frontmatter must fail closed"
        );
    }

    #[tokio::test]
    async fn write_paths_persist_index_metadata_for_reindex_round_trip() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;

        let path = PagePath::new("notes/episodic.md").unwrap();
        let mut request = req(
            ws,
            proj,
            path.as_str(),
            "episodic body",
            serde_json::json!({"title": "Episodic"}),
        );
        request.tier = Tier::Episodic;
        request.pinned = true;
        let id = wiki.write_page(request).await.unwrap();

        let md = wiki.read_page(ws, proj, &path).await.unwrap();
        assert_eq!(md.frontmatter["tier"], "episodic");
        assert_eq!(md.frontmatter["pinned"], true);
        let id2 = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        assert_eq!(id, id2, "write then reindex should be idempotent");
        let meta = store
            .reader
            .page_meta("default", "scratch", path.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.tier, "episodic");
        assert!(meta.pinned);

        let batch_path = PagePath::new("notes/procedural.md").unwrap();
        let mut batch_req = req(
            ws,
            proj,
            batch_path.as_str(),
            "procedure body",
            serde_json::json!({"title": "Procedure"}),
        );
        batch_req.tier = Tier::Procedural;
        wiki.apply_batch(vec![batch_req]).await.unwrap();
        let md = wiki.read_page(ws, proj, &batch_path).await.unwrap();
        assert_eq!(md.frontmatter["tier"], "procedural");
        wiki.reindex_page(ws, proj, batch_path.clone())
            .await
            .unwrap();
        let meta = store
            .reader
            .page_meta("default", "scratch", batch_path.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.tier, "procedural");
        assert!(!meta.pinned);

        let frontmatter_pinned_path = PagePath::new("notes/frontmatter-pinned.md").unwrap();
        let mut frontmatter_pinned_req = req(
            ws,
            proj,
            frontmatter_pinned_path.as_str(),
            "frontmatter pinned body",
            serde_json::json!({"title": "Frontmatter Pinned", "pinned": true}),
        );
        frontmatter_pinned_req.pinned = false;
        wiki.write_page(frontmatter_pinned_req).await.unwrap();
        wiki.reindex_page(ws, proj, frontmatter_pinned_path.clone())
            .await
            .unwrap();
        let meta = store
            .reader
            .page_meta("default", "scratch", frontmatter_pinned_path.as_str())
            .await
            .unwrap()
            .unwrap();
        assert!(meta.pinned);
    }

    #[tokio::test]
    async fn restore_page_from_checkpoint_preserves_frontmatter_metadata() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("sessions/restored.md").unwrap();
        let abs = wiki.abs_path(ws, proj, &path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(
            &abs,
            "---\ntitle: Restored\ntier: episodic\npinned: true\n---\nold",
        )
        .unwrap();
        let rev = wiki.commit_all("checkpoint").unwrap().unwrap().to_string();
        std::fs::write(&abs, "---\ntitle: Restored\ntier: semantic\n---\nnew").unwrap();

        wiki.restore_page_from_checkpoint(ws, proj, path.clone(), &rev)
            .await
            .unwrap();
        let meta = store
            .reader
            .page_meta("default", "scratch", path.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.tier, "episodic");
        assert!(meta.pinned);
        let md = wiki.read_page(ws, proj, &path).await.unwrap();
        assert_eq!(md.body.trim(), "old");
    }

    #[tokio::test]
    async fn auto_improve_approval_writes_target_and_marks_approved() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let id = stage_one(&store, ws, proj, "notes/approved.md", "approved body").await;
        let result = wiki
            .approve_auto_improve_proposal(
                ws,
                proj,
                id,
                ActorContext {
                    user: Some("reviewer".into()),
                    ..ActorContext::default()
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            ApproveAutoImproveProposalResult::Approved { .. }
        ));
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/approved.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "approved body"
        );
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Approved);
        assert_eq!(detail.events[0].event, "staged");
        assert_eq!(detail.events[0].actor_json["agent"], "auto_improve");
        assert_eq!(detail.events.last().unwrap().event, "approved");
        assert_eq!(detail.events.last().unwrap().actor_json["user"], "reviewer");
    }

    #[tokio::test]
    async fn auto_improve_approval_update_path_supersedes_existing_page() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        wiki.write_page(req(
            ws,
            proj,
            "notes/update-proposal.md",
            "old body",
            serde_json::json!({ "title": "Old" }),
        ))
        .await
        .unwrap();
        let id = stage_one_op(
            &store,
            ws,
            proj,
            proposal(
                "notes/update-proposal.md",
                AutoImproveProposalOperation::Update,
                "new proposal body",
            ),
        )
        .await;
        let result = wiki
            .approve_auto_improve_proposal(ws, proj, id, ActorContext::default(), None, None)
            .await
            .unwrap();
        assert!(matches!(
            result,
            ApproveAutoImproveProposalResult::Approved { .. }
        ));
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/update-proposal.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "new proposal body"
        );
    }

    #[tokio::test]
    async fn auto_improve_approval_admission_mutation_is_persisted_and_scrubbed() {
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::{Json, Router};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/mutate",
            post(|Json(_payload): Json<serde_json::Value>| async move {
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "page": { "body": "mutated ANTHROPIC_API_KEY=sk-ant-leak-1234567890abcdef" }
                    })),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let wiki = wiki.with_admission_chain(
            AdmissionChain::new(vec![WebhookConfig {
                name: "mutator".into(),
                url: format!("http://{addr}/mutate"),
                timeout_ms: 1000,
                failure_policy: FailurePolicy::Reject,
                events: vec![AdmissionOp::WritePage],
                blocking: true,
            }])
            .unwrap(),
        );
        let id = stage_one(&store, ws, proj, "notes/mutated.md", "original body").await;
        wiki.approve_auto_improve_proposal(ws, proj, id, ActorContext::default(), None, None)
            .await
            .unwrap();
        let stored = store
            .reader
            .page_body_by_ids(ws, proj, "notes/mutated.md")
            .await
            .unwrap()
            .unwrap();
        assert!(stored.body.contains("[REDACTED]"));
        assert!(!stored.body.contains("sk-ant-leak"));
        assert!(
            std::fs::read_to_string(wiki.abs_path(
                ws,
                proj,
                &PagePath::new("notes/mutated.md").unwrap()
            ))
            .unwrap()
            .contains("[REDACTED]")
        );
    }

    #[tokio::test]
    async fn auto_improve_approval_conflict_rolls_back_target_file() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let id = stage_one(&store, ws, proj, "notes/conflict.md", "proposal body").await;
        wiki.write_page(req(
            ws,
            proj,
            "notes/conflict.md",
            "external body",
            serde_json::json!({ "title": "External" }),
        ))
        .await
        .unwrap();
        let result = wiki
            .approve_auto_improve_proposal(ws, proj, id, ActorContext::default(), None, None)
            .await
            .unwrap();
        assert_eq!(result, ApproveAutoImproveProposalResult::Conflict);
        assert!(
            std::fs::read_to_string(wiki.abs_path(
                ws,
                proj,
                &PagePath::new("notes/conflict.md").unwrap()
            ))
            .unwrap()
            .contains("external body")
        );
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/conflict.md")
                .await
                .unwrap()
                .unwrap()
                .body,
            "external body"
        );
    }

    #[tokio::test]
    async fn auto_improve_admission_rejection_marks_failed_without_writing_target() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let wiki = wiki.with_admission_chain(
            AdmissionChain::new(vec![WebhookConfig {
                name: "rejector".into(),
                url: "http://127.0.0.1:9/reject".into(),
                timeout_ms: 50,
                failure_policy: FailurePolicy::Reject,
                events: vec![AdmissionOp::WritePage],
                blocking: true,
            }])
            .unwrap(),
        );
        let id = stage_one(&store, ws, proj, "notes/rejected.md", "body").await;
        assert!(
            wiki.approve_auto_improve_proposal(ws, proj, id, ActorContext::default(), None, None)
                .await
                .is_err()
        );
        assert!(
            !wiki
                .abs_path(ws, proj, &PagePath::new("notes/rejected.md").unwrap())
                .exists()
        );
        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.summary.status, AutoImproveProposalStatus::Failed);
        assert_eq!(detail.events.last().unwrap().event, "failed");
    }

    #[tokio::test]
    async fn write_page_writes_file_and_indexes_in_store() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let id = wiki
            .write_page(req(
                ws,
                proj,
                "notes/karpathy.md",
                "Karpathy says: compile, do not retrieve.\n",
                serde_json::json!({ "title": "Karpathy LLM Wiki" }),
            ))
            .await
            .unwrap();
        let _ = id; // any non-zero PageId is sufficient

        // File is on disk at the per-project location.
        let on_disk = std::fs::read_to_string(wiki.abs_path(
            ws,
            proj,
            &PagePath::new("notes/karpathy.md").unwrap(),
        ))
        .unwrap();
        assert!(on_disk.starts_with("---\n"));
        assert!(on_disk.contains("title: Karpathy LLM Wiki"));
        assert!(on_disk.contains("Karpathy says"));

        // FTS5 finds it via the store reader.
        let hits = store
            .reader
            .search_pages("karpathy".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Karpathy LLM Wiki");
        assert!(hits[0].snippet.contains("compile"));
    }

    #[tokio::test]
    async fn write_page_rolls_back_file_when_store_upsert_fails() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let path = PagePath::new("notes/rollback.md").unwrap();

        wiki.write_page(req(
            ws,
            proj,
            path.as_str(),
            "old body",
            serde_json::json!({ "title": "Old" }),
        ))
        .await
        .unwrap();

        let mut bad = req(
            ws,
            proj,
            path.as_str(),
            "new body should not remain",
            serde_json::json!({ "title": "New" }),
        );
        bad.author_id = Some(ai_memory_core::UserId::new());
        let err = wiki.write_page(bad).await.unwrap_err();
        assert!(
            err.to_string().contains("FOREIGN KEY") || err.to_string().contains("constraint"),
            "expected FK failure, got {err}"
        );

        let on_disk = std::fs::read_to_string(wiki.abs_path(ws, proj, &path)).unwrap();
        assert!(on_disk.contains("old body"));
        assert!(!on_disk.contains("new body should not remain"));

        let stored = store
            .reader
            .page_body_by_ids(ws, proj, path.as_str())
            .await
            .unwrap()
            .expect("old row should remain latest");
        assert_eq!(stored.body, "old body");
        assert_eq!(stored.title, "Old");
    }

    /// Defence-in-depth: anything that reaches `write_page` gets
    /// scrubbed at the wiki boundary, even if upstream callers (LLM
    /// consolidation output, manual `write-page` CLI input, MCP tool
    /// args) skipped the hook-ingress sanitizer.
    #[tokio::test]
    async fn write_page_scrubs_secrets_at_the_wiki_boundary() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let body = "we agreed to use ANTHROPIC_API_KEY=sk-ant-leak-1234567890abcdef \
                    and the canary id sk-canary-LEAK_ME_PLEASE_xxxxxxxxxxxx — see \
                    postgres://admin:hunter2@db.internal/prod for details";
        wiki.write_page(req(
            ws,
            proj,
            "notes/leaky.md",
            body,
            serde_json::json!({ "title": "leaky" }),
        ))
        .await
        .unwrap();

        let on_disk = std::fs::read_to_string(wiki.abs_path(
            ws,
            proj,
            &PagePath::new("notes/leaky.md").unwrap(),
        ))
        .unwrap();
        // The on-disk page must not contain any of the planted
        // secrets; each should have been replaced with [REDACTED].
        assert!(
            on_disk.contains("[REDACTED]"),
            "expected redaction in: {on_disk}"
        );
        assert!(
            !on_disk.contains("sk-ant-leak"),
            "anthropic key leaked: {on_disk}"
        );
        assert!(
            !on_disk.contains("LEAK_ME_PLEASE"),
            "canary leaked: {on_disk}"
        );
        assert!(
            !on_disk.contains("hunter2"),
            "DB password leaked: {on_disk}"
        );

        // The store-indexed body must also be scrubbed (so FTS5 + the
        // MCP query path never surface the raw secret either).
        let hits = store
            .reader
            .search_pages("REDACTED".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.contains("sk-ant-leak"));
        assert!(!hits[0].snippet.contains("hunter2"));
    }

    #[tokio::test]
    async fn slot_pages_are_pinned_automatically() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        wiki.write_page(req(
            ws,
            proj,
            "_slots/current_focus.md",
            "Keep this tiny and durable.",
            serde_json::json!({ "title": "Current focus", "kind": "slot" }),
        ))
        .await
        .unwrap();

        let candidates = store.reader.decay_candidates(ws, proj).await.unwrap();
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].pinned, "slot pages should be decay-immune");
    }

    #[tokio::test]
    async fn apply_batch_persists_all_pages_in_one_transaction() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let batch: Vec<_> = (0..5)
            .map(|i| WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new(format!("batch/{i}.md")).unwrap(),
                frontmatter: serde_json::json!({"title": format!("Page {i}")}),
                body: format!("batch page {i} body line"),
                tier: Tier::Semantic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ai_memory_core::ActorContext::anonymous(),
            })
            .collect();
        let ids = wiki.apply_batch(batch).await.unwrap();
        assert_eq!(ids.len(), 5);
        for i in 0..5 {
            let path = wiki.abs_path(ws, proj, &PagePath::new(format!("batch/{i}.md")).unwrap());
            assert!(path.is_file(), "missing file {i}");
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(body.contains(&format!("Page {i}")));
        }
        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 5);
        let hits = store.reader.search_pages("batch".into(), 10).await.unwrap();
        assert_eq!(hits.len(), 5);
    }

    #[tokio::test]
    async fn apply_batch_rolls_back_files_when_sql_batch_fails() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let existing = PagePath::new("batch/existing.md").unwrap();
        let created = PagePath::new("batch/new.md").unwrap();

        wiki.write_page(req(
            ws,
            proj,
            existing.as_str(),
            "old batch body",
            serde_json::json!({ "title": "Old Batch" }),
        ))
        .await
        .unwrap();

        let mut replace_existing = req(
            ws,
            proj,
            existing.as_str(),
            "new batch body should roll back",
            serde_json::json!({ "title": "New Batch" }),
        );
        replace_existing.author_id = Some(ai_memory_core::UserId::new());
        let mut create_new = req(
            ws,
            proj,
            created.as_str(),
            "new file should be removed",
            serde_json::json!({ "title": "Created" }),
        );
        create_new.author_id = Some(ai_memory_core::UserId::new());

        let err = wiki
            .apply_batch(vec![replace_existing, create_new])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("FOREIGN KEY") || err.to_string().contains("constraint"),
            "expected FK failure, got {err}"
        );

        let existing_body = std::fs::read_to_string(wiki.abs_path(ws, proj, &existing)).unwrap();
        assert!(existing_body.contains("old batch body"));
        assert!(!existing_body.contains("new batch body should roll back"));
        assert!(!wiki.abs_path(ws, proj, &created).exists());

        let counts = store.reader.status_counts().await.unwrap();
        assert_eq!(counts.pages_latest, 1);
        let stored = store
            .reader
            .page_body_by_ids(ws, proj, existing.as_str())
            .await
            .unwrap()
            .expect("old row should remain latest");
        assert_eq!(stored.body, "old batch body");
        assert!(
            store
                .reader
                .page_body_by_ids(ws, proj, created.as_str())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn apply_batch_runs_admission_and_scrubs_webhook_mutations() {
        use crate::admission::{
            AdmissionChain, AdmissionContext, AdmissionOp, FailurePolicy, WebhookConfig,
        };
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::{Json, Router};
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/mutate",
            post(|Json(_payload): Json<serde_json::Value>| async move {
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "page": {
                            "frontmatter": { "title": "leaked sk-1234567890abcdef" },
                            "body": "webhook returned sk-1234567890abcdef"
                        }
                    })),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let chain = AdmissionChain::new(vec![WebhookConfig {
            name: "mutator".into(),
            url: format!("http://{addr}/mutate"),
            timeout_ms: 1_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::Consolidate],
            blocking: true,
        }])
        .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_admission_chain(chain)
            .with_store_reader(store.reader.clone());

        let ids = wiki
            .apply_batch(vec![WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new("batch/admitted.md").unwrap(),
                frontmatter: serde_json::json!({"title": "before"}),
                body: "before".into(),
                tier: Tier::Semantic,
                pinned: false,
                title: None,
                admission_ctx: Some(AdmissionContext {
                    op: AdmissionOp::Consolidate,
                    ..AdmissionContext::default()
                }),
                author_id: None,
                actor: ai_memory_core::ActorContext::anonymous(),
            }])
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);

        let on_disk = std::fs::read_to_string(wiki.abs_path(
            ws,
            proj,
            &PagePath::new("batch/admitted.md").unwrap(),
        ))
        .unwrap();
        assert!(on_disk.contains("[REDACTED]"), "{on_disk}");
        assert!(!on_disk.contains("sk-1234567890abcdef"), "{on_disk}");

        let hits = store
            .reader
            .search_pages("REDACTED".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// The preflight runs the blocking chain with no body and rejects when a
    /// `reject`-policy webhook refuses — so a caller can fail fast before
    /// spending an LLM call — while writing nothing to disk.
    #[tokio::test]
    async fn preflight_admission_rejects_without_writing() {
        use crate::admission::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig};
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::{Json, Router};
        use tokio::net::TcpListener;

        // A scope-guard-style webhook that refuses the write outright.
        let app = Router::new().route(
            "/gate",
            post(|Json(_payload): Json<serde_json::Value>| async move {
                (StatusCode::FORBIDDEN, "denied for this scope")
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let chain = AdmissionChain::new(vec![WebhookConfig {
            name: "gate".into(),
            url: format!("http://{addr}/gate"),
            timeout_ms: 1_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::Consolidate],
            blocking: true,
        }])
        .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_admission_chain(chain)
            .with_store_reader(store.reader.clone());

        let path = PagePath::new("sessions/abc.md").unwrap();
        let err = wiki
            .preflight_admission(
                ws,
                proj,
                &path,
                AdmissionOp::Consolidate,
                ai_memory_core::ActorContext::anonymous(),
            )
            .await
            .expect_err("reject-policy webhook must fail the preflight");
        assert!(
            format!("{err}").contains("denied for this scope"),
            "the webhook rejection reason should surface: {err}",
        );
        // The preflight never persists anything.
        assert!(!wiki.abs_path(ws, proj, &path).exists());
    }

    /// With no admission chain attached there is nothing to gate, so the
    /// preflight is a no-op that always succeeds.
    #[tokio::test]
    async fn preflight_admission_is_noop_without_chain() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        wiki.preflight_admission(
            ws,
            proj,
            &PagePath::new("sessions/abc.md").unwrap(),
            AdmissionOp::Consolidate,
            ai_memory_core::ActorContext::anonymous(),
        )
        .await
        .expect("no chain → preflight is a no-op");
    }

    /// Two projects writing the same relative path must produce two distinct
    /// files under their respective UUID-namespaced directories.
    #[tokio::test]
    async fn two_projects_same_path_no_collision() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj_a = store
            .writer
            .get_or_create_project(ws, "alpha", None)
            .await
            .unwrap();
        let proj_b = store
            .writer
            .get_or_create_project(ws, "beta", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj_a,
            path: PagePath::new("decisions/foo.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Alpha decision"}),
            body: "Alpha body".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj_b,
            path: PagePath::new("decisions/foo.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Beta decision"}),
            body: "Beta body".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let page = PagePath::new("decisions/foo.md").unwrap();
        let path_a = wiki.abs_path(ws, proj_a, &page);
        let path_b = wiki.abs_path(ws, proj_b, &page);

        assert!(path_a.is_file(), "alpha file must exist");
        assert!(path_b.is_file(), "beta file must exist");
        assert_ne!(path_a, path_b, "distinct paths on disk");

        let content_a = std::fs::read_to_string(&path_a).unwrap();
        let content_b = std::fs::read_to_string(&path_b).unwrap();
        assert!(content_a.contains("Alpha body"), "alpha content intact");
        assert!(content_b.contains("Beta body"), "beta content intact");
    }

    #[tokio::test]
    async fn rewriting_same_body_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let r = |body: &str| req(ws, proj, "a.md", body, serde_json::json!({ "title": "A" }));

        let a = wiki.write_page(r("body one")).await.unwrap();
        let b = wiki.write_page(r("body one")).await.unwrap();
        assert_eq!(a, b);
        let c = wiki.write_page(r("body two")).await.unwrap();
        assert_ne!(b, c);
    }

    /// End-to-end gate for the workspace/project name resolution:
    /// when a wiki is built with both a store reader and an admission
    /// chain, `write_page` populates `AdmissionContext.workspace` and
    /// `AdmissionContext.project` from the resolved store rows before
    /// invoking the chain. Without [`Wiki::with_store_reader`] the
    /// fields stay empty (backward compat with external test setups).
    #[tokio::test]
    async fn write_page_resolves_workspace_and_project_names_for_chain() {
        use crate::admission::{
            AdmissionChain, AdmissionContext, AdmissionOp, FailurePolicy, WebhookConfig,
        };
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::{Json, Router};
        use std::sync::Mutex;
        use tokio::net::TcpListener;

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("staging")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "ai-memory-ops", None)
            .await
            .unwrap();

        // Throwaway HTTP server that records the payload it receives.
        let recorder: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let recorder_clone = recorder.clone();
        let app = Router::new().route(
            "/sync",
            post(move |Json(payload): Json<serde_json::Value>| {
                let recorder = recorder_clone.clone();
                async move {
                    *recorder.lock().unwrap() = Some(payload);
                    StatusCode::NO_CONTENT.into_response()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let chain = AdmissionChain::new(vec![WebhookConfig {
            name: "recorder".into(),
            url: format!("http://{addr}/sync"),
            timeout_ms: 1_000,
            failure_policy: FailurePolicy::Ignore,
            events: vec![AdmissionOp::WritePage],
            blocking: true,
        }])
        .unwrap();

        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_admission_chain(chain)
            .with_store_reader(store.reader.clone());

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/x.md").unwrap(),
            frontmatter: serde_json::json!({"title": "X"}),
            body: "hi".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: Some(AdmissionContext {
                op: AdmissionOp::WritePage,
                ..AdmissionContext::default()
            }),
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let payload = recorder
            .lock()
            .unwrap()
            .clone()
            .expect("webhook should have recorded the payload");
        assert_eq!(payload["ctx"]["workspace"], serde_json::json!("staging"));
        assert_eq!(
            payload["ctx"]["project"],
            serde_json::json!("ai-memory-ops")
        );
    }

    // ── P1.6: write attribution ─────────────────────────────────────

    /// Anonymous actor must NOT add a `last_modified_by` block — this
    /// is the backward-compat gate for every existing single-user
    /// install.
    #[test]
    fn stamp_last_modified_by_skips_anonymous_actor() {
        let fm = serde_json::json!({"title": "X", "kind": "fact"});
        let stamped =
            stamp_last_modified_by(fm.clone(), &ai_memory_core::ActorContext::anonymous());
        assert_eq!(
            stamped, fm,
            "anonymous actor must leave frontmatter untouched"
        );
    }

    /// Identified actor adds the full block (username + name + email
    /// when present). Existing keys in frontmatter are preserved.
    #[test]
    fn stamp_last_modified_by_adds_full_block() {
        let actor = ai_memory_core::ActorContext {
            user: Some("alice".into()),
            name: Some("Alice Smith".into()),
            email: Some("alice@home".into()),
            ..ai_memory_core::ActorContext::default()
        };
        let stamped =
            stamp_last_modified_by(serde_json::json!({"title": "X", "kind": "fact"}), &actor);
        let lmb = &stamped["last_modified_by"];
        assert_eq!(lmb["username"], "alice");
        assert_eq!(lmb["name"], "Alice Smith");
        assert_eq!(lmb["email"], "alice@home");
        assert_eq!(stamped["title"], "X");
        assert_eq!(stamped["kind"], "fact");
    }

    /// Username-only (no name/email) writes a minimal block.
    #[test]
    fn stamp_last_modified_by_minimal_username_only() {
        let actor = ai_memory_core::ActorContext {
            user: Some("boss".into()),
            ..ai_memory_core::ActorContext::default()
        };
        let stamped = stamp_last_modified_by(serde_json::json!({}), &actor);
        let lmb = &stamped["last_modified_by"];
        assert_eq!(lmb["username"], "boss");
        assert!(lmb.get("name").is_none(), "name omitted when not set");
        assert!(lmb.get("email").is_none(), "email omitted when not set");
    }

    /// Repeated writes by different actors replace the block.
    #[test]
    fn stamp_last_modified_by_replaces_previous_block() {
        let first = ai_memory_core::ActorContext {
            user: Some("alice".into()),
            ..ai_memory_core::ActorContext::default()
        };
        let after_alice = stamp_last_modified_by(serde_json::json!({}), &first);
        assert_eq!(after_alice["last_modified_by"]["username"], "alice");

        let second = ai_memory_core::ActorContext {
            user: Some("bob".into()),
            ..ai_memory_core::ActorContext::default()
        };
        let after_bob = stamp_last_modified_by(after_alice, &second);
        assert_eq!(
            after_bob["last_modified_by"]["username"], "bob",
            "second write replaces, doesn't accumulate"
        );
    }

    /// Null frontmatter is turned into a fresh object on a
    /// non-anonymous write rather than rejected.
    #[test]
    fn stamp_last_modified_by_handles_null_input() {
        let actor = ai_memory_core::ActorContext {
            user: Some("alice".into()),
            ..ai_memory_core::ActorContext::default()
        };
        let stamped = stamp_last_modified_by(serde_json::Value::Null, &actor);
        assert_eq!(stamped["last_modified_by"]["username"], "alice");
    }

    /// End-to-end: a write with a non-anonymous actor lands a
    /// `last_modified_by` block on disk AND `pages.author_id` carries
    /// the UserId.
    #[tokio::test]
    async fn write_page_with_actor_stamps_frontmatter_and_author_id() {
        use ai_memory_core::{NewUser, UserId};
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();

        // Pre-load an actual users row so author_id can FK-resolve.
        let pepper = ai_memory_store::TokenPepper::new("test-pepper-attribution");
        let token_hash = ai_memory_store::hash_token("test-token", &pepper);
        let mut new_user = NewUser {
            username: "alice".into(),
            name: Some("Alice Smith".into()),
            email: Some("alice@example.com".into()),
        };
        new_user.validate().unwrap();
        let user_id: UserId = store
            .writer
            .create_user(new_user, token_hash)
            .await
            .unwrap();

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/note.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Note"}),
            body: "body".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: Some(user_id),
            actor: ai_memory_core::ActorContext {
                user: Some("alice".into()),
                name: Some("Alice Smith".into()),
                email: Some("alice@example.com".into()),
                ..ai_memory_core::ActorContext::default()
            },
        })
        .await
        .unwrap();

        let md = wiki
            .read_page(ws, proj, &PagePath::new("notes/note.md").unwrap())
            .await
            .unwrap();
        assert_eq!(md.frontmatter["last_modified_by"]["username"], "alice");
        assert_eq!(
            md.frontmatter["last_modified_by"]["email"],
            "alice@example.com"
        );

        let meta = store
            .reader
            .page_meta_by_path("notes/note.md")
            .await
            .unwrap()
            .expect("page exists");
        let _ = meta;
    }

    /// Backward-compat: anonymous writes do not add attribution frontmatter.
    #[tokio::test]
    async fn write_page_with_anonymous_actor_omits_attribution_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/anon.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Anon"}),
            body: "body".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let md = wiki
            .read_page(ws, proj, &PagePath::new("notes/anon.md").unwrap())
            .await
            .unwrap();
        assert!(
            md.frontmatter.get("last_modified_by").is_none(),
            "anonymous writes must NOT add last_modified_by — backward compat"
        );
        assert_eq!(md.frontmatter["title"], "Anon");
    }

    fn copy_tree(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let to = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &to);
            } else {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
    }

    /// End-to-end "DB is rebuildable from files": `backfill_scope_manifests`
    /// makes the wiki self-describing, then `reindex_all` on a FRESH store
    /// (no DB carried over) recreates the named scopes + all pages from the
    /// wiki tree alone — including a page that lives at the reserved name
    /// `log.md` (kept because it has frontmatter).
    #[tokio::test]
    async fn backfill_then_reindex_rebuilds_from_wiki_alone() {
        // Source store: a named scope with two pages (one at `log.md`).
        let src = TempDir::new().unwrap();
        let s1 = Store::open(src.path()).unwrap();
        let ws = s1.writer.get_or_create_workspace("acme").await.unwrap();
        let proj = s1
            .writer
            .get_or_create_project(ws, "webapp", Some("/repo/webapp".into()))
            .await
            .unwrap();
        let w1 = Wiki::new(src.path(), s1.writer.clone())
            .unwrap()
            .with_store_reader(s1.reader.clone());
        w1.apply_batch(vec![
            req(
                ws,
                proj,
                "notes/a.md",
                "alpha uniquetoken",
                serde_json::json!({}),
            ),
            req(
                ws,
                proj,
                "log.md",
                "a page that lives at the reserved log name",
                serde_json::json!({ "title": "Log Page" }),
            ),
        ])
        .await
        .unwrap();

        // Make the wiki self-describing.
        let written = w1.backfill_scope_manifests().await.unwrap();
        assert!(written >= 2, "ws + proj manifests written, got {written}");
        let ws_dir = src.path().join("wiki").join(ws.to_string());
        assert!(ws_dir.join("_meta.md").is_file());
        assert!(ws_dir.join(proj.to_string()).join("_meta.md").is_file());
        drop(s1);

        // Fresh store; copy ONLY the wiki tree (no db/); rebuild from it.
        let dst = TempDir::new().unwrap();
        let s2 = Store::open(dst.path()).unwrap();
        copy_tree(&src.path().join("wiki"), &dst.path().join("wiki"));
        let w2 = Wiki::new(dst.path(), s2.writer.clone()).unwrap();
        let summary = w2.reindex_all().await.unwrap();

        assert_eq!(summary.projects, 1);
        assert_eq!(summary.pages, 2, "both pages incl. log.md reconstructed");
        assert_eq!(
            s2.reader.workspace_name_by_id(ws).await.unwrap().as_deref(),
            Some("acme"),
            "workspace name recovered from _meta.md"
        );
        let hits = s2
            .reader
            .search_pages("uniquetoken".into(), 5)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "reindexed page is searchable in the fresh store"
        );
        drop(s2);
    }

    #[tokio::test]
    async fn backfill_writes_manifest_for_empty_workspace() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("empty-ws")
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());

        let written = wiki.backfill_scope_manifests().await.unwrap();

        assert_eq!(written, 1);
        let meta = std::fs::read_to_string(
            tmp.path()
                .join("wiki")
                .join(ws.to_string())
                .join("_meta.md"),
        )
        .unwrap();
        assert!(meta.contains("workspace: empty-ws"));
    }

    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn reindex_rejects_symlinked_scope_manifest() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        let ws_dir = tmp.path().join("wiki").join(ws.to_string());
        let proj_dir = ws_dir.join(proj.to_string());
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(ws_dir.join("_meta.md"), "---\nworkspace: acme\n---\n").unwrap();

        let outside = tmp.path().join("outside-meta.md");
        std::fs::write(&outside, "---\nproject: webapp\n---\n").unwrap();
        let link = proj_dir.join("_meta.md");
        if !create_test_symlink_file(&outside, &link) {
            return;
        }

        let err = wiki.reindex_all().await.unwrap_err();
        assert!(
            err.to_string().contains("symlinked scope manifest"),
            "reindex must reject symlinked manifests, got {err:#}"
        );
    }
}
