//! Pluggable content source-of-truth backend.
//!
//! The engine (`ai-memory-wiki`) orchestrates every page mutation —
//! sanitizing, admission webhooks, title/link derivation, embeddings —
//! and then hands the final page to a [`ContentBackend`] to persist it
//! in the content source of truth *and* the derived SQLite index.
//!
//! Two implementations exist:
//! - the filesystem backend in `ai-memory-wiki` (markdown under
//!   `<data_dir>/wiki/` + index row, atomic with rollback) — the
//!   default, historical behaviour;
//! - the Outl adapter in `src/adapters/outl` (op-log append into an
//!   Outl workspace + index row, eventually consistent — a failed index
//!   write is healed by the ops reconciler instead of rolled back).
//!
//! Reads of page *content* go through [`ContentBackend::read_page`] so
//! the engine never assumes markdown files exist on disk. Everything
//! else (search, recency, graph, decay, embeddings) reads the SQLite
//! index in both modes and is deliberately NOT part of this trait.

use std::path::PathBuf;

use ai_memory_core::{NewPage, PageId, PagePath, ProjectId, UserId, WorkspaceId};
use async_trait::async_trait;

use crate::{MoveSummary, StoreError};

/// Error surface for [`ContentBackend`] operations.
#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    /// Filesystem-level failure (atomic write, quarantine, dir removal).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Index (SQLite) failure surfaced by the writer.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Stored content could not be parsed (e.g. malformed frontmatter).
    #[error("content parse: {0}")]
    Parse(String),
    /// A move refused to overwrite an existing destination.
    #[error("destination already exists: {0}")]
    DestinationExists(String),
    /// Backend-specific failure (e.g. Outl op-log append or lock).
    #[error("content backend: {0}")]
    Backend(String),
}

/// Result alias for [`ContentBackend`] operations.
pub type ContentResult<T> = Result<T, ContentError>;

/// A page's content as stored in the source of truth: detached
/// frontmatter (JSON, `Null` when absent) plus the markdown body.
#[derive(Debug, Clone, PartialEq)]
pub struct PageContent {
    /// Frontmatter as JSON. `Null` when the page has none.
    pub frontmatter: serde_json::Value,
    /// Markdown body excluding any frontmatter block.
    pub body: String,
}

/// Where the content source of truth lives and how it is mutated.
///
/// Contract notes:
/// - `persist_*` receives the fully orchestrated page (sanitized,
///   admitted, title/links derived) plus the rendered markdown document
///   (frontmatter + body) for backends that store the emitted form.
/// - Implementations own their atomicity model. The filesystem backend
///   compensates (file rollback on index failure); the Outl backend is
///   eventually consistent (SoT first, index healed by reconcile).
/// - Callers hold the engine's mutation lock across these calls; the
///   backend must not re-enter the engine.
#[async_trait]
pub trait ContentBackend: Send + Sync {
    /// Persist one page in the SoT and upsert the derived index row.
    /// Returns the id of the page version that is now `is_latest = 1`.
    async fn persist_page(&self, page: NewPage, rendered: String) -> ContentResult<PageId>;

    /// Persist a batch of pages. The index rows commit in one
    /// transaction; SoT staging/rollback semantics are per-backend.
    async fn persist_pages_batch(
        &self,
        pages: Vec<(NewPage, String)>,
    ) -> ContentResult<Vec<PageId>>;

    /// Delete a single page from the SoT and drop its index rows.
    /// Idempotent when the page does not exist.
    async fn delete_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        author_id: Option<UserId>,
    ) -> ContentResult<()>;

    /// Remove a project's content from the SoT (directory / trash).
    /// Index purging is the caller's concern (admin flows purge rows
    /// separately with audit + admission).
    async fn remove_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> ContentResult<()>;

    /// Remove a workspace's content from the SoT, including every
    /// project under it. Best-effort: missing content is not an error.
    async fn remove_workspace(&self, workspace_id: WorkspaceId) -> ContentResult<()>;

    /// Move a project between workspaces in the SoT and re-stamp the
    /// index rows. Returns the index re-stamp summary.
    async fn move_project(
        &self,
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
    ) -> ContentResult<MoveSummary>;

    /// Read a page's current content from the SoT (or, for eventually
    /// consistent backends, from the index it keeps in sync).
    async fn read_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: &PagePath,
    ) -> ContentResult<PageContent>;

    /// Root directory of the on-disk SoT when there is one. The
    /// filesystem backend returns the wiki root (used by the watcher,
    /// reindex, and git checkpoints); the Outl backend returns `None`
    /// so those filesystem-coupled subsystems know to stand down.
    fn fs_root(&self) -> Option<PathBuf>;
}

/// Everything an adapter gets to build its [`ContentBackend`].
///
/// The engine hands each adapter the same toolbox: the index writer +
/// reader (the SQLite index stays authoritative for search/recency/
/// graph in every mode), the built-in filesystem backend (for adapters
/// that wrap or mirror it, e.g. shadow modes), and the adapter's own
/// raw `[storage.<name>]` config section to parse however it wants.
pub struct AdapterContext {
    /// Operator home (already normalised by config load), for `~`
    /// expansion in adapter-local paths.
    pub home_dir: Option<String>,
    /// Index writer — every adapter still writes index rows through it.
    pub writer: crate::WriterHandle,
    /// Index reader — for adapters that serve reads from the index.
    pub reader: crate::ReaderPool,
    /// The built-in filesystem backend (markdown wiki + index), for
    /// adapters that keep it as primary and mirror (shadow modes).
    pub fs_backend: std::sync::Arc<dyn ContentBackend>,
    /// Raw `[storage.<adapter-name>]` TOML section as JSON. `Null` when
    /// the operator wrote no section.
    pub settings: serde_json::Value,
}

/// A built adapter: the backend plus any background tasks it runs
/// (reconcilers, sync loops). The engine keeps the handles alive for
/// the lifetime of `serve` and aborts them on shutdown.
pub struct BuiltBackend {
    /// The content backend to install into the engine.
    pub backend: std::sync::Arc<dyn ContentBackend>,
    /// Background loops owned by the adapter.
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Compile-time plugin point for content-backend adapters.
///
/// To ship a new adapter:
/// 1. add a module under `src/adapters/<name>/` implementing
///    [`ContentBackend`] and this factory — see `src/adapters/outl` for
///    the reference implementation;
/// 2. gate it behind a cargo feature carrying its extra dependencies
///    and list its factory in `crate::adapters::registry`;
/// 3. operators select it with `[storage] backend = "<your name>"` and
///    configure it under `[storage.<your name>]`.
///
/// `backend = "sqlite"` is NOT an adapter — it is the built-in default
/// (markdown wiki + SQLite index) and short-circuits the registry.
#[async_trait]
pub trait ContentBackendFactory: Send + Sync {
    /// Name operators put in `[storage] backend = "<name>"`. Lowercase,
    /// stable, unique across the registry (`"sqlite"` is reserved).
    fn name(&self) -> &'static str;

    /// Parse `ctx.settings`, open the backing store, spawn any
    /// background tasks, and return the built backend. Called once at
    /// boot; fail loudly on misconfiguration.
    async fn build(&self, ctx: AdapterContext) -> ContentResult<BuiltBackend>;
}
