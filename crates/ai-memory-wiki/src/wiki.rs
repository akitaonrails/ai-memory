//! [`Wiki`] — the only correct write path for the markdown source-of-truth.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ai_memory_core::{
    ActorContext, AutoImproveProposalId, NewPage, PageId, PagePath, ProjectId, Sanitizer,
    SessionId, Tier, UserId, WorkspaceId,
};
use ai_memory_llm::Embedder;
use ai_memory_store::{
    ApproveAutoImproveProposal, ApproveAutoImproveProposalResult, AutoImproveProposalDetail,
    ConditionalUpsert, FailAutoImproveProposal, MoveSessionSummary, MoveSummary, PagePrecondition,
    PagesMode, PurgeSessionSummary, ReaderPool, ScopeRow, WriterHandle, artifact_path_for,
    f32_vec_to_bytes,
};
use tokio::sync::RwLock;

use crate::admission::{AdmissionChain, AdmissionContext, AdmissionOp};
use crate::confinement::{
    Prepare, project_path, project_root as confined_project_root, tree_path, workspace_path,
};
use crate::error::{WikiError, WikiResult};
use crate::git::{Checkpoint, GitAdapter};
use crate::markdown::{Markdown, derive_title, emit, parse};
use crate::watcher::is_pending_path;

/// Store deletion and best-effort file cleanup from [`Wiki::purge_session`].
#[derive(Debug, Clone)]
pub struct PurgeSessionOutcome {
    /// Committed database deletion counts and paths.
    pub summary: PurgeSessionSummary,
    /// Paths successfully removed from disk.
    pub files_deleted: Vec<PagePath>,
    /// Paths whose file cleanup failed after the database committed.
    pub files_failed: Vec<PagePath>,
}

/// Summary of a [`Wiki::reindex_all`] run.
#[derive(Debug, Default, Clone)]
pub struct ReindexSummary {
    /// Workspaces recreated from `_meta.md`.
    pub workspaces: usize,
    /// Projects recreated from `_meta.md`.
    pub projects: usize,
    /// Pages reindexed from the wiki tree.
    pub pages: usize,
    /// Project directories skipped because their scope was purged and
    /// tombstoned (#607) — a purge whose on-disk removal did not complete.
    /// Their pages are deliberately not resurrected.
    pub skipped_purged: usize,
    /// Session pages skipped because the session was purged and tombstoned
    /// (#701) — the same case one level down, for a purge whose page-file
    /// removal did not complete.
    pub skipped_purged_sessions: usize,
    /// Pages skipped because another page in the same scope differs from them
    /// only by case or Unicode normalization. A wiki authored on a
    /// case-sensitive filesystem can hold such a pair; indexing both is what
    /// the write guard now refuses, and one bad pair must not fail the whole
    /// rebuild.
    pub skipped_collisions: usize,
}

/// The session a page belongs to, for paths that name one.
///
/// Pure path shape, no I/O: only `sessions/<id>.md` can be resurrected by a
/// purge whose file cleanup failed, so this is what decides whether the
/// tombstone set is worth consulting — or, on the single-event path, whether
/// it is worth loading at all.
pub(crate) fn session_id_for_page(path: &PagePath) -> Option<SessionId> {
    path.as_str()
        .strip_prefix("sessions/")
        .and_then(|rest| rest.strip_suffix(".md"))
        .and_then(|id| id.parse::<SessionId>().ok())
}

/// Whether `path` is the page of a session this scope has tombstoned.
///
/// For callers that already hold the scope's set. The empty-set and
/// path-shape checks come first so a sweep over a project with no purges
/// never parses an id.
pub(crate) fn is_purged_session_page(path: &PagePath, purged: &HashSet<SessionId>) -> bool {
    if purged.is_empty() {
        return false;
    }
    session_id_for_page(path).is_some_and(|id| purged.contains(&id))
}

enum PageStoreRemoval {
    Delete {
        author_id: Option<UserId>,
        expected_latest_id: Option<PageId>,
    },
    Decay {
        expected_latest_id: PageId,
    },
}

/// Body + optional L0 abstract text a caller of `reindex_page_locked` should
/// embed once it is safe to do so (i.e. after releasing whatever lock it
/// took to call in). Captured before `upsert_page` moves `md.body` /
/// `md.frontmatter`, and only constructed when the reindex actually produced
/// a new page version worth embedding.
struct PendingEmbed {
    body: String,
    abstract_text: Option<String>,
}

/// What [`Wiki::move_session_page`] did with the on-disk
/// `sessions/<session_id>.md` file of the moved session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPageFile {
    /// Renamed from the source project directory into the destination one
    /// ([`PagesMode::Move`]).
    Moved,
    /// Removed from the source project directory ([`PagesMode::Regenerate`]),
    /// so the watcher cannot re-index it as a live page after its rows were
    /// retired; the next consolidation writes a fresh page in the destination.
    Removed,
    /// No file existed in the source project directory.
    Absent,
}

/// Result of [`Wiki::move_session_page`]: the store summary plus what
/// happened to the page file.
#[derive(Debug, Clone)]
pub struct MoveSessionOutcome {
    /// Row counts and scopes as reported by the store re-stamp.
    pub summary: MoveSessionSummary,
    /// Disposition of the on-disk session page file.
    pub file: SessionPageFile,
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
    /// Per-page write serialization (#607, item 3). Two concurrent writes to
    /// the *same* `(ws, proj, path)` would otherwise interleave their
    /// file-rename and DB-upsert under the shared `mutation_lock` read guard,
    /// transiently leaving on-disk markdown disagreeing with the DB `latest`
    /// row. Each path gets its own async mutex, so same-path writers serialize
    /// while different paths still run concurrently. The map is GC'd
    /// opportunistically — entries no writer currently holds are dropped on the
    /// next acquisition — so it stays bounded to currently-contended paths.
    /// Reindexes share it from disk read through SQL upsert.
    page_locks: Arc<std::sync::Mutex<HashMap<PageKey, Arc<tokio::sync::Mutex<()>>>>>,
    #[cfg(test)]
    reindex_read_barrier: Option<Arc<tokio::sync::Barrier>>,
    /// Scopes this process has already materialized `_meta.md` manifests for.
    /// Keeps [`Wiki::ensure_scope_manifests`] to one hash lookup per page
    /// write after the scope's first — the store query and the two manifest
    /// reads happen once per scope per process, never per page.
    manifested_scopes: Arc<Mutex<HashSet<(WorkspaceId, ProjectId)>>>,
    /// `[maintenance] reconcile_tombstones_deleted_pages` (#929), read once by
    /// `Config::load` and threaded here via [`Wiki::with_reconcile_tombstones_deleted_pages`].
    /// OFF by default: with this `false`, the watcher's reconcile pass never
    /// tombstones a page whose file vanished — deletions still require
    /// `ai-memory delete-page`, unchanged from before this feature existed.
    reconcile_tombstones_deleted_pages: bool,
}

/// Key uniquely identifying a page for per-path write serialization.
type PageKey = (WorkspaceId, ProjectId, PagePath);

impl Wiki {
    /// Construct a wiki handle rooted at `<data_dir>/wiki/`. Creates the
    /// directory if absent and initialises a git repo inside it.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] if the wiki root or git repo cannot be
    /// created.
    pub fn new(data_dir: &Path, writer: WriterHandle) -> WikiResult<Self> {
        let root = data_dir.join("wiki");
        crate::confinement::initialize_root(&root)?;
        let git = GitAdapter::open_or_init(&root)?;
        Ok(Self {
            root,
            writer,
            git,
            embedder: None,
            sanitizer: Sanitizer::builtin(),
            admission_chain: None,
            store_reader: None,
            mutation_lock: Arc::new(RwLock::new(())),
            page_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            #[cfg(test)]
            reindex_read_barrier: None,
            manifested_scopes: Arc::new(Mutex::new(HashSet::new())),
            reconcile_tombstones_deleted_pages: false,
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
    /// The reader is also what makes a scope self-describing: the names a
    /// `_meta.md` manifest carries are resolved through it, so without one
    /// neither [`Self::backfill_scope_manifests`] nor the manifest written
    /// with a scope's first page can run, and both become no-ops. For the
    /// admission chain specifically the reader is still only consulted when
    /// a chain is configured and would actually fire.
    #[must_use]
    pub fn with_store_reader(mut self, reader: ReaderPool) -> Self {
        self.store_reader = Some(reader);
        self
    }

    /// Opt into the watcher reconcile pass's tombstone-on-missing-file safety
    /// net (`[maintenance] reconcile_tombstones_deleted_pages`, #929). `false`
    /// (the default from [`Wiki::new`]) leaves reconcile's behavior exactly
    /// as it was before this feature: deletions require `ai-memory
    /// delete-page`.
    #[must_use]
    pub fn with_reconcile_tombstones_deleted_pages(mut self, enabled: bool) -> Self {
        self.reconcile_tombstones_deleted_pages = enabled;
        self
    }

    /// Whether the watcher's reconcile pass may tombstone a page whose file
    /// disappeared. See [`Wiki::with_reconcile_tombstones_deleted_pages`].
    #[must_use]
    pub(crate) fn reconcile_tombstones_deleted_pages(&self) -> bool {
        self.reconcile_tombstones_deleted_pages
    }

    /// Replace the default built-in-only sanitizer with one carrying
    /// the operator's `[sanitize].extra_patterns` + `allowlist`.
    #[must_use]
    pub fn with_sanitizer(mut self, sanitizer: Sanitizer) -> Self {
        self.sanitizer = sanitizer;
        self
    }

    /// Borrow the configured sanitizer, so components holding a `Wiki`
    /// (e.g. the consolidator scrubbing project-provided prompt
    /// preferences) reuse the operator's patterns instead of
    /// constructing a second, built-in-only instance.
    #[must_use]
    pub fn sanitizer(&self) -> &Sanitizer {
        &self.sanitizer
    }

    /// Attach an embedder. When set, `write_page` computes + stores an
    /// embedding for the new version synchronously. `apply_batch` keeps
    /// the SQL/file fan-out atomic and leaves vector completeness to
    /// admin or scheduled embedding backfill. Without an embedder,
    /// vector search is skipped and `ReaderPool::hybrid_search` uses
    /// FTS5 + entity + graph expansion.
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
        self.inspect_wiki_tree()?;
        self.git.commit_all(message)
    }

    /// Append `bytes` to `<project root>/<file_name>` (the hook ledger)
    /// and report the write.
    ///
    /// # Errors
    /// Propagates the filesystem error.
    pub fn append_under_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        file_name: &str,
        bytes: &[u8],
    ) -> std::io::Result<PathBuf> {
        self.append_under_project_checked(workspace_id, project_id, file_name, bytes)
            .map_err(crate::error::into_io_error)
    }

    pub(crate) fn append_under_project_checked(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        file_name: &str,
        bytes: &[u8],
    ) -> WikiResult<PathBuf> {
        let path = project_path(
            &self.root,
            workspace_id,
            project_id,
            Path::new(file_name),
            Prepare::Parents,
        )?;
        self.git.append_checked(&path, bytes)?;
        Ok(path)
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
        if self.git.commit_count_checked()? == 0 {
            self.commit_all("upgrade baseline: existing wiki tree before recovery checkpoints")
        } else {
            Ok(None)
        }
    }

    /// Path of the wiki root on disk.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The data directory this wiki lives under (`<data_dir>/wiki` is
    /// the root, so this is its parent). Used by the web layer to read
    /// the pre-migration backup receipt.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        self.root.parent().unwrap_or(&self.root)
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
        self.inspect_project_tree(from_workspace, project_id)?;
        self.inspect_project_tree(to_workspace, project_id)?;
        let _guard = self.mutation_lock.write().await;
        self.inspect_project_tree(from_workspace, project_id)?;
        self.inspect_project_tree(to_workspace, project_id)?;
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

        let src = self.confined_project_root(from_workspace, project_id, Prepare::Inspect)?;
        let dst = self.confined_project_root(to_workspace, project_id, Prepare::Parents)?;

        if dst.exists() {
            return Err(crate::WikiError::DestinationExists(
                dst.display().to_string(),
            ));
        }

        let renamed = if src.exists() {
            self.git.rename_checked(&src, &dst)?;
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
            Ok(summary) => {
                if let (Some(chain), Some(ctx)) = (&self.admission_chain, &resolved_ctx) {
                    chain.dispatch_async(None, &serde_json::Value::Null, "", ctx);
                }
                Ok(summary)
            }
            Err(e) => {
                if renamed && let Err(rollback_err) = self.git.rename_checked(&dst, &src) {
                    return Err(crate::WikiError::Io(std::io::Error::other(format!(
                        "INCONSISTENT STATE: files moved but DB re-stamp failed ({e}) and dir rename-back also failed ({rollback_err}); manually move {} -> {} or finish the re-stamp",
                        dst.display(),
                        src.display()
                    ))));
                }
                Err(e.into())
            }
        }
    }

    /// Move one session to another project: relocate its `sessions/<id>.md`
    /// file on disk and re-stamp its store rows in one transaction
    /// ([`WriterHandle::move_session`]).
    ///
    /// Same critical section and ordering as [`Self::move_project_workspace`]:
    /// the exclusive mutation guard is held across the admission chain, the
    /// file step and the store step; disk goes first and SQL last, so the DB
    /// is never ahead of disk. Under [`PagesMode::Move`] the file is renamed
    /// into the destination project directory (refused when a file already
    /// sits there). Under [`PagesMode::Regenerate`] the file is first parked
    /// under the watcher-ignored `.ai-memory-tmp.` prefix and deleted once the
    /// rows are retired: left in place, the next reconciliation pass would
    /// re-index it as a fresh latest page in the source scope. A store failure
    /// puts the file back where it was in both modes. When `from == to` (a
    /// re-home of a session already rooted in the destination) there is no
    /// file to relocate: the file step is skipped and only the store sweep
    /// runs.
    ///
    /// # Errors
    /// Returns [`WikiError::DestinationPageExists`] when the destination
    /// already holds a page file at that path, [`WikiError::Store`] for store
    /// refusals (`NotFound`, `PagePathTaken`), or an I/O error when the file
    /// step or its rollback fails.
    pub async fn move_session_page(
        &self,
        session_id: SessionId,
        from: (WorkspaceId, ProjectId),
        to: (WorkspaceId, ProjectId),
        pages: PagesMode,
        author_id: Option<UserId>,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<MoveSessionOutcome> {
        self.inspect_project_tree(from.0, from.1)?;
        self.inspect_project_tree(to.0, to.1)?;
        let _guard = self.mutation_lock.write().await;
        self.inspect_project_tree(from.0, from.1)?;
        self.inspect_project_tree(to.0, to.1)?;
        let resolved_ctx = if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.op = AdmissionOp::MoveSession;
            self.resolve_admission_names(from.0, from.1, &mut ctx).await;
            chain.notify(None, &ctx).await?;
            Some(ctx)
        } else {
            None
        };

        let file_name = format!("{session_id}.md");
        let src = project_path(
            &self.root,
            from.0,
            from.1,
            &PathBuf::from("sessions").join(&file_name),
            Prepare::Inspect,
        )?;
        let parked = if from != to && src.is_file() {
            let target = match pages {
                PagesMode::Move => {
                    let dst = project_path(
                        &self.root,
                        to.0,
                        to.1,
                        &PathBuf::from("sessions").join(&file_name),
                        Prepare::Parents,
                    )?;
                    if dst.exists() {
                        return Err(WikiError::DestinationPageExists(dst.display().to_string()));
                    }
                    dst
                }
                // Same directory, watcher-ignored name: invisible to
                // reindex/reconcile, trivially renamed back on failure.
                PagesMode::Regenerate => {
                    src.with_file_name(format!(".ai-memory-tmp.move-session.{file_name}"))
                }
            };
            self.git.rename_checked(&src, &target)?;
            Some(target)
        } else {
            None
        };

        match self
            .writer
            .move_session(session_id, to.0, to.1, pages, author_id, true)
            .await
        {
            Ok(summary) => {
                let file = match (&parked, pages) {
                    (None, _) => SessionPageFile::Absent,
                    (Some(_), PagesMode::Move) => SessionPageFile::Moved,
                    (Some(tmp), PagesMode::Regenerate) => {
                        // The rows are retired; a leftover temp file is
                        // ignored by the watcher, so this is best-effort.
                        if let Err(e) = self.git.remove_file_checked(tmp) {
                            tracing::warn!(
                                error = %e,
                                path = %tmp.display(),
                                "move-session: could not remove parked session page file"
                            );
                        }
                        SessionPageFile::Removed
                    }
                };
                if let (Some(chain), Some(ctx)) = (&self.admission_chain, &resolved_ctx) {
                    chain.dispatch_async(None, &serde_json::Value::Null, "", ctx);
                }
                Ok(MoveSessionOutcome { summary, file })
            }
            Err(e) => {
                if let Some(target) = parked
                    && let Err(rollback_err) = self.git.rename_checked(&target, &src)
                {
                    return Err(WikiError::Io(std::io::Error::other(format!(
                        "INCONSISTENT STATE: session page file moved but DB re-stamp failed ({e}) and moving it back also failed ({rollback_err}); manually move {} -> {}",
                        target.display(),
                        src.display()
                    ))));
                }
                Err(e.into())
            }
        }
    }

    /// Ensure the store rows for a scope exist **and** that the scope is
    /// self-describing on disk. Every wiki write path funnels through here,
    /// which is what guarantees a project directory never outlives its
    /// `_meta.md`: the directory comes into existence with the first page
    /// written into it, and this runs first.
    pub(crate) async fn ensure_project_workspace(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<()> {
        self.ensure_project_scope_rows(workspace_id, project_id)
            .await?;
        self.ensure_scope_manifests(workspace_id, project_id).await;
        Ok(())
    }

    /// Ensure the store rows for a scope exist, without touching the wiki
    /// tree. The watcher's two orphan guards want this — the reconcile pass
    /// and the directory-event path. Both run before `reindex_page` takes
    /// the mutation guard, so creating a file there could drop a manifest
    /// into a directory a concurrent project move is renaming away, and both
    /// only ever see directories that already exist, whose manifests were
    /// written with their first page or by the startup backfill.
    pub(crate) async fn ensure_project_scope_rows(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<()> {
        self.writer
            .ensure_project_workspace(workspace_id, project_id)
            .await?;
        Ok(())
    }

    /// Write the workspace and project `_meta.md` manifests the first time
    /// this process writes into a scope, so a project that first materializes
    /// *while the server is up* is rebuildable immediately instead of only
    /// after the next startup backfill (#643). In that window `reindex` could
    /// not rebuild the scope at all: it walks the directories that exist on
    /// disk, and the directory exists from the first page write onward.
    ///
    /// Best-effort on purpose. A manifest that cannot be written must not
    /// fail the page write that triggered it — the page is the operator's
    /// data, the manifest is derived, and
    /// [`Self::backfill_scope_manifests`] rewrites it on the next start.
    async fn ensure_scope_manifests(&self, workspace_id: WorkspaceId, project_id: ProjectId) {
        let Some(reader) = &self.store_reader else {
            return;
        };
        let key = (workspace_id, project_id);
        if self
            .manifested_scopes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
        {
            return;
        }
        let scope = match reader.scope_row_by_ids(workspace_id, project_id).await {
            Ok(Some(scope)) => scope,
            // No row to describe: the scope was purged or moved between the
            // ensure above and this lookup. The next write re-checks.
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "scope-manifest lookup failed (non-fatal)");
                return;
            }
        };
        let ws_dir = match workspace_path(&self.root, workspace_id, Prepare::Directory) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, "scope-manifest path confinement failed (non-fatal)");
                return;
            }
        };
        let workspace_name = scope.workspace_name.clone();
        let project_fm = match Self::project_scope_frontmatter(scope) {
            Ok(frontmatter) => frontmatter,
            Err(e) => {
                tracing::warn!(error = %e, "scope-manifest identity is invalid (non-fatal)");
                return;
            }
        };
        let written = self
            .write_scope_manifest(&ws_dir, serde_json::json!({ "workspace": workspace_name }))
            .and_then(|_| {
                let project_dir =
                    self.confined_project_root(workspace_id, project_id, Prepare::Directory)?;
                self.write_scope_manifest(&project_dir, project_fm)
            });
        match written {
            Ok(_) => {
                self.manifested_scopes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key);
            }
            Err(e) => tracing::warn!(error = %e, "scope-manifest write failed (non-fatal)"),
        }
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

    /// Read the page at `path` from disk for the given project.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] if the file is missing or unreadable, or
    /// [`WikiError::Yaml`] if the frontmatter block is malformed.
    pub fn read_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> WikiResult<Markdown> {
        let abs = self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let raw = std::fs::read_to_string(&abs)?;
        parse(&raw)
    }

    /// Whether the calling agent wrote `sessions/<session_id>.md` itself,
    /// through `memory_write_page` with that session's id: the page carries
    /// `consolidated_by: agent`. The automatic session-page writers leave such
    /// a page alone, because it was compiled by the model the user was
    /// working with. A missing page is `false`.
    ///
    /// # Errors
    /// Propagates [`Self::read_page`] errors other than a missing file.
    pub fn session_page_written_by_agent(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
    ) -> WikiResult<bool> {
        let path = PagePath::new(format!("sessions/{session_id}.md"))?;
        let frontmatter = match self.read_page(workspace_id, project_id, &path) {
            Ok(md) => md.frontmatter,
            Err(WikiError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(err) => return Err(err),
        };
        Ok(frontmatter
            .get("consolidated_by")
            .and_then(serde_json::Value::as_str)
            == Some("agent"))
    }

    fn confined_project_root(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        prepare: Prepare,
    ) -> WikiResult<PathBuf> {
        confined_project_root(&self.root, workspace_id, project_id, prepare)
    }

    /// Refuse a project tree containing a symbolic link or reparse point.
    ///
    /// # Errors
    /// Returns [`WikiError::Confinement`] for any linked component.
    pub fn validate_project_tree(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<()> {
        self.inspect_project_tree(workspace_id, project_id)
    }

    pub(crate) fn validate_project_root(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<()> {
        self.confined_project_root(workspace_id, project_id, Prepare::Inspect)
            .map(|_| ())
    }

    pub(crate) fn validate_page_path(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> WikiResult<()> {
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)
            .map(|_| ())
    }

    /// Refuse a workspace tree containing a symbolic link or reparse point.
    ///
    /// # Errors
    /// Returns [`WikiError::Confinement`] for any linked component.
    pub fn validate_workspace_tree(&self, workspace_id: WorkspaceId) -> WikiResult<()> {
        self.inspect_workspace_tree(workspace_id)
    }

    fn inspect_wiki_tree(&self) -> WikiResult<()> {
        let git_dir = crate::confinement::inspect_git_directory(&self.root)?;
        crate::confinement::inspect_tree_except(&self.root, &[git_dir.as_path()])
    }

    fn inspect_project_tree(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<()> {
        let root = self.confined_project_root(workspace_id, project_id, Prepare::Inspect)?;
        crate::confinement::inspect_tree_if_present(&root)
    }

    fn inspect_workspace_tree(&self, workspace_id: WorkspaceId) -> WikiResult<()> {
        let root = workspace_path(&self.root, workspace_id, Prepare::Inspect)?;
        crate::confinement::inspect_tree_if_present(&root)
    }

    fn confined_page_path(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        prepare: Prepare,
    ) -> WikiResult<PathBuf> {
        project_path(
            &self.root,
            workspace_id,
            project_id,
            Path::new(path.as_str()),
            prepare,
        )
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
        self.confined_page_path(workspace_id, project_id, &path, Prepare::Inspect)?;
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
        let links = crate::markdown::extract_all_links(&md.frontmatter, &md.body, &path);
        let meta = derive_index_metadata(&path, &md.frontmatter)?;

        let _guard = self.mutation_lock.read().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        let abs = self.confined_page_path(workspace_id, project_id, &path, Prepare::Parents)?;
        self.git.write_atomic(&abs, raw.as_bytes())?;
        let id = self
            .writer
            .upsert_page(NewPage {
                workspace_id,
                project_id,
                path,
                title,
                body: md.body,
                tier: meta.tier,
                frontmatter_json: md.frontmatter,
                pinned: meta.pinned,
                links,
                author_id: None,
                expires_at: meta.expires_at,
                entities: meta.entities,
                evidence: Vec::new(),
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
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let _guard = self.mutation_lock.read().await;
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        self.remove_page_locked(
            workspace_id,
            project_id,
            path,
            admission_ctx,
            PageStoreRemoval::Delete {
                author_id,
                expected_latest_id: None,
            },
        )
        .await
        .map(|_| ())
    }

    /// Delete a page only when `expected_latest_id` is still its latest
    /// indexed version. Used by retention so a stale expiry candidate cannot
    /// delete a page that was refreshed after the sweep selected it.
    ///
    /// The exclusive mutation guard keeps normal wiki writes out between the
    /// pre-admission comparison and file quarantine; the writer repeats the
    /// comparison in the delete transaction as the final authority check.
    ///
    /// # Errors
    /// Returns [`WikiError`] when the store reader is unavailable, or on a
    /// filesystem, store, or rejecting-webhook error.
    pub async fn delete_page_if_latest(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        expected_latest_id: PageId,
        admission_ctx: Option<AdmissionContext>,
        author_id: Option<UserId>,
    ) -> WikiResult<bool> {
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let _guard = self.mutation_lock.write().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        let reader = self.store_reader.as_ref().ok_or_else(|| {
            ai_memory_wiki_error("conditional page delete requires a store reader")
        })?;
        let current = reader
            .latest_page_id_by_ids(workspace_id, project_id, path.as_str().to_string())
            .await?;
        if current != Some(expected_latest_id) {
            return Ok(false);
        }
        self.remove_page_locked(
            workspace_id,
            project_id,
            path,
            admission_ctx,
            PageStoreRemoval::Delete {
                author_id,
                expected_latest_id: Some(expected_latest_id),
            },
        )
        .await
    }

    /// Remove the authoritative file and tombstone the expected latest page.
    /// A stale candidate leaves both disk and store
    /// untouched.
    ///
    /// # Errors
    /// Returns [`WikiError`] when the store reader is unavailable, or on a
    /// filesystem, store, or rejecting-webhook error.
    pub async fn evict_page_if_latest(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        expected_latest_id: PageId,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<bool> {
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let _guard = self.mutation_lock.write().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        let reader = self.store_reader.as_ref().ok_or_else(|| {
            ai_memory_wiki_error("conditional page eviction requires a store reader")
        })?;
        let current = reader
            .latest_page_id_by_ids(workspace_id, project_id, path.as_str().to_string())
            .await?;
        if current != Some(expected_latest_id) {
            return Ok(false);
        }
        self.remove_page_locked(
            workspace_id,
            project_id,
            path,
            admission_ctx,
            PageStoreRemoval::Decay { expected_latest_id },
        )
        .await
    }

    /// `(PageId, PagePath)` of every `is_latest = 1` page in a scope, for the
    /// watcher's reconcile-delete safety net (#929) to snapshot before it
    /// walks the wiki tree.
    ///
    /// # Errors
    /// Returns [`WikiError`] when the store reader is unavailable, or on a
    /// store error.
    pub(crate) async fn latest_page_ids(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<Vec<(PageId, PagePath)>> {
        let reader = self.store_reader.as_ref().ok_or_else(|| {
            ai_memory_wiki_error("reconcile-delete candidate snapshot requires a store reader")
        })?;
        reader
            .latest_page_ids(workspace_id, project_id)
            .await
            .map_err(Into::into)
    }

    /// Tombstone the expected latest page after the watcher's reconcile pass
    /// found its file missing on two consecutive passes (opt-in, #929;
    /// `[maintenance] reconcile_tombstones_deleted_pages`).
    ///
    /// Deliberately does NOT go through [`Wiki::remove_page_locked`]: unlike
    /// `evict_page_if_latest` (decay) or `delete_page_if_latest` (explicit
    /// delete), this is a background safety net reacting to a file that is
    /// already gone, not a user-initiated action, so it never runs the
    /// BLOCKING admission gate (`AdmissionChain::notify`/`authorize`) —
    /// nothing gets to refuse it — and it never touches the filesystem —
    /// there is nothing on disk to quarantine or remove. It DOES
    /// fire-and-forget the chain's non-blocking observer/mirror webhooks on
    /// success (#929 review, S6), same as `remove_page_locked`'s post-write
    /// `dispatch_async`, so a mirror learns about the tombstone instead of
    /// silently diverging from the source of truth.
    ///
    /// Re-checks both the store's latest-id and the file's actual absence
    /// while holding the exclusive mutation guard, immediately before
    /// writing — the final gate design item #6 calls for, closing the window
    /// between the reconcile pass's second "missing" observation and this
    /// call (e.g. a concurrent `write_page` recreating the path).
    ///
    /// # Errors
    /// Returns [`WikiError`] when the store reader is unavailable, or on a
    /// filesystem or store error.
    pub(crate) async fn tombstone_missing_page_if_latest(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        expected_latest_id: PageId,
    ) -> WikiResult<bool> {
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let _guard = self.mutation_lock.write().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        let reader = self
            .store_reader
            .as_ref()
            .ok_or_else(|| ai_memory_wiki_error("reconcile tombstone requires a store reader"))?;
        let current = reader
            .latest_page_id_by_ids(workspace_id, project_id, path.as_str().to_string())
            .await?;
        if current != Some(expected_latest_id) {
            return Ok(false);
        }
        let abs = self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        if abs.try_exists()? {
            // The file came back (e.g. a slow atomic-save pattern beyond the
            // two-pass window, or a deliberate rewrite mid-check). Leave the
            // page alone; the next reconcile tick reindexes it normally.
            return Ok(false);
        }
        let tombstoned = self
            .writer
            .soft_delete_for_reconcile_if_latest(
                workspace_id,
                project_id,
                path.clone(),
                expected_latest_id,
            )
            .await?;
        if tombstoned && let Some(chain) = &self.admission_chain {
            // Non-blocking observers/mirrors only (#929 review, S6) — never
            // the blocking `notify`/`authorize` gate: this is a background
            // safety net, not a user-initiated delete, so nothing gets to
            // refuse it. But a mirror that never hears about the tombstone
            // silently diverges from the source of truth, which is a real
            // correctness issue too; fire-and-forget informs it the same way
            // `remove_page_locked` does for `evict_page_if_latest` and
            // `delete_page_if_latest`, just skipping their blocking half.
            let mut ctx = AdmissionContext {
                op: AdmissionOp::Delete,
                ..Default::default()
            };
            self.resolve_admission_names(workspace_id, project_id, &mut ctx)
                .await;
            chain.dispatch_async(Some(path.as_str()), &serde_json::Value::Null, "", &ctx);
        }
        Ok(tombstoned)
    }

    async fn remove_page_locked(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        admission_ctx: Option<AdmissionContext>,
        removal: PageStoreRemoval,
    ) -> WikiResult<bool> {
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let mut resolved_ctx = None;
        if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.op = AdmissionOp::Delete;
            self.resolve_admission_names(workspace_id, project_id, &mut ctx)
                .await;
            chain.notify(Some(path.as_str()), &ctx).await?;
            resolved_ctx = Some(ctx);
        }
        let abs = self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let quarantined = match quarantine_file(&self.git, &abs) {
            Ok(path) => path,
            Err(WikiError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };

        let delete_result = match removal {
            PageStoreRemoval::Delete {
                author_id,
                expected_latest_id: Some(expected),
            } => {
                self.writer
                    .delete_page_if_latest(
                        workspace_id,
                        project_id,
                        path.clone(),
                        expected,
                        author_id,
                    )
                    .await
            }
            PageStoreRemoval::Delete {
                author_id,
                expected_latest_id: None,
            } => self
                .writer
                .delete_page(workspace_id, project_id, path.clone(), author_id)
                .await
                .map(|()| true),
            PageStoreRemoval::Decay { expected_latest_id } => {
                self.writer
                    .soft_delete_for_decay_if_latest(
                        workspace_id,
                        project_id,
                        path.clone(),
                        expected_latest_id,
                    )
                    .await
            }
        };
        let deleted = match delete_result {
            Ok(deleted) => deleted,
            Err(e) => {
                restore_quarantined_file(&self.git, &quarantined, &abs, path);
                return Err(e.into());
            }
        };
        if !deleted {
            restore_quarantined_file(&self.git, &quarantined, &abs, path);
            return Ok(false);
        }

        if let Some(quarantine) = quarantined {
            self.git.remove_file_checked(&quarantine)?;
        }

        if let (Some(chain), Some(ctx)) = (&self.admission_chain, &resolved_ctx) {
            chain.dispatch_async(Some(path.as_str()), &serde_json::Value::Null, "", ctx);
        }
        Ok(true)
    }

    /// Permanently delete one aged decay tombstone and its ancestry chain.
    ///
    /// An existing Markdown file is always preserved. If the watcher has not
    /// indexed it yet (including a legacy pre-fix eviction or an external
    /// recreation), this method reindexes it under the exclusive mutation
    /// guard before deleting only the old chain rooted at `tombstone_id`.
    /// The writer repeats the observed latest-id check so a concurrent store
    /// mutation fails closed.
    ///
    /// # Errors
    /// Returns [`WikiError`] when the store reader is unavailable, or on a
    /// filesystem, store, or rejecting-webhook error.
    pub async fn hard_delete_decay_tombstone(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
        tombstone_id: PageId,
        cutoff_us: i64,
    ) -> WikiResult<usize> {
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        let _guard = self.mutation_lock.write().await;
        self.ensure_project_workspace(workspace_id, project_id)
            .await?;
        let reader = self
            .store_reader
            .as_ref()
            .ok_or_else(|| ai_memory_wiki_error("decay cleanup requires a store reader"))?;
        let mut current_latest = reader
            .latest_page_id_by_ids(workspace_id, project_id, path.as_str().to_string())
            .await?;
        let abs = self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
        if current_latest.is_none() && abs.try_exists()? {
            // Deliberately drops any `PendingEmbed`: this whole function runs
            // under the exclusive write lock (`_guard` above, held until
            // return), and embedding must never happen while that lock is
            // held (#929) — it can block on the configured provider for an
            // unbounded time, stalling every other wiki write meanwhile. The
            // resurrected page's embedding (if any) is recovered later by
            // `ai-memory embed` / the scheduled backfill, same as any other
            // gap in `page_embeddings`.
            let (id, _pending_embed) = self
                .reindex_page_locked(workspace_id, project_id, path.clone())
                .await?;
            current_latest = Some(id);
        }

        self.writer
            .hard_delete_decayed_page_chain(
                workspace_id,
                project_id,
                path.clone(),
                tombstone_id,
                current_latest,
                cutoff_us,
            )
            .await
            .map_err(Into::into)
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
        self.inspect_project_tree(workspace_id, project_id)?;
        let ctx = self
            .admit_purge_project(workspace_id, project_id, admission_ctx)
            .await?;
        self.remove_project_dir(workspace_id, project_id).await?;
        self.dispatch_purge(ctx.as_ref());
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
        self.inspect_project_tree(workspace_id, project_id)?;
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

    /// Run the admission chain for a single-session purge, before any row is
    /// deleted, so a scope-guard webhook can refuse it while the data is
    /// still intact.
    ///
    /// # Errors
    /// Propagates a webhook rejection as [`WikiError`].
    pub async fn admit_purge_session(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        admission_ctx: Option<AdmissionContext>,
    ) -> WikiResult<Option<AdmissionContext>> {
        self.inspect_project_tree(workspace_id, project_id)?;
        if let Some(chain) = &self.admission_chain {
            let mut ctx = admission_ctx.unwrap_or_default();
            ctx.op = AdmissionOp::PurgeSession;
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
        self.inspect_workspace_tree(workspace_id)?;
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

    /// Ask the admission chain about an operation that has no page and no body
    /// — a handoff lifecycle event.
    ///
    /// Handoffs live outside the wiki tree (they have their own table), so they
    /// never passed through `write_page` and were invisible to admission
    /// webhooks. That left the operations that move prompt-derived text between
    /// operators unauthorizable: a webhook that decides who may touch which
    /// scope could not see them at all.
    ///
    /// Only the webhooks that can refuse the operation are awaited — that is
    /// what the caller has to know before doing destructive work, and a
    /// `reject` policy is the operator asking to be waited for. The observers
    /// are handed back with the resolved context: pass it to
    /// [`Self::notify_operation_observers`] once the operation is durable, or
    /// drop it if the operation was abandoned, so no webhook is told about work
    /// that never happened. Every path that raises one of these ops owes its
    /// webhooks the same three steps in the same order — decide, act, notify —
    /// or the same event reaches a mirror from one caller and not from another.
    ///
    /// Returns `None` when no chain is attached (nothing left to notify).
    ///
    /// # Errors
    /// Returns [`WikiError`] when a reject-policy webhook refuses.
    pub async fn authorize_operation(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        op: AdmissionOp,
        actor: ActorContext,
        skip_webhooks: Vec<String>,
    ) -> WikiResult<Option<AdmissionContext>> {
        let Some(chain) = &self.admission_chain else {
            return Ok(None);
        };
        let ctx = self
            .operation_admission_ctx(workspace_id, project_id, op, actor, skip_webhooks)
            .await;
        chain.authorize(None, &ctx).await?;
        Ok(Some(ctx))
    }

    /// Fire-and-forget the observer webhooks for an operation previously gated
    /// by [`Self::authorize_operation`] and since committed.
    pub fn notify_operation_observers(&self, ctx: &AdmissionContext) {
        if let Some(chain) = &self.admission_chain {
            chain.dispatch_notify_observers(None, ctx);
        }
    }

    async fn operation_admission_ctx(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        op: AdmissionOp,
        actor: ActorContext,
        skip_webhooks: Vec<String>,
    ) -> AdmissionContext {
        let mut ctx = AdmissionContext {
            op,
            actor,
            skip_webhooks,
            ..Default::default()
        };
        self.resolve_admission_names(workspace_id, project_id, &mut ctx)
            .await;
        ctx
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
        self.confined_page_path(workspace_id, project_id, path, Prepare::Inspect)?;
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
        self.inspect_project_tree(workspace_id, project_id)?;
        let root = self.confined_project_root(workspace_id, project_id, Prepare::Inspect)?;
        match self.git.remove_dir_all_checked(&root) {
            Ok(()) => Ok(()),
            Err(WikiError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Remove a workspace's on-disk directory (`<wiki_root>/<ws>`), including
    /// every project under it, without running admission. Best-effort: a
    /// missing dir is not an error.
    ///
    /// # Errors
    /// Returns [`WikiError::Io`] on filesystem errors other than NotFound.
    pub async fn remove_workspace_dir(&self, workspace_id: WorkspaceId) -> WikiResult<()> {
        let _guard = self.mutation_lock.write().await;
        self.inspect_workspace_tree(workspace_id)?;
        let root = workspace_path(&self.root, workspace_id, Prepare::Inspect)?;
        match self.git.remove_dir_all_checked(&root) {
            Ok(()) => Ok(()),
            Err(WikiError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Session ids this scope has tombstoned, as a set for the reindex gate.
    ///
    /// The session-level twin of [`WriterHandle::scope_is_purged`], and read
    /// the same way: once per directory per reindex pass, by the callers that
    /// walk a tree, rather than once per page (#701).
    ///
    /// # Errors
    /// Propagates the store error.
    pub async fn purged_sessions(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<HashSet<SessionId>> {
        Ok(self
            .writer
            .purged_session_ids(workspace_id, project_id)
            .await?
            .into_iter()
            .collect())
    }

    /// Purge a session and its page files under one exclusive mutation guard.
    /// Admission must run before this call. File failures do not roll back the
    /// committed database purge and are returned for partial-failure reporting.
    /// Holding the guard before submitting SQL prevents a watcher reindex,
    /// page write or project move from landing between SQL and file cleanup.
    ///
    /// # Errors
    /// Returns [`WikiError::Store`] without removing files if the database
    /// purge fails.
    pub async fn purge_session(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        author_id: Option<UserId>,
        compaction: ai_memory_store::Compaction,
    ) -> WikiResult<PurgeSessionOutcome> {
        self.inspect_project_tree(workspace_id, project_id)?;
        let _guard = self.mutation_lock.write().await;
        self.inspect_project_tree(workspace_id, project_id)?;
        // Always `Commit`: a caller that only wants a preview (the HTTP
        // handler's `dry_run` branch) goes straight through
        // `WriterHandle::purge_session` instead, bypassing this wrapper
        // entirely, so the file-removal loop below never runs over paths
        // that a preview only *predicts* would be removed.
        let summary = self
            .writer
            .purge_session(
                workspace_id,
                project_id,
                session_id,
                author_id,
                compaction,
                ai_memory_store::PurgeMode::Commit,
            )
            .await?;
        let mut files_deleted = Vec::with_capacity(summary.removed_paths.len());
        let mut files_failed = Vec::new();
        for path in &summary.removed_paths {
            let abs = self.abs_path(workspace_id, project_id, path);
            match self.git.remove_file_checked(&abs) {
                Ok(()) => {
                    sync_parent_best_effort(&abs);
                    files_deleted.push(path.clone());
                }
                Err(WikiError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        operation = "purge-session",
                        path = path.as_str(),
                        error = %error,
                        "session purge failed to remove wiki page file",
                    );
                    files_failed.push(path.clone());
                }
            }
        }
        Ok(PurgeSessionOutcome {
            summary,
            files_deleted,
            files_failed,
        })
    }

    /// Dispatch non-blocking purge webhooks after the caller's purge has
    /// completed its durable DB/filesystem work. The purge kind (project,
    /// session, workspace) travels in `ctx.op`, set by the matching `admit_*`.
    pub fn dispatch_purge(&self, admission_ctx: Option<&AdmissionContext>) {
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
        project_path(
            &self.root,
            workspace_id,
            project_id,
            Path::new(&artifact_path_for(proposal_id)),
            Prepare::Inspect,
        )?;
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
        self.git.write_atomic(&path, content.as_bytes())?;
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
        self.inspect_project_tree(workspace_id, project_id)?;
        let reader = self
            .store_reader
            .as_ref()
            .ok_or_else(|| ai_memory_wiki_error("auto-improve approval requires a store reader"))?;
        let detail = reader
            .auto_improve_proposal_detail(workspace_id, project_id, proposal_id)
            .await?
            .ok_or_else(|| ai_memory_wiki_error("auto-improve proposal not found in scope"))?;

        let path = detail.summary.target_path.clone();
        path.ensure_portable()?;
        self.confined_page_path(workspace_id, project_id, &path, Prepare::Inspect)?;
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
        // Approved pages must land conformant like every other write —
        // this emit skipped the disk conformance seam, leaving files
        // without type/generated that blocked export-okf and, worse,
        // phantom-superseded on the first reindex after a binary upgrade
        // (the row was conformed with the approving binary's version, the
        // file with the reindexing one's). Post-audit finding.
        conform_frontmatter_for_disk(
            &self.abs_path(workspace_id, project_id, &path),
            path.as_str(),
            &mut markdown,
        );
        let title = self.sanitizer.scrub(&detail.summary.title);
        let links =
            crate::markdown::extract_all_links(&markdown.frontmatter, &markdown.body, &path);
        let expires_at = parse_expires_at(&path, &markdown.frontmatter)?;
        let entities = parse_entities(&path, &markdown.frontmatter)?;
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
            expires_at,
            entities,
            evidence: Vec::new(),
        };

        let result = {
            let _guard = self.mutation_lock.write().await;
            self.confined_page_path(workspace_id, project_id, &path, Prepare::Inspect)?;
            self.ensure_project_workspace(workspace_id, project_id)
                .await?;
            let abs = self.confined_page_path(workspace_id, project_id, &path, Prepare::Parents)?;
            let installed =
                replace_file_with_rollback_snapshot(&self.git, &abs, emitted.as_bytes())?;
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
                        &self.git,
                        std::slice::from_ref(&installed),
                        &"proposal conflict",
                    )?;
                    ApproveAutoImproveProposalResult::Conflict
                }
                Err(e) => {
                    rollback_or_inconsistent(&self.git, std::slice::from_ref(&installed), &e)?;
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
        self.confined_page_path(workspace_id, project_id, &path, Prepare::Inspect)?;
        if is_pending_path(&path) {
            return Err(ai_memory_wiki_error(
                "refusing to index pending proposal sidecar",
            ));
        }
        // Scoped so both mutation guards are dropped before the embed call
        // below — same shape as `write_page` (#607/#929): embedding calls
        // out to the configured provider, and holding the guard across that
        // would block every other write in the wiki for its duration.
        let (id, pending_embed) = {
            let _guard = self.mutation_lock.read().await;
            self.confined_page_path(workspace_id, project_id, &path, Prepare::Inspect)?;
            self.ensure_project_workspace(workspace_id, project_id)
                .await?;
            self.reindex_page_locked(workspace_id, project_id, path)
                .await?
        };
        // A failed `store_embedding` here is caught and recorded inside
        // `embed_page_version` (`record_embed_failure`), not retried by the
        // watcher; `ai-memory embed` (manual) or the scheduled backfill tick
        // is what recovers it.
        if let Some(pending) = pending_embed {
            self.embed_page_version(id, &pending.body, pending.abstract_text.as_deref())
                .await?;
        }
        Ok(id)
    }

    /// Reindex `path` from disk into the store, returning the resulting page
    /// id and — when the reindex produced a new version worth embedding —
    /// the inputs for that embed. Embedding itself is deferred to the
    /// caller, which must not call [`Wiki::embed_page_version`] while
    /// holding a lock this function was called under: it may block on the
    /// configured embedder for an arbitrary amount of time, and this path
    /// has two very different callers with two different locking needs
    /// (`reindex_page`'s brief read guard; `hard_delete_decay_tombstone`'s
    /// long-lived write guard, which must never embed at all — see there).
    /// The caller already holds the global mutation guard (shared or
    /// exclusive); acquire only the page mutex here, always after that guard.
    async fn reindex_page_locked(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
    ) -> WikiResult<(PageId, Option<PendingEmbed>)> {
        let _page_guard = self.lock_page(workspace_id, project_id, &path).await;
        let abs = self.confined_page_path(workspace_id, project_id, &path, Prepare::Inspect)?;
        let raw = std::fs::read_to_string(&abs)?;
        let md = parse(&raw)?;
        let title = derive_title(&md.frontmatter, &md.body, &path);
        let links = crate::markdown::extract_all_links(&md.frontmatter, &md.body, &path);
        // Markdown is the source of truth: preserve explicit tier/pinned
        // metadata on reindex instead of forcing every page back to semantic.
        let meta = derive_index_metadata(&path, &md.frontmatter)?;

        // The watcher's own writes never go through `write_page` (it must
        // never touch disk — see `crate::watcher`'s module docs), so this is
        // the only place that can embed a page an external editor created or
        // rewrote. Resolve the pre-upsert latest id (when an embedder and a
        // store reader are both available) so a no-op reindex — the
        // reconcile pass walks every page every 30s — never re-embeds a
        // stable tree; only a genuine new/rewritten version, recognised by
        // `upsert_page` minting a fresh id, does.
        //
        // Fails CLOSED, unlike most of this crate's reconcile-adjacent
        // lookups: without a reader there is no cheap way to tell "new
        // version" from "unchanged", and guessing wrong the open way (embed
        // unconditionally) would re-embed the entire tree every
        // `RECONCILE_INTERVAL` for as long as no reader is attached, which
        // is worse than the rare bare `Wiki::new` fixture simply not getting
        // watcher-driven embeddings. Skipped up front when no embedder is
        // configured too, so the zero-LLM default path (invariant #13)
        // never pays for the extra lookup.
        let previous_id_and_body = match (&self.embedder, &self.store_reader) {
            (Some(_), Some(reader)) => {
                let previous_id = reader
                    .latest_page_id_by_ids(workspace_id, project_id, path.as_str().to_string())
                    .await?;
                Some((
                    previous_id,
                    md.body.clone(),
                    frontmatter_abstract(&md.frontmatter).map(str::to_owned),
                ))
            }
            _ => None,
        };

        #[cfg(test)]
        if let Some(barrier) = &self.reindex_read_barrier {
            barrier.wait().await;
            barrier.wait().await;
        }

        let id = self
            .writer
            .upsert_page(NewPage {
                workspace_id,
                project_id,
                path,
                title,
                body: md.body,
                tier: meta.tier,
                frontmatter_json: md.frontmatter,
                pinned: meta.pinned,
                links,
                author_id: None,
                expires_at: meta.expires_at,
                entities: meta.entities,
                evidence: Vec::new(),
            })
            .await?;

        let pending_embed = previous_id_and_body.and_then(|(previous_id, body, abstract_text)| {
            (previous_id != Some(id)).then_some(PendingEmbed {
                body,
                abstract_text,
            })
        });
        Ok((id, pending_embed))
    }

    fn project_scope_frontmatter(scope: ScopeRow) -> WikiResult<serde_json::Value> {
        let project_id = scope.project_id;
        let identity = match (scope.identity.as_deref(), scope.identity_source.as_deref()) {
            (Some(identity), Some(source)) => Some(
                ai_memory_core::repository_identity::accept_wire_identity(identity, source)
                    .ok_or_else(|| {
                        WikiError::Io(std::io::Error::other(format!(
                            "project {} has an invalid identity/source pair",
                            scope.project_id
                        )))
                    })?,
            ),
            (None, None) => None,
            _ => {
                return Err(WikiError::Io(std::io::Error::other(format!(
                    "project {} has an incomplete identity/source pair",
                    scope.project_id
                ))));
            }
        };
        let canonical_name = identity
            .as_ref()
            .and_then(ai_memory_core::repository_identity::path_style_name);
        let legacy_name = identity
            .as_ref()
            .and_then(ai_memory_core::repository_identity::legacy_basename_name);
        if scope.canonical_name != canonical_name || scope.legacy_name != legacy_name {
            return Err(WikiError::Io(std::io::Error::other(format!(
                "project {project_id} has coordinate keys inconsistent with its identity"
            ))));
        }
        let manifest = ai_memory_core::repository_identity::ProjectManifest {
            project: scope.project_name,
            repo_path: scope.repo_path,
            identity: identity.as_ref().map(|value| value.identity.clone()),
            identity_source: identity.map(|value| value.source),
            canonical_name,
            legacy_name,
        };
        serde_json::to_value(manifest).map_err(WikiError::from)
    }

    /// Read a `_meta.md` scope-manifest's frontmatter from `dir`.
    fn read_scope_meta(dir: &Path) -> WikiResult<serde_json::Value> {
        let path = dir.join("_meta.md");
        let meta = std::fs::symlink_metadata(&path).map_err(|error| {
            let message = if error.kind() == std::io::ErrorKind::NotFound {
                format!("scope manifest {} is missing", path.display())
            } else {
                format!(
                    "could not inspect scope manifest {}: {error}",
                    path.display()
                )
            };
            WikiError::Io(std::io::Error::new(error.kind(), message))
        })?;
        if crate::confinement::is_link_like(&meta) {
            return Err(WikiError::Io(std::io::Error::other(format!(
                "refusing to read symlinked scope manifest {}",
                path.display()
            ))));
        }
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            WikiError::Io(std::io::Error::new(
                error.kind(),
                format!("could not read scope manifest {}: {error}", path.display()),
            ))
        })?;
        Ok(parse(&raw)?.frontmatter)
    }

    /// Rebuild the **entire** store index from the on-disk wiki tree — the
    /// "DB is rebuildable from files" guarantee made concrete. Walks every
    /// `<ws-uuid>/<proj-uuid>/` directory, recreates workspace/project rows and
    /// optional repository identity from each self-describing `_meta.md`
    /// manifest (preserving the ids
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
    /// self-describing). Scopes written by this engine always have one: it
    /// is materialized with the scope's first page, and
    /// [`Self::backfill_scope_manifests`] repairs trees written by older
    /// ones on every start.
    pub async fn reindex_all(&self) -> WikiResult<ReindexSummary> {
        self.inspect_wiki_tree()?;
        let root = self.root().to_path_buf();
        let project_dirs =
            tokio::task::spawn_blocking(move || crate::watcher::walk_project_dirs(&root))
                .await
                .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;

        let mut summary = ReindexSummary::default();
        let mut seen_ws = std::collections::HashSet::new();

        for (ws, proj, proj_root) in project_dirs {
            // A purged scope must not be resurrected from an on-disk directory
            // its purge failed to remove (#607). The tombstone is terminal:
            // skip the whole directory rather than recreating the scope row and
            // reindexing its pages. The files stay inert (the watcher skips
            // them too, #613) until a later purge or manual cleanup removes
            // them; reindex stays non-destructive.
            if self.writer.scope_is_purged(ws, proj).await? {
                tracing::debug!(
                    workspace = %ws,
                    project = %proj,
                    "skipping reindex of a purged (tombstoned) scope",
                );
                summary.skipped_purged += 1;
                continue;
            }
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
            if meta
                .get("project")
                .and_then(|value| value.as_str())
                .is_none()
            {
                return Err(WikiError::Io(std::io::Error::other(format!(
                    "{}/_meta.md is missing the `project` name",
                    proj_root.display()
                ))));
            }
            let manifest: ai_memory_core::repository_identity::ProjectManifest =
                serde_json::from_value(meta)?;
            let identity = match (manifest.identity.as_deref(), manifest.identity_source) {
                (Some(identity), Some(source)) => Some(
                    ai_memory_core::repository_identity::accept_wire_identity(
                        identity,
                        source.as_str(),
                    )
                    .ok_or_else(|| {
                        WikiError::Io(std::io::Error::other(format!(
                            "{}/_meta.md has an invalid repository identity/source pair",
                            proj_root.display()
                        )))
                    })?,
                ),
                (None, None) => None,
                _ => {
                    return Err(WikiError::Io(std::io::Error::other(format!(
                        "{}/_meta.md must provide identity and identity_source together",
                        proj_root.display()
                    ))));
                }
            };
            let derived_canonical = identity
                .as_ref()
                .and_then(ai_memory_core::repository_identity::path_style_name);
            let derived_legacy = identity
                .as_ref()
                .and_then(ai_memory_core::repository_identity::legacy_basename_name);
            let has_coordinate_assertion =
                manifest.canonical_name.is_some() || manifest.legacy_name.is_some();
            if has_coordinate_assertion
                && (manifest.canonical_name != derived_canonical
                    || manifest.legacy_name != derived_legacy)
            {
                return Err(WikiError::Io(std::io::Error::other(format!(
                    "{}/_meta.md repository coordinate keys do not match its identity",
                    proj_root.display()
                ))));
            }
            self.writer
                .ensure_project_with_id(proj, ws, manifest.project, manifest.repo_path, identity)
                .await?;
            summary.projects += 1;

            let pr = proj_root.clone();
            let pages = tokio::task::spawn_blocking(move || crate::watcher::walk_markdown(&pr))
                .await
                .map_err(|e| WikiError::Io(std::io::Error::other(e.to_string())))??;
            let purged = self.purged_sessions(ws, proj).await?;
            for path in pages {
                if is_purged_session_page(&path, &purged) {
                    tracing::debug!(
                        path = %path,
                        "skipping reindex of a purged (tombstoned) session page",
                    );
                    summary.skipped_purged_sessions += 1;
                    continue;
                }
                match self.reindex_page(ws, proj, path).await {
                    Ok(_) => summary.pages += 1,
                    Err(WikiError::Store(ai_memory_store::StoreError::PagePathCollides {
                        requested,
                        existing,
                    })) => {
                        tracing::warn!(
                            path = %requested,
                            existing = %existing,
                            "skipping page: it differs from an already-indexed page only by case \
                             or Unicode normalization, so both cannot coexist on macOS/Windows",
                        );
                        summary.skipped_collisions += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(summary)
    }

    /// Write a `_meta.md` scope manifest under `dir` from `frontmatter`,
    /// idempotently — unchanged content is left untouched so a startup
    /// backfill never churns the wiki git history. Returns `true` if written.
    fn write_scope_manifest(
        &self,
        dir: &Path,
        mut frontmatter: serde_json::Value,
    ) -> WikiResult<bool> {
        let relative = dir
            .strip_prefix(&self.root)
            .map_err(|_| WikiError::Confinement {
                path: dir.to_path_buf(),
                reason: "scope manifest directory is outside the wiki root",
            })?;
        let dir = tree_path(&self.root, relative, Prepare::Directory)?;
        // OKF conformance at the manifest choke point: every non-reserved
        // .md needs a `type`, and the startup backfill's byte-compare must
        // agree with what the OKF migration writes — a typeless emit here
        // silently reverted migrated manifests on the same boot
        // (post-audit finding). `type` is appended last, matching the
        // migration's entry-insertion order, so the two emitters converge
        // on identical bytes.
        if let Some(map) = frontmatter.as_object_mut() {
            map.entry("type".to_string())
                .or_insert(serde_json::Value::String("Scope Manifest".into()));
        }
        let content = emit(&Markdown {
            frontmatter,
            body: String::new(),
        })?;
        let path = dir.join("_meta.md");
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if crate::confinement::is_link_like(&meta) => {
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
        self.git.write_atomic(&path, content.as_bytes())?;
        Ok(true)
    }

    /// Refresh one committed renamed scope from its current store row and checkpoint it.
    ///
    /// # Errors
    /// Returns [`WikiError`] for store, filesystem, or checkpoint errors.
    pub async fn refresh_renamed_scope(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> WikiResult<Option<String>> {
        let reader = self.store_reader.as_ref().ok_or_else(|| {
            WikiError::Io(std::io::Error::other(
                "scope refresh requires a store reader",
            ))
        })?;
        let _guard = self.mutation_lock.write().await;
        self.manifested_scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(workspace_id, project_id));
        let scope = reader
            .scope_row_by_ids(workspace_id, project_id)
            .await?
            .ok_or_else(|| {
                WikiError::Io(std::io::Error::other("renamed scope no longer exists"))
            })?;
        let ws_dir = self.root.join(workspace_id.to_string());
        self.write_scope_manifest(
            &ws_dir,
            serde_json::json!({ "workspace": scope.workspace_name }),
        )?;
        let project_fm = Self::project_scope_frontmatter(scope)?;
        self.write_scope_manifest(&ws_dir.join(project_id.to_string()), project_fm)?;
        let checkpoint = self
            .commit_all(&format!("rename-project {workspace_id}/{project_id}"))?
            .map(|oid| oid.to_string());
        self.manifested_scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert((workspace_id, project_id));
        Ok(checkpoint)
    }

    /// Ensure every workspace/project scope has its self-describing `_meta.md`
    /// manifest on disk (names, `repo_path`, and optional repository identity
    /// coordinates). Idempotent, safe at startup, and a no-op without a store
    /// reader.
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
            let ws_dir = workspace_path(&self.root, ws.workspace_id, Prepare::Directory)?;
            if self.write_scope_manifest(
                &ws_dir,
                serde_json::json!({ "workspace": ws.workspace_name }),
            )? {
                written += 1;
            }
        }
        for s in scopes {
            let project_dir =
                self.confined_project_root(s.workspace_id, s.project_id, Prepare::Directory)?;
            let fm = Self::project_scope_frontmatter(s)?;
            if self.write_scope_manifest(&project_dir, fm)? {
                written += 1;
            }
        }
        Ok(written)
    }

    /// Repair the OKF `stale_after` that builds before this one copied
    /// verbatim from a date-only `expires_at` (docs/okf.md): the index rows
    /// in place through the writer, then each page's file, body and
    /// `generated.at` untouched, in one git commit. Idempotent and cheap on
    /// a repaired store (only pages with a date-only TTL are read); safe to
    /// run on every startup. Returns `(rows, files)` repaired.
    ///
    /// # Errors
    /// Returns [`WikiError`] for store, filesystem or git errors.
    pub async fn repair_date_only_stale_after(&self) -> WikiResult<(u64, usize)> {
        self.inspect_wiki_tree()?;
        let _guard = self.mutation_lock.write().await;
        self.inspect_wiki_tree()?;
        let repair = self.writer.repair_date_only_stale_after().await?;
        let mut files = 0;
        for page in &repair.date_only_pages {
            let page_path = PagePath::new(&page.path)?;
            let abs = self.confined_page_path(
                page.workspace_id,
                page.project_id,
                &page_path,
                Prepare::Inspect,
            )?;
            let raw = match std::fs::read_to_string(&abs) {
                Ok(raw) => raw,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let Ok(mut markdown) = parse(&raw) else {
                continue;
            };
            if ai_memory_core::okf::repair_date_only_stale_after(&mut markdown.frontmatter) {
                self.git.write_atomic(&abs, emit(&markdown)?.as_bytes())?;
                files += 1;
            }
        }
        if files > 0 {
            self.commit_all("okf: repair date-only stale_after on existing pages")?;
        }
        Ok((repair.rows_repaired, files))
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
        // Reject any path that cannot be materialised and checkpointed on every
        // supported platform, before anything is written.
        for req in &requests {
            req.path.ensure_portable()?;
            self.confined_page_path(
                req.workspace_id,
                req.project_id,
                &req.path,
                Prepare::Inspect,
            )?;
        }
        // Pre-compute markdown for each request. Filesystem work happens only
        // after the mutation guard + project/workspace validation below.
        let mut staged: Vec<(
            WritePageRequest,
            String,
            std::path::PathBuf,
            Option<AdmissionContext>,
        )> = Vec::with_capacity(requests.len());
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
            conform_frontmatter_for_disk(
                &self.abs_path(req.workspace_id, req.project_id, &req.path),
                req.path.as_str(),
                &mut markdown,
            );

            let title = req
                .title
                .take()
                .unwrap_or_else(|| derive_title(&markdown.frontmatter, &markdown.body, &req.path));
            let emitted = emit(&markdown)?;
            let abs = self.abs_path(req.workspace_id, req.project_id, &req.path);
            req.frontmatter = markdown.frontmatter;
            req.body = markdown.body;
            let req_with_title = WritePageRequest {
                title: Some(title),
                ..req
            };
            staged.push((req_with_title, emitted, abs, resolved_ctx));
        }

        let (ids, dispatches) = {
            let _guard = self.mutation_lock.read().await;
            for (req, _, _, _) in &staged {
                self.confined_page_path(
                    req.workspace_id,
                    req.project_id,
                    &req.path,
                    Prepare::Inspect,
                )?;
            }
            // Serialize every path in this batch against concurrent single-page
            // or batch writers to the same path (#607). Sorted acquisition
            // keeps a batch deadlock-free against any other writer.
            let batch_keys: Vec<PageKey> = staged
                .iter()
                .map(|(req, _, _, _)| (req.workspace_id, req.project_id, req.path.clone()))
                .collect();
            let _page_guards = self.lock_pages(&batch_keys).await;
            let mut staged_files: Vec<(
                WritePageRequest,
                tempfile::NamedTempFile,
                std::path::PathBuf,
                Option<AdmissionContext>,
            )> = Vec::with_capacity(staged.len());
            for (req, emitted, _abs, ctx) in staged {
                self.ensure_project_workspace(req.workspace_id, req.project_id)
                    .await?;
                let abs = self.confined_page_path(
                    req.workspace_id,
                    req.project_id,
                    &req.path,
                    Prepare::Parents,
                )?;
                let parent = abs.parent().ok_or_else(|| {
                    ai_memory_wiki_error("page path has no parent (cannot stage tempfile)")
                })?;
                let mut tmp = tempfile::Builder::new()
                    .prefix(".ai-memory-tmp.")
                    .tempfile_in(parent)?;
                use std::io::Write as _;
                tmp.write_all(emitted.as_bytes())?;
                tmp.as_file().sync_data()?;
                staged_files.push((req, tmp, abs, ctx));
            }

            // Build NewPage batch with the precomputed titles.
            let pages: Vec<ai_memory_core::NewPage> = staged_files
                .iter()
                .map(|(req, _, _, _)| {
                    Ok(ai_memory_core::NewPage {
                        workspace_id: req.workspace_id,
                        project_id: req.project_id,
                        path: req.path.clone(),
                        title: req.title.clone().unwrap_or_default(),
                        body: req.body.clone(),
                        tier: req.tier,
                        frontmatter_json: req.frontmatter.clone(),
                        pinned: req.pinned,
                        links: crate::markdown::extract_all_links(
                            &req.frontmatter,
                            &req.body,
                            &req.path,
                        ),
                        author_id: req.author_id,
                        expires_at: parse_expires_at(&req.path, &req.frontmatter)?,
                        entities: parse_entities(&req.path, &req.frontmatter)?,
                        evidence: req.evidence.clone(),
                    })
                })
                .collect::<WikiResult<Vec<_>>>()?;

            // Install files first so the DB is never ahead of markdown. If the
            // SQL batch fails below, rollback restores the prior disk state;
            // if the process crashes in this window, startup/reindex repairs
            // the derived DB from the markdown source of truth.
            let mut installed = Vec::with_capacity(staged_files.len());
            let mut dispatches = Vec::with_capacity(staged_files.len());
            for (req, tmp, abs, ctx) in staged_files {
                let install = match persist_tmp_with_rollback_snapshot(&self.git, tmp, &abs) {
                    Ok(install) => install,
                    Err(e) => {
                        rollback_or_inconsistent(&self.git, &installed, &e)?;
                        return Err(e);
                    }
                };
                installed.push(install);
                dispatches.push((req.path, req.frontmatter, req.body, ctx));
            }

            let ids = match self.writer.upsert_pages_batch(pages).await {
                Ok(ids) => ids,
                Err(e) => {
                    rollback_or_inconsistent(&self.git, &installed, &e)?;
                    return Err(e.into());
                }
            };

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

    /// Acquire the per-path write lock for one page, serializing concurrent
    /// writers to the same `(ws, proj, path)` while different paths run
    /// concurrently. See [`Wiki::page_locks`].
    async fn lock_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut map = self.page_locks.lock().expect("page_locks mutex poisoned");
            // Opportunistic GC under the same short critical section that hands
            // out locks: an entry whose only remaining strong ref is the map's
            // own (count == 1) has no live guard and no waiter, so drop it.
            map.retain(|_, v| Arc::strong_count(v) > 1);
            Arc::clone(
                map.entry((workspace_id, project_id, path.clone()))
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        lock.lock_owned().await
    }

    /// Acquire per-path write locks for a batch of pages in a deterministic
    /// global order (sorted by id/path bytes, deduped) so a batch write and any
    /// concurrent single-page or batch writer can never deadlock. See
    /// [`Wiki::page_locks`].
    async fn lock_pages(&self, keys: &[PageKey]) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
        let mut ordered: Vec<PageKey> = keys.to_vec();
        // The id newtypes are not `Ord`, so order by their byte / string
        // representation — any total order works, it only has to be consistent
        // across all acquirers.
        ordered.sort_by(|a, b| {
            (a.0.as_bytes(), a.1.as_bytes(), a.2.as_str()).cmp(&(
                b.0.as_bytes(),
                b.1.as_bytes(),
                b.2.as_str(),
            ))
        });
        ordered.dedup();
        let mut guards = Vec::with_capacity(ordered.len());
        for (ws, proj, path) in ordered {
            guards.push(self.lock_page(ws, proj, &path).await);
        }
        guards
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
        match self.write_page_inner(req, None).await? {
            ConditionalWrite::Written(id) => Ok(id),
            ConditionalWrite::Mismatch { .. } => Err(ai_memory_wiki_error(
                "an unconditional page write reported a precondition mismatch",
            )),
        }
    }

    /// [`Self::write_page`] only when `precondition` holds for the page's
    /// latest version. A mismatch writes nothing: no file change, no new
    /// version, no webhook dispatch.
    ///
    /// Checked twice under the page lock: before the file is replaced, so a
    /// normal mismatch never touches disk, and again in the store transaction
    /// that writes the version, which is the authority (a plain delete takes
    /// only the shared mutation lock and can land between the two); a
    /// mismatch there restores the file.
    ///
    /// # Errors
    /// As [`Self::write_page`].
    pub async fn write_page_if(
        &self,
        req: WritePageRequest,
        precondition: PagePrecondition,
    ) -> WikiResult<ConditionalWrite> {
        self.write_page_inner(req, Some(precondition)).await
    }

    async fn write_page_inner(
        &self,
        req: WritePageRequest,
        precondition: Option<PagePrecondition>,
    ) -> WikiResult<ConditionalWrite> {
        self.confined_page_path(
            req.workspace_id,
            req.project_id,
            &req.path,
            Prepare::Inspect,
        )?;
        // Reject a path that cannot be materialised and checkpointed on every
        // supported platform, before anything is written.
        //
        // This is the single funnel every page creation passes through, and
        // it is the moment the two failing operations happen: the file hits
        // the filesystem and libgit2 checkpoints it. Catching it earlier (in
        // `PagePath::new`) would break reads of already-stored pages, and
        // catching it later leaves the worst outcome observed in #462 — a
        // page written successfully but uncheckpointable and undeletable
        // through the normal API.
        req.path.ensure_portable()?;

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
            evidence,
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
        conform_frontmatter_for_disk(
            &self.abs_path(workspace_id, project_id, &path),
            path.as_str(),
            &mut markdown,
        );

        // Re-derive title + links from the (possibly mutated) markdown.
        // We do this after the chain so explicit title overrides survive
        // mutations and webhooks that rename or restructure the body
        // still get the right title/links extracted.
        let title = explicit_title
            .clone()
            .map(|t| self.sanitizer.scrub(&t))
            .unwrap_or_else(|| derive_title(&markdown.frontmatter, &markdown.body, &path));
        let links =
            crate::markdown::extract_all_links(&markdown.frontmatter, &markdown.body, &path);
        let expires_at = parse_expires_at(&path, &markdown.frontmatter)?;
        let entities = parse_entities(&path, &markdown.frontmatter)?;
        // Read before the destructuring move below: the embed step needs the
        // L0 `abstract:` line out of the final frontmatter.
        let abstract_text = frontmatter_abstract(&markdown.frontmatter).map(str::to_owned);

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
            // Serialize with any concurrent write to this same path, so the
            // file-rename and DB-upsert below cannot interleave with another
            // writer's and leave disk disagreeing with the DB `latest` row
            // (#607). Different paths still proceed concurrently.
            let _page_guard = self.lock_page(workspace_id, project_id, &path).await;
            self.confined_page_path(workspace_id, project_id, &path, Prepare::Inspect)?;
            self.ensure_project_workspace(workspace_id, project_id)
                .await?;
            if let (Some(precondition), Some(reader)) = (precondition, &self.store_reader) {
                let current = reader
                    .latest_page_id_by_ids(workspace_id, project_id, path.as_str().to_string())
                    .await?;
                if !precondition_holds(precondition, current) {
                    return Ok(ConditionalWrite::Mismatch { current });
                }
            }
            let abs = self.confined_page_path(workspace_id, project_id, &path, Prepare::Parents)?;
            let installed =
                replace_file_with_rollback_snapshot(&self.git, &abs, emitted.as_bytes())?; // lgtm [rust/path-injection]

            let page = NewPage {
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
                expires_at,
                entities,
                evidence,
            };
            let written = match precondition {
                None => self
                    .writer
                    .upsert_page(page)
                    .await
                    .map(ConditionalUpsert::Upserted),
                Some(precondition) => self.writer.upsert_page_if(page, precondition).await,
            };
            match written {
                Ok(ConditionalUpsert::Upserted(id)) => id,
                Ok(ConditionalUpsert::Mismatch { current }) => {
                    rollback_or_inconsistent(
                        &self.git,
                        std::slice::from_ref(&installed),
                        &"a page write precondition no longer held",
                    )?;
                    return Ok(ConditionalWrite::Mismatch { current });
                }
                Err(e) => {
                    rollback_or_inconsistent(&self.git, std::slice::from_ref(&installed), &e)?;
                    return Err(e.into());
                }
            }
        };
        // Embed if configured. We do this on the caller's task so the
        // tool reply still happens "indexes commit in the same
        // transaction" (basic-memory #763 lesson): no fire-and-forget
        // background embedding.
        self.embed_page_version(page_id, &final_body, abstract_text.as_deref())
            .await?;

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
        Ok(ConditionalWrite::Written(page_id))
    }

    /// Embed `body` (and its L0 `abstract:` line, if present) into
    /// `page_id`'s vectors, when an embedder is configured.
    ///
    /// Shared by [`Wiki::write_page`] (every version written through the
    /// API) and [`Wiki::reindex_page_locked`] (external file writes and
    /// rewrites picked up by the watcher), so a page version becomes
    /// hybrid-searchable through the same mechanism regardless of which path
    /// produced it. Before this was pulled out, only `write_page` embedded —
    /// a watcher-driven rewrite got a new page id (the supersession chain
    /// worked) but no embedding until a manual `ai-memory embed` (issue
    /// #929): hybrid search silently fell back to FTS-only ranking for
    /// every rewritten page.
    ///
    /// Best-effort: an embedder failure is logged and recorded via
    /// `record_embed_failure` but never propagated, so a working page write
    /// is never undone by an embedding hiccup.
    ///
    /// # Errors
    /// Returns [`WikiError`] only for a store error while persisting a
    /// successfully computed vector; provider/embedder failures are caught
    /// and recorded instead.
    async fn embed_page_version(
        &self,
        page_id: PageId,
        body: &str,
        abstract_text: Option<&str>,
    ) -> WikiResult<()> {
        let Some(embedder) = &self.embedder else {
            return Ok(());
        };
        match embedder.embed_document(body).await {
            Ok(vec) => {
                let bytes = f32_vec_to_bytes(&vec);
                self.writer
                    .store_embedding(
                        page_id,
                        bytes,
                        embedder.provider().to_string(),
                        // Not `.model()`: a configured document prefix must land
                        // under a distinct stored identity so a prefix change is
                        // never silently mixed with vectors embedded under a
                        // different (or no) prefix. See `Embedder::model_identity`.
                        embedder.model_identity(),
                        embedder.dim(),
                    )
                    .await?;
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %page_id, "embedding failed; page indexed without it");
                // The warning alone dies with the container. Record it so
                // the page is attributable later (#528); best-effort,
                // because failing to note a failure must not fail the
                // write that already succeeded.
                let _ = self
                    .writer
                    .record_embed_failure(
                        page_id,
                        ai_memory_store::EmbedOutcome::Failed,
                        Some(e.to_string()),
                    )
                    .await;
            }
        }
        // L0 abstract: the frontmatter `abstract:` line is embedded on
        // its own so the opt-in abstract stream can rank on the sharp
        // one-line summary. A page rewritten without the key has its
        // stale abstract row removed, mirroring the body row's
        // replace-on-write semantics.
        match abstract_text {
            Some(abstract_text) => match embedder.embed_document(abstract_text).await {
                Ok(vec) => {
                    self.writer
                        .store_abstract_embeddings(vec![ai_memory_store::EmbeddingWrite {
                            page_id,
                            vector_bytes: f32_vec_to_bytes(&vec),
                            provider: embedder.provider().to_string(),
                            // See the body-embedding write above.
                            model: embedder.model_identity(),
                            dim: embedder.dim(),
                        }])
                        .await?;
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %page_id, "abstract embedding failed; page indexed without it");
                    let _ = self
                        .writer
                        .record_embed_failure(
                            page_id,
                            ai_memory_store::EmbedOutcome::Failed,
                            Some(e.to_string()),
                        )
                        .await;
                }
            },
            None => self.writer.delete_abstract_embedding(page_id).await?,
        }
        Ok(())
    }
}

/// The outcome of [`Wiki::write_page_if`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalWrite {
    /// The precondition held and the page was written; the id is current.
    Written(PageId),
    /// The precondition did not hold and nothing was written.
    Mismatch {
        /// The latest version id at the path, `None` when there is none.
        current: Option<PageId>,
    },
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
    /// Evidence sources backing this write (P2,
    /// docs/design-hindsight-borrowings.md §3), forwarded verbatim to
    /// [`ai_memory_core::NewPage::evidence`]. Populated by the
    /// consolidator from the session(s) it drew on; empty for every
    /// other caller (MCP tool, admin endpoints, lint rewriters).
    pub evidence: Vec<ai_memory_core::PageEvidence>,
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
) -> WikiResult<IndexMetadata> {
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
    let expires_at = parse_expires_at(path, frontmatter)?;
    let entities = parse_entities(path, frontmatter)?;
    Ok(IndexMetadata {
        tier,
        pinned,
        expires_at,
        entities,
    })
}

/// Index-relevant metadata derived from a page's frontmatter. Markdown
/// is the source of truth; every field here is rebuildable by a reindex.
struct IndexMetadata {
    tier: Tier,
    pinned: bool,
    expires_at: Option<jiff::Timestamp>,
    entities: Vec<String>,
}

/// Derive a page's indexed entities — the salient nouns it is *about* —
/// from its frontmatter, for the entity retrieval stream and the `as_of`
/// entity-timeline queries (docs/temporal.md).
///
/// Two sources, merged and normalised:
///
/// 1. The explicit `entities:` list an LLM consolidator emits when it
///    runs. Highest precision, but absent on the bulk of a real store —
///    bootstrapped pages predate it and stable pages are never
///    re-consolidated, so relying on it alone leaves the entity index
///    (and therefore `as_of`) empty on any mature deployment.
/// 2. The frontmatter `tags:` list, which nearly every page carries and
///    which is, by construction, "what this page is about" — the same
///    thing entities are meant to capture. Deriving from tags populates
///    the index deterministically, with no LLM and no re-consolidation,
///    for the whole corpus. Broad tags do not distort ranking: the
///    entity stream weights each match by inverse page-frequency, so a
///    tag shared by many pages contributes proportionally little.
///
/// Explicit entities come first so they win the per-page cap
/// ([`normalize_entities`] dedupes and bounds the result). A non-array
/// `entities` value is still a structural error; `tags` is treated
/// leniently (a malformed or missing list simply contributes nothing),
/// because a bad tag must never fail a page write.
pub(crate) fn parse_entities(
    path: &PagePath,
    frontmatter: &serde_json::Value,
) -> WikiResult<Vec<String>> {
    // Strict structural check on the write path only: a non-array
    // `entities` value is a mistake worth surfacing. `tags` stays lenient
    // (see below); the derivation itself is shared with the store's
    // one-shot backfill so the two never drift.
    match frontmatter.get("entities") {
        None | Some(serde_json::Value::Null | serde_json::Value::Array(_)) => {}
        Some(_) => {
            return Err(ai_memory_wiki_error(&format!(
                "invalid non-array entities in frontmatter for {}",
                path.as_str()
            )));
        }
    }
    Ok(ai_memory_core::frontmatter_entity_names(frontmatter))
}

/// Parse the optional frontmatter `expires_at:` key. Accepts RFC3339
/// (`2026-09-01T12:00:00Z`) or a bare date (`2026-09-01` = end of that
/// day, UTC). Anything else is rejected in the same style as an
/// invalid `tier`, so a typo can't silently mean "never expires".
pub(crate) fn parse_expires_at(
    path: &PagePath,
    frontmatter: &serde_json::Value,
) -> WikiResult<Option<jiff::Timestamp>> {
    let raw = match frontmatter.get("expires_at") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(serde_json::Value::String(s)) => s.trim(),
        Some(_) => {
            return Err(ai_memory_wiki_error(&format!(
                "invalid non-string expires_at in frontmatter for {}",
                path.as_str()
            )));
        }
    };
    if raw.is_empty() {
        return Ok(None);
    }
    if let Some(ts) = ai_memory_core::parse_expires_at_instant(raw) {
        return Ok(Some(ts));
    }
    Err(ai_memory_wiki_error(&format!(
        "invalid expires_at in frontmatter for {} (want RFC3339 or YYYY-MM-DD): {raw}",
        path.as_str()
    )))
}

/// OKF conformance for the on-disk file (docs/okf.md): fill the
/// deterministic keys, then stamp `generated.at` — inheriting the
/// current file's value when nothing but the timestamp would change, so
/// an idempotent rewrite emits byte-identical markdown (no git churn,
/// and the store's modulo-`generated.at` comparison keeps the row).
/// The frontmatter `abstract:` line, when it is a non-empty string.
fn frontmatter_abstract(frontmatter: &serde_json::Value) -> Option<&str> {
    frontmatter
        .get("abstract")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn conform_frontmatter_for_disk(abs: &Path, page_path: &str, markdown: &mut Markdown) {
    ai_memory_core::okf::conform_frontmatter(page_path, &mut markdown.frontmatter);
    let inherited = std::fs::read_to_string(abs)
        .ok()
        .and_then(|s| crate::markdown::parse(&s).ok())
        .filter(|cur| {
            ai_memory_core::okf::strip_generated_at(&cur.frontmatter)
                == ai_memory_core::okf::strip_generated_at(&markdown.frontmatter)
                && cur.body == markdown.body
        })
        .and_then(|cur| ai_memory_core::okf::generated_at(&cur.frontmatter).map(str::to_string));
    let at = inherited.unwrap_or_else(|| {
        jiff::Timestamp::now()
            .strftime("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    });
    ai_memory_core::okf::stamp_generated_at(&mut markdown.frontmatter, &at);
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

#[derive(Debug)]
struct InstalledFile {
    path: PathBuf,
    previous: Option<Vec<u8>>,
}

fn snapshot_existing_file(path: &Path) -> WikiResult<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WikiError::Io(e)),
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
    git: &GitAdapter,
    tmp: tempfile::NamedTempFile,
    path: &Path,
) -> WikiResult<InstalledFile> {
    let previous = snapshot_existing_file(path)?;
    let persisted = git.persist(tmp, path)?;
    persisted.sync_data()?;
    sync_parent_best_effort(path);
    Ok(InstalledFile {
        path: path.to_path_buf(),
        previous,
    })
}

fn replace_file_with_rollback_snapshot(
    git: &GitAdapter,
    path: &Path,
    bytes: &[u8],
) -> WikiResult<InstalledFile> {
    let previous = snapshot_existing_file(path)?;
    git.write_atomic(path, bytes)?;
    Ok(InstalledFile {
        path: path.to_path_buf(),
        previous,
    })
}

fn rollback_installed_files(git: &GitAdapter, installed: &[InstalledFile]) -> WikiResult<()> {
    for file in installed.iter().rev() {
        match &file.previous {
            Some(bytes) => git.write_atomic(&file.path, bytes)?,
            None => match git.remove_file_checked(&file.path) {
                Ok(()) => sync_parent_best_effort(&file.path),
                Err(WikiError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
        }
    }
    Ok(())
}

fn precondition_holds(precondition: PagePrecondition, current: Option<PageId>) -> bool {
    match precondition {
        PagePrecondition::Absent => current.is_none(),
        PagePrecondition::Latest(expected) => current == Some(expected),
    }
}

fn rollback_or_inconsistent<E: std::fmt::Display>(
    git: &GitAdapter,
    installed: &[InstalledFile],
    cause: &E,
) -> WikiResult<()> {
    if let Err(rollback_err) = rollback_installed_files(git, installed) {
        return Err(WikiError::Io(std::io::Error::other(format!(
            "INCONSISTENT STATE: wiki files changed but store write failed ({cause}) and rollback failed ({rollback_err})"
        ))));
    }
    Ok(())
}

fn quarantine_file(git: &GitAdapter, path: &Path) -> WikiResult<Option<PathBuf>> {
    let Some(parent) = path.parent() else {
        return Err(WikiError::Io(std::io::Error::other(
            "page path has no parent (cannot quarantine delete)",
        )));
    };
    let tmp = tempfile::Builder::new()
        .prefix(".ai-memory-delete.")
        .tempfile_in(parent)?;
    let (_file, quarantine) = tmp.keep().map_err(|e| e.error)?;
    git.remove_file_checked(&quarantine)?;
    match git.rename_checked(path, &quarantine) {
        Ok(()) => Ok(Some(quarantine)),
        Err(e) => {
            let _ = git.remove_file_checked(&quarantine);
            Err(e)
        }
    }
}

fn restore_quarantined_file(
    git: &GitAdapter,
    quarantined: &Option<PathBuf>,
    path: &Path,
    page_path: &PagePath,
) {
    if let Some(quarantine) = quarantined
        && let Err(error) = git.rename_checked(quarantine, path)
    {
        tracing::error!(
            path = %page_path.as_str(),
            quarantine = %quarantine.display(),
            %error,
            "page removal: conditional store mutation failed and restoring quarantined file also failed"
        );
    }
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
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::admission::{FailurePolicy, WebhookConfig};
    use ai_memory_store::{
        AutoImproveProposalOperation, AutoImproveProposalStatus, NewAutoImproveProposal,
        StageAutoImproveRun, Store,
    };
    use tempfile::TempDir;

    #[test]
    fn expires_at_accepts_rfc3339_and_date_only_utc() {
        let path = PagePath::new("notes/ttl.md").unwrap();
        let rfc3339 = serde_json::json!({"expires_at": "2026-08-01T12:00:00-03:00"});
        assert_eq!(
            parse_expires_at(&path, &rfc3339).unwrap().unwrap(),
            "2026-08-01T15:00:00Z".parse::<jiff::Timestamp>().unwrap()
        );

        let date_only = serde_json::json!({"expires_at": "2026-08-01"});
        assert_eq!(
            parse_expires_at(&path, &date_only).unwrap().unwrap(),
            "2026-08-01T23:59:59.999999Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
        );
    }

    #[test]
    fn expires_at_fails_closed_for_invalid_values() {
        let path = PagePath::new("notes/ttl.md").unwrap();
        assert!(
            parse_expires_at(&path, &serde_json::json!({}))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_expires_at(&path, &serde_json::json!({"expires_at": "  "}))
                .unwrap()
                .is_none()
        );
        assert!(parse_expires_at(&path, &serde_json::json!({"expires_at": "soon"})).is_err());
        assert!(parse_expires_at(&path, &serde_json::json!({"expires_at": 7})).is_err());
    }

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

    /// The file a consumer reads off disk is the OKF concept file
    /// (docs/okf.md): required `type`, a `generated {by, at}` stanza,
    /// provenance — emitted by the wiki, not just stored in the index.
    #[tokio::test]
    async fn written_files_are_okf_conformant_on_disk() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        wiki.write_page(req(
            ws,
            proj,
            "gotchas/linker.md",
            "mind the linker",
            serde_json::json!({"title": "Linker gotcha"}),
        ))
        .await
        .unwrap();

        let raw =
            std::fs::read_to_string(wiki.project_root(ws, proj).join("gotchas/linker.md")).unwrap();
        let parsed = crate::markdown::parse(&raw).unwrap();
        assert_eq!(parsed.frontmatter["type"], "Gotcha");
        assert!(
            parsed.frontmatter["generated"]["by"]
                .as_str()
                .unwrap()
                .starts_with("process:ai-memory/")
        );
        assert!(
            parsed.frontmatter["generated"]["at"]
                .as_str()
                .unwrap()
                .ends_with('Z')
        );
        assert!(ai_memory_core::okf::is_conformant(&parsed.frontmatter));
    }

    /// A `[[project:path]]` written as inline code is an example, not a
    /// dependency: the page shows it as code. Indexing it made lint report a
    /// broken cross-project link on a page that has none.
    #[tokio::test]
    async fn a_cross_project_link_in_inline_code_is_not_a_dangling_dependency() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        wiki.write_page(req(
            ws,
            proj,
            "notes/linking.md",
            "Link across projects with `[[other-project:notes/example]]`.\n\
             This page depends on [[other-project:notes/real]].\n",
            serde_json::json!({"title": "Linking"}),
        ))
        .await
        .unwrap();

        let dangling = store
            .reader
            .dangling_cross_project_links(ws, proj)
            .await
            .unwrap();
        let paths: Vec<&str> = dangling.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["notes/real.md"]);
    }

    /// OKF requires every timestamp to carry an explicit UTC offset, while
    /// the TTL key also accepts a bare date. The file on disk must name the
    /// instant the TTL machinery hides the page, not the date as typed.
    #[tokio::test]
    async fn a_date_only_ttl_lands_on_disk_as_an_okf_stale_after_instant() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let frontmatter = serde_json::json!({"title": "Freeze", "expires_at": "2099-08-01"});
        wiki.write_page(req(
            ws,
            proj,
            "notes/freeze.md",
            "release freeze",
            frontmatter.clone(),
        ))
        .await
        .unwrap();

        let raw =
            std::fs::read_to_string(wiki.project_root(ws, proj).join("notes/freeze.md")).unwrap();
        let parsed = crate::markdown::parse(&raw).unwrap();
        assert_eq!(parsed.frontmatter["expires_at"], "2099-08-01");
        assert_eq!(
            parsed.frontmatter["stale_after"],
            "2099-08-01T23:59:59.999999Z"
        );
        let path = PagePath::new("notes/freeze.md").unwrap();
        let ttl = parse_expires_at(&path, &frontmatter).unwrap().unwrap();
        assert_eq!(
            parsed.frontmatter["stale_after"]
                .as_str()
                .unwrap()
                .parse::<jiff::Timestamp>()
                .unwrap(),
            ttl
        );
    }

    /// Put `notes/<name>.md` back in the state a pre-fix build left it:
    /// `stale_after` copied verbatim from the date-only `expires_at`, in both
    /// the index row and the file. Returns the row's `(id, updated_at)`.
    fn regress_to_date_only_stale_after(
        tmp: &TempDir,
        wiki: &Wiki,
        ws: WorkspaceId,
        proj: ProjectId,
        rel: &str,
        row: bool,
        file: bool,
    ) -> (Vec<u8>, i64) {
        let db = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
        let (id, updated_at, fm_str): (Vec<u8>, i64, String) = db
            .query_row(
                "SELECT id, updated_at, frontmatter_json FROM pages \
                 WHERE path = ?1 AND is_latest = 1",
                [rel],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let regress = |fm: &mut serde_json::Value| {
            let expires = fm["expires_at"].clone();
            fm["stale_after"] = expires;
        };
        if row {
            let mut fm: serde_json::Value = serde_json::from_str(&fm_str).unwrap();
            regress(&mut fm);
            db.execute(
                "UPDATE pages SET frontmatter_json = ?1 WHERE id = ?2",
                rusqlite::params![serde_json::to_string(&fm).unwrap(), id],
            )
            .unwrap();
        }
        if file {
            let abs = wiki.project_root(ws, proj).join(rel);
            let mut md = crate::markdown::parse(&std::fs::read_to_string(&abs).unwrap()).unwrap();
            regress(&mut md.frontmatter);
            std::fs::write(&abs, crate::markdown::emit(&md).unwrap()).unwrap();
        }
        (id, updated_at)
    }

    fn latest_row(tmp: &TempDir, rel: &str) -> (Vec<u8>, i64, serde_json::Value) {
        let db = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
        let (id, updated_at, fm_str): (Vec<u8>, i64, String) = db
            .query_row(
                "SELECT id, updated_at, frontmatter_json FROM pages \
                 WHERE path = ?1 AND is_latest = 1",
                [rel],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        (id, updated_at, serde_json::from_str(&fm_str).unwrap())
    }

    /// Stores written before the `stale_after` fix hold the bare date the old
    /// derivation copied. The startup repair rewrites row and file in place
    /// (same version row, same `updated_at`, same `generated.at`, same body),
    /// leaves a `stale_after` that is not that copy alone, finishes a file
    /// whose row an interrupted run already repaired, and then does nothing.
    #[tokio::test]
    async fn startup_repair_fixes_a_date_only_stale_after_in_place() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        for (rel, fm) in [
            (
                "notes/freeze.md",
                serde_json::json!({"title": "Freeze", "expires_at": "2099-08-01"}),
            ),
            (
                "notes/interrupted.md",
                serde_json::json!({"title": "Interrupted", "expires_at": "2099-09-01"}),
            ),
            (
                "notes/authored.md",
                serde_json::json!({
                    "title": "Authored",
                    "expires_at": "2099-10-01",
                    "stale_after": "2099-07-01",
                }),
            ),
        ] {
            wiki.write_page(req(ws, proj, rel, "body stays", fm))
                .await
                .unwrap();
        }
        let (freeze_id, freeze_updated) =
            regress_to_date_only_stale_after(&tmp, &wiki, ws, proj, "notes/freeze.md", true, true);
        regress_to_date_only_stale_after(
            &tmp,
            &wiki,
            ws,
            proj,
            "notes/interrupted.md",
            false,
            true,
        );
        let freeze_path = wiki.project_root(ws, proj).join("notes/freeze.md");
        let before =
            crate::markdown::parse(&std::fs::read_to_string(&freeze_path).unwrap()).unwrap();
        assert_eq!(before.frontmatter["stale_after"], "2099-08-01");
        let commits = wiki.git.commit_count();

        assert_eq!(wiki.repair_date_only_stale_after().await.unwrap(), (1, 2));

        let after =
            crate::markdown::parse(&std::fs::read_to_string(&freeze_path).unwrap()).unwrap();
        assert_eq!(
            after.frontmatter["stale_after"],
            "2099-08-01T23:59:59.999999Z"
        );
        assert_eq!(
            after.frontmatter["generated"],
            before.frontmatter["generated"]
        );
        assert_eq!(after.body, before.body);
        let (id, updated_at, row_fm) = latest_row(&tmp, "notes/freeze.md");
        assert_eq!(id, freeze_id, "the repair must not mint a new version");
        assert_eq!(updated_at, freeze_updated);
        assert_eq!(row_fm["stale_after"], "2099-08-01T23:59:59.999999Z");

        let interrupted = crate::markdown::parse(
            &std::fs::read_to_string(wiki.project_root(ws, proj).join("notes/interrupted.md"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            interrupted.frontmatter["stale_after"],
            "2099-09-01T23:59:59.999999Z"
        );
        let (_, _, authored) = latest_row(&tmp, "notes/authored.md");
        assert_eq!(authored["stale_after"], "2099-07-01");
        assert_eq!(wiki.git.commit_count(), commits + 1);

        assert_eq!(wiki.repair_date_only_stale_after().await.unwrap(), (0, 0));
        assert_eq!(wiki.git.commit_count(), commits + 1);
    }

    /// Refused on every platform, not only the case-insensitive ones: the
    /// wiki is synced between them, so what it accepts cannot depend on the
    /// filesystem underneath.
    #[tokio::test]
    async fn a_case_only_path_variant_cannot_destroy_an_existing_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        wiki.write_page(req(
            ws,
            proj,
            "concepts/alpha.md",
            "the original alpha body",
            serde_json::json!({"title": "Alpha"}),
        ))
        .await
        .unwrap();

        let collision = wiki
            .write_page(req(
                ws,
                proj,
                "concepts/Alpha.md",
                "a different page entirely",
                serde_json::json!({"title": "Alpha Upper"}),
            ))
            .await;

        let err = collision.expect_err("case-only variant must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("concepts/alpha.md") && msg.contains("concepts/Alpha.md"),
            "the error must name both pages so the caller can act on it: {msg}"
        );
        let raw =
            std::fs::read_to_string(wiki.project_root(ws, proj).join("concepts/alpha.md")).unwrap();
        assert!(
            raw.contains("the original alpha body"),
            "the existing page's file must survive the refused write: {raw}"
        );
    }

    /// The same collision through Unicode normalization rather than case.
    #[tokio::test]
    async fn an_nfd_path_variant_cannot_destroy_its_nfc_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let nfc = "concepts/caf\u{00e9}.md";
        let nfd = "concepts/cafe\u{0301}.md";

        wiki.write_page(req(
            ws,
            proj,
            nfc,
            "the composed page body",
            serde_json::json!({"title": "Cafe NFC"}),
        ))
        .await
        .unwrap();

        let collision = wiki
            .write_page(req(
                ws,
                proj,
                nfd,
                "a different page entirely",
                serde_json::json!({"title": "Cafe NFD"}),
            ))
            .await;

        assert!(
            collision.is_err(),
            "a decomposed variant of an existing composed path must be refused"
        );
        let raw = std::fs::read_to_string(wiki.project_root(ws, proj).join(nfc)).unwrap();
        assert!(
            raw.contains("the composed page body"),
            "the existing page's file must survive the refused write: {raw}"
        );
    }

    /// The guard must not turn page paths into case-insensitive keys.
    #[tokio::test]
    async fn distinct_paths_are_unaffected_by_the_collision_guard() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        for path in [
            "concepts/alpha.md",
            "concepts/alphabet.md",
            "concepts/nested/alpha.md",
            "decisions/alpha.md",
        ] {
            wiki.write_page(req(ws, proj, path, "body", serde_json::json!({})))
                .await
                .unwrap_or_else(|e| panic!("{path} must be accepted: {e}"));
        }

        // A rewrite is a supersede, not a collision.
        wiki.write_page(req(
            ws,
            proj,
            "concepts/alpha.md",
            "revised body",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    }

    /// An unchanged rewrite must emit byte-identical markdown: the
    /// `generated.at` inheritance keeps the timestamp, so neither git
    /// nor the store sees a phantom new version.
    #[tokio::test]
    async fn an_idempotent_rewrite_keeps_the_file_bytes_and_the_row() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let request = || {
            req(
                ws,
                proj,
                "notes/idem.md",
                "stable body",
                serde_json::json!({"title": "Idem"}),
            )
        };
        let id1 = wiki.write_page(request()).await.unwrap();
        let abs = wiki.project_root(ws, proj).join("notes/idem.md");
        let bytes1 = std::fs::read(&abs).unwrap();

        // Far enough apart that a re-stamped `generated.at` would differ.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let id2 = wiki.write_page(request()).await.unwrap();
        let bytes2 = std::fs::read(&abs).unwrap();

        assert_eq!(id1, id2, "identical rewrite superseded the page");
        assert_eq!(bytes1, bytes2, "identical rewrite changed the file bytes");
    }

    /// A real content change updates `generated.at` — inheritance only
    /// covers the unchanged case.
    #[tokio::test]
    async fn a_content_change_updates_generated_at() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "p", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        wiki.write_page(req(
            ws,
            proj,
            "notes/evolving.md",
            "v1",
            serde_json::json!({"title": "Evolving"}),
        ))
        .await
        .unwrap();
        let abs = wiki.project_root(ws, proj).join("notes/evolving.md");
        let at1 = ai_memory_core::okf::generated_at(
            &crate::markdown::parse(&std::fs::read_to_string(&abs).unwrap())
                .unwrap()
                .frontmatter,
        )
        .unwrap()
        .to_string();

        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        wiki.write_page(req(
            ws,
            proj,
            "notes/evolving.md",
            "v2 - changed",
            serde_json::json!({"title": "Evolving"}),
        ))
        .await
        .unwrap();
        let at2 = ai_memory_core::okf::generated_at(
            &crate::markdown::parse(&std::fs::read_to_string(&abs).unwrap())
                .unwrap()
                .frontmatter,
        )
        .unwrap()
        .to_string();
        assert_ne!(at1, at2, "content change kept the old generated.at");
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
            evidence: Vec::new(),
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

    #[test]
    fn append_under_project_public_api_remains_io_result() {
        let method: fn(&Wiki, WorkspaceId, ProjectId, &str, &[u8]) -> std::io::Result<PathBuf> =
            Wiki::append_under_project;
        let _ = method;
    }

    #[cfg(unix)]
    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(unix)]
    fn assert_confined<T>(result: WikiResult<T>) {
        assert!(
            matches!(result, Err(WikiError::Confinement { .. })),
            "expected confinement refusal"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn page_reads_refuse_leaf_and_ancestor_symlinks_without_fallback_classification() {
        let tmp = TempDir::new().unwrap();
        let (_store, wiki, ws, proj) = scoped(&tmp).await;
        let outside = TempDir::new().unwrap();
        let outside_page = outside.path().join("page.md");
        std::fs::write(&outside_page, "# Outside\n\ncanary\n").unwrap();
        let project = wiki.project_root(ws, proj);
        std::fs::create_dir_all(&project).unwrap();

        let leaf = PagePath::new("leaf.md").unwrap();
        symlink(&outside_page, &wiki.abs_path(ws, proj, &leaf));
        let leaf_error = wiki.read_page(ws, proj, &leaf).unwrap_err();
        assert!(matches!(leaf_error, WikiError::Confinement { .. }));
        assert!(
            !matches!(leaf_error, WikiError::Io(ref error) if error.kind() == std::io::ErrorKind::NotFound)
        );

        let outside_dir = outside.path().join("tree");
        std::fs::create_dir(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("page.md"), "# Outside ancestor\n").unwrap();
        symlink(&outside_dir, &project.join("linked"));
        assert_confined(wiki.read_page(ws, proj, &PagePath::new("linked/page.md").unwrap()));

        let dangling = PagePath::new("dangling.md").unwrap();
        symlink(
            &outside.path().join("missing.md"),
            &wiki.abs_path(ws, proj, &dangling),
        );
        assert_confined(wiki.read_page(ws, proj, &dangling));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn page_mutations_refuse_ancestor_symlink_and_leave_outside_and_store_unchanged() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let outside = TempDir::new().unwrap();
        let project = wiki.project_root(ws, proj);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(outside.path().join("write.md"), "outside canary").unwrap();
        symlink(outside.path(), &project.join("linked"));

        assert_confined(
            wiki.write_page(req(
                ws,
                proj,
                "linked/write.md",
                "blocked",
                serde_json::json!({}),
            ))
            .await,
        );
        assert_confined(
            wiki.apply_batch(vec![req(
                ws,
                proj,
                "linked/batch.md",
                "blocked",
                serde_json::json!({}),
            )])
            .await,
        );
        assert_confined(
            wiki.delete_page(
                ws,
                proj,
                &PagePath::new("linked/write.md").unwrap(),
                None,
                None,
            )
            .await,
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("write.md")).unwrap(),
            "outside canary"
        );
        assert!(!outside.path().join("batch.md").exists());
        assert!(
            store
                .reader
                .search_pages("blocked".into(), 10, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confinement_refusal_happens_before_blocking_and_observer_admission() {
        use axum::Router;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/observe",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::NO_CONTENT
                }),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let tmp = TempDir::new().unwrap();
        let (_store, wiki, ws, proj) = scoped(&tmp).await;
        let outside = TempDir::new().unwrap();
        let project = wiki.project_root(ws, proj);
        std::fs::create_dir_all(&project).unwrap();
        symlink(outside.path(), &project.join("linked"));
        let wiki = wiki.with_admission_chain(
            AdmissionChain::new(vec![WebhookConfig {
                name: "observer".into(),
                url: format!("http://{address}/observe"),
                timeout_ms: 1_000,
                failure_policy: FailurePolicy::Ignore,
                events: vec![AdmissionOp::WritePage, AdmissionOp::Delete],
                blocking: true,
            }])
            .unwrap(),
        );

        assert_confined(
            wiki.write_page(req(
                ws,
                proj,
                "linked/write.md",
                "blocked",
                serde_json::json!({}),
            ))
            .await,
        );
        assert_confined(
            wiki.delete_page(
                ws,
                proj,
                &PagePath::new("linked/delete.md").unwrap(),
                None,
                None,
            )
            .await,
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_and_reindex_refuse_ancestor_symlink_without_touching_canary_or_db() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let committed = PagePath::new("notes/source.md").unwrap();
        wiki.write_page(req(
            ws,
            proj,
            committed.as_str(),
            "safe",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        wiki.commit_all("fixture").unwrap();

        let outside = TempDir::new().unwrap();
        let canary = outside.path().join("target.md");
        std::fs::write(&canary, "canary").unwrap();
        symlink(outside.path(), &wiki.project_root(ws, proj).join("linked"));
        let linked = PagePath::new("linked/target.md").unwrap();
        assert_confined(wiki.reindex_page(ws, proj, linked.clone()).await);
        assert_confined(
            wiki.restore_page_from_checkpoint(ws, proj, linked.clone(), "HEAD")
                .await,
        );
        assert_eq!(std::fs::read_to_string(&canary).unwrap(), "canary");
        assert!(
            store
                .reader
                .page_body_by_ids(ws, proj, linked.as_str())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_wiki_root_is_refused_during_construction() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        symlink(outside.path(), &tmp.path().join("wiki"));
        let store = Store::open(tmp.path()).unwrap();
        assert!(matches!(
            Wiki::new(tmp.path(), store.writer.clone()),
            Err(WikiError::Confinement { .. })
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn linked_or_dangling_scope_roots_are_refused_but_real_roots_work() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        wiki.write_page(req(
            ws,
            proj,
            "notes/control.md",
            "control",
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        let outside = TempDir::new().unwrap();
        let linked_ws = WorkspaceId::new();
        symlink(outside.path(), &wiki.root().join(linked_ws.to_string()));
        assert_confined(
            wiki.write_page(req(
                linked_ws,
                ProjectId::new(),
                "notes/refused.md",
                "blocked",
                serde_json::json!({}),
            ))
            .await,
        );

        let workspace = wiki.root().join(ws.to_string());
        let linked_project = ProjectId::new();
        symlink(outside.path(), &workspace.join(linked_project.to_string()));
        assert_confined(
            wiki.write_page(req(
                ws,
                linked_project,
                "notes/refused.md",
                "blocked",
                serde_json::json!({}),
            ))
            .await,
        );

        let dangling_project = ProjectId::new();
        symlink(
            &outside.path().join("missing"),
            &workspace.join(dangling_project.to_string()),
        );
        assert_confined(
            wiki.write_page(req(
                ws,
                dangling_project,
                "notes/refused.md",
                "blocked",
                serde_json::json!({}),
            ))
            .await,
        );
        assert_eq!(
            wiki.read_page(ws, proj, &PagePath::new("notes/control.md").unwrap())
                .unwrap()
                .body,
            "control"
        );
        assert!(
            store
                .reader
                .search_pages("blocked".into(), 10, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn write_page_embeds_frontmatter_abstract_and_cleans_stale_rows() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let embedder: Arc<dyn ai_memory_llm::Embedder> =
            Arc::new(ai_memory_llm::SyntheticEmbedder::new(64));
        let wiki = wiki.with_embedder(embedder);
        let write_abs = |frontmatter: serde_json::Value| {
            wiki.write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new("notes/abs.md").unwrap(),
                frontmatter,
                body: "alpha bravo".to_string(),
                tier: Tier::Semantic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::anonymous(),
                evidence: Vec::new(),
            })
        };

        write_abs(serde_json::json!({"title": "abs", "abstract": "one-line summary"}))
            .await
            .unwrap();
        let ids = store
            .reader
            .abstract_embedded_page_ids(
                ws,
                proj,
                "synthetic".to_string(),
                "bag-of-words-v1".to_string(),
                64,
            )
            .await
            .unwrap();
        assert_eq!(ids.len(), 1, "abstract embedded at write time");

        // Rewriting without the key leaves the latest version without an
        // abstract row; the superseded version's row stays behind, matching
        // the body embedding's versioning semantics.
        write_abs(serde_json::json!({"title": "abs"}))
            .await
            .unwrap();
        let ids = store
            .reader
            .abstract_embedded_page_ids(
                ws,
                proj,
                "synthetic".to_string(),
                "bag-of-words-v1".to_string(),
                64,
            )
            .await
            .unwrap();
        assert!(
            ids.is_empty(),
            "latest version must not carry an abstract row"
        );
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
                .search_pages_for_project(ws, proj, "proposed body".into(), 10, None)
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
                serde_json::json!([{ "secret": "GH_TOKEN=ghp_FAKEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }]),
            ),
        )
        .await;
        let sidecar = wiki.write_auto_improve_sidecar(ws, proj, id).await.unwrap();
        let content = std::fs::read_to_string(sidecar).unwrap();
        assert!(content.contains("[REDACTED:"));
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

    /// The same page saved with CRLF line endings (a Windows editor, or a
    /// wiki checked out with `core.autocrlf=true`) must index the same way:
    /// its frontmatter is still the source of truth for tier/pinned/title,
    /// not body text ahead of the H1.
    #[tokio::test]
    async fn reindex_page_reads_crlf_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;

        let path = PagePath::new("sessions/crlf.md").unwrap();
        let abs = wiki.abs_path(ws, proj, &path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(
            &abs,
            "---\r\ntitle: S\r\ntier: episodic\r\npinned: true\r\n---\r\nbody\r\n",
        )
        .unwrap();

        wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        let meta = store
            .reader
            .page_meta("default", "scratch", path.as_str())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.title, "S", "the frontmatter title must win");
        assert_eq!(meta.tier, "episodic");
        assert!(meta.pinned, "a CRLF `pinned: true` must still pin the page");
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

        let md = wiki.read_page(ws, proj, &path).unwrap();
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
        let md = wiki.read_page(ws, proj, &batch_path).unwrap();
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
        let md = wiki.read_page(ws, proj, &path).unwrap();
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
        assert!(stored.body.contains("[REDACTED:"));
        assert!(!stored.body.contains("sk-ant-leak"));
        assert!(
            std::fs::read_to_string(wiki.abs_path(
                ws,
                proj,
                &PagePath::new("notes/mutated.md").unwrap()
            ))
            .unwrap()
            .contains("[REDACTED:")
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
            .search_pages("karpathy".into(), 5, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Karpathy LLM Wiki");
        assert!(hits[0].snippet.contains("compile"));
    }

    #[tokio::test]
    async fn decay_eviction_refuses_a_stale_latest_id_without_touching_disk() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("sessions/stale-decay.md").unwrap();
        let stale = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "old candidate",
                serde_json::json!({"tier": "episodic"}),
            ))
            .await
            .unwrap();
        let current = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "new current body",
                serde_json::json!({"tier": "episodic"}),
            ))
            .await
            .unwrap();

        assert!(
            !wiki
                .evict_page_if_latest(ws, proj, &path, stale, None)
                .await
                .unwrap()
        );
        assert!(
            std::fs::read_to_string(wiki.abs_path(ws, proj, &path))
                .unwrap()
                .contains("new current body")
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(current),
        );
    }

    #[tokio::test]
    async fn rejecting_delete_admission_leaves_decay_candidate_live() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/reject",
            post(|| async { (StatusCode::FORBIDDEN, "retention denied") }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("sessions/admission-decay.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "must remain live",
                serde_json::json!({"tier": "episodic"}),
            ))
            .await
            .unwrap();
        let wiki = wiki.with_admission_chain(
            AdmissionChain::new(vec![WebhookConfig {
                name: "retention-guard".into(),
                url: format!("http://{addr}/reject"),
                timeout_ms: 1_000,
                failure_policy: FailurePolicy::Reject,
                events: vec![AdmissionOp::Delete],
                blocking: true,
            }])
            .unwrap(),
        );

        assert!(
            wiki.evict_page_if_latest(ws, proj, &path, page_id, None)
                .await
                .is_err()
        );
        assert!(wiki.abs_path(ws, proj, &path).exists());
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(page_id),
        );
    }

    // --- #929: reconcile-delete tombstone op (`tombstone_missing_page_if_latest`) ---

    #[tokio::test]
    async fn reconcile_tombstone_refuses_a_stale_latest_id() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("sessions/stale-reconcile.md").unwrap();
        let stale = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "old candidate",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        let current = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "new current body",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        std::fs::remove_file(wiki.abs_path(ws, proj, &path)).unwrap();

        assert!(
            !wiki
                .tombstone_missing_page_if_latest(ws, proj, &path, stale)
                .await
                .unwrap(),
            "a stale expected id must leave the current version untouched"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(current),
        );
    }

    /// Design item #6: the final recheck, immediately before acting, must
    /// refuse when the file has actually come back — even though the caller
    /// (the watcher's reconcile pass) only calls this after two consecutive
    /// "missing" observations, a page can still be recreated in the narrow
    /// window between the second observation and this call.
    #[tokio::test]
    async fn reconcile_tombstone_refuses_when_the_file_still_exists() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("sessions/present-reconcile.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "still here",
                serde_json::json!({}),
            ))
            .await
            .unwrap();

        assert!(
            !wiki
                .tombstone_missing_page_if_latest(ws, proj, &path, page_id)
                .await
                .unwrap(),
            "the file exists on disk; the final recheck must refuse to tombstone it"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            Some(page_id),
        );
        assert!(wiki.abs_path(ws, proj, &path).exists());
    }

    /// The tombstone op is a background safety net reacting to a file that is
    /// already gone, not a user-initiated delete: it must never run the
    /// BLOCKING admission gate, which could refuse it. A webhook configured
    /// to reject every `Delete` event proves this by NOT blocking the
    /// tombstone — if `tombstone_missing_page_if_latest` went through the
    /// same path as `evict_page_if_latest`/`delete_page_if_latest`, this
    /// webhook would turn the call into an error
    /// (`rejecting_delete_admission_leaves_decay_candidate_live` shows
    /// exactly that for the decay path). It DOES still fire-and-forget any
    /// non-blocking observer/mirror webhook on success — see
    /// `reconcile_tombstone_dispatches_non_blocking_observer_webhook` below
    /// (#929 review, S6).
    #[tokio::test]
    async fn reconcile_tombstone_does_not_dispatch_admission_webhook() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::post;
        use tokio::net::TcpListener;

        let app = Router::new().route(
            "/reject",
            post(|| async { (StatusCode::FORBIDDEN, "must never be called") }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("sessions/webhook-reconcile.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "vanishing",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        std::fs::remove_file(wiki.abs_path(ws, proj, &path)).unwrap();
        let wiki = wiki.with_admission_chain(
            AdmissionChain::new(vec![WebhookConfig {
                name: "reconcile-guard".into(),
                url: format!("http://{addr}/reject"),
                timeout_ms: 1_000,
                failure_policy: FailurePolicy::Reject,
                events: vec![AdmissionOp::Delete],
                blocking: true,
            }])
            .unwrap(),
        );

        assert!(
            wiki.tombstone_missing_page_if_latest(ws, proj, &path, page_id)
                .await
                .unwrap(),
            "no admission dispatch means the rejecting webhook is never consulted"
        );
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            None,
            "the page is no longer latest",
        );
    }

    /// S6 (#929 review): the tombstone op skips the BLOCKING admission gate
    /// (proven above), but a mirror that never hears about the tombstone at
    /// all would silently diverge from the source of truth — a real
    /// correctness issue on its own. A non-blocking observer webhook must
    /// still be fired, fire-and-forget, on a successful tombstone.
    #[tokio::test]
    async fn reconcile_tombstone_dispatches_non_blocking_observer_webhook() {
        use axum::Router;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::net::TcpListener;

        let observed = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route(
                "/observe",
                post(|State(flag): State<Arc<AtomicBool>>| async move {
                    flag.store(true, Ordering::SeqCst);
                    StatusCode::OK
                }),
            )
            .with_state(observed.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let tmp = TempDir::new().unwrap();
        let (_store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("notes/mirror-reconcile.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "vanishing",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        std::fs::remove_file(wiki.abs_path(ws, proj, &path)).unwrap();
        let wiki = wiki.with_admission_chain(
            AdmissionChain::new(vec![WebhookConfig {
                name: "mirror-observer".into(),
                url: format!("http://{addr}/observe"),
                timeout_ms: 1_000,
                failure_policy: FailurePolicy::Ignore,
                events: vec![AdmissionOp::Delete],
                blocking: false,
            }])
            .unwrap(),
        );

        assert!(
            wiki.tombstone_missing_page_if_latest(ws, proj, &path, page_id)
                .await
                .unwrap()
        );

        // `dispatch_async` fires the request off the caller's path; give it a
        // bounded window to land instead of asserting immediately.
        let mut delivered = false;
        for _ in 0..50 {
            if observed.load(Ordering::SeqCst) {
                delivered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            delivered,
            "the non-blocking observer webhook must be notified of the tombstone"
        );
    }

    /// The op never touches the filesystem (design item #7: there is nothing
    /// to touch — the file is already gone). Proven by the project
    /// directory's file listing being byte-for-byte identical before and
    /// after the call: no quarantine artifact, no recreated file, nothing.
    #[tokio::test]
    async fn reconcile_tombstone_never_touches_the_filesystem() {
        let tmp = TempDir::new().unwrap();
        let (_store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("notes/vanishing-fs.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "gone soon",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        let proj_dir = wiki.project_root(ws, proj);
        std::fs::remove_file(wiki.abs_path(ws, proj, &path)).unwrap();

        let listing_before = list_dir_recursive(&proj_dir);
        assert!(
            wiki.tombstone_missing_page_if_latest(ws, proj, &path, page_id)
                .await
                .unwrap()
        );
        let listing_after = list_dir_recursive(&proj_dir);
        assert_eq!(
            listing_before, listing_after,
            "the tombstone op must not create, remove, or rename any file"
        );
    }

    /// `restore-page`/wiki history depend on this: a reconcile tombstone is
    /// `is_latest = 0` + `superseded_at`, mirroring decay eviction exactly —
    /// and is picked up by the SAME aged-tombstone sweep decay eviction uses
    /// (`hard_delete_after_days`, tier/pin-agnostic), not exempted from it.
    /// Proven by fetching the row back through the same query that sweep
    /// uses ([`ai_memory_store::ReaderPool::decay_tombstones_before`]): if
    /// the row were gone, or if only `is_latest` had flipped without
    /// `superseded_at`, it would not show up there. What actually protects a
    /// reconcile tombstone from that sweep permanently destroying a
    /// self-healed false positive is `upsert_page_in_tx`'s resurrection path
    /// (`ops.rs`): if the file comes back, the new version re-links onto
    /// this chain and clears `superseded_at`, which is covered separately by
    /// `reconcile_tombstone_resurrects_instead_of_orphaning_on_recreate`.
    #[tokio::test]
    async fn reconcile_tombstone_is_a_soft_delete_visible_to_normal_retention() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("notes/tombstone-shape.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "about to vanish",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        std::fs::remove_file(wiki.abs_path(ws, proj, &path)).unwrap();

        assert!(
            wiki.tombstone_missing_page_if_latest(ws, proj, &path, page_id)
                .await
                .unwrap()
        );

        // is_latest = 0: no longer the scope's current version.
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            None,
        );
        // superseded_at set (not just is_latest): the row surfaces through the
        // exact query the decay retention sweep uses to find eligible aged
        // tombstones — proving it is a real tombstone, not merely hidden, and
        // that hard cleanup is left to that same normal retention path.
        let far_future = jiff::Timestamp::now().as_microsecond() + 365 * 24 * 3_600 * 1_000_000;
        let tombstones = store
            .reader
            .decay_tombstones_before(ws, proj, far_future)
            .await
            .unwrap();
        assert!(
            tombstones.iter().any(|t| t.id == page_id && t.path == path),
            "the reconcile tombstone must be indistinguishable from a decay tombstone to the \
             retention sweep: {tombstones:?}",
        );
    }

    fn list_dir_recursive(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    out.push(
                        path.strip_prefix(root)
                            .unwrap_or(&path)
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    #[tokio::test]
    async fn decay_cleanup_refuses_to_reindex_a_symlinked_recreation() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("sessions/symlink-decay.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "evicted body",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert!(
            wiki.evict_page_if_latest(ws, proj, &path, page_id, None)
                .await
                .unwrap()
        );

        let outside = tmp.path().join("outside.md");
        std::fs::write(&outside, "outside secret must not be indexed").unwrap();
        let linked = wiki.abs_path(ws, proj, &path);
        if !create_test_symlink_file(&outside, &linked) {
            return;
        }

        let error = wiki
            .hard_delete_decay_tombstone(ws, proj, &path, page_id, i64::MAX)
            .await
            .expect_err("symlinked recreation must fail closed");
        assert!(matches!(error, WikiError::Confinement { .. }), "{error}");
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
                .await
                .unwrap(),
            None,
        );
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "outside secret must not be indexed"
        );
        assert_eq!(
            store
                .reader
                .decay_tombstones_before(ws, proj, i64::MAX)
                .await
                .unwrap()
                .len(),
            1,
            "cleanup failure must leave the tombstone for a safe retry"
        );
    }

    /// #929: the reindex `hard_delete_decay_tombstone` performs when a
    /// decayed page's file survived and was rewritten externally must NOT
    /// embed the resurrected version. That whole function runs under the
    /// exclusive write lock (`mutation_lock.write()`, held for the entire
    /// call), and embedding can block on the configured provider for an
    /// unbounded time — doing it there would stall every other wiki write
    /// for as long as the provider call takes.
    #[tokio::test]
    async fn decay_cleanup_reindex_does_not_embed_under_the_exclusive_lock() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let embedder: Arc<dyn ai_memory_llm::Embedder> =
            Arc::new(ai_memory_llm::SyntheticEmbedder::new(32));
        let wiki = wiki.with_embedder(embedder);

        let path = PagePath::new("sessions/decay-embed.md").unwrap();
        let page_id = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "original decayed body",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert!(
            wiki.evict_page_if_latest(ws, proj, &path, page_id, None)
                .await
                .unwrap(),
            "precondition: the page is tombstoned by decay"
        );

        // The file survives the tombstone (`evict_page_if_latest` never
        // touches disk) and is rewritten externally before the cleanup
        // runs, so `hard_delete_decay_tombstone` must reindex it fresh
        // rather than reuse the tombstoned version.
        std::fs::write(wiki.abs_path(ws, proj, &path), "resurrected body content\n").unwrap();

        wiki.hard_delete_decay_tombstone(ws, proj, &path, page_id, i64::MAX)
            .await
            .unwrap();

        let resurrected_id = store
            .reader
            .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
            .await
            .unwrap()
            .expect("the resurrected version must be indexed");
        assert_ne!(
            resurrected_id, page_id,
            "the rewrite must have produced a new version, not reused the tombstone"
        );

        let embedded = store
            .reader
            .embedded_page_ids(ws, proj, "synthetic".into(), "bag-of-words-v1".into(), 32)
            .await
            .unwrap();
        assert!(
            !embedded.contains(&resurrected_id),
            "reindexing inside hard_delete_decay_tombstone must not embed \
             while the exclusive lock is held"
        );
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
            on_disk.contains("[REDACTED:"),
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
            .search_pages("REDACTED".into(), 5, None)
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
                evidence: Vec::new(),
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
        let hits = store
            .reader
            .search_pages("batch".into(), 10, None)
            .await
            .unwrap();
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
                evidence: Vec::new(),
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
        assert!(on_disk.contains("[REDACTED:api_key]"), "{on_disk}");
        assert!(!on_disk.contains("sk-1234567890abcdef"), "{on_disk}");

        let hits = store
            .reader
            .search_pages("REDACTED".into(), 5, None)
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
            evidence: Vec::new(),
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
            evidence: Vec::new(),
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
            evidence: Vec::new(),
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
        let mut new_user = NewUser {
            username: "alice".into(),
            name: Some("Alice Smith".into()),
            email: Some("alice@example.com".into()),
        };
        new_user.validate().unwrap();
        let user_id: UserId = store
            .writer
            .create_human_user(new_user, ai_memory_core::UserRole::User, None, false)
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
            evidence: Vec::new(),
        })
        .await
        .unwrap();

        let md = wiki
            .read_page(ws, proj, &PagePath::new("notes/note.md").unwrap())
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

    /// #607 item 2: a purge commits the DB deletion but removes on-disk files
    /// only afterward (best-effort). If that removal is interrupted, the
    /// markdown directory — `_meta.md` included — survives with no `projects`
    /// row. `reindex_all` must NOT resurrect it: the tombstone written in the
    /// purge transaction makes the deletion terminal.
    #[tokio::test]
    async fn reindex_does_not_resurrect_a_purged_scope_whose_files_survived() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
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

        let path = PagePath::new("notes/keep.md").unwrap();
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/keep.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Keep"}),
            body: "survivor".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
        // Manifests reindex reads to recreate scopes from disk.
        wiki.backfill_scope_manifests().await.unwrap();

        // Purge the DB only (store side); the on-disk files intentionally
        // survive — exactly the crash-before-file-removal window.
        store
            .writer
            .purge_project(
                ws,
                proj,
                "default/scratch",
                None,
                false,
                ai_memory_store::Compaction::Skip,
                ai_memory_store::PurgeMode::Commit,
            )
            .await
            .unwrap();
        let meta_on_disk = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join(proj.to_string())
            .join("_meta.md");
        assert!(
            meta_on_disk.exists(),
            "the purge left the manifest on disk (the crash window this guards)"
        );

        let summary = wiki.reindex_all().await.unwrap();

        assert_eq!(
            summary.skipped_purged, 1,
            "the purged scope must be skipped"
        );
        assert_eq!(
            summary.projects, 0,
            "the purged project must not be recreated"
        );
        assert!(
            store
                .reader
                .page_body_by_ids(ws, proj, path.as_str())
                .await
                .unwrap()
                .is_none(),
            "a purged page must not reappear in the index"
        );
    }

    async fn finish_lock_test<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(std::time::Duration::from_secs(5), future)
            .await
            .expect("wiki lock operation must finish without deadlock")
    }

    // Poll the real operation until it queues behind the held page mutex.
    // Unlike a delay, this proves it reached that lock before we release it.
    async fn wait_for_page_waiter<F: std::future::Future>(
        wiki: &Wiki,
        key: &PageKey,
        mut future: std::pin::Pin<&mut F>,
    ) {
        finish_lock_test(std::future::poll_fn(|cx| {
            assert!(
                future.as_mut().poll(cx).is_pending(),
                "operation bypassed the held page lock"
            );
            let locks = wiki.page_locks.lock().unwrap();
            if locks
                .get(key)
                .is_some_and(|lock| Arc::strong_count(lock) >= 3)
            {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        }))
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_page_lock_waits_before_reading_and_allows_another_path() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("notes/held.md").unwrap();
        let other = PagePath::new("notes/other.md").unwrap();
        let original = wiki
            .write_page(req(
                ws,
                proj,
                path.as_str(),
                "original",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        wiki.write_page(req(
            ws,
            proj,
            other.as_str(),
            "control",
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        let global = wiki.mutation_lock.read().await;
        let page = wiki.lock_page(ws, proj, &path).await;
        let abs = wiki.abs_path(ws, proj, &path);
        std::fs::write(&abs, "transientsnapshot\n").unwrap();
        let reindex = wiki.reindex_page(ws, proj, path.clone());
        tokio::pin!(reindex);
        wait_for_page_waiter(&wiki, &(ws, proj, path.clone()), reindex.as_mut()).await;
        assert_eq!(
            store
                .reader
                .latest_page_id_by_ids(ws, proj, path.as_str().into())
                .await
                .unwrap(),
            Some(original),
            "the transient disk snapshot must not enter SQL"
        );

        let other_abs = wiki.abs_path(ws, proj, &other);
        std::fs::write(&other_abs, "independentcontrol\n").unwrap();
        finish_lock_test(wiki.reindex_page(ws, proj, other.clone()))
            .await
            .unwrap();
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, other.as_str())
                .await
                .unwrap()
                .unwrap()
                .body,
            "independentcontrol\n"
        );
        assert_eq!(
            std::fs::read_to_string(&other_abs).unwrap(),
            "independentcontrol\n"
        );

        std::fs::write(&abs, "committedwinner\n").unwrap();
        drop(page);
        drop(global);
        let id = finish_lock_test(reindex).await.unwrap();
        assert_ne!(id, original);
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, path.as_str())
                .await
                .unwrap()
                .unwrap()
                .body,
            "committedwinner\n"
        );
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "committedwinner\n");
        assert!(
            store
                .reader
                .search_pages("transientsnapshot".into(), 10, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_page_lock_serializes_write_through_the_upsert() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("notes/race.md").unwrap();
        wiki.write_page(req(
            ws,
            proj,
            path.as_str(),
            "original",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        std::fs::write(wiki.abs_path(ws, proj, &path), "watcher snapshot\n").unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut watcher = wiki.clone();
        watcher.reindex_read_barrier = Some(barrier.clone());
        let reindex_path = path.clone();
        let reindex =
            tokio::spawn(async move { watcher.reindex_page(ws, proj, reindex_path).await });
        finish_lock_test(barrier.wait()).await;

        let write = wiki.write_page(req(
            ws,
            proj,
            path.as_str(),
            "writer winner",
            serde_json::json!({}),
        ));
        tokio::pin!(write);
        wait_for_page_waiter(&wiki, &(ws, proj, path.clone()), write.as_mut()).await;
        assert_eq!(
            wiki.read_page(ws, proj, &path).unwrap().body,
            "watcher snapshot\n"
        );
        finish_lock_test(barrier.wait()).await;
        let watcher_id = finish_lock_test(reindex).await.unwrap().unwrap();
        let writer_id = finish_lock_test(write).await.unwrap();
        assert_ne!(writer_id, watcher_id);
        assert_eq!(
            wiki.read_page(ws, proj, &path).unwrap().body.trim(),
            "writer winner"
        );
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, proj, path.as_str())
                .await
                .unwrap()
                .unwrap()
                .body,
            "writer winner"
        );
        store
            .reader
            .with_conn(move |conn| {
                let (body, supersedes): (String, Vec<u8>) = conn.query_row(
                    "SELECT body, supersedes FROM pages WHERE id = ?1 AND is_latest = 1",
                    [writer_id.as_bytes()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                assert_eq!(body, "writer winner");
                assert_eq!(supersedes, watcher_id.as_bytes());
                let prior: String = conn.query_row(
                    "SELECT body FROM pages WHERE id = ?1",
                    [watcher_id.as_bytes()],
                    |row| row.get(0),
                )?;
                assert_eq!(
                    prior, "watcher snapshot\n",
                    "the earlier version stays reachable"
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reindex_page_lock_and_reversed_duplicate_batches_do_not_deadlock() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let a = PagePath::new("notes/a.md").unwrap();
        let z = PagePath::new("notes/z.md").unwrap();
        let batch = |paths: &[&PagePath], body: &str| {
            paths
                .iter()
                .map(|path| req(ws, proj, path.as_str(), body, serde_json::json!({})))
                .collect()
        };
        wiki.apply_batch(batch(&[&a, &z], "original"))
            .await
            .unwrap();
        std::fs::write(wiki.abs_path(ws, proj, &z), "watcher snapshot\n").unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut watcher = wiki.clone();
        watcher.reindex_read_barrier = Some(barrier.clone());
        let reindex_path = z.clone();
        let reindex =
            tokio::spawn(async move { watcher.reindex_page(ws, proj, reindex_path).await });
        finish_lock_test(barrier.wait()).await;

        let reverse = wiki.apply_batch(batch(&[&z, &a, &a], "first batch"));
        tokio::pin!(reverse);
        wait_for_page_waiter(&wiki, &(ws, proj, z.clone()), reverse.as_mut()).await;
        let forward = wiki.apply_batch(batch(&[&a, &z], "second batch winner"));
        tokio::pin!(forward);
        wait_for_page_waiter(&wiki, &(ws, proj, a.clone()), forward.as_mut()).await;
        finish_lock_test(barrier.wait()).await;
        let watcher_id = finish_lock_test(reindex).await.unwrap().unwrap();
        let first = finish_lock_test(reverse).await.unwrap();
        let second = finish_lock_test(forward).await.unwrap();
        assert_eq!(
            first[1], first[2],
            "duplicate paths share one lock and no-op version"
        );
        for (path, id) in [(&a, second[0]), (&z, second[1])] {
            assert_eq!(
                wiki.read_page(ws, proj, path).unwrap().body.trim(),
                "second batch winner"
            );
            assert_eq!(
                store
                    .reader
                    .latest_page_id_by_ids(ws, proj, path.as_str().into())
                    .await
                    .unwrap(),
                Some(id)
            );
            assert_eq!(
                store
                    .reader
                    .page_body_by_ids(ws, proj, path.as_str())
                    .await
                    .unwrap()
                    .unwrap()
                    .body,
                "second batch winner"
            );
        }
        store
            .reader
            .with_conn(move |conn| {
                let ancestor: Vec<u8> = conn.query_row(
                    "SELECT supersedes FROM pages WHERE id = ?1",
                    [second[1].as_bytes()],
                    |row| row.get(0),
                )?;
                assert_eq!(ancestor, first[0].as_bytes());
                let ancestor: Vec<u8> = conn.query_row(
                    "SELECT supersedes FROM pages WHERE id = ?1",
                    [first[0].as_bytes()],
                    |row| row.get(0),
                )?;
                assert_eq!(ancestor, watcher_id.as_bytes());
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn reindex_page_lock_releases_both_guards_before_embedding() {
        struct LockCheckingEmbedder {
            wiki: Wiki,
            key: PageKey,
            calls: std::sync::atomic::AtomicUsize,
            inner: ai_memory_llm::SyntheticEmbedder,
        }
        #[async_trait::async_trait]
        impl Embedder for LockCheckingEmbedder {
            fn provider(&self) -> &'static str {
                self.inner.provider()
            }
            fn model(&self) -> &str {
                self.inner.model()
            }
            fn dim(&self) -> u32 {
                self.inner.dim()
            }
            async fn embed(&self, text: &str) -> ai_memory_llm::LlmResult<Vec<f32>> {
                let global = self
                    .wiki
                    .mutation_lock
                    .try_write()
                    .expect("embedding must run outside the global lock");
                let page =
                    finish_lock_test(self.wiki.lock_page(self.key.0, self.key.1, &self.key.2))
                        .await;
                drop(page);
                drop(global);
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.embed(text).await
            }
        }
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj) = scoped(&tmp).await;
        let path = PagePath::new("notes/embed.md").unwrap();
        std::fs::create_dir_all(wiki.abs_path(ws, proj, &path).parent().unwrap()).unwrap();
        std::fs::write(
            wiki.abs_path(ws, proj, &path),
            "---\nabstract: short summary\n---\nembedded body\n",
        )
        .unwrap();
        let embedder = Arc::new(LockCheckingEmbedder {
            wiki: wiki.clone(),
            key: (ws, proj, path.clone()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            inner: ai_memory_llm::SyntheticEmbedder::new(32),
        });
        let wiki = wiki.with_embedder(embedder.clone());
        let id = finish_lock_test(wiki.reindex_page(ws, proj, path))
            .await
            .unwrap();
        assert_eq!(embedder.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            store
                .reader
                .embedded_page_ids(ws, proj, "synthetic".into(), "bag-of-words-v1".into(), 32)
                .await
                .unwrap()
                .contains(&id)
        );
    }

    /// #607 item 3: two concurrent writes to the SAME page path must not
    /// interleave their file-rename and DB-upsert. Per-path serialization
    /// guarantees the winning writer's body lands in both the on-disk file and
    /// the DB `is_latest` row — they never disagree about who won.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_same_path_writes_keep_file_and_db_consistent() {
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

        let path = PagePath::new("notes/hot.md").unwrap();
        let mk = |tag: char| WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/hot.md").unwrap(),
            frontmatter: serde_json::json!({"title": "Hot"}),
            body: format!("body-{tag}"),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
            evidence: Vec::new(),
        };

        // Many rounds so any interleave window is likely to be exercised.
        for _ in 0..25 {
            let (wa, wb) = (wiki.clone(), wiki.clone());
            let (ra, rb) = (mk('a'), mk('b'));
            let ha = tokio::spawn(async move { wa.write_page(ra).await });
            let hb = tokio::spawn(async move { wb.write_page(rb).await });
            ha.await.unwrap().unwrap();
            hb.await.unwrap().unwrap();

            let on_disk = wiki.read_page(ws, proj, &path).unwrap().body;
            let in_db = store
                .reader
                .page_body_by_ids(ws, proj, path.as_str())
                .await
                .unwrap()
                .expect("a latest row exists")
                .body;
            // Compare who won, not exact bytes (emit/parse normalizes newlines):
            // the file and the DB latest row must reflect the same writer.
            assert_eq!(
                on_disk.contains("body-a"),
                in_db.contains("body-a"),
                "file ({on_disk:?}) and DB latest ({in_db:?}) disagree on the winning writer"
            );
        }
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
            evidence: Vec::new(),
        })
        .await
        .unwrap();

        let md = wiki
            .read_page(ws, proj, &PagePath::new("notes/anon.md").unwrap())
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
    /// repairs a tree whose manifests are missing (one written by a release
    /// before scopes described themselves from their first page), then
    /// `reindex_all` on a FRESH store (no DB carried over) recreates the
    /// named scopes + all pages from the wiki tree alone — including a page
    /// that lives at the reserved name `log.md` (kept because it has
    /// frontmatter).
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
                serde_json::json!({"entities": ["NATS JetStream"]}),
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

        // Simulate a tree written by an engine that had no manifests at all
        // (pre-#643 releases), so the backfill is exercised as the repair
        // path it now is — the writes above already made this scope
        // self-describing on their own.
        let ws_dir = src.path().join("wiki").join(ws.to_string());
        std::fs::remove_file(ws_dir.join("_meta.md")).unwrap();
        std::fs::remove_file(ws_dir.join(proj.to_string()).join("_meta.md")).unwrap();

        // Make the wiki self-describing again.
        let written = w1.backfill_scope_manifests().await.unwrap();
        assert!(written >= 2, "ws + proj manifests written, got {written}");
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
            .search_pages("uniquetoken".into(), 5, None)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "reindexed page is searchable in the fresh store"
        );
        let entity_hits = s2
            .reader
            .hybrid_search(
                ws,
                proj,
                "jetstream".into(),
                None,
                String::new(),
                String::new(),
                0,
                5,
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            entity_hits.len(),
            1,
            "reindex must rebuild the entity stream from canonical frontmatter"
        );
        assert_eq!(entity_hits[0].path.as_str(), "notes/a.md");
        drop(s2);
    }

    #[tokio::test]
    async fn promoted_identity_round_trips_through_manifest_and_clean_reindex() {
        let src = TempDir::new().unwrap();
        let source = Store::open(src.path()).unwrap();
        let workspace = source
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let identity = ai_memory_core::repository_identity::RepositoryIdentity {
            identity: "github.com/acme/platform/api".into(),
            source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
        };
        let (project, _) = source
            .writer
            .resolve_project_by_identity(
                workspace,
                identity.clone(),
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "api",
                Some("/src/api".into()),
                None,
                None,
            )
            .await
            .unwrap();
        let wiki = Wiki::new(src.path(), source.writer.clone())
            .unwrap()
            .with_store_reader(source.reader.clone());
        wiki.write_page(req(
            workspace,
            project,
            "notes/history.md",
            "history survives reindex",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        let promoted =
            ai_memory_store::create_explicit_scope(&source.writer, "default", "acme-platform-api")
                .await
                .unwrap();
        assert_eq!(promoted.scope.project_id, project);
        assert_eq!(promoted.promoted_from.as_deref(), Some("api"));
        assert!(
            wiki.refresh_renamed_scope(workspace, project)
                .await
                .unwrap()
                .is_some()
        );
        let manifest_path = src
            .path()
            .join("wiki")
            .join(workspace.to_string())
            .join(project.to_string())
            .join("_meta.md");
        let manifest = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(
            manifest.contains("project: acme-platform-api"),
            "{manifest}"
        );
        assert!(
            manifest.contains("identity: github.com/acme/platform/api"),
            "{manifest}"
        );
        assert!(
            manifest.contains("identity_source: git_remote"),
            "{manifest}"
        );
        assert!(
            manifest.contains("canonical_name: acme-platform-api"),
            "{manifest}"
        );
        assert!(manifest.contains("legacy_name: api"), "{manifest}");
        let source_history = wiki.recent_checkpoints(10).unwrap();
        assert!(
            source_history
                .iter()
                .any(|checkpoint| checkpoint.summary.contains("rename-project"))
        );
        drop(source);

        let dst = TempDir::new().unwrap();
        copy_tree(&src.path().join("wiki"), &dst.path().join("wiki"));
        let rebuilt = Store::open(dst.path()).unwrap();
        let rebuilt_wiki = Wiki::new(dst.path(), rebuilt.writer.clone()).unwrap();
        let summary = rebuilt_wiki.reindex_all().await.unwrap();
        assert_eq!(summary.projects, 1);
        assert_eq!(summary.pages, 1);
        let restored_history = rebuilt_wiki.recent_checkpoints(10).unwrap();
        assert_eq!(
            restored_history
                .iter()
                .map(|checkpoint| &checkpoint.oid)
                .collect::<Vec<_>>(),
            source_history
                .iter()
                .map(|checkpoint| &checkpoint.oid)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            rebuilt
                .reader
                .project_name_by_id(workspace, project)
                .await
                .unwrap()
                .as_deref(),
            Some("acme-platform-api")
        );
        let rebuilt_scope = rebuilt
            .reader
            .scope_row_by_ids(workspace, project)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rebuilt_scope.identity.as_deref(),
            Some("github.com/acme/platform/api")
        );
        assert_eq!(rebuilt_scope.identity_source.as_deref(), Some("git_remote"));
        assert_eq!(
            rebuilt_scope.canonical_name.as_deref(),
            Some("acme-platform-api")
        );
        assert_eq!(rebuilt_scope.legacy_name.as_deref(), Some("api"));
        assert!(
            rebuilt
                .reader
                .page_body_by_ids(workspace, project, "notes/history.md")
                .await
                .unwrap()
                .unwrap()
                .body
                .contains("history survives reindex")
        );
        for key in ["acme-platform-api", "api"] {
            assert_eq!(
                ai_memory_store::lookup_existing_scope(&rebuilt.reader, "default", key)
                    .await
                    .unwrap()
                    .project_id,
                project
            );
        }
        let (captured, resolution) = rebuilt
            .writer
            .resolve_project_by_identity(
                workspace,
                identity,
                ai_memory_core::repository_identity::IdentityStyle::Path,
                "another-checkout",
                Some("/other/api".into()),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(captured, project);
        assert_eq!(resolution, ai_memory_store::IdentityResolution::Matched);
        let count: i64 = rebuilt
            .reader
            .with_conn(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))?)
            })
            .await
            .unwrap();
        assert_eq!(count, 1, "capture must not create a second project UUID");
    }

    #[tokio::test]
    async fn reindex_rejects_project_coordinate_fields_that_do_not_match_identity() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let workspace = WorkspaceId::new();
        let project = ProjectId::new();
        let workspace_dir = tmp.path().join("wiki").join(workspace.to_string());
        let project_dir = workspace_dir.join(project.to_string());
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            workspace_dir.join("_meta.md"),
            "---\nworkspace: default\ntype: Scope Manifest\n---\n",
        )
        .unwrap();
        std::fs::write(
            project_dir.join("_meta.md"),
            "---\nproject: acme-api\nidentity: github.com/acme/api\nidentity_source: git_remote\ncanonical_name: forged-api\nlegacy_name: api\ntype: Scope Manifest\n---\n",
        )
        .unwrap();

        let error = wiki.reindex_all().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("coordinate keys do not match its identity"),
            "{error}"
        );
        assert!(
            store
                .reader
                .scope_row_by_ids(workspace, project)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn identity_manifest_without_derived_keys_reindexes_and_derives_them() {
        let src = TempDir::new().unwrap();
        let source = Store::open(src.path()).unwrap();
        let wiki = Wiki::new(src.path(), source.writer.clone())
            .unwrap()
            .with_store_reader(source.reader.clone());
        let workspace = source
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (project, _) = source
            .writer
            .resolve_project_by_identity(
                workspace,
                ai_memory_core::repository_identity::RepositoryIdentity {
                    identity: "github.com/acme/api".into(),
                    source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
                },
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "api",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        wiki.write_page(req(
            workspace,
            project,
            "notes/a.md",
            "identity history",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        let manifest_path = src
            .path()
            .join("wiki")
            .join(workspace.to_string())
            .join(project.to_string())
            .join("_meta.md");
        std::fs::write(
            &manifest_path,
            "---\nproject: api\nidentity: github.com/acme/api\nidentity_source: git_remote\ntype: Scope Manifest\n---\n",
        ).unwrap();
        drop(source);

        let dst = TempDir::new().unwrap();
        copy_tree(&src.path().join("wiki"), &dst.path().join("wiki"));
        let rebuilt = Store::open(dst.path()).unwrap();
        Wiki::new(dst.path(), rebuilt.writer.clone())
            .unwrap()
            .reindex_all()
            .await
            .unwrap();
        let scope = rebuilt
            .reader
            .scope_row_by_ids(workspace, project)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(scope.canonical_name.as_deref(), Some("acme-api"));
        assert_eq!(scope.legacy_name.as_deref(), Some("api"));
    }

    #[tokio::test]
    async fn old_project_manifest_without_identity_still_reindexes() {
        let src = TempDir::new().unwrap();
        let source = Store::open(src.path()).unwrap();
        let wiki = Wiki::new(src.path(), source.writer.clone())
            .unwrap()
            .with_store_reader(source.reader.clone());
        let workspace = source
            .writer
            .get_or_create_workspace("legacy")
            .await
            .unwrap();
        let project = source
            .writer
            .get_or_create_project(workspace, "old-format", Some("/legacy".into()))
            .await
            .unwrap();
        wiki.write_page(req(
            workspace,
            project,
            "notes/legacy.md",
            "legacy manifest history",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        let manifest_path = src
            .path()
            .join("wiki")
            .join(workspace.to_string())
            .join(project.to_string())
            .join("_meta.md");
        std::fs::write(
            &manifest_path,
            "---\nproject: old-format\nrepo_path: /legacy\ntype: Scope Manifest\n---\n",
        )
        .unwrap();
        drop(source);

        let dst = TempDir::new().unwrap();
        copy_tree(&src.path().join("wiki"), &dst.path().join("wiki"));
        let rebuilt = Store::open(dst.path()).unwrap();
        Wiki::new(dst.path(), rebuilt.writer.clone())
            .unwrap()
            .reindex_all()
            .await
            .unwrap();
        let row = rebuilt
            .reader
            .scope_row_by_ids(workspace, project)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.project_name, "old-format");
        assert_eq!(row.repo_path.as_deref(), Some("/legacy"));
        assert!(row.identity.is_none());
        assert!(row.identity_source.is_none());
        assert!(row.canonical_name.is_none());
        assert!(row.legacy_name.is_none());
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

    /// #643: a scope that first materializes *while the server is up* is
    /// self-describing from its first page, without waiting for the next
    /// startup backfill. The manifests must be byte-identical to the ones
    /// the backfill would have written, or every restart would rewrite them
    /// and churn the wiki's git history.
    #[tokio::test]
    async fn first_write_into_a_new_scope_writes_its_manifests() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());

        // The scope appears after startup — no backfill has run for it.
        let ws = store.writer.get_or_create_workspace("acme").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "webapp", Some("/repo/webapp".into()))
            .await
            .unwrap();
        wiki.write_page(req(ws, proj, "notes/a.md", "alpha", serde_json::json!({})))
            .await
            .unwrap();

        let ws_dir = tmp.path().join("wiki").join(ws.to_string());
        let ws_meta = std::fs::read_to_string(ws_dir.join("_meta.md")).unwrap();
        assert!(ws_meta.contains("workspace: acme"), "{ws_meta}");
        let proj_meta =
            std::fs::read_to_string(ws_dir.join(proj.to_string()).join("_meta.md")).unwrap();
        assert!(proj_meta.contains("project: webapp"), "{proj_meta}");
        assert!(
            proj_meta.contains("repo_path: /repo/webapp"),
            "repo_path is carried, as the backfill carries it: {proj_meta}"
        );
        assert!(proj_meta.contains("type: Scope Manifest"), "{proj_meta}");

        assert_eq!(
            wiki.backfill_scope_manifests().await.unwrap(),
            0,
            "a later backfill finds nothing to write; the two emitters agree byte for byte"
        );
    }

    /// #643 end-to-end: stop the server inside the window the issue
    /// describes — a project created after startup, so no backfill has ever
    /// seen it — and `reindex` still rebuilds it from the wiki tree alone.
    /// Before the manifest was written with the first page, this aborted with
    /// a bare `No such file or directory (os error 2)`.
    #[tokio::test]
    async fn scope_created_after_startup_reindexes_without_a_restart() {
        let src = TempDir::new().unwrap();
        let s1 = Store::open(src.path()).unwrap();
        let w1 = Wiki::new(src.path(), s1.writer.clone())
            .unwrap()
            .with_store_reader(s1.reader.clone());
        // Startup backfill: the server has seen nothing yet, so it writes
        // nothing. The scope below is created afterwards, mid-run.
        assert_eq!(w1.backfill_scope_manifests().await.unwrap(), 0);

        let ws = s1.writer.get_or_create_workspace("acme").await.unwrap();
        let proj = s1
            .writer
            .get_or_create_project(ws, "webapp", None)
            .await
            .unwrap();
        w1.write_page(req(
            ws,
            proj,
            "notes/a.md",
            "alpha uniquetoken",
            serde_json::json!({}),
        ))
        .await
        .unwrap();
        drop(s1);

        // Fresh store, wiki tree only — the recovery the operator runs.
        let dst = TempDir::new().unwrap();
        let s2 = Store::open(dst.path()).unwrap();
        copy_tree(&src.path().join("wiki"), &dst.path().join("wiki"));
        let w2 = Wiki::new(dst.path(), s2.writer.clone()).unwrap();
        let summary = w2.reindex_all().await.unwrap();

        assert_eq!(summary.projects, 1);
        assert_eq!(summary.pages, 1);
        assert_eq!(
            s2.reader.workspace_name_by_id(ws).await.unwrap().as_deref(),
            Some("acme"),
            "workspace name recovered from the manifest written mid-run"
        );
        assert_eq!(
            s2.reader
                .project_name_by_id(ws, proj)
                .await
                .unwrap()
                .as_deref(),
            Some("webapp"),
        );
    }

    /// The watcher's reconcile pre-check runs outside the mutation guard, so
    /// it deliberately ensures store rows ONLY. Writing a manifest there
    /// could drop a file into a directory a concurrent project move is
    /// renaming away, and buys nothing: a directory it can see already got
    /// its manifest with its first page, or from the backfill.
    #[tokio::test]
    async fn reconcile_scope_check_does_not_write_manifests() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store.writer.get_or_create_workspace("acme").await.unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "webapp", None)
            .await
            .unwrap();

        wiki.ensure_project_scope_rows(ws, proj).await.unwrap();

        let ws_dir = tmp.path().join("wiki").join(ws.to_string());
        assert!(!ws_dir.join("_meta.md").exists());
        assert!(!ws_dir.join(proj.to_string()).join("_meta.md").exists());
    }

    /// Post-audit regression: manifests are OKF-typed at the writer
    /// choke point, and a typeless manifest (the tug-of-war era, or a
    /// hand edit) is HEALED by the next backfill instead of reverting
    /// the migration's typing.
    #[tokio::test]
    async fn backfill_types_manifests_and_heals_typeless_ones() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("w").await.unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());

        wiki.backfill_scope_manifests().await.unwrap();
        let meta_path = tmp
            .path()
            .join("wiki")
            .join(ws.to_string())
            .join("_meta.md");
        let meta = std::fs::read_to_string(&meta_path).unwrap();
        assert!(meta.contains("type: Scope Manifest"), "{meta}");

        // Second backfill: byte-stable, no churn.
        assert_eq!(wiki.backfill_scope_manifests().await.unwrap(), 0);

        // A typeless manifest (pre-fix state) is healed, not preserved.
        std::fs::write(&meta_path, "---\nworkspace: w\n---\n").unwrap();
        assert_eq!(wiki.backfill_scope_manifests().await.unwrap(), 1);
        let healed = std::fs::read_to_string(&meta_path).unwrap();
        assert!(healed.contains("type: Scope Manifest"), "{healed}");
    }

    #[tokio::test]
    async fn reindex_names_a_missing_workspace_manifest() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        let ws_dir = tmp.path().join("wiki").join(ws.to_string());
        let missing = ws_dir.join("_meta.md");
        std::fs::create_dir_all(ws_dir.join(proj.to_string())).unwrap();

        let err = wiki.reindex_all().await.unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("scope manifest {} is missing", missing.display())
        );
    }

    #[tokio::test]
    async fn reindex_names_a_missing_project_manifest() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        let ws_dir = tmp.path().join("wiki").join(ws.to_string());
        let proj_dir = ws_dir.join(proj.to_string());
        let missing = proj_dir.join("_meta.md");
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            ws_dir.join("_meta.md"),
            "---\nworkspace: acme\ntype: Scope Manifest\n---\n",
        )
        .unwrap();

        let err = wiki.reindex_all().await.unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("scope manifest {} is missing", missing.display())
        );
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
            matches!(err, WikiError::Confinement { .. }),
            "reindex must reject symlinked manifests, got {err:#}"
        );
    }

    /// Frontmatter `entities:` round-trips through the write path and is
    /// normalised on the way into the index. A non-array value is a
    /// structural error; bad *items* are dropped, because entities are a
    /// ranking signal and one long hand-typed entry must not cost the page.
    #[test]
    fn parse_entities_normalises_and_rejects_non_arrays() {
        let path = PagePath::new("concepts/x.md").unwrap();

        assert!(
            parse_entities(&path, &serde_json::json!({}))
                .unwrap()
                .is_empty(),
            "absent key means no entities",
        );
        assert_eq!(
            parse_entities(
                &path,
                &serde_json::json!({"entities": ["SQLite", "  sqlite ", "Writer\nActor", 42]}),
            )
            .unwrap(),
            vec!["sqlite".to_string(), "writer actor".to_string()],
            "lowercased, whitespace-collapsed, de-duplicated, non-strings dropped",
        );
        assert!(
            parse_entities(
                &path,
                &serde_json::json!({"entities": ["ok", "x".repeat(200)]}),
            )
            .unwrap()
                == vec!["ok".to_string()],
            "over-long items are dropped, not fatal",
        );
        let err = parse_entities(&path, &serde_json::json!({"entities": "sqlite"}))
            .expect_err("a string instead of a list is a structural error");
        assert!(err.to_string().contains("non-array entities"), "{err}");
    }

    /// Entities are derived from `tags` too, so the index (and `as_of`)
    /// populate on real stores whose pages predate LLM entity extraction.
    #[test]
    fn parse_entities_derives_from_tags() {
        let path = PagePath::new("concepts/x.md").unwrap();

        // Tags alone populate entities — the common case on a mature store.
        assert_eq!(
            parse_entities(&path, &serde_json::json!({"tags": ["Storage", "SQLite"]})).unwrap(),
            vec!["storage".to_string(), "sqlite".to_string()],
            "tags become entities, normalised",
        );

        // Explicit entities come first (they win the cap), tags fill in,
        // and a tag duplicating an entity is de-duplicated.
        assert_eq!(
            parse_entities(
                &path,
                &serde_json::json!({"entities": ["FTS5"], "tags": ["fts5", "search"]}),
            )
            .unwrap(),
            vec!["fts5".to_string(), "search".to_string()],
            "explicit first, tags merged, duplicates dropped",
        );

        // A malformed tags list is lenient — it must never fail a write —
        // while a malformed entities list still errors.
        assert_eq!(
            parse_entities(
                &path,
                &serde_json::json!({"entities": ["ok"], "tags": "not-a-list"}),
            )
            .unwrap(),
            vec!["ok".to_string()],
            "non-array tags contribute nothing rather than erroring",
        );
    }

    /// Two projects in one workspace, one ended session in the first with a
    /// consolidated `sessions/<id>.md` page written through the wiki.
    async fn session_with_page(
        tmp: &TempDir,
    ) -> (
        Store,
        Wiki,
        WorkspaceId,
        ProjectId,
        ProjectId,
        SessionId,
        PagePath,
    ) {
        let (store, wiki, ws, src) = scoped(tmp).await;
        let dst = store
            .writer
            .get_or_create_project(ws, "target", None)
            .await
            .unwrap();
        let sid = SessionId::new();
        store
            .writer
            .begin_session(ai_memory_core::NewSession {
                occurred_at: None,
                id: sid,
                workspace_id: ws,
                project_id: src,
                agent_kind: ai_memory_core::AgentKind::ClaudeCode,
                cwd: Some("/repo/src".into()),
                actor_user: None,
            })
            .await
            .unwrap();
        store.writer.end_session(sid, None).await.unwrap();
        let path = PagePath::new(format!("sessions/{sid}.md")).unwrap();
        wiki.write_page(req(
            ws,
            src,
            path.as_str(),
            "consolidated session body",
            serde_json::json!({ "title": "Session" }),
        ))
        .await
        .unwrap();
        (store, wiki, ws, src, dst, sid, path)
    }

    #[tokio::test]
    async fn purge_session_waits_for_in_flight_reindex_before_deleting_rows() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj, _, sid, path) = session_with_page(&tmp).await;
        let page_id = wiki.reindex_page(ws, proj, path.clone()).await.unwrap();
        store.writer.end_session(sid, Some(page_id)).await.unwrap();

        // Model a watcher that has acquired the shared guard but has not
        // indexed its file yet. Poll the purge once, then fence the writer
        // queue: any SQL it submitted must finish before this command replies.
        let reader_guard = wiki.mutation_lock.read().await;
        let purge = wiki.purge_session(ws, proj, sid, None, ai_memory_store::Compaction::Skip);
        tokio::pin!(purge);
        tokio::select! {
            biased;
            result = &mut purge => panic!("purge bypassed an active reader: {result:?}"),
            () = std::future::ready(()) => {}
        }
        store
            .writer
            .get_or_create_workspace("queue-fence")
            .await
            .unwrap();
        assert!(
            store
                .reader
                .find_session_scope(sid)
                .await
                .unwrap()
                .is_some(),
            "purge must wait for the wiki guard before deleting SQL rows"
        );
        // Finish the already-admitted reindex without recursively locking.
        wiki.reindex_page_locked(ws, proj, path.clone())
            .await
            .unwrap();
        drop(reader_guard);
        let outcome = purge.await.unwrap();
        assert_eq!(outcome.files_deleted, vec![path.clone()]);
        assert!(outcome.files_failed.is_empty());
        assert!(
            store
                .reader
                .find_session_scope(sid)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!wiki.abs_path(ws, proj, &path).exists());
        assert!(wiki.reindex_page(ws, proj, path).await.is_err());
        assert!(
            store
                .reader
                .search_pages("consolidated".into(), 10, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The writer's half of the same claim. `purge_session` holds the
    /// exclusive guard across SQL and cleanup — proved by
    /// `purge_session_waits_for_in_flight_reindex_before_deleting_rows` — and
    /// `write_page` takes the shared one, so a write cannot install its file
    /// while a purge owns the guard and would otherwise delete it during
    /// cleanup. Removing the shared guard from `write_page` makes this fail.
    #[tokio::test]
    async fn page_write_cannot_land_while_the_purge_guard_is_held() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj, _, _sid, _path) = session_with_page(&tmp).await;
        let target = PagePath::new("decisions/keep.md").unwrap();
        let abs = wiki.abs_path(ws, proj, &target);

        let purge_guard = wiki.mutation_lock.write().await;
        let write = wiki.write_page(req(
            ws,
            proj,
            target.as_str(),
            "kept body",
            serde_json::json!({ "title": "Keep" }),
        ));
        tokio::pin!(write);
        // A single poll would only prove the write did not finish in one
        // step, which is true even without the guard: `write_page` parks on
        // the writer queue before it touches disk. Give it real time instead.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut write)
                .await
                .is_err(),
            "the page write must not complete while a purge owns the guard"
        );
        assert!(
            !abs.exists(),
            "no page file may be installed while a purge owns the guard"
        );

        drop(purge_guard);
        write.await.unwrap();
        assert!(abs.exists(), "the write completes once the purge releases");
        assert!(
            !store
                .reader
                .search_pages("kept".into(), 10, None)
                .await
                .unwrap()
                .is_empty(),
            "disk and index agree after the guard is released"
        );
    }

    #[tokio::test]
    async fn purge_session_store_failure_preserves_page_file() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj, other, sid, path) = session_with_page(&tmp).await;
        let abs = wiki.abs_path(ws, proj, &path);
        let before = std::fs::read(&abs).unwrap();
        let result = wiki
            .purge_session(ws, other, sid, None, ai_memory_store::Compaction::Skip)
            .await;
        assert!(matches!(
            result,
            Err(WikiError::Store(ai_memory_store::StoreError::NotFound(_)))
        ));
        assert_eq!(std::fs::read(&abs).unwrap(), before);
        assert!(
            store
                .reader
                .find_session_scope(sid)
                .await
                .unwrap()
                .is_some()
        );
    }

    fn leftover_tempfiles(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with(".ai-memory-tmp."))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn move_session_page_moves_file_and_rows() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, src, dst, sid, path) = session_with_page(&tmp).await;

        let outcome = wiki
            .move_session_page(sid, (ws, src), (ws, dst), PagesMode::Move, None, None)
            .await
            .unwrap();
        assert_eq!(outcome.file, SessionPageFile::Moved);
        assert!(outcome.summary.session_moved);
        assert_eq!(outcome.summary.page_versions_moved, 1);

        assert!(
            !wiki.abs_path(ws, src, &path).exists(),
            "source file must be gone"
        );
        let moved = std::fs::read_to_string(wiki.abs_path(ws, dst, &path)).unwrap();
        assert!(moved.contains("consolidated session body"));
        assert_eq!(
            store.reader.session_project_ids(sid).await.unwrap(),
            Some((ws, dst))
        );
        assert!(
            store
                .reader
                .page_body_by_ids(ws, dst, path.as_str())
                .await
                .unwrap()
                .is_some(),
            "latest page row must now sit in the destination"
        );
        assert!(
            store
                .reader
                .page_body_by_ids(ws, src, path.as_str())
                .await
                .unwrap()
                .is_none()
        );
        // The moved file is byte-identical to what the store indexed, so the
        // watcher's follow-up reindex in the destination is a no-op.
        wiki.reindex_page(ws, dst, path.clone()).await.unwrap();
        assert_eq!(
            store
                .reader
                .page_body_by_ids(ws, dst, path.as_str())
                .await
                .unwrap()
                .unwrap()
                .body,
            "consolidated session body"
        );
    }

    #[tokio::test]
    async fn move_session_page_regenerate_removes_source_file() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, src, dst, sid, path) = session_with_page(&tmp).await;

        let outcome = wiki
            .move_session_page(sid, (ws, src), (ws, dst), PagesMode::Regenerate, None, None)
            .await
            .unwrap();
        assert_eq!(outcome.file, SessionPageFile::Removed);
        assert_eq!(outcome.summary.pages_regenerated, 1);

        let src_abs = wiki.abs_path(ws, src, &path);
        assert!(!src_abs.exists(), "retired page file must not linger");
        assert!(!wiki.abs_path(ws, dst, &path).exists());
        assert!(
            leftover_tempfiles(src_abs.parent().unwrap()).is_empty(),
            "parked copy must be deleted after the store commit"
        );
        assert!(
            store
                .reader
                .page_body_by_ids(ws, src, path.as_str())
                .await
                .unwrap()
                .is_none(),
            "no latest version left in the source"
        );
        assert_eq!(
            store.reader.session_project_ids(sid).await.unwrap(),
            Some((ws, dst))
        );
    }

    #[tokio::test]
    async fn move_session_page_puts_file_back_when_store_refuses() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, src, dst, sid, path) = session_with_page(&tmp).await;
        // A latest page row at the same path in the destination, without a
        // file, so the disk step succeeds and only the SQL step refuses.
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: dst,
                path: path.clone(),
                title: "taken".into(),
                body: "already here".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: vec![],
                author_id: None,
                expires_at: None,
                entities: vec![],
                evidence: Vec::new(),
            })
            .await
            .unwrap();

        let err = wiki
            .move_session_page(sid, (ws, src), (ws, dst), PagesMode::Move, None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                WikiError::Store(ai_memory_store::StoreError::PagePathTaken { .. })
            ),
            "unexpected error: {err}"
        );
        assert!(
            wiki.abs_path(ws, src, &path).exists(),
            "source file must be back after the store refused"
        );
        assert!(!wiki.abs_path(ws, dst, &path).exists());
        assert_eq!(
            store.reader.session_project_ids(sid).await.unwrap(),
            Some((ws, src))
        );
    }

    #[tokio::test]
    async fn move_session_page_refuses_existing_destination_file() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, src, dst, sid, path) = session_with_page(&tmp).await;
        wiki.write_page(req(
            ws,
            dst,
            path.as_str(),
            "destination already has this page",
            serde_json::json!({}),
        ))
        .await
        .unwrap();

        let err = wiki
            .move_session_page(sid, (ws, src), (ws, dst), PagesMode::Move, None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, WikiError::DestinationPageExists(_)),
            "unexpected error: {err}"
        );
        assert!(wiki.abs_path(ws, src, &path).exists());
        assert_eq!(
            store.reader.session_project_ids(sid).await.unwrap(),
            Some((ws, src)),
            "nothing may move when the destination file exists"
        );
    }

    #[tokio::test]
    async fn move_session_page_without_file_reports_absent() {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, src, dst, sid, path) = session_with_page(&tmp).await;
        std::fs::remove_file(wiki.abs_path(ws, src, &path)).unwrap();

        let outcome = wiki
            .move_session_page(sid, (ws, src), (ws, dst), PagesMode::Move, None, None)
            .await
            .unwrap();
        assert_eq!(outcome.file, SessionPageFile::Absent);
        // The rows still move even though the file was already gone.
        assert_eq!(outcome.summary.page_versions_moved, 1);
        assert_eq!(
            store.reader.session_project_ids(sid).await.unwrap(),
            Some((ws, dst))
        );
    }

    #[tokio::test]
    async fn write_page_and_apply_batch_refuse_git_reserved_and_non_portable_paths() {
        let tmp = TempDir::new().unwrap();
        let (_store, wiki, ws, proj) = scoped(&tmp).await;

        for bad in [
            "CON.md",
            "notes/aux.md",
            ".git",
            ".git/config",
            "notes/.git",
            "notes/.git/sub.md",
            "notes/.GIT/sub.md",
            "notes/git~1",
            "notes/git~1/foo.md",
            "notes/GIT~2/bar.md",
        ] {
            let bad_path = PagePath::new(bad).unwrap();
            let write_err = wiki
                .write_page(req(
                    ws,
                    proj,
                    bad_path.as_str(),
                    "content",
                    serde_json::json!({}),
                ))
                .await
                .unwrap_err();
            assert!(
                matches!(
                    write_err,
                    WikiError::Memory(ai_memory_core::MemoryError::InvalidPagePath(_))
                ),
                "write_page should refuse {bad:?}, got: {write_err}"
            );

            let batch_err = wiki
                .apply_batch(vec![req(
                    ws,
                    proj,
                    bad_path.as_str(),
                    "content",
                    serde_json::json!({}),
                )])
                .await
                .unwrap_err();
            assert!(
                matches!(
                    batch_err,
                    WikiError::Memory(ai_memory_core::MemoryError::InvalidPagePath(_))
                ),
                "apply_batch should refuse {bad:?}, got: {batch_err}"
            );

            assert!(
                !wiki.abs_path(ws, proj, &bad_path).exists(),
                "file for {bad:?} must not be written to disk"
            );
        }
    }
}
