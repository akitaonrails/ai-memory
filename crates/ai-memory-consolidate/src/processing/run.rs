//! The job driver: plan, map, reduce, final call. Publication is in
//! `publish`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::{
    ActorContext, IdentityKey, ProcessingJobId, ProcessingJobKind, ProcessingJobState,
    ProcessingJobStatus, ProjectId, SessionId, UserId, WorkspaceId,
};
use ai_memory_llm::{
    ChatMessage, ChatRequest, GenerationAttemptGuard, LlmError, LlmOperationId, LlmProvider,
    LlmResult, LlmWorkload, Role, capture_call_timing, complete_structured_with_operation_id,
    with_generation_attempt_guard,
};
use ai_memory_store::{
    CallReservation, ClaimedProcessingJob, ProcessingCheckpoint, ProcessingClaim,
    ProcessingNodeState, ProcessingNodeWrite, ProcessingOutcome, ProcessingOwnerFilter,
    ProcessingPublication, ProjectAccess, ReaderPool, ResolvedScope, ScopeResolutionError,
    StoreError, WriterHandle, authorize_scope_for,
};
use ai_memory_wiki::Wiki;

use super::budget::{ENVELOPE_COST, StageBudgets, final_prompt, fingerprint};
use super::chunk_digest::{
    ChunkDigest, MAP_SYSTEM_PROMPT, QuoteCheck, REDUCE_SYSTEM_PROMPT, RefScope,
    collect_refs_and_quotes, digest_cost, schema_cost, scrub_value, validate_digest,
};
use super::chunk_pipeline::{Action, NodeRecord, Output, PipelineError, next_action, progress};
use super::chunk_plan::{
    ChunkPlanner, CoverageLedger, PlannedChunk, SourceView, Span, map_key, render_chunk,
    split_spans, text_cost,
};
use super::final_stage::{
    ConsolidateContext, EvalProgress, FinalRecord, consolidate_request, eval_progress,
    gather_consolidate_context, gather_review_context, prepare_review, render_groups, report_key,
    review_request, started_key,
};
use super::publish::Plan;
use super::{JobOptions, LEASE, Overrides, ProcessingError, ProcessingService};
use crate::auto_improve::{
    AutoImproveLlmResponse, AutoImproveReviewConfig, preflight_rejection,
    session_duration_from_span,
};
use crate::consolidator::ConsolidatorError;
use crate::types::{ConsolidatedBatch, ConsolidatedPage};

const SOURCE_PAGE: usize = 256;
const SOURCE_MAX_BYTES: usize = 4 * 1024 * 1024;
const NODE_PAGE: usize = 500;
/// Level recorded for final-stage nodes.
const FINAL_LEVEL: u32 = 9_999;
const CALL_ATTEMPTS: u32 = 3;
const CALL_RETRY_DELAY: Duration = Duration::from_secs(2);
const RETRY_AFTER_SECS: i64 = 60;
const MAX_FINAL_SHRINKS: u32 = 3;
const MAX_ERROR_RETRIES: u32 = 5;
const MAX_PLAN_BYTES: usize = 240_000;

/// Why a run stopped early.
pub(super) enum Abort {
    /// The claim was lost: another worker owns the job now.
    Stale,
    /// The job already ended (for example the call budget was exhausted).
    Done(Box<ProcessingJobStatus>),
    /// End the job with this outcome.
    Finish(ProcessingOutcome),
    /// An unexpected error; the job is released for a bounded retry.
    Error(ProcessingError),
}

pub(super) fn store_abort(e: StoreError) -> Abort {
    if e.is_stale_processing_claim() {
        Abort::Stale
    } else {
        Abort::Error(ProcessingError::Store(e))
    }
}

fn source_abort(e: StoreError) -> Abort {
    match e {
        e if e.is_stale_processing_claim() => Abort::Stale,
        StoreError::InvalidState(_) | StoreError::NotFound(_) => {
            Abort::Finish(ProcessingOutcome::Invalidated {
                reason: "the session snapshot changed or was purged".into(),
            })
        }
        other => Abort::Error(ProcessingError::Store(other)),
    }
}

pub(super) fn failed(reason: impl Into<String>) -> Abort {
    Abort::Finish(ProcessingOutcome::Failed {
        error: reason.into(),
    })
}

fn consolidator_abort(e: ConsolidatorError) -> Abort {
    Abort::Error(ProcessingError::Consolidator(e))
}

/// Why one provider call produced no usable answer.
enum CallFailure {
    Budget,
    Stale,
    /// Truncated, context-exceeded or invalid output: subdivide, never repeat
    /// the same payload and never treat as empty.
    Recoverable,
    Transient(LlmError),
    Fatal(LlmError),
    Store(StoreError),
    /// The job viewer lost access; carries the ending abort.
    Denied(Abort),
}

/// Spends every generation send of one provider invocation beyond the first.
///
/// `call_llm` durably reserves one attempt before it invokes the provider; a
/// strict→tolerant re-send or a fallback candidate inside that invocation is a
/// further real send, so the provider layer asks here before each one. A
/// refusal reaches the provider as local back-pressure (`CapacityBusy`, which
/// neither marks a provider unhealthy nor advances a fallback chain); the
/// true cause is kept in `stop` for `call_llm` to return.
struct CallGuard {
    reader: ReaderPool,
    writer: WriterHandle,
    claim: ProcessingClaim,
    ws: WorkspaceId,
    proj: ProjectId,
    viewer: Option<UserId>,
    stop: std::sync::Mutex<Option<CallFailure>>,
}

impl CallGuard {
    async fn admit(&self, prepaid: bool) -> Result<(), CallFailure> {
        authorize_viewer(
            &self.reader,
            self.viewer,
            self.ws,
            self.proj,
            ProjectAccess::Write,
        )
        .await
        .map_err(CallFailure::Denied)?;
        if prepaid {
            // Revalidate ownership after waiting for admission, without
            // spending the reservation a second time.
            return self
                .writer
                .confirm_processing_fence()
                .await
                .map_err(claim_failure);
        }
        match self.writer.reserve_processing_call(&self.claim).await {
            Ok(CallReservation::Reserved { .. }) => Ok(()),
            Ok(CallReservation::BudgetExhausted { .. }) => Err(CallFailure::Budget),
            Err(e) => Err(claim_failure(e)),
        }
    }

    fn take_stop(&self) -> Option<CallFailure> {
        self.stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

#[async_trait::async_trait]
impl GenerationAttemptGuard for CallGuard {
    async fn before_send(&self, prepaid: bool) -> LlmResult<()> {
        let Err(failure) = self.admit(prepaid).await else {
            return Ok(());
        };
        self.stop
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(failure);
        Err(LlmError::CapacityBusy("chunking call budget"))
    }
}

fn claim_failure(e: StoreError) -> CallFailure {
    if e.is_stale_processing_claim() {
        CallFailure::Stale
    } else {
        CallFailure::Store(e)
    }
}

/// Re-check the persisted viewer against the live user and project grants.
/// A job without a viewer is server-owned (root or an install without
/// database users) and passes; a revoked viewer invalidates the job.
async fn authorize_viewer(
    reader: &ReaderPool,
    viewer: Option<UserId>,
    ws: WorkspaceId,
    proj: ProjectId,
    need: ProjectAccess,
) -> Result<(), Abort> {
    let Some(user) = viewer else {
        return Ok(());
    };
    let revoked = || {
        Abort::Finish(ProcessingOutcome::Invalidated {
            reason: "the access of the user who started the job was revoked".into(),
        })
    };
    match reader.find_user_by_id(user).await {
        Ok(Some(found)) if found.disabled_at.is_none() => {}
        Ok(_) => return Err(revoked()),
        Err(e) => return Err(Abort::Error(ProcessingError::Store(e))),
    }
    let scope = ResolvedScope {
        workspace_id: ws,
        project_id: proj,
    };
    match authorize_scope_for(reader, None, scope, Some(user), need).await {
        Ok(_) => Ok(()),
        Err(ScopeResolutionError::Forbidden(_)) => Err(revoked()),
        Err(other) => Err(Abort::Error(ProcessingError::Other(format!(
            "authorization check failed: {other:?}"
        )))),
    }
}

fn context_overflow(body: &str) -> bool {
    let b = body.to_lowercase();
    [
        "context",
        "too long",
        "too many tokens",
        "maximum",
        "exceed",
    ]
    .iter()
    .any(|k| b.contains(k))
}

fn recoverable(e: &LlmError) -> bool {
    match e {
        LlmError::TruncatedResponse { .. }
        | LlmError::Serde(_)
        | LlmError::UnexpectedShape(_)
        | LlmError::EmptyContent { .. } => true,
        LlmError::Provider { status, body, .. } => {
            matches!(*status, 400 | 413 | 422) && context_overflow(body)
        }
        _ => false,
    }
}

fn redacted_llm(e: &LlmError) -> String {
    format!(
        "provider call failed: class={} status={}",
        e.class(),
        e.http_status()
            .map_or_else(|| "none".to_string(), |s| s.to_string())
    )
}

fn later(secs: i64) -> Option<jiff::Timestamp> {
    jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + secs).ok()
}

async fn keep_lease(writer: WriterHandle, claim: ProcessingClaim) {
    loop {
        tokio::time::sleep(LEASE / 3).await;
        if writer.renew_processing_job(&claim, LEASE).await.is_err() {
            return;
        }
    }
}

/// What the final step produced.
pub(super) enum FinalStep {
    Plan {
        plan: Box<Plan>,
        identity: String,
    },
    /// The final call was recovered by asking for a smaller final input.
    Shrunk,
}

/// One claimed job in flight.
pub(super) struct Run {
    pub(super) svc: ProcessingService,
    pub(super) claim: ProcessingClaim,
    pub(super) writer: WriterHandle,
    pub(super) wiki: Wiki,
    pub(super) llm: Arc<dyn LlmProvider>,
    pub(super) ws: WorkspaceId,
    pub(super) proj: ProjectId,
    pub(super) session_id: SessionId,
    pub(super) job_id: ProcessingJobId,
    pub(super) kind: ProcessingJobKind,
    pub(super) ctx_fp: String,
    pub(super) opts: JobOptions,
    pub(super) review: Option<AutoImproveReviewConfig>,
    pub(super) budgets: StageBudgets,
    pub(super) overrides: Overrides,
    pub(super) actor: ActorContext,
    pub(super) author_id: Option<UserId>,
    /// Persisted viewer whose access is re-checked before every effect.
    pub(super) viewer: Option<UserId>,
    pub(super) owner: Option<IdentityKey>,
    pub(super) status: ProcessingJobStatus,
    publication: Option<ProcessingPublication>,
    manifest_count: u32,
    map_schema: usize,
    final_schema: usize,
    ord_lo: u32,
    ord_hi: u32,
    chunks: Vec<PlannedChunk>,
    expected: BTreeMap<u32, u64>,
    duration_secs: u64,
    nodes: HashMap<String, NodeRecord>,
    finals: HashMap<String, FinalRecord>,
    shrinks: u32,
}

impl ProcessingService {
    /// Drive one claimed job until it ends, pauses or loses its claim.
    pub(super) async fn run_claimed(
        &self,
        claimed: ClaimedProcessingJob,
        overrides: Overrides,
    ) -> Result<ProcessingJobStatus, ProcessingError> {
        let claim = claimed.claim.clone();
        let writer = self.inner.writer.fenced(&claim);
        let (ws, proj) = (claimed.status.workspace_id, claimed.status.project_id);
        let claim_count = claimed.claim_count;
        let outcome = match Run::new(self, claimed, writer.clone(), overrides) {
            Ok(mut run) => {
                tokio::select! {
                    r = run.drive() => r,
                    () = keep_lease(writer.clone(), claim.clone()) => Err(Abort::Stale),
                }
            }
            Err(abort) => Err(abort),
        };
        match outcome {
            Ok(status) => Ok(status),
            Err(abort) => {
                self.settle(abort, &writer, &claim, ws, proj, claim_count)
                    .await
            }
        }
    }

    async fn current_status(
        &self,
        ws: WorkspaceId,
        proj: ProjectId,
        job_id: ProcessingJobId,
    ) -> Result<ProcessingJobStatus, ProcessingError> {
        self.inner
            .reader
            .processing_job_status(ws, proj, job_id, ProcessingOwnerFilter::Any)
            .await?
            .ok_or_else(|| ProcessingError::Other("processing job disappeared".into()))
    }

    async fn settle(
        &self,
        abort: Abort,
        writer: &WriterHandle,
        claim: &ProcessingClaim,
        ws: WorkspaceId,
        proj: ProjectId,
        claim_count: u32,
    ) -> Result<ProcessingJobStatus, ProcessingError> {
        match abort {
            Abort::Done(status) => Ok(*status),
            Abort::Stale => self.current_status(ws, proj, claim.job_id).await,
            Abort::Finish(outcome) => match writer.finish_processing_job(claim, outcome).await {
                Ok(status) => Ok(status),
                Err(e) if e.is_stale_processing_claim() => {
                    self.current_status(ws, proj, claim.job_id).await
                }
                Err(e) => Err(e.into()),
            },
            Abort::Error(error) => {
                // Release for a bounded number of retries; a job that keeps
                // erroring without making progress ends instead of looping.
                let outcome = if claim_count >= MAX_ERROR_RETRIES {
                    ProcessingOutcome::Failed {
                        error: error.redacted_summary(),
                    }
                } else {
                    ProcessingOutcome::Retry {
                        error: error.redacted_summary(),
                        retry_at: later(30),
                    }
                };
                let _ = writer.finish_processing_job(claim, outcome).await;
                Err(error)
            }
        }
    }
}

impl Run {
    pub(super) fn new(
        svc: &ProcessingService,
        claimed: ClaimedProcessingJob,
        writer: WriterHandle,
        overrides: Overrides,
    ) -> Result<Self, Abort> {
        let inner = &svc.inner;
        let llm = inner
            .llm
            .clone()
            .ok_or(Abort::Error(ProcessingError::NoProvider))?;
        let kind = claimed.status.kind;
        let opts: JobOptions = serde_json::from_value(claimed.options_json.clone())
            .map_err(|_| failed("the job options are unreadable"))?;
        let review = (kind == ProcessingJobKind::AutoImprove).then(|| {
            opts.review
                .clone()
                .unwrap_or_else(|| inner.auto_improve.clone())
        });
        // A job whose publication already began keeps its effects: it resumes
        // the tail whatever changed since. Everything earlier is bound to the
        // model, prompts, schemas and policy it was created under.
        if claimed.publication.is_none() {
            let fp = svc
                .context_fingerprint(kind, review.as_ref())
                .map_err(Abort::Error)?;
            if fp != claimed.context_fingerprint {
                return Err(Abort::Finish(ProcessingOutcome::Invalidated {
                    reason: "model, prompts, schemas or policy changed since the job was created"
                        .into(),
                }));
            }
        }
        let budgets = svc.budgets(kind, review.as_ref()).map_err(failed)?;
        let (_, final_schema) = final_prompt(kind);
        // Every page and staging effect goes through the claim-fenced writer.
        let wiki = inner.wiki.clone().with_writer(writer.clone());
        Ok(Self {
            svc: svc.clone(),
            claim: claimed.claim,
            writer,
            wiki,
            llm,
            ws: claimed.status.workspace_id,
            proj: claimed.status.project_id,
            session_id: claimed.status.session_id,
            job_id: claimed.status.job_id,
            kind,
            ctx_fp: claimed.context_fingerprint,
            opts,
            review,
            budgets,
            overrides,
            actor: claimed.actor,
            author_id: claimed.author_id,
            viewer: claimed.viewer,
            owner: claimed.owner,
            status: claimed.status,
            publication: claimed.publication,
            manifest_count: claimed.manifest_count,
            map_schema: schema_cost::<ChunkDigest>(),
            final_schema,
            ord_lo: 0,
            ord_hi: 0,
            chunks: Vec::new(),
            expected: BTreeMap::new(),
            duration_secs: 0,
            nodes: HashMap::new(),
            finals: HashMap::new(),
            shrinks: 0,
        })
    }

    fn workload(&self) -> LlmWorkload {
        if self.kind == ProcessingJobKind::AutoImprove {
            LlmWorkload::AutoImprove
        } else {
            LlmWorkload::Consolidation
        }
    }

    fn focus(&self) -> &'static str {
        if self.kind == ProcessingJobKind::AutoImprove {
            "Focus: durable, reusable lessons only (gotchas with their cause, decisions with \
             rationale, rules, procedures, corrections); skip one-off task narration."
        } else {
            "Focus: what happened in this coding session, what was decided and what was learned."
        }
    }

    pub(super) async fn drive(&mut self) -> Result<ProcessingJobStatus, Abort> {
        self.authorize(ProjectAccess::Write).await?;
        if let Some(publication) = self.publication.take() {
            return self.publish_resume(publication).await;
        }
        self.prepare().await?;
        if let Some(status) = self.preflight_skip().await? {
            return Ok(status);
        }
        let step = loop {
            let final_cap = self.budgets.final_cap_after(self.shrinks);
            match next_action(
                &self.chunks,
                &self.nodes,
                self.budgets.reduce_cap,
                final_cap,
            ) {
                Err(PipelineError::Stalled(m)) => {
                    return Err(failed(format!(
                        "reduction cannot shrink further ({m}); completed intermediate results \
                         were kept but nothing was published — raise chunking.max_input_tokens \
                         or lower the session size"
                    )));
                }
                Err(PipelineError::Corrupt(m)) => {
                    return Err(Abort::Finish(ProcessingOutcome::Invalidated {
                        reason: format!("inconsistent checkpoints: {m}"),
                    }));
                }
                Ok(Action::Map { key, spans }) => self.map_step(key, spans).await?,
                Ok(Action::Reduce {
                    key,
                    level,
                    children,
                }) => self.reduce_step(key, level, children).await?,
                Ok(Action::Final { outputs, leaves }) => {
                    match self.final_step(outputs, leaves).await? {
                        FinalStep::Shrunk => {}
                        step @ FinalStep::Plan { .. } => break step,
                    }
                }
            }
        };
        let FinalStep::Plan { plan, identity } = step else {
            return Err(failed("final step produced no plan"));
        };
        self.begin_and_publish(*plan, identity).await
    }

    /// Re-check the persisted viewer against the live user and project grants.
    /// A job without a viewer is server-owned (root or an install without
    /// database users) and passes; a revoked viewer invalidates the job.
    pub(super) async fn authorize(&self, need: ProjectAccess) -> Result<(), Abort> {
        authorize_viewer(
            &self.svc.inner.reader,
            self.viewer,
            self.ws,
            self.proj,
            need,
        )
        .await
    }

    async fn begin_and_publish(
        &mut self,
        plan: Plan,
        identity: String,
    ) -> Result<ProcessingJobStatus, Abort> {
        if !matches!(plan, Plan::Report { .. }) {
            self.authorize(ProjectAccess::Write).await?;
        }
        let plan_json = serde_json::to_value(&plan)
            .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?;
        if plan_json.to_string().len() > MAX_PLAN_BYTES {
            return Err(failed("the publication plan exceeds its size bound"));
        }
        // Identity and plan are stored before the first effect; a resumed job
        // returns the stored pair and that is what is published.
        let publication = self
            .writer
            .begin_processing_publication(&self.claim, identity, plan_json)
            .await
            .map_err(store_abort)?;
        self.publish_resume(publication).await
    }

    // ---- planning ------------------------------------------------------

    async fn prepare(&mut self) -> Result<(), Abort> {
        let mut planner = ChunkPlanner::new(self.budgets.map_evidence_cap).map_err(failed)?;
        let mut after: Option<u32> = None;
        let (mut first, mut last): (Option<i64>, Option<i64>) = (None, None);
        loop {
            let page = self
                .svc
                .inner
                .reader
                .read_processing_sources(
                    self.ws,
                    self.proj,
                    self.job_id,
                    after,
                    SOURCE_PAGE,
                    SOURCE_MAX_BYTES,
                )
                .await
                .map_err(source_abort)?;
            if page.is_empty() {
                break;
            }
            for source in page {
                after = Some(source.ordinal);
                let micros = source.created_at.as_microsecond();
                first.get_or_insert(micros);
                last = Some(micros);
                self.expected
                    .insert(source.ordinal, source.body.len() as u64);
                planner.push(&source_view(source));
            }
        }
        if self.expected.len() != self.manifest_count as usize || self.expected.is_empty() {
            return Err(Abort::Finish(ProcessingOutcome::Invalidated {
                reason: "the snapshot no longer matches its manifest".into(),
            }));
        }
        self.ord_lo = self.expected.keys().next().copied().unwrap_or(0);
        self.ord_hi = self.expected.keys().next_back().copied().unwrap_or(0);
        self.chunks = planner.finish();
        self.duration_secs = session_duration_from_span(first, last);
        self.load_nodes().await?;
        // A ceiling that cannot cover the remaining map calls plus the final
        // call is reported now, before any call is spent on partial coverage.
        let remaining_maps = self
            .chunks
            .iter()
            .filter(|c| !self.nodes.contains_key(&map_key(&c.spans)))
            .count() as u64;
        let calls_left = u64::from(self.status.max_calls.saturating_sub(self.status.calls_used));
        if remaining_maps + 1 > calls_left {
            return Err(Abort::Finish(ProcessingOutcome::BudgetExceeded {
                reason: format!(
                    "the session needs at least {} more provider calls but only {calls_left} \
                     remain of chunking.max_calls_per_run; nothing was published",
                    remaining_maps + 1
                ),
            }));
        }
        let (total, done) = progress(&self.chunks, &self.nodes);
        self.writer
            .checkpoint_processing_job(
                &self.claim,
                ProcessingCheckpoint {
                    state: Some(ProcessingJobState::Running),
                    chunks_total: Some(total),
                    chunks_done: Some(done),
                    ..ProcessingCheckpoint::default()
                },
            )
            .await
            .map_err(store_abort)?;
        Ok(())
    }

    async fn load_nodes(&mut self) -> Result<(), Abort> {
        let mut after: Option<String> = None;
        loop {
            let page = self
                .svc
                .inner
                .reader
                .processing_nodes(self.ws, self.proj, self.job_id, after.clone(), NODE_PAGE)
                .await
                .map_err(store_abort)?;
            let Some(last) = page.last() else { break };
            after = Some(last.key.clone());
            for node in page {
                if node.key.starts_with("f:") {
                    if let Ok(rec) = serde_json::from_value::<FinalRecord>(node.json) {
                        self.finals.insert(node.key, rec);
                    }
                } else if node.key.starts_with("sf:") {
                    self.shrinks += 1;
                } else if node.input_fingerprint == self.ctx_fp
                    && let Ok(rec) = serde_json::from_value::<NodeRecord>(node.json)
                {
                    self.nodes.insert(node.key, rec);
                }
            }
        }
        Ok(())
    }

    /// Auto-improve sessions the preflight filters skip end without a call.
    async fn preflight_skip(&mut self) -> Result<Option<ProcessingJobStatus>, Abort> {
        let Some(cfg) = self.review.clone() else {
            return Ok(None);
        };
        let total = self.expected.len();
        let Some(rejection) = preflight_rejection(total, self.duration_secs, &cfg) else {
            return Ok(None);
        };
        let report = super::final_stage::skipped_report(
            self.session_id,
            &cfg,
            total,
            self.duration_secs,
            rejection,
        );
        let plan = self.plan_for_report(report, &cfg).await?;
        let identity = format!("{}:preflight", self.job_id);
        self.begin_and_publish(plan, identity).await.map(Some)
    }

    // ---- calls ---------------------------------------------------------

    async fn call_llm<T>(&self, request: ChatRequest) -> Result<T, CallFailure>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + Send + 'static,
    {
        let operation_id = LlmOperationId::for_workload(self.workload());
        let guard = Arc::new(CallGuard {
            reader: self.svc.inner.reader.clone(),
            writer: self.writer.clone(),
            claim: self.claim.clone(),
            ws: self.ws,
            proj: self.proj,
            viewer: self.viewer,
            stop: std::sync::Mutex::new(None),
        });
        let mut attempt = 0u32;
        loop {
            // The first send of every invocation is durably spent before
            // anything is sent, retries and uncertain results included; the
            // guard spends each further send the provider layer makes inside
            // the invocation (strict→tolerant re-send, fallback candidates).
            if let Err(abort) = self.authorize(ProjectAccess::Write).await {
                return Err(CallFailure::Denied(abort));
            }
            match self.writer.reserve_processing_call(&self.claim).await {
                Ok(CallReservation::Reserved { .. }) => {}
                Ok(CallReservation::BudgetExhausted { .. }) => return Err(CallFailure::Budget),
                Err(e) if e.is_stale_processing_claim() => return Err(CallFailure::Stale),
                Err(e) => return Err(CallFailure::Store(e)),
            }
            attempt += 1;
            let (outcome, timing) = capture_call_timing(with_generation_attempt_guard(
                guard.clone(),
                complete_structured_with_operation_id::<T>(
                    &*self.llm,
                    request.clone(),
                    operation_id,
                ),
            ))
            .await;
            // Measured time is durable under the claim before any result or
            // error checkpoint, and never touches the reserved attempt. An
            // exhausted budget has already cleared the claim, so this may see
            // it as stale; the guard's stop below names the real cause.
            let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
            let timed = self
                .writer
                .record_processing_call_timing(&self.claim, ms(timing.wait), ms(timing.exec))
                .await;
            if let Some(stop) = guard.take_stop() {
                return Err(stop);
            }
            match timed {
                Ok(()) => {}
                Err(e) if e.is_stale_processing_claim() => return Err(CallFailure::Stale),
                Err(e) => return Err(CallFailure::Store(e)),
            }
            match outcome {
                Ok(value) => return Ok(value),
                Err(e) if recoverable(&e) => return Err(CallFailure::Recoverable),
                Err(e) if matches!(e, LlmError::CapacityBusy(_)) => {
                    return Err(CallFailure::Transient(e));
                }
                Err(e) if e.is_transient() && attempt < CALL_ATTEMPTS => {
                    tokio::time::sleep(CALL_RETRY_DELAY).await;
                }
                Err(e) if e.is_transient() => return Err(CallFailure::Transient(e)),
                Err(e) => return Err(CallFailure::Fatal(e)),
            }
        }
    }

    async fn abort_for(&self, failure: CallFailure) -> Abort {
        match failure {
            CallFailure::Budget => {
                match self
                    .svc
                    .current_status(self.ws, self.proj, self.job_id)
                    .await
                {
                    Ok(status) => Abort::Done(Box::new(status)),
                    Err(e) => Abort::Error(e),
                }
            }
            CallFailure::Stale => Abort::Stale,
            CallFailure::Recoverable => failed("a provider answer could not be used"),
            CallFailure::Transient(e) => Abort::Finish(ProcessingOutcome::Retry {
                error: redacted_llm(&e),
                retry_at: later(RETRY_AFTER_SECS),
            }),
            CallFailure::Fatal(e) => failed(redacted_llm(&e)),
            CallFailure::Store(e) => store_abort(e),
            CallFailure::Denied(abort) => abort,
        }
    }

    fn request(&self, system: &str, user: String, max_tokens: u32) -> ChatRequest {
        ChatRequest {
            system: Some(system.to_string()),
            messages: vec![ChatMessage {
                role: Role::User,
                content: user,
            }],
            max_tokens,
            temperature: Some(0.1),
        }
    }

    /// A call whose prompt would not fit the budget is a defect, not a
    /// provider problem: refuse to send it.
    fn ensure_fits(&self, request: &ChatRequest, schema: usize) -> Result<(), Abort> {
        let user: usize = request.messages.iter().map(|m| text_cost(&m.content)).sum();
        let system = request.system.as_deref().map_or(0, text_cost);
        if system + user + schema + ENVELOPE_COST > self.budgets.call_cost {
            return Err(failed("an assembled prompt exceeds the chunk input budget"));
        }
        Ok(())
    }

    // ---- persistence ---------------------------------------------------

    async fn commit_node(
        &mut self,
        level: u32,
        key: &str,
        record: NodeRecord,
    ) -> Result<(), Abort> {
        let state = match record {
            NodeRecord::Map { .. } | NodeRecord::Reduce { .. } => ProcessingNodeState::Completed,
            NodeRecord::SplitMap { .. } | NodeRecord::SplitReduce { .. } => {
                ProcessingNodeState::Recoverable
            }
        };
        let json = serde_json::to_value(&record)
            .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?;
        self.nodes.insert(key.to_string(), record);
        let (total, done) = progress(&self.chunks, &self.nodes);
        self.writer
            .checkpoint_processing_job(
                &self.claim,
                ProcessingCheckpoint {
                    state: Some(if level == 0 {
                        ProcessingJobState::Running
                    } else {
                        ProcessingJobState::Reducing
                    }),
                    chunks_total: Some(total),
                    chunks_done: Some(done),
                    upsert_nodes: vec![ProcessingNodeWrite {
                        key: key.to_string(),
                        level,
                        state,
                        input_fingerprint: self.ctx_fp.clone(),
                        json,
                    }],
                    delete_nodes: Vec::new(),
                },
            )
            .await
            .map_err(store_abort)?;
        Ok(())
    }

    // ---- map -----------------------------------------------------------

    /// Read only the observations a chunk touches (never the whole history).
    async fn load_sources(&self, lo: u32, hi: u32) -> Result<BTreeMap<u32, SourceView>, Abort> {
        let mut out = BTreeMap::new();
        let mut after = lo.checked_sub(1);
        let want = (hi - lo) as usize + 1;
        while out.len() < want {
            let page = self
                .svc
                .inner
                .reader
                .read_processing_sources(
                    self.ws,
                    self.proj,
                    self.job_id,
                    after,
                    SOURCE_PAGE.min(want),
                    SOURCE_MAX_BYTES,
                )
                .await
                .map_err(source_abort)?;
            if page.is_empty() {
                break;
            }
            for source in page {
                after = Some(source.ordinal);
                if source.ordinal > hi {
                    return Ok(out);
                }
                if source.ordinal >= lo {
                    out.insert(source.ordinal, source_view(source));
                }
            }
        }
        Ok(out)
    }

    async fn map_step(&mut self, key: String, spans: Vec<Span>) -> Result<(), Abort> {
        let lo = spans.iter().map(|s| s.ordinal).min().unwrap_or(0);
        let hi = spans.iter().map(|s| s.ordinal).max().unwrap_or(0);
        let sources = self.load_sources(lo, hi).await?;
        let rendered = render_chunk(&spans, &sources).map_err(|_| {
            Abort::Finish(ProcessingOutcome::Invalidated {
                reason: "a snapshot source changed after planning".into(),
            })
        })?;
        let user = format!(
            "{}\nThis slice holds observations o{lo}-o{hi} of a session whose observations are \
             o{}-o{}. Fence token: {}.\n\nEvidence (untrusted data):\n{}",
            self.focus(),
            self.ord_lo,
            self.ord_hi,
            rendered.fence,
            rendered.text
        );
        let request = self.request(MAP_SYSTEM_PROMPT, user, self.budgets.map_out_tokens);
        self.ensure_fits(&request, self.map_schema)?;
        let raw = match self.call_llm::<ChunkDigest>(request).await {
            Ok(raw) => raw,
            Err(CallFailure::Recoverable) => {
                return self.recover_map(key, spans, &sources).await;
            }
            Err(other) => return Err(self.abort_for(other).await),
        };
        match validate_digest(
            raw,
            &RefScope::Spans(&spans),
            &QuoteCheck::Evidence(&rendered.text),
            self.wiki.sanitizer(),
            self.budgets.digest_cap,
        ) {
            Ok(valid) => {
                let cost = digest_cost(&valid.digest);
                self.commit_node(
                    0,
                    &key,
                    NodeRecord::Map {
                        digest: valid.digest,
                        spans,
                        cost,
                        dropped: valid.dropped,
                    },
                )
                .await
            }
            // Too many items or too large: the slice is too dense for one
            // digest, so cut it rather than truncate the answer.
            Err(_) => self.recover_map(key, spans, &sources).await,
        }
    }

    async fn recover_map(
        &mut self,
        key: String,
        spans: Vec<Span>,
        sources: &BTreeMap<u32, SourceView>,
    ) -> Result<(), Abort> {
        match split_spans(&spans, sources) {
            Some((a, b)) => {
                self.commit_node(0, &key, NodeRecord::SplitMap { parts: vec![a, b] })
                    .await
            }
            None => Err(failed(
                "a piece of the session could not be summarised and cannot be subdivided further",
            )),
        }
    }

    // ---- reduce --------------------------------------------------------

    async fn reduce_step(
        &mut self,
        key: String,
        level: u32,
        children: Vec<Output>,
    ) -> Result<(), Abort> {
        let lo = children.iter().map(|c| c.lo).min().unwrap_or(0);
        let hi = children.iter().map(|c| c.hi).max().unwrap_or(0);
        let mut refs = BTreeSet::new();
        let mut quotes = BTreeSet::new();
        for child in &children {
            collect_refs_and_quotes(&child.digest, &mut refs, &mut quotes);
        }
        let user = format!(
            "{}\nMerge these {} digests covering observations o{lo}-o{hi}.\n\n{}",
            self.focus(),
            children.len(),
            render_groups(&children)
        );
        let request = self.request(REDUCE_SYSTEM_PROMPT, user, self.budgets.reduce_out_tokens);
        self.ensure_fits(&request, self.map_schema)?;
        let raw = match self.call_llm::<ChunkDigest>(request).await {
            Ok(raw) => raw,
            Err(CallFailure::Recoverable) => {
                return self.recover_reduce(key, level, &children).await;
            }
            Err(other) => return Err(self.abort_for(other).await),
        };
        match validate_digest(
            raw,
            &RefScope::Known(&refs),
            &QuoteCheck::Known(&quotes),
            self.wiki.sanitizer(),
            self.budgets.digest_cap,
        ) {
            Ok(valid) => {
                let cost = digest_cost(&valid.digest);
                self.commit_node(
                    level,
                    &key,
                    NodeRecord::Reduce {
                        digest: valid.digest,
                        children: children.iter().map(|c| c.key.clone()).collect(),
                        lo,
                        hi,
                        cost,
                        dropped: valid.dropped,
                    },
                )
                .await
            }
            Err(_) => self.recover_reduce(key, level, &children).await,
        }
    }

    async fn recover_reduce(
        &mut self,
        key: String,
        level: u32,
        children: &[Output],
    ) -> Result<(), Abort> {
        let mid = children.len() / 2;
        if mid == 0 {
            return Err(failed("a digest group could not be merged"));
        }
        let keys: Vec<String> = children.iter().map(|c| c.key.clone()).collect();
        self.commit_node(
            level,
            &key,
            NodeRecord::SplitReduce {
                parts: vec![keys[..mid].to_vec(), keys[mid..].to_vec()],
            },
        )
        .await
    }

    // ---- final ---------------------------------------------------------

    fn check_coverage(&self, leaves: &[Span]) -> Result<(), Abort> {
        let mut ledger = CoverageLedger::new(self.expected.clone());
        for span in leaves {
            ledger
                .record(span)
                .map_err(|_| failed("a completed node covers bytes outside the snapshot"))?;
        }
        if !ledger.is_complete() {
            return Err(failed(format!(
                "coverage is incomplete for {} observation(s); nothing was published",
                ledger.incomplete()
            )));
        }
        Ok(())
    }

    async fn final_step(
        &mut self,
        outputs: Vec<Output>,
        leaves: Vec<Span>,
    ) -> Result<FinalStep, Abort> {
        self.check_coverage(&leaves)?;
        if outputs.is_empty() {
            return Err(failed("no digest was produced for the session"));
        }
        if self.kind == ProcessingJobKind::AutoImprove {
            self.final_review(outputs).await
        } else {
            self.final_consolidate(outputs).await
        }
    }

    fn final_key(&self, context_hash: &str, outputs: &[Output]) -> String {
        let children = outputs
            .iter()
            .map(|o| o.key.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let shrinks = self.shrinks.to_string();
        let fp = fingerprint(&[
            self.ctx_fp.as_str(),
            context_hash,
            children.as_str(),
            shrinks.as_str(),
        ]);
        format!("f:{}", &fp[..24])
    }

    async fn store_final(&mut self, key: &str, record: &FinalRecord) -> Result<(), Abort> {
        let json = serde_json::to_value(record)
            .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?;
        self.finals.insert(key.to_string(), record.clone());
        self.writer
            .checkpoint_processing_job(
                &self.claim,
                ProcessingCheckpoint {
                    state: Some(ProcessingJobState::Reducing),
                    upsert_nodes: vec![ProcessingNodeWrite {
                        key: key.to_string(),
                        level: FINAL_LEVEL,
                        state: ProcessingNodeState::Completed,
                        input_fingerprint: self.ctx_fp.clone(),
                        json,
                    }],
                    ..ProcessingCheckpoint::default()
                },
            )
            .await
            .map_err(store_abort)?;
        Ok(())
    }

    /// The final call answered unusably (truncated, over context, invalid):
    /// shrink the room final inputs may use, which forces one more reduction.
    async fn shrink(&mut self) -> Result<FinalStep, Abort> {
        if self.shrinks >= MAX_FINAL_SHRINKS {
            return Err(failed(
                "the final call could not be completed within bounds; intermediate results were \
                 kept but nothing was published",
            ));
        }
        let key = format!("sf:{}", self.shrinks + 1);
        self.shrinks += 1;
        self.writer
            .checkpoint_processing_job(
                &self.claim,
                ProcessingCheckpoint {
                    state: Some(ProcessingJobState::Reducing),
                    upsert_nodes: vec![ProcessingNodeWrite {
                        key,
                        level: FINAL_LEVEL,
                        state: ProcessingNodeState::Recoverable,
                        input_fingerprint: self.ctx_fp.clone(),
                        json: serde_json::json!({}),
                    }],
                    ..ProcessingCheckpoint::default()
                },
            )
            .await
            .map_err(store_abort)?;
        Ok(FinalStep::Shrunk)
    }

    /// One final call; `None` when the answer was unusable and recoverable.
    async fn final_call(&self, request: ChatRequest) -> Result<Option<FinalRecord>, Abort> {
        self.ensure_fits(&request, self.final_schema)?;
        let result = match self.kind {
            ProcessingJobKind::Consolidate => self
                .call_llm::<ConsolidatedPage>(request)
                .await
                .map(|page| FinalRecord::Page { page }),
            ProcessingJobKind::ConsolidateMulti => self
                .call_llm::<ConsolidatedBatch>(request)
                .await
                .map(|batch| FinalRecord::Batch { batch }),
            ProcessingJobKind::AutoImprove => self
                .call_llm::<AutoImproveLlmResponse>(request)
                .await
                .map(|response| FinalRecord::Review { response }),
        };
        let record = match result {
            Ok(record) => record,
            Err(CallFailure::Recoverable) => return Ok(None),
            Err(other) => return Err(self.abort_for(other).await),
        };
        let usable = match &record {
            FinalRecord::Page { page } => {
                !page.title.trim().is_empty() && !page.body_markdown.trim().is_empty()
            }
            FinalRecord::Batch { batch } => !batch.updates.is_empty(),
            FinalRecord::Review { .. }
            | FinalRecord::EvalStarted
            | FinalRecord::Evaluated { .. } => true,
        };
        if !usable {
            return Ok(None);
        }
        // Only sanitized output is ever persisted.
        let mut value = serde_json::to_value(&record)
            .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?;
        scrub_value(&mut value, self.wiki.sanitizer());
        let record = serde_json::from_value(value)
            .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?;
        Ok(Some(record))
    }

    pub(super) async fn consolidate_context(
        &self,
        multi: bool,
    ) -> Result<ConsolidateContext, Abort> {
        let legacy = self.svc.legacy().map_err(Abort::Error)?;
        gather_consolidate_context(
            &legacy,
            &self.wiki,
            &self.svc.inner.reader,
            self.ws,
            self.proj,
            self.session_id,
            multi,
            &self.actor,
            self.opts.instructions.as_deref(),
            self.budgets.final_context_cap,
        )
        .await
        .map_err(consolidator_abort)
    }

    async fn final_consolidate(&mut self, outputs: Vec<Output>) -> Result<FinalStep, Abort> {
        let multi = self.kind == ProcessingJobKind::ConsolidateMulti;
        for _ in 0..3 {
            let ctx = self.consolidate_context(multi).await?;
            let key = self.final_key(&ctx.hash, &outputs);
            let record = match self.finals.get(&key).cloned() {
                Some(record) => record,
                None => {
                    let request = consolidate_request(
                        self.kind,
                        &ctx,
                        self.session_id,
                        &outputs,
                        self.manifest_count,
                        &self.budgets,
                    );
                    let Some(record) = self.final_call(request).await? else {
                        return self.shrink().await;
                    };
                    self.store_final(&key, &record).await?;
                    record
                }
            };
            // The page and its neighbours may have changed while the call ran:
            // recompute the context and only then plan against it.
            let fresh = self.consolidate_context(multi).await?;
            if fresh.hash != ctx.hash {
                self.finals.remove(&key);
                continue;
            }
            let plan = self.plan_pages(record, &fresh).await?;
            return Ok(FinalStep::Plan {
                plan: Box::new(plan),
                identity: format!("{}:{key}", self.job_id),
            });
        }
        Err(failed(
            "the target page context kept changing while the final call ran",
        ))
    }

    async fn final_review(&mut self, outputs: Vec<Output>) -> Result<FinalStep, Abort> {
        let cfg = self
            .review
            .clone()
            .ok_or_else(|| failed("a review job without a review configuration"))?;
        match eval_progress(&self.finals) {
            EvalProgress::Fresh => {}
            EvalProgress::Started => {
                return Err(failed(
                    "the auto-improve eval command was started by an earlier attempt that left \
                     no result, so it may have run; it is not repeated automatically and \
                     nothing was staged — inspect the evaluator before reviewing a new snapshot",
                ));
            }
            EvalProgress::Done { key, report } => {
                let plan = self.plan_for_report(*report, &cfg).await?;
                return Ok(FinalStep::Plan {
                    plan: Box::new(plan),
                    identity: format!("{}:{key}", self.job_id),
                });
            }
        }
        for _ in 0..3 {
            let ctx = gather_review_context(
                &self.svc.inner.reader,
                self.ws,
                self.proj,
                &cfg,
                self.budgets.final_context_cap,
            )
            .await
            .map_err(|e| Abort::Error(ProcessingError::AutoImprove(e)))?;
            let key = self.final_key(&ctx.hash, &outputs);
            let request = review_request(
                &cfg,
                &ctx,
                self.session_id,
                &outputs,
                self.manifest_count,
                self.duration_secs,
                &self.budgets,
            );
            let prompt_text: String = request
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let record = match self.finals.get(&key).cloned() {
                Some(record) => record,
                None => {
                    let Some(record) = self.final_call(request).await? else {
                        return self.shrink().await;
                    };
                    self.store_final(&key, &record).await?;
                    record
                }
            };
            let fresh = gather_review_context(
                &self.svc.inner.reader,
                self.ws,
                self.proj,
                &cfg,
                self.budgets.final_context_cap,
            )
            .await
            .map_err(|e| Abort::Error(ProcessingError::AutoImprove(e)))?;
            if fresh.hash != ctx.hash {
                self.finals.remove(&key);
                continue;
            }
            let FinalRecord::Review { response } = record else {
                return Err(failed("the stored final record has the wrong shape"));
            };
            // Validation and caps run once on the merged proposals of the
            // whole session. The eval command has effects of its own: its
            // start is persisted before it runs and its result right after,
            // so a restart reuses the result and never starts it again.
            let draft = prepare_review(
                &self.svc.inner.reader,
                self.llm.name(),
                self.llm.model(),
                self.ws,
                self.proj,
                self.session_id,
                &cfg,
                &fresh,
                response,
                self.manifest_count as usize,
                self.duration_secs,
                &prompt_text,
            )
            .await
            .map_err(|e| Abort::Error(ProcessingError::AutoImprove(e)))?;
            let external = draft.runs_external_eval();
            if external {
                self.store_final(&started_key(&key), &FinalRecord::EvalStarted)
                    .await?;
            }
            let report = draft.finish(&cfg.eval).await;
            if external {
                self.store_final(&report_key(&key), &FinalRecord::evaluated(&report))
                    .await?;
            }
            let plan = self.plan_for_report(report, &cfg).await?;
            return Ok(FinalStep::Plan {
                plan: Box::new(plan),
                identity: format!("{}:{key}", self.job_id),
            });
        }
        Err(failed(
            "the review context kept changing while the final call ran",
        ))
    }
}

fn source_view(source: ai_memory_store::ProcessingSource) -> SourceView {
    SourceView {
        ordinal: source.ordinal,
        kind: source.kind.as_str().to_string(),
        title: source.title,
        created_at_micros: source.created_at.as_microsecond(),
        body: source.body,
    }
}
