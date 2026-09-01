//! Single-writer SQLite actor.
//!
//! Every mutating SQL statement flows through one dedicated OS thread that
//! owns the writer [`rusqlite::Connection`]. Callers send [`WriteCmd`]
//! variants over an mpsc channel and receive results back through a
//! `oneshot`. This pattern eliminates the `database is locked` failure
//! mode that bit cognee (#2717) — there is exactly one writer at all
//! times, by construction.

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use ai_memory_core::{
    AgentKind, HandoffAcceptance, HandoffId, IdentityKey, ManagedRunId, NewHandoff, NewObservation,
    NewPage, NewSession, NewUser, ObservationId, OwnerFilter, PageId, PagePath, ProjectId,
    Sanitized, SessionId, UserId, WorkspaceId,
};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::auto_improve::{
    ApproveAutoImproveProposal, ApproveAutoImproveProposalResult, FailAutoImproveProposal,
    RejectAutoImproveProposal, StageAutoImproveRun, StagedAutoImproveRun,
    StagedAutoImproveRunReport,
};
use crate::error::{StoreError, StoreResult};
use crate::ops::{
    self, AdmittedSession, DeleteWorkspaceSummary, EmbeddingWrite, HookSessionAdmission,
    IngestObservationOutcome, LifecycleOnlyEndOutcome, MoveSessionSummary, MoveSummary,
    ObservationPruneOutcome, PagesMode, PurgeSummary, ReorgSummary,
};
use crate::session_consolidation::SessionConsolidationJob;
use crate::users::{self, TOKEN_HASH_LEN};
use crate::workstream::{
    FinishWorkstreamRun, FinishedWorkstreamRun, PrepareWorkstreamRun, PreparedWorkstreamRun,
    RenameWorkstream, RenamedWorkstream,
};

/// Result of atomically claiming the startup context assembled for one hook.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartupContextAcceptance {
    /// The requested single-use handoff changed from open to accepted.
    pub handoff_accepted: bool,
    /// The requested managed synchronization packet changed to delivered.
    pub managed_context_accepted: bool,
}

/// Commands accepted by the writer thread.
pub(crate) enum WriteCmd {
    GetOrCreateWorkspace {
        name: String,
        reply: oneshot::Sender<StoreResult<WorkspaceId>>,
    },
    GetOrCreateProject {
        workspace_id: WorkspaceId,
        name: String,
        repo_path: Option<String>,
        reply: oneshot::Sender<StoreResult<ProjectId>>,
    },
    EnsureProjectWorkspace {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EnsureWorkspaceWithId {
        id: WorkspaceId,
        name: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EnsureProjectWithId {
        id: ProjectId,
        workspace_id: WorkspaceId,
        name: String,
        repo_path: Option<String>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    UpsertPage {
        page: NewPage,
        reply: oneshot::Sender<StoreResult<PageId>>,
    },
    UpsertPageBatch {
        pages: Vec<NewPage>,
        reply: oneshot::Sender<StoreResult<Vec<PageId>>>,
    },
    DeletePage {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        /// Authenticated operator recorded in the `audit_log` row (NULL when
        /// single-user / unauthenticated).
        author_id: Option<ai_memory_core::UserId>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    DeletePageIfLatest {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        expected_latest_id: PageId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    BeginSession {
        session: NewSession,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EndSession {
        session_id: SessionId,
        summary_page_id: Option<PageId>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EndSessionWithHandoff {
        session_id: SessionId,
        summary_page_id: Option<PageId>,
        handoff: NewHandoff,
        reply: oneshot::Sender<StoreResult<HandoffId>>,
    },
    EndLifecycleOnlySession {
        session_id: SessionId,
        reply: oneshot::Sender<StoreResult<LifecycleOnlyEndOutcome>>,
    },
    SweepHollowProjects {
        min_age_days: u32,
        reply: oneshot::Sender<StoreResult<Vec<String>>>,
    },
    InsertObservation {
        obs: NewObservation,
        reply: oneshot::Sender<StoreResult<ObservationId>>,
    },
    InsertObservationIngest {
        obs: NewObservation,
        ingest_key: String,
        reply: oneshot::Sender<StoreResult<IngestObservationOutcome>>,
    },
    AdmitHookSessionEvent {
        session: NewSession,
        obs: NewObservation,
        owner_filter: OwnerFilter,
        ingest_key: Option<String>,
        reply: oneshot::Sender<StoreResult<HookSessionAdmission>>,
    },
    EndAdmittedSession {
        admitted: AdmittedSession,
        summary_page_id: Option<PageId>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EndAdmittedSessionWithHandoff {
        admitted: AdmittedSession,
        summary_page_id: Option<PageId>,
        handoff: NewHandoff,
        reply: oneshot::Sender<StoreResult<HandoffId>>,
    },
    EndAdmittedLifecycleOnlySession {
        admitted: AdmittedSession,
        reply: oneshot::Sender<StoreResult<LifecycleOnlyEndOutcome>>,
    },
    CompleteObservationIngest {
        project_id: ProjectId,
        ingest_key: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CompleteObservationIngestIfClaimed {
        project_id: ProjectId,
        ingest_key: String,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    EnqueueSessionConsolidation {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    ClaimSessionConsolidation {
        now: i64,
        stale_before: i64,
        reply: oneshot::Sender<StoreResult<Option<SessionConsolidationJob>>>,
    },
    CompleteSessionConsolidation {
        job: SessionConsolidationJob,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    FailSessionConsolidation {
        job: SessionConsolidationJob,
        error: String,
        retry_at: Option<i64>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    ReleaseSessionConsolidation {
        job: SessionConsolidationJob,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    InsertHandoff {
        handoff: NewHandoff,
        reply: oneshot::Sender<StoreResult<HandoffId>>,
    },
    AcceptHandoff {
        acceptance: HandoffAcceptance,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    /// Expire every open handoff in one scope (#513). See
    /// [`ops::expire_open_handoffs`] for why this ignores the automatic
    /// sweep's exemptions.
    ExpireOpenHandoffs {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        owner_filter: OwnerFilter,
        older_than_us: Option<i64>,
        author_id: Option<ai_memory_core::UserId>,
        reply: oneshot::Sender<StoreResult<u64>>,
    },
    /// Record that an embed attempt produced no embedding (#528).
    RecordEmbedFailure {
        page_id: PageId,
        outcome: crate::ops::EmbedOutcome,
        detail: Option<String>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CancelHandoff {
        handoff_id: HandoffId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        owner_filter: OwnerFilter,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    /// Retro-fit sessions + observations to per-cwd projects and graveyard
    /// mash-up pages. Executed in one transaction for atomicity.
    Reorg {
        /// Workspace whose legacy mash-up pages and sessions are being reorged.
        workspace_id: WorkspaceId,
        /// Each entry is `(session_id, new_project_id)`.
        plan: Vec<(SessionId, ProjectId)>,
        reply: oneshot::Sender<StoreResult<ReorgSummary>>,
    },
    BumpAccess {
        page_ids: Vec<PageId>,
        actor: Option<IdentityKey>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    BumpClientActivity {
        /// `(client, utc_day, reads_delta, writes_delta)` buckets.
        entries: Vec<(String, i64, u32, u32)>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    RecordPageFeedback {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        kind: ai_memory_core::FeedbackKind,
        reason: Option<String>,
        author_id: Option<ai_memory_core::UserId>,
        params: crate::decay::DecayParams,
        reply: oneshot::Sender<StoreResult<Option<(PageId, f64)>>>,
    },
    SoftDeleteForDecayIfLatest {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        expected_latest_id: PageId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    HardDeleteDecayedPageChain {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        tombstone_id: PageId,
        expected_latest_id: Option<PageId>,
        cutoff_us: i64,
        reply: oneshot::Sender<StoreResult<usize>>,
    },
    PruneConsolidatedObservations {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cutoff_us: i64,
        batch: usize,
        reply: oneshot::Sender<StoreResult<ObservationPruneOutcome>>,
    },
    HealCatchAllRepoPaths {
        home: Option<String>,
        reply: oneshot::Sender<StoreResult<u64>>,
    },
    StoreEmbedding {
        page_id: PageId,
        vector_bytes: Vec<u8>,
        provider: String,
        model: String,
        dim: u32,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    StoreEmbeddingBatch {
        embeddings: Vec<EmbeddingWrite>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    DeleteStalePageEmbeddings {
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
        provider: String,
        model: String,
        dim: u32,
        reply: oneshot::Sender<StoreResult<u64>>,
    },
    /// Delete a project's rows (pages, sessions, observations, handoffs,
    /// embeddings) in one transaction. Returns the paths of every page file
    /// that must be removed from disk by the caller.
    ///
    /// A logical delete unless `compaction` is [`ops::Compaction::Reclaim`]:
    /// freed bytes stay in the file until it is rewritten.
    PurgeProject {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        /// Human-readable `workspace/project` label forwarded into the summary.
        label: String,
        /// Authenticated operator recorded in the `audit_log` row (NULL when
        /// single-user / unauthenticated).
        author_id: Option<ai_memory_core::UserId>,
        /// Purge even when a managed workstream still holds a live run lease.
        force: bool,
        /// Whether to reclaim the freed bytes afterwards (`VACUUM`).
        compaction: crate::ops::Compaction,
        reply: oneshot::Sender<StoreResult<PurgeSummary>>,
    },
    /// Delete one session and everything derived from it, inside a single
    /// `(workspace, project)` scope. See [`ops::purge_session`].
    PurgeSession {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        /// Authenticated operator recorded in the `audit_log` row.
        author_id: Option<ai_memory_core::UserId>,
        /// Whether to reclaim the freed bytes afterwards (`VACUUM`).
        compaction: crate::ops::Compaction,
        reply: oneshot::Sender<StoreResult<crate::ops::PurgeSessionSummary>>,
    },
    /// Reclaim free pages on demand: rebuild the FTS indexes and `VACUUM`,
    /// deleting nothing. See [`ops::compact`].
    Compact {
        reply: oneshot::Sender<StoreResult<crate::ops::CompactSummary>>,
    },
    /// Delete a workspace row (its `workspace_id` FKs cascade projects/pages/
    /// sessions/…). Refused when non-empty unless `force`.
    DeleteWorkspace {
        workspace_id: WorkspaceId,
        force: bool,
        /// Whether to reclaim the freed bytes afterwards (`VACUUM`).
        compaction: crate::ops::Compaction,
        reply: oneshot::Sender<StoreResult<DeleteWorkspaceSummary>>,
    },
    /// Rename a workspace's `name` column (UUID-keyed dir doesn't move).
    RenameWorkspace {
        workspace_id: WorkspaceId,
        new_name: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    /// Re-stamp a project's `workspace_id` across every domain table in one
    /// transaction, keeping the same `project_id` (a lossless cross-workspace
    /// "true move"). The caller renames the on-disk dir and ensures the
    /// destination workspace row exists first.
    MoveProjectWorkspace {
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
        reply: oneshot::Sender<StoreResult<MoveSummary>>,
    },
    /// Re-stamp one session and its dependent rows (observations, handoffs,
    /// consolidation jobs, auto-improve runs and claim, session page) into
    /// another scope in one transaction. `commit = false` rolls back after
    /// counting so the reply is an exact dry run.
    MoveSession {
        session_id: SessionId,
        target_workspace: WorkspaceId,
        target_project: ProjectId,
        pages: PagesMode,
        author_id: Option<UserId>,
        commit: bool,
        reply: oneshot::Sender<StoreResult<MoveSessionSummary>>,
    },
    /// Rename a project's `name` column without moving any files (the wiki
    /// is flat on disk). Fails with [`crate::error::StoreError::ProjectNameTaken`]
    /// when `new_name` is already used in the same workspace.
    RenameProject {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        new_name: String,
        /// Authenticated operator recorded in the `audit_log` row (NULL when
        /// single-user / unauthenticated).
        author_id: Option<ai_memory_core::UserId>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    /// Record a successfully-applied wiki-structure migration.
    InsertWikiMigration {
        name: String,
        /// Unix microseconds UTC.
        applied_at: i64,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    CreateUser {
        new_user: NewUser,
        token_hash: [u8; TOKEN_HASH_LEN],
        reply: oneshot::Sender<StoreResult<UserId>>,
    },
    RotateUserToken {
        user_id: UserId,
        new_token_hash: [u8; TOKEN_HASH_LEN],
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    ExpireUserToken {
        user_id: UserId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    ReviveUserToken {
        user_id: UserId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    TouchUserLastSeen {
        user_id: UserId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    StageAutoImproveRun {
        input: StageAutoImproveRun,
        reply: oneshot::Sender<StoreResult<StagedAutoImproveRun>>,
    },
    StageAutoImproveRunForOwner {
        input: StageAutoImproveRun,
        owner: Option<IdentityKey>,
        reply: oneshot::Sender<StoreResult<StagedAutoImproveRunReport>>,
    },
    RejectAutoImproveProposal {
        input: RejectAutoImproveProposal,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    FailAutoImproveProposal {
        input: FailAutoImproveProposal,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    ApproveAutoImproveProposal {
        input: ApproveAutoImproveProposal,
        reply: oneshot::Sender<StoreResult<ApproveAutoImproveProposalResult>>,
    },
    EnsureAutoImproveSchedulerState {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    ClaimAutoImproveSchedulerSession {
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        ended_at: i64,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    RecordMaintenanceJobSuccess {
        job: crate::maintenance::MaintenanceJob,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    PrepareWorkstreamRun {
        input: PrepareWorkstreamRun,
        reply: oneshot::Sender<StoreResult<PreparedWorkstreamRun>>,
    },
    HeartbeatManagedRun {
        run_id: ManagedRunId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    CancelManagedRun {
        run_id: ManagedRunId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    LinkManagedRunSession {
        run_id: ManagedRunId,
        agent: AgentKind,
        native_session_id: String,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    AcceptManagedRunContext {
        run_id: ManagedRunId,
        reply: oneshot::Sender<StoreResult<bool>>,
    },
    AcceptStartupContext {
        handoff: Option<HandoffAcceptance>,
        managed_run_id: Option<ManagedRunId>,
        receiving_session: Option<NewSession>,
        reply: oneshot::Sender<StoreResult<StartupContextAcceptance>>,
    },
    FinishWorkstreamRun {
        input: FinishWorkstreamRun,
        reply: oneshot::Sender<StoreResult<FinishedWorkstreamRun>>,
    },
    RenameWorkstream {
        input: RenameWorkstream,
        reply: oneshot::Sender<StoreResult<RenamedWorkstream>>,
    },
    Shutdown,
}

/// Cheap, cloneable handle that submits commands to the writer.
#[derive(Clone)]
pub struct WriterHandle {
    inner: Arc<WriterInner>,
}

struct WriterInner {
    tx: mpsc::Sender<WriteCmd>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl WriterHandle {
    /// Take ownership of `conn` and spawn the writer thread.
    pub(crate) fn spawn(conn: Connection) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let handle = thread::Builder::new()
            .name("ai-memory-writer".into())
            .spawn(move || worker_loop(conn, rx))
            .expect("spawn writer thread");

        Self {
            inner: Arc::new(WriterInner {
                tx,
                join: Mutex::new(Some(handle)),
            }),
        }
    }

    /// Resolve a workspace by name, creating it atomically if missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::get_or_create_workspace`].
    pub async fn get_or_create_workspace(
        &self,
        name: impl Into<String>,
    ) -> StoreResult<WorkspaceId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::GetOrCreateWorkspace {
            name: name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Resolve a project by `(workspace_id, name)`, creating it atomically
    /// if missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::get_or_create_project`].
    pub async fn get_or_create_project(
        &self,
        workspace_id: WorkspaceId,
        name: impl Into<String>,
        repo_path: Option<String>,
    ) -> StoreResult<ProjectId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::GetOrCreateProject {
            workspace_id,
            name: name.into(),
            repo_path,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Assert that a project still belongs to the supplied workspace.
    ///
    /// # Errors
    /// Returns [`StoreError::NotFound`] when the `(workspace_id, project_id)`
    /// pair is stale, [`StoreError::WriterClosed`] if the actor has shut down,
    /// or propagates the SQL error.
    pub async fn ensure_project_workspace(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureProjectWorkspace {
            workspace_id,
            project_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a workspace with an **explicit id** (idempotent). Used by
    /// `reindex` to recreate a scope under the id recovered from the wiki
    /// directory name. See [`ops::ensure_workspace_with_id`].
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn ensure_workspace_with_id(
        &self,
        id: WorkspaceId,
        name: impl Into<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureWorkspaceWithId {
            id,
            name: name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a project with an **explicit id** under `workspace_id`
    /// (idempotent). See [`ops::ensure_project_with_id`].
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn ensure_project_with_id(
        &self,
        id: ProjectId,
        workspace_id: WorkspaceId,
        name: impl Into<String>,
        repo_path: Option<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureProjectWithId {
            id,
            workspace_id,
            name: name.into(),
            repo_path,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Begin a session (idempotent on the supplied id).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn begin_session(&self, session: NewSession) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::BeginSession { session, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a session ended, optionally linking its summary page, and persist
    /// the observation generation covered by this end.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn end_session(
        &self,
        session_id: SessionId,
        summary_page_id: Option<PageId>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EndSession {
            session_id,
            summary_page_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Atomically end a session and insert its automatic handoff.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL/state errors.
    pub async fn end_session_with_handoff(
        &self,
        session_id: SessionId,
        summary_page_id: Option<PageId>,
        handoff: NewHandoff,
    ) -> StoreResult<HandoffId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EndSessionWithHandoff {
            session_id,
            summary_page_id,
            handoff,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Atomically end a lifecycle-only session and reopen the handoff claimed
    /// by that exact receiver, if one exists.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL/state errors.
    pub async fn end_lifecycle_only_session(
        &self,
        session_id: SessionId,
    ) -> StoreResult<LifecycleOnlyEndOutcome> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EndLifecycleOnlySession {
            session_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete hollow project rows (no data of any kind) older than
    /// `min_age_days`; returns the deleted names. See
    /// [`ops::sweep_hollow_projects`].
    ///
    /// # Errors
    /// Propagates store failures.
    pub async fn sweep_hollow_projects(&self, min_age_days: u32) -> StoreResult<Vec<String>> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::SweepHollowProjects {
            min_age_days,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Append an observation row.
    ///
    /// Takes [`Sanitized<NewObservation>`] — the privacy strip is enforced by
    /// the type system at the store boundary, so an unsanitized observation
    /// cannot reach disk by construction ([`Sanitized::new`] is the single
    /// constructor and applies the scrub).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn insert_observation(
        &self,
        obs: Sanitized<NewObservation>,
    ) -> StoreResult<ObservationId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::InsertObservation {
            obs: obs.into_inner(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Claim a keyed hook event and append its observation atomically.
    ///
    /// The outcome tells the hook router whether it inserted a new observation,
    /// must resume incomplete downstream effects, or can skip a fully completed
    /// replay. Keys are scoped to the observation's project.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn insert_observation_ingest(
        &self,
        obs: Sanitized<NewObservation>,
        ingest_key: String,
    ) -> StoreResult<IngestObservationOutcome> {
        let obs = obs.into_inner();
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::InsertObservationIngest {
            obs,
            ingest_key,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Atomically validate/admit a hook session and insert its observation.
    pub async fn admit_hook_session_event(
        &self,
        session: NewSession,
        obs: Sanitized<NewObservation>,
        owner_filter: OwnerFilter,
        ingest_key: Option<String>,
    ) -> StoreResult<HookSessionAdmission> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::AdmitHookSessionEvent {
            session,
            obs: obs.into_inner(),
            owner_filter,
            ingest_key,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Guarded hook end.
    pub async fn end_admitted_session(
        &self,
        admitted: AdmittedSession,
        summary_page_id: Option<PageId>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EndAdmittedSession {
            admitted,
            summary_page_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Guarded hook end plus automatic handoff.
    pub async fn end_admitted_session_with_handoff(
        &self,
        admitted: AdmittedSession,
        summary_page_id: Option<PageId>,
        handoff: NewHandoff,
    ) -> StoreResult<HandoffId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EndAdmittedSessionWithHandoff {
            admitted,
            summary_page_id,
            handoff,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Guarded hook lifecycle-only end.
    pub async fn end_admitted_lifecycle_only_session(
        &self,
        admitted: AdmittedSession,
    ) -> StoreResult<LifecycleOnlyEndOutcome> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EndAdmittedLifecycleOnlySession {
            admitted,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a keyed hook event complete after all downstream effects finish.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL/state errors.
    pub async fn complete_observation_ingest(
        &self,
        project_id: ProjectId,
        ingest_key: String,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CompleteObservationIngest {
            project_id,
            ingest_key,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a keyed hook event complete if its observation claim exists.
    ///
    /// Returns `false` for an unkeyed or unrelated duplicate recovery attempt.
    pub async fn complete_observation_ingest_if_claimed(
        &self,
        project_id: ProjectId,
        ingest_key: String,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CompleteObservationIngestIfClaimed {
            project_id,
            ingest_key,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Persist one opt-in SessionEnd consolidation job for the current
    /// observation generation. Duplicate generations are idempotent.
    pub async fn enqueue_session_consolidation(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnqueueSessionConsolidation {
            workspace_id,
            project_id,
            session_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Atomically claim the oldest due SessionEnd consolidation job.
    pub async fn claim_session_consolidation(
        &self,
        now: i64,
        stale_before: i64,
    ) -> StoreResult<Option<SessionConsolidationJob>> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ClaimSessionConsolidation {
            now,
            stale_before,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a claimed SessionEnd consolidation job complete.
    pub async fn complete_session_consolidation(
        &self,
        job: SessionConsolidationJob,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CompleteSessionConsolidation { job, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Record a failed SessionEnd consolidation attempt.
    pub async fn fail_session_consolidation(
        &self,
        job: SessionConsolidationJob,
        error: String,
        retry_at: Option<i64>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::FailSessionConsolidation {
            job,
            error,
            retry_at,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Return an in-flight SessionEnd consolidation job to the durable queue.
    pub async fn release_session_consolidation(
        &self,
        job: SessionConsolidationJob,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ReleaseSessionConsolidation { job, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a new handoff in `open` state.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn insert_handoff(&self, handoff: NewHandoff) -> StoreResult<HandoffId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::InsertHandoff { handoff, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a handoff accepted by the given agent / session.
    ///
    /// Returns whether this call is the one that claimed it; `false` means the
    /// row was already taken, does not belong to the expected workspace and
    /// project, or its owner does not admit this caller. The body must not reach
    /// the agent on `false`. `receiving_cwd` is where the claiming session is
    /// starting, and bounds the sweep of superseded automatic handoffs.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn accept_handoff(&self, acceptance: HandoffAcceptance) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::AcceptHandoff {
            acceptance,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Expire every open handoff in one scope, optionally only those older
    /// than `older_than_us`. Returns how many changed.
    ///
    /// Unlike the automatic sweep this does not spare manual or
    /// different-directory handoffs: those are precisely what a leftover
    /// backlog is made of. It is a state change, not a delete — the summary
    /// and provenance survive.
    ///
    /// # Errors
    /// [`StoreError::WriterClosed`], or a propagated SQL error.
    pub async fn expire_open_handoffs(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        owner_filter: OwnerFilter,
        older_than_us: Option<i64>,
        author_id: Option<ai_memory_core::UserId>,
    ) -> StoreResult<u64> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ExpireOpenHandoffs {
            workspace_id,
            project_id,
            owner_filter,
            older_than_us,
            author_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Record that an embed attempt on `page_id` did not produce an
    /// embedding, so a failed or skipped page is attributable afterwards
    /// rather than only in container logs (#528).
    ///
    /// Success records nothing — that is already implied by a
    /// `page_embeddings` row — so the common path pays no extra write.
    ///
    /// # Errors
    /// [`StoreError::WriterClosed`], or a propagated SQL error.
    pub async fn record_embed_failure(
        &self,
        page_id: PageId,
        outcome: crate::ops::EmbedOutcome,
        detail: Option<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RecordEmbedFailure {
            page_id,
            outcome,
            detail,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark an open handoff expired so it will no longer be consumed.
    ///
    /// Returns `true` when an open handoff was changed, `false` when the id was
    /// already accepted/expired, outside the expected workspace and project,
    /// or missing.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn cancel_handoff(
        &self,
        handoff_id: HandoffId,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        owner_filter: OwnerFilter,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CancelHandoff {
            handoff_id,
            workspace_id,
            project_id,
            owner_filter,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Store (or replace) the embedding for one page (M9).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn store_embedding(
        &self,
        page_id: PageId,
        vector_bytes: Vec<u8>,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::StoreEmbedding {
            page_id,
            vector_bytes,
            provider,
            model,
            dim,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Store or replace a batch of embeddings in one SQLite transaction.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn store_embeddings(&self, embeddings: Vec<EmbeddingWrite>) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::StoreEmbeddingBatch {
            embeddings,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Remove embedding rows in a workspace/project scope whose triple does not match the configured provider/model/dim.
    ///
    /// Used when re-embedding after a model migration (e.g. Gemini → OpenRouter).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn delete_stale_page_embeddings(
        &self,
        workspace_id: WorkspaceId,
        project_id: Option<ProjectId>,
        provider: String,
        model: String,
        dim: u32,
    ) -> StoreResult<u64> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::DeleteStalePageEmbeddings {
            workspace_id,
            project_id,
            provider,
            model,
            dim,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Bump access counters for a set of pages (M8 reinforcement term).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn bump_access(&self, page_ids: Vec<PageId>) -> StoreResult<()> {
        self.bump_access_for_actor(page_ids, None).await
    }

    /// Bump shared access counters and record each identified operator once per
    /// page for the optional access-breadth retention term.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn bump_access_for_actor(
        &self,
        page_ids: Vec<PageId>,
        actor: Option<IdentityKey>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::BumpAccess {
            page_ids,
            actor,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Fold buffered per-client MCP tool-call counts into their
    /// `(client, day)` buckets. Entries are deltas, not totals. Labels must
    /// already be normalized; each UTC day retains a bounded number of named
    /// clients and folds later names into the stable overflow bucket.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`], rejects malformed labels with
    /// [`StoreError::InvalidState`], or propagates SQL errors.
    pub async fn bump_client_activity(
        &self,
        entries: Vec<(String, i64, u32, u32)>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::BumpClientActivity { entries, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Record one explicit feedback signal for a page and update its
    /// derived salience. Returns `None` when the path has no latest
    /// version in that scope.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_page_feedback(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        kind: ai_memory_core::FeedbackKind,
        reason: Option<String>,
        author_id: Option<ai_memory_core::UserId>,
        params: crate::decay::DecayParams,
    ) -> StoreResult<Option<(PageId, f64)>> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RecordPageFeedback {
            workspace_id,
            project_id,
            path,
            kind,
            reason,
            author_id,
            params,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Tombstone the expected latest page identified by the forget sweep.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn soft_delete_for_decay_if_latest(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        expected_latest_id: PageId,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::SoftDeleteForDecayIfLatest {
            workspace_id,
            project_id,
            path,
            expected_latest_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Permanently delete one eligible decay tombstone and its ancestry chain.
    /// The expected latest-page state is checked in the same transaction so a
    /// page recreated at the same path cannot be removed accidentally.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn hard_delete_decayed_page_chain(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        tombstone_id: PageId,
        expected_latest_id: Option<PageId>,
        cutoff_us: i64,
    ) -> StoreResult<usize> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::HardDeleteDecayedPageChain {
            workspace_id,
            project_id,
            path,
            tombstone_id,
            expected_latest_id,
            cutoff_us,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete one bounded batch of raw observations whose session was already
    /// consolidated into a live summary page, repairing the session end
    /// watermark in the same transaction.
    ///
    /// One batch is one transaction and one mailbox message, so a multi-million
    /// row prune never holds the write lock across the whole run — every other
    /// pending write interleaves between batches.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn prune_consolidated_observations(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        cutoff_us: i64,
        batch: usize,
    ) -> StoreResult<ObservationPruneOutcome> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::PruneConsolidatedObservations {
            workspace_id,
            project_id,
            cutoff_us,
            batch,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// NULL out catch-all project `repo_path` rows so existing installs
    /// self-heal on upgrade: the broad sentinels (`$HOME` and the filesystem
    /// root) plus any path that exists locally but is not a git work-tree
    /// root. Idempotent; returns the number of rows healed.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] or propagates SQL errors.
    pub async fn heal_catch_all_repo_paths(&self, home: Option<String>) -> StoreResult<u64> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::HealCatchAllRepoPaths { home, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete a project's rows in one atomic transaction. Logical unless
    /// `compaction` is [`ops::Compaction::Reclaim`].
    ///
    /// ON DELETE CASCADE propagates the delete through pages, sessions,
    /// observations, handoffs, and page_embeddings automatically. The
    /// returned [`PurgeSummary`] includes pre-delete row counts and
    /// the distinct page paths that the caller must remove from disk.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from the purge transaction.
    pub async fn purge_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        label: impl Into<String>,
        author_id: Option<ai_memory_core::UserId>,
        force: bool,
        compaction: crate::ops::Compaction,
    ) -> StoreResult<PurgeSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::PurgeProject {
            workspace_id,
            project_id,
            label: label.into(),
            author_id,
            force,
            compaction,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete one session by id, with everything derived from it, inside a
    /// single `(workspace, project)` scope.
    ///
    /// Scope is enforced in the store: a session that does not belong to the
    /// named workspace and project is [`StoreError::NotFound`] and nothing is
    /// deleted. See [`ops::purge_session`] for what is and is not removed —
    /// in particular, handoffs this session *accepted* are left alone.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the session is absent from that scope,
    /// [`StoreError::WriterClosed`], or a propagated SQL error.
    pub async fn purge_session(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        author_id: Option<ai_memory_core::UserId>,
        compaction: crate::ops::Compaction,
    ) -> StoreResult<crate::ops::PurgeSessionSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::PurgeSession {
            workspace_id,
            project_id,
            session_id,
            author_id,
            compaction,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete a workspace and, via the `workspace_id` cascade, every project /
    /// page / session under it. Refuses a non-empty workspace unless `force`.
    ///
    /// # Errors
    /// [`StoreError::WorkspaceNotEmpty`] when it still holds projects and
    /// `force` is false; [`StoreError::NotFound`] when the workspace is absent;
    /// [`StoreError::WriterClosed`] if the actor has shut down.
    pub async fn delete_workspace(
        &self,
        workspace_id: WorkspaceId,
        force: bool,
        compaction: crate::ops::Compaction,
    ) -> StoreResult<DeleteWorkspaceSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::DeleteWorkspace {
            workspace_id,
            force,
            compaction,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Reclaim free pages on demand, deleting nothing.
    ///
    /// Runs through the writer actor like every other exclusive operation, so
    /// it cannot overlap a write: `VACUUM` takes an exclusive lock and would
    /// otherwise contend with one.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from the rebuild or `VACUUM`.
    pub async fn compact(&self) -> StoreResult<crate::ops::CompactSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::Compact { reply: tx }).await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Rename a workspace (column-only; the on-disk dir is UUID-keyed).
    ///
    /// # Errors
    /// [`StoreError::WorkspaceNameTaken`] / [`StoreError::InvalidWorkspaceName`]
    /// / [`StoreError::NotFound`] / [`StoreError::WriterClosed`].
    pub async fn rename_workspace(
        &self,
        workspace_id: WorkspaceId,
        new_name: impl Into<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RenameWorkspace {
            workspace_id,
            new_name: new_name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Re-stamp a project's `workspace_id` to `to_workspace` across every
    /// domain table in one transaction, keeping the same `project_id`. This is
    /// the lossless cross-workspace move: pages, sessions, observations and
    /// supersession history all follow. The destination workspace row must
    /// already exist; the caller renames the on-disk dir afterwards.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down,
    /// [`StoreError::NotFound`] if the project is absent from the source
    /// workspace, or propagates the SQL error (e.g. a
    /// `UNIQUE(workspace_id, name)` collision when the destination already
    /// holds a same-named project — the caller routes that merge case to
    /// copy+purge instead).
    pub async fn move_project_workspace(
        &self,
        project_id: ProjectId,
        from_workspace: WorkspaceId,
        to_workspace: WorkspaceId,
    ) -> StoreResult<MoveSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::MoveProjectWorkspace {
            project_id,
            from_workspace,
            to_workspace,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Re-stamp one session, its observations, the handoffs it produced, its
    /// consolidation jobs, auto-improve runs and scheduler claim, and its
    /// `sessions/<id>.md` page (per `pages`) into
    /// `(target_workspace, target_project)` in one transaction. With
    /// `commit = false` the transaction is rolled back after counting, so the
    /// summary is an exact dry run. The target project must already exist;
    /// the caller moves the on-disk page file afterwards. `author_id` is the
    /// operator recorded on the audit row. A target equal to the session
    /// row's own scope re-homes only the rows still lying elsewhere
    /// (`session_moved = false`); see [`ops::move_session`].
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down,
    /// [`StoreError::NotFound`] if the session or the target project is
    /// absent, [`StoreError::PagePathTaken`] when [`PagesMode::Move`] would
    /// collide with a latest page in the target scope, or propagates the SQL
    /// error.
    pub async fn move_session(
        &self,
        session_id: SessionId,
        target_workspace: WorkspaceId,
        target_project: ProjectId,
        pages: PagesMode,
        author_id: Option<UserId>,
        commit: bool,
    ) -> StoreResult<MoveSessionSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::MoveSession {
            session_id,
            target_workspace,
            target_project,
            pages,
            author_id,
            commit,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Rename a project within its workspace (column-only; no file moves).
    ///
    /// # Errors
    /// Returns [`crate::error::StoreError::WriterClosed`] if the actor has
    /// shut down, [`crate::error::StoreError::ProjectNameTaken`] if
    /// `new_name` is already in use in the same workspace, or
    /// [`crate::error::StoreError::InvalidProjectName`] for invalid names.
    pub async fn rename_project(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        new_name: impl Into<String>,
        author_id: Option<ai_memory_core::UserId>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RenameProject {
            workspace_id,
            project_id,
            new_name: new_name.into(),
            author_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Record a wiki-structure migration as successfully applied.
    ///
    /// Called by the wiki migration runner immediately after [`WikiMigration::up`]
    /// returns `Ok`. `applied_at` is unix microseconds UTC. If the name is
    /// already present the call is a no-op (idempotent insert-or-ignore).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error.
    pub async fn insert_wiki_migration(&self, name: String, applied_at: i64) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::InsertWikiMigration {
            name,
            applied_at,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Retro-fit sessions and their observations to per-cwd projects and
    /// graveyard any mash-up pages. The `plan` slice contains
    /// `(session_id, new_project_id)` pairs. Everything runs in one
    /// SQLite transaction — either fully committed or fully rolled back.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from the reorg transaction.
    pub async fn reorg_sessions(
        &self,
        workspace_id: WorkspaceId,
        plan: Vec<(SessionId, ProjectId)>,
    ) -> StoreResult<ReorgSummary> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::Reorg {
            workspace_id,
            plan,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Upsert a batch of pages atomically (one SQL transaction).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down,
    /// or propagates the SQL error from [`ops::upsert_pages_batch`].
    pub async fn upsert_pages_batch(&self, pages: Vec<NewPage>) -> StoreResult<Vec<PageId>> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::UpsertPageBatch { pages, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Upsert a page (creating it or superseding the existing latest
    /// version when the body has changed).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::upsert_page`].
    pub async fn upsert_page(&self, page: NewPage) -> StoreResult<PageId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::UpsertPage { page, reply: tx }).await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete every version of a page (by path) from the index. The wiki file
    /// removal is the caller's concern; this drops the derived rows so the
    /// page stops appearing in search/recent (the watcher does NOT reconcile
    /// file deletions — it only handles create/modify events).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or a
    /// SQL error from the delete.
    pub async fn delete_page(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        author_id: Option<ai_memory_core::UserId>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::DeletePage {
            workspace_id,
            project_id,
            path,
            author_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Delete every version of `path` only if `expected_latest_id` is still
    /// the latest version. Returns `false` without mutation when the page was
    /// refreshed or removed after the caller selected it.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or a
    /// SQL error from the conditional delete.
    pub async fn delete_page_if_latest(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        path: PagePath,
        expected_latest_id: PageId,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::DeletePageIfLatest {
            workspace_id,
            project_id,
            path,
            expected_latest_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Insert a new user. `new_user` MUST already have been validated by
    /// [`NewUser::validate`](ai_memory_core::NewUser::validate); the
    /// caller (CLI or admin handler) generates the plaintext token,
    /// hashes it with the per-server pepper via
    /// [`crate::users::hash_token`], and passes only the digest in.
    ///
    /// # Errors
    /// - [`StoreError::WriterClosed`] if the writer has shut down.
    /// - [`StoreError::Duplicate`] when the username or email collides.
    /// - [`StoreError::Sqlite`] for any other SQL failure.
    pub async fn create_user(
        &self,
        new_user: NewUser,
        token_hash: [u8; TOKEN_HASH_LEN],
    ) -> StoreResult<UserId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CreateUser {
            new_user,
            token_hash,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Replace a user's token hash with a freshly-generated digest.
    /// Implicitly clears `token_expired_at` — rotating an expired
    /// token only makes sense to make it usable again. Returns `false`
    /// when the user id doesn't exist; `true` on successful update.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`]
    /// per the usual writer-actor flow.
    pub async fn rotate_user_token(
        &self,
        user_id: UserId,
        new_token_hash: [u8; TOKEN_HASH_LEN],
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RotateUserToken {
            user_id,
            new_token_hash,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Stamp `token_expired_at = now()` so the user's current token stops
    /// authenticating. Idempotent — repeating the call leaves the original
    /// expiry timestamp untouched. Returns `false` when the user doesn't exist.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`].
    pub async fn expire_user_token(&self, user_id: UserId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ExpireUserToken { user_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Clear `token_expired_at`, re-activating the user's existing token.
    /// Idempotent. Returns `false` when the user doesn't exist.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`].
    pub async fn revive_user_token(&self, user_id: UserId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ReviveUserToken { user_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Update `last_seen_at = now()` for the user. Called fire-and-forget
    /// by the auth middleware on every authenticated request. Returns
    /// `false` when the user doesn't exist (caller authenticated against
    /// a token whose user vanished mid-request — already a logic error
    /// elsewhere).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] / [`StoreError::Sqlite`].
    pub async fn touch_user_last_seen(&self, user_id: UserId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::TouchUserLastSeen { user_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Stage one auto-improvement run and all proposals in one SQL transaction.
    pub async fn stage_auto_improve_run(
        &self,
        input: StageAutoImproveRun,
    ) -> StoreResult<StagedAutoImproveRun> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::StageAutoImproveRun { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Stage an auto-improvement run in an optional typed operator bucket,
    /// reporting target collisions without discarding sibling proposals.
    pub async fn stage_auto_improve_run_for_owner(
        &self,
        input: StageAutoImproveRun,
        owner: Option<IdentityKey>,
    ) -> StoreResult<StagedAutoImproveRunReport> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::StageAutoImproveRunForOwner {
            input,
            owner,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Reject a pending auto-improvement proposal and append an event.
    pub async fn reject_auto_improve_proposal(
        &self,
        input: RejectAutoImproveProposal,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RejectAutoImproveProposal { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Mark a pending auto-improvement proposal failed and append an event.
    pub async fn fail_auto_improve_proposal(
        &self,
        input: FailAutoImproveProposal,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::FailAutoImproveProposal { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Approve a pending auto-improvement proposal in a DB-only transaction.
    pub async fn approve_auto_improve_proposal(
        &self,
        input: ApproveAutoImproveProposal,
    ) -> StoreResult<ApproveAutoImproveProposalResult> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ApproveAutoImproveProposal { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Initialise background auto-improvement scheduler state without resetting
    /// an existing watermark.
    pub async fn ensure_auto_improve_scheduler_state(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureAutoImproveSchedulerState {
            workspace_id,
            project_id,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Atomically claim a completed session for scheduled auto-improvement
    /// before any LLM work runs. Returns `false` if another scheduler/manual run
    /// already claimed or reviewed the session.
    pub async fn claim_auto_improve_scheduler_session(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        ended_at: i64,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::ClaimAutoImproveSchedulerSession {
            workspace_id,
            project_id,
            session_id,
            ended_at,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Persist a global maintenance job's successful completion time.
    pub async fn record_maintenance_job_success(
        &self,
        job: crate::maintenance::MaintenanceJob,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RecordMaintenanceJobSuccess { job, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Select a workstream and open its single active managed-run lease.
    pub async fn prepare_workstream_run(
        &self,
        input: PrepareWorkstreamRun,
    ) -> StoreResult<PreparedWorkstreamRun> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::PrepareWorkstreamRun { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Extend a managed-run lease. Returns false for missing/stale runs.
    pub async fn heartbeat_managed_run(&self, run_id: ManagedRunId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::HeartbeatManagedRun { run_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Release a managed-run lease after a handled launcher failure.
    pub async fn cancel_managed_run(&self, run_id: ManagedRunId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::CancelManagedRun { run_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Attach the harness-native session observed by a managed SessionStart.
    pub async fn link_managed_run_session(
        &self,
        run_id: ManagedRunId,
        agent: AgentKind,
        native_session_id: impl Into<String>,
    ) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::LinkManagedRunSession {
            run_id,
            agent,
            native_session_id: native_session_id.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Acknowledge successful SessionStart delivery for a managed run.
    pub async fn accept_managed_run_context(&self, run_id: ManagedRunId) -> StoreResult<bool> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::AcceptManagedRunContext { run_id, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Atomically claim the handoff and managed ledger selected for one
    /// SessionStart response.
    ///
    /// When a managed run was requested but is no longer claimable, the
    /// handoff remains open and both result fields are false.
    pub async fn accept_startup_context(
        &self,
        handoff: Option<HandoffAcceptance>,
        managed_run_id: Option<ManagedRunId>,
        receiving_session: Option<NewSession>,
    ) -> StoreResult<StartupContextAcceptance> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::AcceptStartupContext {
            handoff,
            managed_run_id,
            receiving_session,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Index an immutable transcript segment and release the run lease.
    pub async fn finish_workstream_run(
        &self,
        input: FinishWorkstreamRun,
    ) -> StoreResult<FinishedWorkstreamRun> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::FinishWorkstreamRun { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Retitle one checkout-local workstream, leaving its selection and
    /// activity timestamps untouched.
    pub async fn rename_workstream(
        &self,
        input: RenameWorkstream,
    ) -> StoreResult<RenamedWorkstream> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::RenameWorkstream { input, reply: tx })
            .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    async fn send(&self, cmd: WriteCmd) -> StoreResult<()> {
        self.inner
            .tx
            .send(cmd)
            .await
            .map_err(|_| StoreError::WriterClosed)
    }
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        let _ = self.tx.try_send(WriteCmd::Shutdown);
        if let Some(handle) = self.join.lock().expect("writer join mutex poisoned").take() {
            let _ = handle.join();
        }
    }
}

/// Dispatch one operation's result back to its caller. Logs a warn
/// when the receiver was dropped (caller cancelled their `.await` or
/// hit a timeout) so the operator sees backpressure / cancellation
/// noise instead of silent loss. The result itself is consumed by
/// the failed `send` and discarded — the caller's await has already
/// returned a JoinError-shaped failure by this point.
fn send_or_warn<T>(reply: oneshot::Sender<T>, result: T, op: &'static str) {
    if reply.send(result).is_err() {
        tracing::warn!(
            op,
            "writer reply dropped — caller cancelled or oneshot receiver closed"
        );
    }
}

fn worker_loop(mut conn: Connection, mut rx: mpsc::Receiver<WriteCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCmd::Shutdown => break,
            WriteCmd::GetOrCreateWorkspace { name, reply } => {
                let result = ops::get_or_create_workspace(&mut conn, &name);
                send_or_warn(reply, result, "get_or_create_workspace");
            }
            WriteCmd::GetOrCreateProject {
                workspace_id,
                name,
                repo_path,
                reply,
            } => {
                let result = ops::get_or_create_project(
                    &mut conn,
                    &workspace_id,
                    &name,
                    repo_path.as_deref(),
                );
                send_or_warn(reply, result, "get_or_create_project");
            }
            WriteCmd::EnsureProjectWorkspace {
                workspace_id,
                project_id,
                reply,
            } => {
                let result = ops::ensure_project_workspace(&conn, &workspace_id, &project_id);
                send_or_warn(reply, result, "ensure_project_workspace");
            }
            WriteCmd::EnsureWorkspaceWithId { id, name, reply } => {
                let result = ops::ensure_workspace_with_id(&mut conn, id, &name);
                send_or_warn(reply, result, "ensure_workspace_with_id");
            }
            WriteCmd::EnsureProjectWithId {
                id,
                workspace_id,
                name,
                repo_path,
                reply,
            } => {
                let result = ops::ensure_project_with_id(
                    &mut conn,
                    id,
                    workspace_id,
                    &name,
                    repo_path.as_deref(),
                );
                send_or_warn(reply, result, "ensure_project_with_id");
            }
            WriteCmd::UpsertPage { page, reply } => {
                let result = ops::upsert_page(&mut conn, &page);
                send_or_warn(reply, result, "upsert_page");
            }
            WriteCmd::DeletePage {
                workspace_id,
                project_id,
                path,
                author_id,
                reply,
            } => {
                let result =
                    ops::delete_page(&mut conn, workspace_id, project_id, &path, author_id);
                send_or_warn(reply, result, "delete_page");
            }
            WriteCmd::DeletePageIfLatest {
                workspace_id,
                project_id,
                path,
                expected_latest_id,
                reply,
            } => {
                let result = ops::delete_page_if_latest(
                    &mut conn,
                    workspace_id,
                    project_id,
                    &path,
                    expected_latest_id,
                    None,
                );
                send_or_warn(reply, result, "delete_page_if_latest");
            }
            WriteCmd::UpsertPageBatch { pages, reply } => {
                let result = ops::upsert_pages_batch(&mut conn, &pages);
                send_or_warn(reply, result, "upsert_pages_batch");
            }
            WriteCmd::BeginSession { session, reply } => {
                let result = ops::begin_session(&mut conn, &session);
                send_or_warn(reply, result, "begin_session");
            }
            WriteCmd::EndSession {
                session_id,
                summary_page_id,
                reply,
            } => {
                let result = ops::end_session(&mut conn, &session_id, summary_page_id.as_ref());
                send_or_warn(reply, result, "end_session");
            }
            WriteCmd::EndSessionWithHandoff {
                session_id,
                summary_page_id,
                handoff,
                reply,
            } => {
                let result = ops::end_session_with_handoff(
                    &mut conn,
                    &session_id,
                    summary_page_id.as_ref(),
                    &handoff,
                );
                send_or_warn(reply, result, "end_session_with_handoff");
            }
            WriteCmd::EndLifecycleOnlySession { session_id, reply } => {
                let result = ops::end_lifecycle_only_session(&mut conn, &session_id);
                send_or_warn(reply, result, "end_lifecycle_only_session");
            }
            WriteCmd::SweepHollowProjects {
                min_age_days,
                reply,
            } => {
                let result = ops::sweep_hollow_projects(&mut conn, min_age_days);
                send_or_warn(reply, result, "sweep_hollow_projects");
            }
            WriteCmd::InsertObservation { obs, reply } => {
                let result = ops::insert_observation(&mut conn, &obs);
                send_or_warn(reply, result, "insert_observation");
            }
            WriteCmd::InsertObservationIngest {
                obs,
                ingest_key,
                reply,
            } => {
                let result = ops::insert_observation_keyed(&mut conn, &obs, &ingest_key);
                send_or_warn(reply, result, "insert_observation_ingest");
            }
            WriteCmd::AdmitHookSessionEvent {
                session,
                obs,
                owner_filter,
                ingest_key,
                reply,
            } => {
                let result = ops::admit_hook_session_event(
                    &mut conn,
                    &session,
                    &obs,
                    &owner_filter,
                    ingest_key.as_deref(),
                );
                send_or_warn(reply, result, "admit_hook_session_event");
            }
            WriteCmd::EndAdmittedSession {
                admitted,
                summary_page_id,
                reply,
            } => {
                let result =
                    ops::end_admitted_session(&mut conn, &admitted, summary_page_id.as_ref());
                send_or_warn(reply, result, "end_admitted_session");
            }
            WriteCmd::EndAdmittedSessionWithHandoff {
                admitted,
                summary_page_id,
                handoff,
                reply,
            } => {
                let result = ops::end_admitted_session_with_handoff(
                    &mut conn,
                    &admitted,
                    summary_page_id.as_ref(),
                    &handoff,
                );
                send_or_warn(reply, result, "end_admitted_session_with_handoff");
            }
            WriteCmd::EndAdmittedLifecycleOnlySession { admitted, reply } => {
                let result = ops::end_admitted_lifecycle_only_session(&mut conn, &admitted);
                send_or_warn(reply, result, "end_admitted_lifecycle_only_session");
            }
            WriteCmd::CompleteObservationIngest {
                project_id,
                ingest_key,
                reply,
            } => {
                let result = ops::complete_observation_ingest(&mut conn, &project_id, &ingest_key);
                send_or_warn(reply, result, "complete_observation_ingest");
            }
            WriteCmd::CompleteObservationIngestIfClaimed {
                project_id,
                ingest_key,
                reply,
            } => {
                let result = ops::complete_observation_ingest_if_claimed(
                    &mut conn,
                    &project_id,
                    &ingest_key,
                );
                send_or_warn(reply, result, "complete_observation_ingest_if_claimed");
            }
            WriteCmd::EnqueueSessionConsolidation {
                workspace_id,
                project_id,
                session_id,
                reply,
            } => {
                let result = crate::session_consolidation::enqueue(
                    &mut conn,
                    workspace_id,
                    project_id,
                    session_id,
                );
                send_or_warn(reply, result, "enqueue_session_consolidation");
            }
            WriteCmd::ClaimSessionConsolidation {
                now,
                stale_before,
                reply,
            } => {
                let result = crate::session_consolidation::claim_next(&mut conn, now, stale_before);
                send_or_warn(reply, result, "claim_session_consolidation");
            }
            WriteCmd::CompleteSessionConsolidation { job, reply } => {
                let result = crate::session_consolidation::complete(&mut conn, &job);
                send_or_warn(reply, result, "complete_session_consolidation");
            }
            WriteCmd::FailSessionConsolidation {
                job,
                error,
                retry_at,
                reply,
            } => {
                let result = crate::session_consolidation::fail(&mut conn, &job, &error, retry_at);
                send_or_warn(reply, result, "fail_session_consolidation");
            }
            WriteCmd::ReleaseSessionConsolidation { job, reply } => {
                let result = crate::session_consolidation::release(&mut conn, &job);
                send_or_warn(reply, result, "release_session_consolidation");
            }
            WriteCmd::InsertHandoff { handoff, reply } => {
                let result = ops::insert_handoff(&mut conn, &handoff);
                send_or_warn(reply, result, "insert_handoff");
            }
            WriteCmd::AcceptHandoff { acceptance, reply } => {
                let result = ops::accept_handoff(&mut conn, &acceptance);
                send_or_warn(reply, result, "accept_handoff");
            }
            WriteCmd::ExpireOpenHandoffs {
                workspace_id,
                project_id,
                owner_filter,
                older_than_us,
                author_id,
                reply,
            } => {
                let result = ops::expire_open_handoffs(
                    &mut conn,
                    &workspace_id,
                    &project_id,
                    &owner_filter,
                    older_than_us,
                    author_id,
                );
                send_or_warn(reply, result, "expire_open_handoffs");
            }
            WriteCmd::RecordEmbedFailure {
                page_id,
                outcome,
                detail,
                reply,
            } => {
                let result = ops::record_embed_failure(&conn, &page_id, outcome, detail.as_deref());
                send_or_warn(reply, result, "record_embed_failure");
            }
            WriteCmd::CancelHandoff {
                handoff_id,
                workspace_id,
                project_id,
                owner_filter,
                reply,
            } => {
                let result = ops::cancel_handoff(
                    &mut conn,
                    &handoff_id,
                    &workspace_id,
                    &project_id,
                    &owner_filter,
                );
                send_or_warn(reply, result, "cancel_handoff");
            }
            WriteCmd::Reorg {
                workspace_id,
                plan,
                reply,
            } => {
                let result = ops::reorg_sessions(&mut conn, &workspace_id, &plan);
                send_or_warn(reply, result, "reorg_sessions");
            }
            WriteCmd::RecordPageFeedback {
                workspace_id,
                project_id,
                path,
                kind,
                reason,
                author_id,
                params,
                reply,
            } => {
                let result = ops::record_page_feedback(
                    &mut conn,
                    workspace_id,
                    project_id,
                    &path,
                    kind,
                    reason.as_deref(),
                    author_id,
                    &params,
                );
                send_or_warn(reply, result, "record_page_feedback");
            }
            WriteCmd::BumpAccess {
                page_ids,
                actor,
                reply,
            } => {
                let result =
                    ops::bump_access_for_pages_for_actor(&mut conn, &page_ids, actor.as_ref());
                send_or_warn(reply, result, "bump_access_for_pages");
            }
            WriteCmd::BumpClientActivity { entries, reply } => {
                let result = ops::bump_client_activity(&mut conn, &entries);
                send_or_warn(reply, result, "bump_client_activity");
            }
            WriteCmd::SoftDeleteForDecayIfLatest {
                workspace_id,
                project_id,
                path,
                expected_latest_id,
                reply,
            } => {
                let result = ops::soft_delete_for_decay_if_latest(
                    &mut conn,
                    workspace_id,
                    project_id,
                    &path,
                    expected_latest_id,
                );
                send_or_warn(reply, result, "soft_delete_for_decay_if_latest");
            }
            WriteCmd::HardDeleteDecayedPageChain {
                workspace_id,
                project_id,
                path,
                tombstone_id,
                expected_latest_id,
                cutoff_us,
                reply,
            } => {
                let result = ops::hard_delete_decayed_page_chain(
                    &mut conn,
                    workspace_id,
                    project_id,
                    &path,
                    tombstone_id,
                    expected_latest_id,
                    cutoff_us,
                );
                send_or_warn(reply, result, "hard_delete_decayed_page_chain");
            }
            WriteCmd::PruneConsolidatedObservations {
                workspace_id,
                project_id,
                cutoff_us,
                batch,
                reply,
            } => {
                let result = ops::prune_consolidated_observations(
                    &mut conn,
                    workspace_id,
                    project_id,
                    cutoff_us,
                    batch,
                );
                send_or_warn(reply, result, "prune_consolidated_observations");
            }
            WriteCmd::HealCatchAllRepoPaths { home, reply } => {
                let result = ops::heal_catch_all_repo_paths(&mut conn, home.as_deref());
                send_or_warn(reply, result, "heal_catch_all_repo_paths");
            }
            WriteCmd::StoreEmbedding {
                page_id,
                vector_bytes,
                provider,
                model,
                dim,
                reply,
            } => {
                let result = ops::store_embedding(
                    &mut conn,
                    &page_id,
                    &vector_bytes,
                    &provider,
                    &model,
                    dim,
                );
                send_or_warn(reply, result, "store_embedding");
            }
            WriteCmd::StoreEmbeddingBatch { embeddings, reply } => {
                let result = ops::store_embeddings(&mut conn, &embeddings);
                send_or_warn(reply, result, "store_embeddings");
            }
            WriteCmd::DeleteStalePageEmbeddings {
                workspace_id,
                project_id,
                provider,
                model,
                dim,
                reply,
            } => {
                let result = ops::delete_stale_page_embeddings(
                    &mut conn,
                    &workspace_id,
                    project_id.as_ref(),
                    &provider,
                    &model,
                    dim,
                );
                send_or_warn(reply, result, "delete_stale_page_embeddings");
            }
            WriteCmd::PurgeProject {
                workspace_id,
                project_id,
                label,
                author_id,
                force,
                compaction,
                reply,
            } => {
                let result = ops::purge_project(
                    &mut conn,
                    &workspace_id,
                    &project_id,
                    &label,
                    author_id,
                    force,
                    compaction,
                );
                send_or_warn(reply, result, "purge_project");
            }
            WriteCmd::PurgeSession {
                workspace_id,
                project_id,
                session_id,
                author_id,
                compaction,
                reply,
            } => {
                let result = ops::purge_session(
                    &mut conn,
                    workspace_id,
                    project_id,
                    session_id,
                    author_id,
                    compaction,
                );
                send_or_warn(reply, result, "purge_session");
            }
            WriteCmd::Compact { reply } => {
                let result = ops::compact(&mut conn);
                send_or_warn(reply, result, "compact");
            }
            WriteCmd::DeleteWorkspace {
                workspace_id,
                force,
                compaction,
                reply,
            } => {
                let result = ops::delete_workspace(&mut conn, &workspace_id, force, compaction);
                send_or_warn(reply, result, "delete_workspace");
            }
            WriteCmd::RenameWorkspace {
                workspace_id,
                new_name,
                reply,
            } => {
                let result = ops::rename_workspace(&mut conn, &workspace_id, &new_name);
                send_or_warn(reply, result, "rename_workspace");
            }
            WriteCmd::MoveProjectWorkspace {
                project_id,
                from_workspace,
                to_workspace,
                reply,
            } => {
                let result = ops::move_project_workspace(
                    &mut conn,
                    &project_id,
                    &from_workspace,
                    &to_workspace,
                );
                send_or_warn(reply, result, "move_project_workspace");
            }
            WriteCmd::MoveSession {
                session_id,
                target_workspace,
                target_project,
                pages,
                author_id,
                commit,
                reply,
            } => {
                let result = ops::move_session(
                    &mut conn,
                    session_id,
                    target_workspace,
                    target_project,
                    pages,
                    author_id,
                    commit,
                );
                send_or_warn(reply, result, "move_session");
            }
            WriteCmd::RenameProject {
                workspace_id,
                project_id,
                new_name,
                author_id,
                reply,
            } => {
                let result = ops::rename_project(
                    &mut conn,
                    &workspace_id,
                    &project_id,
                    &new_name,
                    author_id,
                );
                send_or_warn(reply, result, "rename_project");
            }
            WriteCmd::InsertWikiMigration {
                name,
                applied_at,
                reply,
            } => {
                let result = ops::insert_wiki_migration(&mut conn, &name, applied_at);
                send_or_warn(reply, result, "insert_wiki_migration");
            }
            WriteCmd::CreateUser {
                new_user,
                token_hash,
                reply,
            } => {
                let result = users::insert_user(&conn, &new_user, &token_hash);
                send_or_warn(reply, result, "create_user");
            }
            WriteCmd::RotateUserToken {
                user_id,
                new_token_hash,
                reply,
            } => {
                let result = users::rotate_user_token(&conn, user_id, &new_token_hash);
                send_or_warn(reply, result, "rotate_user_token");
            }
            WriteCmd::ExpireUserToken { user_id, reply } => {
                let result = users::expire_user_token(&conn, user_id);
                send_or_warn(reply, result, "expire_user_token");
            }
            WriteCmd::ReviveUserToken { user_id, reply } => {
                let result = users::revive_user_token(&conn, user_id);
                send_or_warn(reply, result, "revive_user_token");
            }
            WriteCmd::TouchUserLastSeen { user_id, reply } => {
                let result = users::touch_user_last_seen(&conn, user_id);
                send_or_warn(reply, result, "touch_user_last_seen");
            }
            WriteCmd::StageAutoImproveRun { input, reply } => {
                let result = crate::auto_improve::stage_run(&mut conn, &input);
                send_or_warn(reply, result, "stage_auto_improve_run");
            }
            WriteCmd::StageAutoImproveRunForOwner {
                input,
                owner,
                reply,
            } => {
                let result =
                    crate::auto_improve::stage_run_for_owner(&mut conn, &input, owner.as_ref());
                send_or_warn(reply, result, "stage_auto_improve_run_for_owner");
            }
            WriteCmd::RejectAutoImproveProposal { input, reply } => {
                let result = crate::auto_improve::reject_proposal(&mut conn, &input);
                send_or_warn(reply, result, "reject_auto_improve_proposal");
            }
            WriteCmd::FailAutoImproveProposal { input, reply } => {
                let result = crate::auto_improve::fail_proposal(&mut conn, &input);
                send_or_warn(reply, result, "fail_auto_improve_proposal");
            }
            WriteCmd::ApproveAutoImproveProposal { input, reply } => {
                let result = crate::auto_improve::approve_proposal(&mut conn, &input);
                send_or_warn(reply, result, "approve_auto_improve_proposal");
            }
            WriteCmd::EnsureAutoImproveSchedulerState {
                workspace_id,
                project_id,
                reply,
            } => {
                let result = crate::auto_improve::ensure_scheduler_state(
                    &mut conn,
                    workspace_id,
                    project_id,
                );
                send_or_warn(reply, result, "ensure_auto_improve_scheduler_state");
            }
            WriteCmd::ClaimAutoImproveSchedulerSession {
                workspace_id,
                project_id,
                session_id,
                ended_at,
                reply,
            } => {
                let result = crate::auto_improve::claim_scheduler_session(
                    &mut conn,
                    workspace_id,
                    project_id,
                    session_id,
                    ended_at,
                );
                send_or_warn(reply, result, "claim_auto_improve_scheduler_session");
            }
            WriteCmd::RecordMaintenanceJobSuccess { job, reply } => {
                let result = crate::maintenance::record_success(&conn, job);
                send_or_warn(reply, result, "record_maintenance_job_success");
            }
            WriteCmd::PrepareWorkstreamRun { input, reply } => {
                let result = crate::workstream::prepare_run(&mut conn, &input);
                send_or_warn(reply, result, "prepare_workstream_run");
            }
            WriteCmd::HeartbeatManagedRun { run_id, reply } => {
                let result = crate::workstream::heartbeat(&mut conn, run_id);
                send_or_warn(reply, result, "heartbeat_managed_run");
            }
            WriteCmd::CancelManagedRun { run_id, reply } => {
                let result = crate::workstream::cancel_run(&mut conn, run_id);
                send_or_warn(reply, result, "cancel_managed_run");
            }
            WriteCmd::LinkManagedRunSession {
                run_id,
                agent,
                native_session_id,
                reply,
            } => {
                let result = crate::workstream::link_native_session(
                    &mut conn,
                    run_id,
                    agent,
                    &native_session_id,
                );
                send_or_warn(reply, result, "link_managed_run_session");
            }
            WriteCmd::AcceptManagedRunContext { run_id, reply } => {
                let result = crate::workstream::accept_context(&mut conn, run_id);
                send_or_warn(reply, result, "accept_managed_run_context");
            }
            WriteCmd::AcceptStartupContext {
                handoff,
                managed_run_id,
                receiving_session,
                reply,
            } => {
                let result = (|| {
                    let tx = conn.transaction()?;
                    if let Some(session) = &receiving_session {
                        let matching_acceptance = handoff.as_ref().is_some_and(|acceptance| {
                            acceptance.accepting_session == Some(session.id)
                                && acceptance.workspace_id == session.workspace_id
                                && acceptance.project_id == session.project_id
                                && acceptance.accepting_agent == session.agent_kind
                        });
                        if !matching_acceptance {
                            return Err(StoreError::InvalidState(
                                "startup receiver session does not match its handoff claim".into(),
                            ));
                        }
                        ops::begin_session_in_transaction(&tx, session)?;
                    }
                    let managed_context_accepted = match managed_run_id {
                        Some(run_id) => {
                            crate::workstream::claim_context_in_transaction(&tx, run_id)?
                        }
                        None => false,
                    };
                    if managed_run_id.is_some() && !managed_context_accepted {
                        return Ok(StartupContextAcceptance::default());
                    }
                    let handoff_accepted = match handoff {
                        Some(acceptance) => ops::accept_handoff_in_transaction(&tx, &acceptance)?,
                        None => false,
                    };
                    tx.commit()?;
                    Ok(StartupContextAcceptance {
                        handoff_accepted,
                        managed_context_accepted,
                    })
                })();
                send_or_warn(reply, result, "accept_startup_context");
            }
            WriteCmd::FinishWorkstreamRun { input, reply } => {
                let result = crate::workstream::finish_run(&mut conn, &input);
                send_or_warn(reply, result, "finish_workstream_run");
            }
            WriteCmd::RenameWorkstream { input, reply } => {
                let result = crate::workstream::rename(&mut conn, &input);
                send_or_warn(reply, result, "rename_workstream");
            }
        }
    }
    tracing::debug!("writer thread exiting cleanly");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use ai_memory_core::{NewPage, PagePath, Tier};
    use std::time::Duration;
    use tempfile::TempDir;

    fn sample_page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
        NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: "test".into(),
            body: body.into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        }
    }

    /// Enqueue a workspace creation without awaiting the reply, returning
    /// the oneshot receiver so the caller decides when to collect it.
    async fn enqueue_workspace(
        writer: &WriterHandle,
        name: &str,
    ) -> oneshot::Receiver<StoreResult<WorkspaceId>> {
        let (tx, rx) = oneshot::channel();
        writer
            .inner
            .tx
            .send(WriteCmd::GetOrCreateWorkspace {
                name: name.to_owned(),
                reply: tx,
            })
            .await
            .expect("writer channel closed while enqueueing");
        rx
    }

    /// Commands enqueued on the bounded channel run on the writer thread in
    /// FIFO order: insertion rowids must match enqueue order exactly.
    #[tokio::test]
    async fn executes_commands_in_fifo_order() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        let mut pending = Vec::new();
        for i in 0..8 {
            pending.push(enqueue_workspace(&store.writer, &format!("fifo-{i:02}")).await);
        }
        for rx in pending {
            rx.await.unwrap().unwrap();
        }

        let conn = Connection::open(store.db_path()).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM workspaces WHERE name LIKE 'fifo-%' ORDER BY rowid")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let expected: Vec<String> = (0..8).map(|i| format!("fifo-{i:02}")).collect();
        assert_eq!(
            names, expected,
            "single writer thread must apply commands in enqueue order"
        );
    }

    /// A command whose SQL fails returns the error on *its* oneshot only;
    /// the actor keeps processing — the next command succeeds.
    #[tokio::test]
    async fn sql_error_reaches_caller_without_killing_actor() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();

        // FK violation: no project row exists for this id.
        let err = store
            .writer
            .upsert_page(sample_page(ws, ProjectId::new(), "notes/bogus.md", "x"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Sqlite(_)),
            "expected the raw SQL failure to reach the caller, got {err:?}"
        );

        // The actor survived: a well-formed write lands afterwards.
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/fine.md", "ok"))
            .await
            .unwrap();
        let conn = Connection::open(store.db_path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "only the post-error write may be present");
    }

    /// A batch command is one transaction: a failure part-way through rolls
    /// back the pages that already succeeded inside the same batch.
    #[tokio::test]
    async fn batch_upsert_rolls_back_on_mid_batch_failure() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "app", None)
            .await
            .unwrap();

        let pages = vec![
            sample_page(ws, proj, "notes/good.md", "good"),
            // Second page references a project that does not exist.
            sample_page(ws, ProjectId::new(), "notes/bad.md", "bad"),
        ];
        let err = store.writer.upsert_pages_batch(pages).await.unwrap_err();
        assert!(
            matches!(err, StoreError::Sqlite(_)),
            "expected FK failure from the batch, got {err:?}"
        );

        let conn = Connection::open(store.db_path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "mid-batch failure must roll back the earlier page too"
        );

        // Actor is still alive after the rolled-back batch.
        store
            .writer
            .upsert_page(sample_page(ws, proj, "notes/after.md", "after"))
            .await
            .unwrap();
    }

    /// Dropping the last handle queues `Shutdown` behind the pending
    /// commands and joins the thread: every already-enqueued command is
    /// answered, and the join returning proves the thread exited (no hang).
    #[tokio::test]
    async fn drop_completes_pending_commands_and_joins_thread() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let writer = store.writer.clone();
        let db_path = store.db_path().to_owned();
        drop(store);

        let mut pending = Vec::new();
        for i in 0..8 {
            pending.push(enqueue_workspace(&writer, &format!("drain-{i:02}")).await);
        }
        // Joins the writer thread inside `Drop for WriterInner`; returns
        // only after the queue (including Shutdown) was drained.
        drop(writer);

        for rx in pending {
            let id = tokio::time::timeout(Duration::from_secs(10), rx)
                .await
                .expect("pending command was not answered before the writer stopped")
                .expect("reply oneshot dropped")
                .unwrap();
            let conn = Connection::open(&db_path).unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM workspaces WHERE id = ?1",
                    [id.as_bytes()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "pending command must have committed");
        }
    }

    /// The dry run and the real move both go through the actor; only the
    /// latter changes the session row a reader sees.
    #[tokio::test]
    async fn move_session_dry_run_then_commit_through_writer() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let src = store
            .writer
            .get_or_create_project(ws, "src", None)
            .await
            .unwrap();
        let dst = store
            .writer
            .get_or_create_project(ws, "dst", None)
            .await
            .unwrap();
        let sid = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: sid,
                workspace_id: ws,
                project_id: src,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();

        let dry = store
            .writer
            .move_session(sid, ws, dst, PagesMode::Move, None, false)
            .await
            .unwrap();
        assert!(dry.session_moved);
        assert_eq!(
            store.reader.session_project_ids(sid).await.unwrap(),
            Some((ws, src)),
            "dry run must roll back"
        );

        let moved = store
            .writer
            .move_session(sid, ws, dst, PagesMode::Move, None, true)
            .await
            .unwrap();
        assert!(moved.session_moved);
        assert_eq!(
            store.reader.session_project_ids(sid).await.unwrap(),
            Some((ws, dst))
        );
    }

    /// Once the actor has stopped, calls fail fast with
    /// [`StoreError::WriterClosed`] instead of hanging on a dead channel.
    #[tokio::test]
    async fn commands_after_shutdown_return_writer_closed() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let writer = store.writer.clone();
        drop(store);

        writer
            .inner
            .tx
            .send(WriteCmd::Shutdown)
            .await
            .expect("writer channel closed before Shutdown");

        // FIFO guarantees Shutdown is processed first; whether the send
        // itself fails or the reply oneshot is dropped, the caller sees
        // WriterClosed either way.
        let err = writer
            .get_or_create_workspace("too-late")
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::WriterClosed),
            "expected WriterClosed, got {err:?}"
        );
    }
}
