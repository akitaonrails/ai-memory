//! Durable, chunked session processing (`docs/design-session-chunking.md`).
//!
//! [`ProcessingService`] is the single entry point: it enqueues a job (the
//! store snapshots the session's observation manifest), drives it through
//! bounded map and reduce calls whose results are checkpointed as nodes,
//! makes one final call in the existing structured types, and then publishes
//! through the existing wiki and staging paths, reconciling the publication
//! identity after a crash instead of repeating an effect.
//!
//! Nothing here runs unless `[chunking].enabled`: with the flag off no job is
//! created, no worker polls, and the legacy consolidator and reviewer paths
//! are untouched.

mod budget;
mod chunk_digest;
mod chunk_pipeline;
mod chunk_plan;
mod final_stage;
mod publish;
mod run;

use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::chunking::ChunkingSettings;
use ai_memory_core::{
    ActorContext, AutoImproveProposalId, AutoImproveRunId, IdentityKey, PagePath, ProcessingJobId,
    ProcessingJobKind, ProcessingJobState, ProcessingJobStatus, ProjectId, SessionId, UserId,
    WorkspaceId,
};
use ai_memory_llm::LlmProvider;
use ai_memory_store::{
    NewProcessingJob, ProcessingJobCreated, ProcessingOwnerFilter, ReaderPool, SkippedProposal,
    StoreError, WriterHandle,
};
use ai_memory_wiki::{AdmissionContext, AdmissionOp, Wiki, WikiError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::auto_improve::{AutoImproveError, AutoImproveReviewConfig};
use crate::consolidator::{
    Consolidator, ConsolidatorError, ConsolidatorResult,
    DEFAULT_CONSOLIDATION_INPUT_TOKEN_SAFETY_MARGIN, DEFAULT_CONSOLIDATION_MAX_INPUT_TOKENS,
    DEFAULT_CONSOLIDATION_MAX_OUTPUT_TOKENS, redacted_error_summary,
};
use crate::types::ConsolidationOutcome;
use budget::{PIPELINE_VERSION, StageBudgets, final_prompt, fingerprint, schema_fingerprint};
use chunk_digest::{ChunkDigest, MAP_SYSTEM_PROMPT, REDUCE_SYSTEM_PROMPT, ReviewEnvelope};

/// Lease a worker holds on a job between renewals.
pub(crate) const LEASE: Duration = Duration::from_secs(120);
/// Output ceiling of the legacy auto-improve review call (its own constant is
/// private to `auto_improve`); the chunked review is bounded by the same.
const AUTO_IMPROVE_REVIEW_OUTPUT_TOKENS: u32 = 16_000;
const IDLE_POLL: Duration = Duration::from_secs(2);
const ERROR_BACKOFF: Duration = Duration::from_secs(5);
const CLEANUP_EVERY: Duration = Duration::from_secs(60 * 60);
const INLINE_POLL: Duration = Duration::from_secs(1);

const ALL_KINDS: [ProcessingJobKind; 3] = [
    ProcessingJobKind::Consolidate,
    ProcessingJobKind::ConsolidateMulti,
    ProcessingJobKind::AutoImprove,
];

/// Errors of the processing surface.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProcessingError {
    /// `[chunking].enabled` is false: the new operation is not available.
    #[error("chunked processing is disabled")]
    Disabled,
    /// No LLM provider is configured on this server.
    #[error("LLM provider not configured on server")]
    NoProvider,
    /// The session does not exist in the requested project.
    #[error("session not found")]
    SessionNotFound,
    /// The session has no observations to process.
    #[error("session has no observations")]
    EmptySession,
    /// A paused or otherwise unfinished chunked job exists for this exact
    /// session, operation and owner: the legacy path must not run beside it.
    #[error("an equivalent chunked processing job for this session is paused or unfinished")]
    PausedJob,
    /// The durable queue is at its fixed admission limit and the request is
    /// not equivalent to an unfinished job. Retryable once jobs finish.
    #[error("the processing queue is full; retry after unfinished jobs finish")]
    QueueFull,
    /// The request is invalid.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// The chunk budget or call ceiling cannot cover the work.
    #[error("chunking budget: {0}")]
    Budget(String),
    /// Store failure.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Consolidation failure.
    #[error(transparent)]
    Consolidator(#[from] ConsolidatorError),
    /// Auto-improve failure.
    #[error(transparent)]
    AutoImprove(#[from] AutoImproveError),
    /// Wiki failure.
    #[error(transparent)]
    Wiki(#[from] WikiError),
    /// Anything else (redacted text only).
    #[error("{0}")]
    Other(String),
}

impl ProcessingError {
    /// One-line class summary safe to log or return: never provider bodies,
    /// prompts, SQL or paths.
    #[must_use]
    pub fn redacted_summary(&self) -> String {
        match self {
            Self::Disabled => "chunked processing is disabled".into(),
            Self::NoProvider => "LLM provider not configured on server".into(),
            Self::SessionNotFound => "session not found".into(),
            Self::EmptySession => "session has no observations".into(),
            Self::QueueFull => "the processing queue is full for this server or for this \
                 project and operator; retry after unfinished jobs finish"
                .into(),
            Self::PausedJob => "an equivalent chunked processing job for this session is paused \
                 or unfinished; re-enable [chunking] to resume it or wait for retention cleanup"
                .into(),
            Self::Invalid(m) | Self::Budget(m) | Self::Other(m) => m.clone(),
            Self::Store(_) => "processing failed: class=store".into(),
            Self::Wiki(_) => "processing failed: class=wiki".into(),
            Self::Consolidator(e) => redacted_error_summary(e),
            Self::AutoImprove(e) => crate::auto_improve::redacted_auto_improve_summary(e),
        }
    }
}

/// A request to start (or join) one durable job.
#[derive(Debug, Clone)]
pub struct ProcessingJobStart {
    /// Scope the session's observations live in.
    pub workspace_id: WorkspaceId,
    /// Scope the session's observations live in.
    pub project_id: ProjectId,
    /// Session to process.
    pub session_id: SessionId,
    /// Operation to run.
    pub kind: ProcessingJobKind,
    /// Attribution of the eventual publication.
    pub actor: ActorContext,
    /// Attribution author of the eventual publication.
    pub author_id: Option<UserId>,
    /// Staging owner bucket (`owner_identity(..)`), as for auto-improve staging.
    pub owner: Option<IdentityKey>,
    /// Who may read the status and result.
    pub viewer: Option<UserId>,
    /// Per-call consolidation instructions.
    pub instructions: Option<String>,
}

/// Who is asking for a job's status.
#[derive(Debug, Clone, Default)]
pub struct ProcessingJobAccess {
    /// Staging owner bucket of the caller.
    pub owner: Option<IdentityKey>,
    /// Calling user, when identified.
    pub viewer: Option<UserId>,
    /// Root: may read every owner's jobs (derived from auth, never input).
    pub any_owner: bool,
}

/// The authenticated caller of a synchronous consolidation or a SessionEnd
/// consolidation. `owner` is the same `owner_identity(..)` bucket an
/// asynchronous start computes, and `viewer` is the same authorization
/// viewer (`AuthorizedViewer`, re-checked before every effect; `None` is a
/// server-owned/trusted job). Both are part of the job's equivalence, so one
/// operator's entries converge and never join a weaker or foreign
/// authorization. `author_id` is attribution only and is never used for
/// authorization.
#[derive(Debug, Clone, Default)]
pub struct ProcessingCaller {
    /// Attribution of the eventual publication.
    pub actor: ActorContext,
    /// Attribution author of the eventual publication.
    pub author_id: Option<UserId>,
    /// Authorization viewer (`AuthorizedViewer`), distinct from `author_id`.
    pub viewer: Option<UserId>,
    /// Staging owner bucket (`owner_identity(..)`); `None` is the shared one.
    pub owner: Option<IdentityKey>,
}

/// A durable review + staging + approval request (admin/MCP stage branch).
#[derive(Debug, Clone)]
pub struct AutoImproveStageRequest {
    /// Scope.
    pub workspace_id: WorkspaceId,
    /// Scope.
    pub project_id: ProjectId,
    /// Session to review.
    pub session_id: SessionId,
    /// Effective review config for this call.
    pub cfg: AutoImproveReviewConfig,
    /// Actor attribution.
    pub actor: ActorContext,
    /// Author attribution.
    pub author_id: Option<UserId>,
    /// Authorization viewer (`AuthorizedViewer`), distinct from `author_id`.
    pub viewer: Option<UserId>,
    /// Staging owner bucket.
    pub owner: Option<IdentityKey>,
    /// Admission context of the originating request, used for approvals.
    pub admission_ctx: Option<AdmissionContext>,
}

/// One staged proposal and what became of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoImproveStagedProposal {
    /// Proposal id.
    pub id: AutoImproveProposalId,
    /// Wiki-relative sidecar path.
    pub sidecar_path: String,
    /// `pending`, `approved`, `conflict`, `rejected` or `failed`.
    pub status: String,
    /// Page written by an approval.
    pub page_id: Option<String>,
}

/// What a completed review job staged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoImproveStaged {
    /// Staged run.
    pub run_id: AutoImproveRunId,
    /// Whether manual approval is required.
    pub approval_required: bool,
    /// Review summary.
    pub summary: String,
    /// Non-fatal notes.
    pub warnings: Vec<String>,
    /// Candidates rejected by the reviewer or validators.
    pub rejected_candidates_count: usize,
    /// Staged proposals in staging order.
    pub proposals: Vec<AutoImproveStagedProposal>,
    /// Proposals the store declined to stage.
    pub skipped: Vec<SkippedProposal>,
}

#[derive(Clone)]
struct ConsolidationLimits {
    max_input_tokens: usize,
    max_output_tokens: u32,
    safety_margin: f64,
}

#[derive(Clone)]
struct Inner {
    reader: ReaderPool,
    writer: WriterHandle,
    wiki: Wiki,
    llm: Option<Arc<dyn LlmProvider>>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    chunking: ChunkingSettings,
    consolidation: ConsolidationLimits,
    per_user_slots: bool,
    auto_improve: AutoImproveReviewConfig,
    require_approval: bool,
}

/// Options a job carries in its (equivalence-defining) `options_json`.
#[derive(Debug, Clone, Default)]
pub(crate) struct StartOptions {
    pub instructions: Option<String>,
    pub review: Option<AutoImproveReviewConfig>,
    pub trigger: &'static str,
}

/// What the driver reads back from `options_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobOptions {
    pub instructions: Option<String>,
    pub review: Option<AutoImproveReviewConfig>,
    pub trigger: String,
}

/// Per-run overrides that never persist (the originating request's context).
#[derive(Debug, Clone, Default)]
pub(crate) struct Overrides {
    pub admission_ctx: Option<AdmissionContext>,
}

/// Shared, cloneable handle to the durable processing engine.
#[derive(Clone)]
pub struct ProcessingService {
    inner: Arc<Inner>,
}

impl ProcessingService {
    /// Build the service. `llm` is `None` on a server without a provider: the
    /// service then only serves status reads and `pause_active_jobs`.
    #[must_use]
    pub fn new(
        reader: ReaderPool,
        writer: WriterHandle,
        wiki: Wiki,
        llm: Option<Arc<dyn LlmProvider>>,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        chunking: ChunkingSettings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                reader,
                writer,
                wiki,
                llm,
                workspace_id,
                project_id,
                chunking,
                consolidation: ConsolidationLimits {
                    max_input_tokens: DEFAULT_CONSOLIDATION_MAX_INPUT_TOKENS,
                    max_output_tokens: DEFAULT_CONSOLIDATION_MAX_OUTPUT_TOKENS,
                    safety_margin: DEFAULT_CONSOLIDATION_INPUT_TOKEN_SAFETY_MARGIN,
                },
                per_user_slots: false,
                auto_improve: AutoImproveReviewConfig::default(),
                require_approval: true,
            }),
        }
    }

    /// The `[consolidation]` limits: per-call ceilings, never a reason to drop
    /// the part of a session that exceeds them.
    #[must_use]
    pub fn with_consolidation_limits(
        mut self,
        max_input_tokens: usize,
        max_output_tokens: u32,
        safety_margin: f64,
    ) -> Self {
        Arc::make_mut(&mut self.inner).consolidation = ConsolidationLimits {
            max_input_tokens,
            max_output_tokens,
            safety_margin,
        };
        self
    }

    /// Namespace engine-written slots per operator (`[slots] per_user`).
    #[must_use]
    pub fn with_per_user_slots(mut self, enabled: bool) -> Self {
        Arc::make_mut(&mut self.inner).per_user_slots = enabled;
        self
    }

    /// The server-owned `[auto_improve]` review config and approval policy.
    #[must_use]
    pub fn with_auto_improve(
        mut self,
        review: AutoImproveReviewConfig,
        require_approval: bool,
    ) -> Self {
        let inner = Arc::make_mut(&mut self.inner);
        inner.auto_improve = review;
        inner.require_approval = require_approval;
        self
    }

    /// Whether `[chunking].enabled` is on.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.inner.chunking.enabled
    }

    /// Validate the whole path (map, reduce and every final envelope) against
    /// the configured budgets.
    ///
    /// # Errors
    /// A message naming the knob that must change.
    pub fn validate(&self) -> Result<(), String> {
        self.inner.chunking.validate()?;
        for kind in ALL_KINDS {
            self.budgets(kind, None)?;
        }
        Ok(())
    }

    fn budgets(
        &self,
        kind: ProcessingJobKind,
        review: Option<&AutoImproveReviewConfig>,
    ) -> Result<StageBudgets, String> {
        let inner = &self.inner;
        let (op_tokens, op_out) = match kind {
            ProcessingJobKind::AutoImprove => (
                review.unwrap_or(&inner.auto_improve).max_input_tokens,
                AUTO_IMPROVE_REVIEW_OUTPUT_TOKENS,
            ),
            _ => (
                inner.consolidation.max_input_tokens,
                inner.consolidation.max_output_tokens,
            ),
        };
        let margin = inner
            .consolidation
            .safety_margin
            .min(DEFAULT_CONSOLIDATION_INPUT_TOKEN_SAFETY_MARGIN);
        StageBudgets::derive(
            kind,
            inner.chunking.max_input_tokens.min(op_tokens),
            margin,
            op_out,
        )
    }

    fn llm(&self) -> Result<&Arc<dyn LlmProvider>, ProcessingError> {
        self.inner.llm.as_ref().ok_or(ProcessingError::NoProvider)
    }

    /// A legacy consolidator over the same handles, used only for its
    /// publication guards and prompt-context helpers (never for projection).
    pub(crate) fn legacy(&self) -> Result<Consolidator, ProcessingError> {
        let inner = &self.inner;
        Ok(Consolidator::new(
            inner.reader.clone(),
            inner.writer.clone(),
            inner.wiki.clone(),
            self.llm()?.clone(),
            inner.workspace_id,
            inner.project_id,
        )
        .with_prompt_limits(
            inner.consolidation.max_input_tokens,
            inner.consolidation.max_output_tokens,
            inner.consolidation.safety_margin,
        )
        .with_per_user_slots(inner.per_user_slots))
    }

    /// Fingerprint of everything that decides what a job's results mean:
    /// pipeline version, provider/model, prompts, schemas, budgets and the
    /// operation's policy. A job whose stored value differs is invalidated.
    fn context_fingerprint(
        &self,
        kind: ProcessingJobKind,
        review: Option<&AutoImproveReviewConfig>,
    ) -> Result<String, ProcessingError> {
        let llm = self.llm()?;
        let budgets = self
            .budgets(kind, review)
            .map_err(ProcessingError::Budget)?;
        let (final_system, _) = final_prompt(kind);
        let final_schema = match kind {
            ProcessingJobKind::Consolidate => {
                schema_fingerprint::<crate::types::ConsolidatedPage>()
            }
            ProcessingJobKind::ConsolidateMulti => {
                schema_fingerprint::<crate::types::ConsolidatedBatch>()
            }
            ProcessingJobKind::AutoImprove => schema_fingerprint::<ReviewEnvelope>(),
        };
        let c = &self.inner.consolidation;
        let parts: Vec<String> = vec![
            PIPELINE_VERSION.into(),
            kind.as_str().into(),
            llm.cache_identity(),
            fingerprint(&[MAP_SYSTEM_PROMPT]),
            fingerprint(&[REDUCE_SYSTEM_PROMPT]),
            fingerprint(&[final_system]),
            schema_fingerprint::<ChunkDigest>(),
            final_schema,
            format!("{budgets:?}"),
            format!(
                "{}:{}:{}:{}",
                self.inner.chunking.max_input_tokens,
                c.max_input_tokens,
                c.max_output_tokens,
                c.safety_margin.to_bits()
            ),
            format!(
                "{}:{}",
                self.inner.per_user_slots, self.inner.require_approval
            ),
        ];
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        Ok(fingerprint(&refs))
    }

    fn options_json(&self, kind: ProcessingJobKind, opts: &StartOptions) -> serde_json::Value {
        let review = (kind == ProcessingJobKind::AutoImprove).then(|| {
            opts.review
                .clone()
                .unwrap_or_else(|| self.inner.auto_improve.clone())
        });
        serde_json::to_value(JobOptions {
            instructions: opts.instructions.clone(),
            review,
            trigger: opts.trigger.to_string(),
        })
        .unwrap_or(serde_json::Value::Null)
    }

    /// Create (or join the compatible) job.
    pub(crate) async fn enqueue(
        &self,
        req: &ProcessingJobStart,
        opts: &StartOptions,
    ) -> Result<ProcessingJobCreated, ProcessingError> {
        if !self.enabled() {
            return Err(ProcessingError::Disabled);
        }
        self.llm()?;
        let review = (req.kind == ProcessingJobKind::AutoImprove).then(|| {
            opts.review
                .clone()
                .unwrap_or_else(|| self.inner.auto_improve.clone())
        });
        let context_fingerprint = self.context_fingerprint(req.kind, review.as_ref())?;
        if req.kind != ProcessingJobKind::AutoImprove {
            // The same pre-LLM admission the legacy path runs: a rejected
            // scope or actor fails before any job exists.
            let anchor = PagePath::new(format!("sessions/{}.md", req.session_id))
                .map_err(|e| ProcessingError::Invalid(e.to_string()))?;
            self.inner
                .wiki
                .preflight_admission(
                    req.workspace_id,
                    req.project_id,
                    &anchor,
                    AdmissionOp::Consolidate,
                    req.actor.clone(),
                )
                .await?;
        }
        self.inner
            .writer
            .create_processing_job(NewProcessingJob {
                workspace_id: req.workspace_id,
                project_id: req.project_id,
                session_id: req.session_id,
                kind: req.kind,
                actor: req.actor.clone(),
                author_id: req.author_id,
                viewer: req.viewer,
                owner: req.owner.clone(),
                options_json: self.options_json(req.kind, opts),
                context_fingerprint,
                max_calls: self.inner.chunking.max_calls_per_run,
            })
            .await
            .map_err(|e| match e {
                StoreError::NotFound(_) => ProcessingError::SessionNotFound,
                StoreError::InvalidState(m) if m.contains("no observations") => {
                    ProcessingError::EmptySession
                }
                StoreError::InvalidState(m) => ProcessingError::Invalid(m),
                StoreError::ProcessingQueueFull(_) => ProcessingError::QueueFull,
                other => ProcessingError::Store(other),
            })
    }

    /// Start (or join the compatible) durable job and return immediately.
    ///
    /// # Errors
    /// [`ProcessingError::Disabled`] while the flag is off,
    /// [`ProcessingError::NoProvider`] without a provider,
    /// [`ProcessingError::QueueFull`] when a *new* job would exceed the
    /// store's unfinished-job limits (joining an equivalent job never does).
    pub async fn start(
        &self,
        req: ProcessingJobStart,
    ) -> Result<ProcessingJobStatus, ProcessingError> {
        let opts = StartOptions {
            instructions: req.instructions.clone(),
            review: None,
            trigger: "durable",
        };
        Ok(self.enqueue(&req, &opts).await?.status)
    }

    /// Status of one job; `None` for an unknown id or a foreign scope/owner.
    /// Works while the flag is off so earlier jobs stay observable.
    ///
    /// # Errors
    /// Store failures.
    pub async fn status(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        job_id: ProcessingJobId,
        access: ProcessingJobAccess,
    ) -> Result<Option<ProcessingJobStatus>, ProcessingError> {
        let filter = if access.any_owner {
            ProcessingOwnerFilter::Any
        } else {
            ProcessingOwnerFilter::Owner(access.owner)
        };
        Ok(self
            .inner
            .reader
            .processing_job_status(workspace_id, project_id, job_id, filter)
            .await?)
    }

    /// Refuse the legacy path while an unfinished chunked job (queued,
    /// running, paused, over budget) exists for this exact session,
    /// operation and owner bucket, so an old truncated summary can never be
    /// published beside it. A store read only: it works with the flag off and
    /// without a provider, and a fresh install (no jobs) is unaffected.
    ///
    /// # Errors
    /// [`ProcessingError::PausedJob`] when such a job exists.
    pub async fn reject_paused(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        kind: ProcessingJobKind,
        owner: Option<IdentityKey>,
    ) -> Result<(), ProcessingError> {
        if self
            .inner
            .reader
            .has_unfinished_processing_job(workspace_id, project_id, session_id, kind, owner)
            .await?
        {
            return Err(ProcessingError::PausedJob);
        }
        Ok(())
    }

    /// Pause every active job (flag turned off): nothing is processed and no
    /// intermediate result is published.
    ///
    /// # Errors
    /// Store failures.
    pub async fn pause_active_jobs(&self) -> Result<u64, ProcessingError> {
        if self.enabled() {
            return Ok(0);
        }
        Ok(self.inner.writer.pause_processing_jobs().await?)
    }

    /// Claim one runnable job server-wide and drive it.
    ///
    /// # Errors
    /// Store failures while claiming or checkpointing.
    pub async fn execute_next(&self) -> Result<Option<ProcessingJobStatus>, ProcessingError> {
        if !self.enabled() || self.inner.llm.is_none() {
            return Ok(None);
        }
        let Some(claimed) = self.inner.writer.claim_next_processing_job(LEASE).await? else {
            return Ok(None);
        };
        self.run_claimed(claimed, Overrides::default())
            .await
            .map(Some)
    }

    /// Worker loop: resumes paused jobs, polls for work until `stop` fires and
    /// prunes old terminal jobs hourly.
    pub async fn run_worker(&self, mut stop: tokio::sync::watch::Receiver<bool>) {
        if !self.enabled() || self.inner.llm.is_none() {
            return;
        }
        match self.inner.writer.resume_paused_processing_jobs().await {
            Ok(0) => {}
            Ok(n) => info!(jobs = n, "resumed paused processing jobs"),
            Err(e) => warn!(error = %e, "could not resume paused processing jobs"),
        }
        let mut last_cleanup: Option<std::time::Instant> = None;
        loop {
            if *stop.borrow() {
                break;
            }
            if last_cleanup.is_none_or(|t| t.elapsed() >= CLEANUP_EVERY) {
                last_cleanup = Some(std::time::Instant::now());
                if let Err(e) = self
                    .inner
                    .writer
                    .cleanup_processing_jobs(
                        ai_memory_store::processing_jobs::PROCESSING_JOB_RETENTION_DEFAULT,
                    )
                    .await
                {
                    warn!(error = %e, "processing job cleanup failed");
                }
            }
            let ran = tokio::select! {
                r = self.execute_next() => r,
                _ = stop.changed() => break,
            };
            let pause = match ran {
                Ok(Some(_)) => continue,
                Ok(None) => IDLE_POLL,
                Err(e) => {
                    warn!(error = %e.redacted_summary(), "processing worker step failed");
                    ERROR_BACKOFF
                }
            };
            tokio::select! {
                () = tokio::time::sleep(pause) => {}
                _ = stop.changed() => break,
            }
        }
    }

    /// Enqueue (or join) and drive a job on the calling task, polling when
    /// another worker holds the lease. Returns the first non-active status.
    pub(crate) async fn run_inline(
        &self,
        req: &ProcessingJobStart,
        opts: &StartOptions,
        overrides: Overrides,
    ) -> Result<ProcessingJobStatus, ProcessingError> {
        let mut status = self.enqueue(req, opts).await?.status;
        loop {
            if status.state == ProcessingJobState::Completed {
                return Ok(status);
            }
            match self
                .inner
                .writer
                .claim_processing_job(req.workspace_id, req.project_id, status.job_id, LEASE)
                .await?
            {
                Some(claimed) => return self.run_claimed(claimed, overrides).await,
                None => {
                    let Some(current) = self
                        .inner
                        .reader
                        .processing_job_status(
                            req.workspace_id,
                            req.project_id,
                            status.job_id,
                            ProcessingOwnerFilter::Any,
                        )
                        .await?
                    else {
                        return Err(ProcessingError::Other("processing job disappeared".into()));
                    };
                    if current.state.is_terminal()
                        || matches!(
                            current.state,
                            ProcessingJobState::Paused | ProcessingJobState::BudgetExceeded
                        )
                    {
                        return Ok(current);
                    }
                    status = current;
                    tokio::time::sleep(INLINE_POLL).await;
                }
            }
        }
    }

    fn incomplete(status: &ProcessingJobStatus) -> ProcessingError {
        let reason = status.last_error.clone().unwrap_or_default();
        let message = format!("processing job {}: {reason}", status.state);
        match status.state {
            ProcessingJobState::BudgetExceeded => ProcessingError::Budget(message),
            _ => ProcessingError::Other(message),
        }
    }

    fn to_consolidator_error(session_id: SessionId, error: ProcessingError) -> ConsolidatorError {
        match error {
            ProcessingError::EmptySession => ConsolidatorError::EmptySession(session_id),
            ProcessingError::SessionNotFound => ConsolidatorError::SessionNotFound(session_id),
            ProcessingError::Store(e) => ConsolidatorError::Store(e),
            ProcessingError::Wiki(e) => ConsolidatorError::Wiki(e),
            ProcessingError::Consolidator(e) => e,
            other => ConsolidatorError::Processing(other.redacted_summary()),
        }
    }

    /// The shared bucket and, when named, the caller's own: a paused or
    /// unfinished job in either must block the legacy path.
    async fn reject_paused_for(
        &self,
        workspace_id: WorkspaceId,
        project_id: ProjectId,
        session_id: SessionId,
        kind: ProcessingJobKind,
        owner: Option<IdentityKey>,
    ) -> Result<(), ProcessingError> {
        self.reject_paused(workspace_id, project_id, session_id, kind, None)
            .await?;
        if owner.is_some() {
            self.reject_paused(workspace_id, project_id, session_id, kind, owner)
                .await?;
        }
        Ok(())
    }

    async fn consolidate_inner(
        &self,
        kind: ProcessingJobKind,
        session_id: SessionId,
        caller: ProcessingCaller,
        instructions: Option<&str>,
    ) -> ConsolidatorResult<Vec<ConsolidationOutcome>> {
        let legacy = self
            .legacy()
            .map_err(|e| Self::to_consolidator_error(session_id, e))?;
        // Scope comes from where the session's observations landed, exactly
        // like the legacy path; the server default only covers pre-routing
        // sessions.
        let (ws, proj) = legacy.resolve_target(session_id).await?;
        let req = ProcessingJobStart {
            workspace_id: ws,
            project_id: proj,
            session_id,
            kind,
            actor: caller.actor,
            author_id: caller.author_id,
            owner: caller.owner,
            viewer: caller.viewer,
            instructions: instructions.map(str::to_string),
        };
        let opts = StartOptions {
            instructions: req.instructions.clone(),
            review: None,
            trigger: "durable",
        };
        let status = self
            .run_inline(&req, &opts, Overrides::default())
            .await
            .map_err(|e| Self::to_consolidator_error(session_id, e))?;
        if status.state != ProcessingJobState::Completed {
            return Err(Self::to_consolidator_error(
                session_id,
                Self::incomplete(&status),
            ));
        }
        publish::outcomes_from_result(status.result.as_ref())
    }

    /// Drop-in for [`Consolidator::consolidate_session`] while chunking is on.
    ///
    /// # Errors
    /// As the legacy method; a job that does not complete becomes
    /// [`ConsolidatorError::Processing`].
    pub async fn consolidate_session(
        &self,
        session_id: SessionId,
        dry_run: bool,
        caller: ProcessingCaller,
        instructions: Option<&str>,
    ) -> ConsolidatorResult<ConsolidationOutcome> {
        if dry_run || !self.enabled() {
            // A dry run never calls the model; the legacy plan is the answer.
            let legacy = self
                .legacy()
                .map_err(|e| Self::to_consolidator_error(session_id, e))?;
            if !dry_run {
                let (ws, proj) = legacy.resolve_target(session_id).await?;
                self.reject_paused_for(
                    ws,
                    proj,
                    session_id,
                    ProcessingJobKind::Consolidate,
                    caller.owner,
                )
                .await
                .map_err(|e| Self::to_consolidator_error(session_id, e))?;
            }
            return legacy
                .consolidate_session(
                    session_id,
                    dry_run,
                    caller.actor,
                    caller.author_id,
                    instructions,
                )
                .await;
        }
        self.consolidate_inner(
            ProcessingJobKind::Consolidate,
            session_id,
            caller,
            instructions,
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ConsolidatorError::Processing("job produced no page".into()))
    }

    /// Drop-in for [`Consolidator::consolidate_session_multi`] while chunking
    /// is on.
    ///
    /// # Errors
    /// As [`Self::consolidate_session`].
    pub async fn consolidate_session_multi(
        &self,
        session_id: SessionId,
        dry_run: bool,
        caller: ProcessingCaller,
        instructions: Option<&str>,
    ) -> ConsolidatorResult<Vec<ConsolidationOutcome>> {
        if dry_run || !self.enabled() {
            let legacy = self
                .legacy()
                .map_err(|e| Self::to_consolidator_error(session_id, e))?;
            if !dry_run {
                let (ws, proj) = legacy.resolve_target(session_id).await?;
                self.reject_paused_for(
                    ws,
                    proj,
                    session_id,
                    ProcessingJobKind::ConsolidateMulti,
                    caller.owner,
                )
                .await
                .map_err(|e| Self::to_consolidator_error(session_id, e))?;
            }
            return legacy
                .consolidate_session_multi(
                    session_id,
                    dry_run,
                    caller.actor,
                    caller.author_id,
                    instructions,
                )
                .await;
        }
        self.consolidate_inner(
            ProcessingJobKind::ConsolidateMulti,
            session_id,
            caller,
            instructions,
        )
        .await
    }

    /// Durable review + staging + approval, rendered as the legacy stage
    /// response; a crash after the stage commit reuses the same proposal ids.
    ///
    /// # Errors
    /// [`ProcessingError`] (a job that does not complete is `Other`/`Budget`).
    pub async fn auto_improve_staged(
        &self,
        req: AutoImproveStageRequest,
    ) -> Result<AutoImproveStaged, ProcessingError> {
        self.auto_improve_staged_inner(req).await
    }

    async fn auto_improve_staged_inner(
        &self,
        req: AutoImproveStageRequest,
    ) -> Result<AutoImproveStaged, ProcessingError> {
        match self
            .inner
            .reader
            .session_project_ids(req.session_id)
            .await?
        {
            Some((w, p)) if w == req.workspace_id && p == req.project_id => {}
            Some(_) | None => return Err(ProcessingError::SessionNotFound),
        }
        let start = ProcessingJobStart {
            workspace_id: req.workspace_id,
            project_id: req.project_id,
            session_id: req.session_id,
            kind: ProcessingJobKind::AutoImprove,
            actor: req.actor,
            author_id: req.author_id,
            owner: req.owner,
            viewer: req.viewer,
            instructions: None,
        };
        let opts = StartOptions {
            instructions: None,
            review: Some(req.cfg),
            trigger: "durable",
        };
        let status = self
            .run_inline(
                &start,
                &opts,
                Overrides {
                    admission_ctx: req.admission_ctx,
                },
            )
            .await?;
        if status.state != ProcessingJobState::Completed {
            return Err(Self::incomplete(&status));
        }
        publish::staged_from_result(status.result.as_ref()).map_err(ProcessingError::Other)
    }
}

#[cfg(test)]
mod tests;
