//! Publication of a finished job: pages through the wiki and staged
//! proposals through the store, both on claim-fenced handles and both
//! reconciled against their durable identity before any effect is repeated.
//!
//! The plan (final pages, or staged proposals with their eval results) is
//! stored with the publication identity before the first effect. After a
//! crash between an effect's commit and its checkpoint the resumed run finds
//! the effect by that identity (a frontmatter stamp for pages, the run's
//! `config_json.processing_job` for staging) and adopts it instead of
//! writing again. A fence trigger in the store aborts any write from a worker
//! that lost its claim, even if it still holds a response in memory.

use ai_memory_core::{
    ActorContext, AutoImproveProposalId, AutoImproveRunId, PageEvidence, PageEvidenceKind, PageId,
    PagePath, Tier,
};
use ai_memory_store::{
    ApproveAutoImproveProposalResult, AutoImproveProposalOperation, AutoImproveProposalStatus,
    NewAutoImproveProposal, PagePrecondition, ProcessingOutcome, ProcessingPublication,
    SkippedProposal, StageAutoImproveRun,
};
use ai_memory_wiki::{
    AdmissionContext, AdmissionOp, ConditionalBatchWrite, ConditionalWrite, WikiError,
    WritePageRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::final_stage::{ConsolidateContext, FinalRecord};
use super::run::{Abort, Run, failed, store_abort};
use super::{AutoImproveStaged, AutoImproveStagedProposal, ProcessingError};
use crate::auto_improve::{AutoImproveReport, AutoImproveReviewConfig};
use crate::auto_improve_schedule::AUTO_IMPROVE_AUTO_APPROVE_AGENT;
use crate::consolidator::{ConsolidatorError, ConsolidatorResult, short_id};
use crate::types::ConsolidationOutcome;

/// What a finished job will publish; stored in the publication plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(super) enum Plan {
    Pages(PagesPlan),
    Stage(StagePlan),
    /// Report-only review: nothing is staged, the caller stages as today.
    Report {
        report: Value,
        eval_results: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PlannedPage {
    path: String,
    frontmatter: Value,
    body: String,
    tier: Tier,
    title: Option<String>,
    /// Latest page version the final call was derived from (`None` = absent).
    base: Option<String>,
    out_title: String,
    out_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PagesPlan {
    pages: Vec<PlannedPage>,
    commit_message: String,
    multi: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PlannedProposal {
    operation: String,
    target_path: String,
    kind: String,
    title: String,
    confidence: f64,
    rationale: String,
    evidence_json: Value,
    body_markdown: String,
    edit_mode: Option<String>,
    patch_json: Option<Value>,
    expected_base_body_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StagePlan {
    provider: String,
    model: String,
    summary: String,
    warnings: Vec<String>,
    rejected: Value,
    config: Value,
    proposal_actor: String,
    proposals: Vec<PlannedProposal>,
    rejected_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct OutcomeJson {
    path: String,
    new_title: String,
    new_body_markdown: String,
    tags: Vec<String>,
    page_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PagesResult {
    outcomes: Vec<OutcomeJson>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StageEffect {
    run_id: String,
    proposal_ids: Vec<String>,
    skipped: Vec<SkippedProposal>,
}

fn wiki_abort(e: WikiError) -> Abort {
    match e {
        WikiError::Store(s) if s.is_stale_processing_claim() => Abort::Stale,
        other => Abort::Error(ProcessingError::Wiki(other)),
    }
}

fn hex_to_sha256(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err("expected 64 hex chars".into());
    }
    let mut out = [0_u8; 32];
    for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        out[i] = u8::from_str_radix(s, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

impl StagePlan {
    pub(super) fn stage_input(&self, run: &Run) -> Result<StageAutoImproveRun, Abort> {
        let mut proposals = Vec::with_capacity(self.proposals.len());
        for p in &self.proposals {
            let expected = p
                .expected_base_body_sha256
                .as_deref()
                .map(hex_to_sha256)
                .transpose()
                .map_err(|_| failed("a planned proposal carries an invalid base hash"))?;
            proposals.push(NewAutoImproveProposal {
                operation: if p.operation == "update" {
                    AutoImproveProposalOperation::Update
                } else {
                    AutoImproveProposalOperation::Create
                },
                target_path: PagePath::new(p.target_path.clone())
                    .map_err(|_| failed("a planned proposal has an invalid target path"))?,
                kind: p.kind.clone(),
                title: p.title.clone(),
                confidence: p.confidence,
                rationale: p.rationale.clone(),
                evidence_json: p.evidence_json.clone(),
                body_markdown: p.body_markdown.clone(),
                artifact_sha256: None,
                edit_mode: p.edit_mode.clone(),
                patch_json: p.patch_json.clone(),
                expected_base_body_sha256: expected,
            });
        }
        Ok(StageAutoImproveRun {
            workspace_id: run.ws,
            project_id: run.proj,
            session_id: Some(run.session_id),
            provider: Some(self.provider.clone()),
            model: Some(self.model.clone()),
            summary: Some(self.summary.clone()),
            warnings_json: serde_json::to_value(&self.warnings).unwrap_or_else(|_| json!([])),
            rejected_candidates_json: self.rejected.clone(),
            config_json: self.config.clone(),
            proposal_actor: ActorContext {
                agent: Some(self.proposal_actor.clone()),
                ..ActorContext::default()
            },
            proposals,
        })
    }
}

impl Run {
    // ---- planning ------------------------------------------------------

    pub(super) async fn plan_pages(
        &self,
        record: FinalRecord,
        ctx: &ConsolidateContext,
    ) -> Result<Plan, Abort> {
        let legacy = self.svc.legacy().map_err(Abort::Error)?;
        let session = short_id(&self.session_id.to_string());
        let to_abort = |e: ConsolidatorError| Abort::Error(ProcessingError::Consolidator(e));
        let (requests, labels, commit_message, multi) = match record {
            FinalRecord::Page { mut page } => {
                let req = legacy
                    .plan_single_write(
                        self.ws,
                        self.proj,
                        self.session_id,
                        ctx.agent_kind,
                        &ctx.anchor,
                        &ctx.existing_titles,
                        &mut page,
                        &self.actor,
                        self.author_id,
                    )
                    .await
                    .map_err(to_abort)?;
                let message = format!(
                    "consolidate(session {session}): {}",
                    page.title.chars().take(60).collect::<String>()
                );
                (
                    vec![req],
                    vec![(page.title.clone(), page.tags.clone())],
                    message,
                    false,
                )
            }
            FinalRecord::Batch { batch } => {
                let (requests, outcomes) = legacy
                    .plan_batch_writes(
                        self.ws,
                        self.proj,
                        self.session_id,
                        ctx.agent_kind,
                        &ctx.anchor,
                        &ctx.existing_titles,
                        &batch,
                        &self.actor,
                        self.author_id,
                    )
                    .await
                    .map_err(to_abort)?;
                let message = format!(
                    "consolidate-batch(session {session}): {} page(s) — {}",
                    requests.len(),
                    batch.rationale.chars().take(60).collect::<String>()
                );
                (
                    requests,
                    outcomes
                        .into_iter()
                        .map(|o| (o.new_title, o.tags))
                        .collect(),
                    message,
                    true,
                )
            }
            FinalRecord::Review { .. }
            | FinalRecord::EvalStarted
            | FinalRecord::Evaluated { .. } => {
                return Err(failed("a consolidation job holds a review record"));
            }
        };
        // (bases come from the context, never from a later read)
        let mut pages = Vec::with_capacity(requests.len());
        for (mut req, (out_title, out_tags)) in requests.into_iter().zip(labels) {
            if let Some(map) = req.frontmatter.as_object_mut() {
                map.insert("processing_job".into(), json!(self.job_id.to_string()));
            }
            // Captured with the context the model saw. A path outside that
            // context was never read: it may only be created, never replaced.
            let base = ctx.bases.get(req.path.as_str()).cloned().flatten();
            pages.push(PlannedPage {
                path: req.path.as_str().to_string(),
                frontmatter: req.frontmatter,
                body: req.body,
                tier: req.tier,
                title: req.title,
                base,
                out_title,
                out_tags,
            });
        }
        Ok(Plan::Pages(PagesPlan {
            pages,
            commit_message,
            multi,
        }))
    }

    pub(super) async fn plan_for_report(
        &self,
        report: AutoImproveReport,
        cfg: &AutoImproveReviewConfig,
    ) -> Result<Plan, Abort> {
        let eval_results = serde_json::to_value(report.eval_results())
            .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?;
        if self.opts.report_only {
            let report_json = serde_json::to_value(&report)
                .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?;
            return Ok(Plan::Report {
                report: report_json,
                eval_results,
            });
        }
        let reader = &self.svc.inner.reader;
        let mut proposals = Vec::with_capacity(report.proposals.len());
        for p in &report.proposals {
            let path = PagePath::new(p.path.clone())
                .map_err(|_| failed("a validated proposal has an invalid path"))?;
            let exists = reader
                .page_body_by_ids(self.ws, self.proj, path.as_str())
                .await
                .map_err(store_abort)?
                .is_some();
            let operation = if p.edit_mode == "patch"
                || (exists && path.as_str() == "_slots/current-focus.md")
            {
                "update"
            } else {
                "create"
            };
            proposals.push(PlannedProposal {
                operation: operation.into(),
                target_path: p.path.clone(),
                kind: p.kind.clone(),
                title: p.title.clone(),
                confidence: f64::from(p.confidence),
                rationale: p.rationale.clone(),
                evidence_json: report
                    .proposal_evidence_json(p)
                    .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?,
                body_markdown: p.body_markdown.clone(),
                edit_mode: Some(p.edit_mode.clone()),
                patch_json: serde_json::to_value(&p.edits).ok(),
                expected_base_body_sha256: p.expected_base_body_sha256.clone(),
            });
        }
        let config = json!({
            "trigger": if self.scheduled() { "scheduler" } else { "chunked" },
            "processing_job": self.job_id.to_string(),
            "chunked": true,
            "min_observations": cfg.min_observations,
            "min_session_duration_secs": cfg.min_session_duration_secs,
            "min_confidence": cfg.min_confidence,
            "max_input_tokens": cfg.max_input_tokens,
            "max_proposals_per_run": cfg.max_proposals_per_run,
            "include_raw_fallback": cfg.include_raw_fallback,
            "max_patchable_pages": cfg.max_patchable_pages,
            "max_patchable_body_chars": cfg.max_patchable_body_chars,
            "max_edits_per_proposal": cfg.max_edits_per_proposal,
            "max_edit_content_chars": cfg.max_edit_content_chars,
            "max_changed_chars_per_proposal": cfg.max_changed_chars_per_proposal,
            "max_patch_edits_per_run": cfg.max_patch_edits_per_run,
            "max_rejection_context": cfg.max_rejection_context,
            "rejection_context_days": cfg.rejection_context_days,
            "max_final_body_chars": cfg.max_final_body_chars,
            "max_rule_page_tokens": cfg.max_rule_page_tokens,
            "max_procedure_page_tokens": cfg.max_procedure_page_tokens,
            "eval": cfg.eval,
            "eval_results": eval_results,
            "require_approval": self.svc.inner.require_approval,
        });
        Ok(Plan::Stage(StagePlan {
            provider: report.provider.clone(),
            model: report.model.clone(),
            summary: report.summary.clone(),
            warnings: report.warnings.clone(),
            rejected: serde_json::to_value(&report.rejected_candidates)
                .unwrap_or_else(|_| json!([])),
            config,
            proposal_actor: cfg.proposal_actor.clone(),
            proposals,
            rejected_count: report.rejected_candidates.len(),
        }))
    }

    // ---- publication ---------------------------------------------------

    pub(super) async fn publish_resume(
        &mut self,
        publication: ProcessingPublication,
    ) -> Result<ai_memory_core::ProcessingJobStatus, Abort> {
        let plan: Plan = serde_json::from_value(publication.plan_json)
            .map_err(|_| failed("the stored publication plan is unreadable"))?;
        if !matches!(plan, Plan::Report { .. }) {
            self.authorize(ai_memory_store::ProjectAccess::Write)
                .await?;
        }
        let result = match plan {
            Plan::Pages(p) => self.publish_pages(&p).await?,
            Plan::Stage(s) => self.publish_stage(&s).await?,
            Plan::Report {
                report,
                eval_results,
            } => json!({ "report": report, "eval_results": eval_results }),
        };
        self.writer
            .finish_processing_job(&self.claim, ProcessingOutcome::Completed { result })
            .await
            .map_err(store_abort)
    }

    async fn effects(&self) -> Result<Vec<ai_memory_store::ProcessingEffect>, Abort> {
        self.svc
            .inner
            .reader
            .processing_effects(self.ws, self.proj, self.job_id)
            .await
            .map_err(store_abort)
    }

    fn request_for(&self, page: &PlannedPage) -> Result<WritePageRequest, Abort> {
        Ok(WritePageRequest {
            workspace_id: self.ws,
            project_id: self.proj,
            path: PagePath::new(page.path.clone())
                .map_err(|_| failed("a planned page has an invalid path"))?,
            frontmatter: page.frontmatter.clone(),
            body: page.body.clone(),
            tier: page.tier,
            pinned: false,
            title: page.title.clone(),
            admission_ctx: Some(AdmissionContext {
                op: AdmissionOp::Consolidate,
                actor: self.actor.clone(),
                ..Default::default()
            }),
            author_id: self.author_id,
            actor: self.actor.clone(),
            evidence: vec![PageEvidence {
                kind: PageEvidenceKind::Session,
                source_id: self.session_id.to_string(),
            }],
        })
    }

    /// Whether every planned page already carries this job's stamp.
    fn stamped(&self, plan: &PagesPlan) -> bool {
        let job = self.job_id.to_string();
        plan.pages.iter().all(|p| {
            PagePath::new(p.path.clone()).is_ok_and(|path| {
                self.wiki
                    .read_page(self.ws, self.proj, &path)
                    .is_ok_and(|md| {
                        md.frontmatter.get("processing_job").and_then(Value::as_str)
                            == Some(job.as_str())
                    })
            })
        })
    }

    async fn current_ids(&self, plan: &PagesPlan) -> Result<Vec<Option<PageId>>, Abort> {
        let mut out = Vec::with_capacity(plan.pages.len());
        for p in &plan.pages {
            out.push(
                self.svc
                    .inner
                    .reader
                    .latest_page_id_by_ids(self.ws, self.proj, p.path.clone())
                    .await
                    .map_err(store_abort)?,
            );
        }
        Ok(out)
    }

    pub(super) async fn write_pages(&self, plan: &PagesPlan) -> Result<Vec<PageId>, Abort> {
        if plan.pages.is_empty() {
            return Ok(Vec::new());
        }
        let bases: Vec<Option<PageId>> = plan
            .pages
            .iter()
            .map(|p| p.base.as_deref().and_then(|b| b.parse::<PageId>().ok()))
            .collect();
        let current = self.current_ids(plan).await?;
        let adopt = |ids: Vec<Option<PageId>>| -> Result<Vec<PageId>, Abort> {
            ids.into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| failed("a published page disappeared before it was confirmed"))
        };
        if current != bases {
            // Either this job already committed (a crash before the effect
            // was recorded) or somebody else changed the page: never write
            // over the latter, never repeat the former.
            if self.stamped(plan) {
                return adopt(current);
            }
            return Err(failed(
                "a target page changed after the final call read it; nothing was overwritten",
            ));
        }
        let mut requests = Vec::with_capacity(plan.pages.len());
        for (page, base) in plan.pages.iter().zip(&bases) {
            let pre = base.map_or(PagePrecondition::Absent, PagePrecondition::Latest);
            requests.push((self.request_for(page)?, pre));
        }
        if !plan.multi && requests.len() == 1 {
            let (req, pre) = requests.remove(0);
            return match self
                .wiki
                .write_page_if(req, pre)
                .await
                .map_err(wiki_abort)?
            {
                ConditionalWrite::Written(id) => Ok(vec![id]),
                ConditionalWrite::Mismatch { .. } => self.after_mismatch(plan).await,
            };
        }
        match self
            .wiki
            .apply_batch_if(requests)
            .await
            .map_err(wiki_abort)?
        {
            ConditionalBatchWrite::Written(ids) => Ok(ids),
            ConditionalBatchWrite::Mismatch { .. } => self.after_mismatch(plan).await,
        }
    }

    async fn after_mismatch(&self, plan: &PagesPlan) -> Result<Vec<PageId>, Abort> {
        if self.stamped(plan) {
            return self
                .current_ids(plan)
                .await?
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| failed("a published page disappeared before it was confirmed"));
        }
        Err(failed(
            "a target page changed while publishing; nothing was overwritten",
        ))
    }

    async fn publish_pages(&self, plan: &PagesPlan) -> Result<Value, Abort> {
        let effects = self.effects().await?;
        let ids: Vec<PageId> = if let Some(effect) = effects.iter().find(|e| e.key == "pages") {
            effect
                .json
                .get("ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|v| v.as_str().and_then(|s| s.parse::<PageId>().ok()))
                        .collect()
                })
                .ok_or_else(|| failed("the recorded page effect is unreadable"))?
        } else {
            let ids = self.write_pages(plan).await?;
            self.writer
                .record_processing_effect(
                    &self.claim,
                    "pages".into(),
                    json!({ "ids": ids.iter().map(ToString::to_string).collect::<Vec<_>>() }),
                )
                .await
                .map_err(store_abort)?;
            ids
        };
        // Auto-commit so the supersession lands in git, as in the legacy path.
        // Idempotent and also run on resume: a crash between the recorded
        // effect and the commit must not leave the pages uncommitted.
        let _ = self.wiki.commit_all(&plan.commit_message);
        let outcomes = plan
            .pages
            .iter()
            .zip(ids.iter().map(Some).chain(std::iter::repeat(None)))
            .map(|(page, id)| OutcomeJson {
                path: page.path.clone(),
                new_title: page.out_title.clone(),
                new_body_markdown: page.body.clone(),
                tags: page.out_tags.clone(),
                page_id: id.map(ToString::to_string),
            })
            .collect();
        serde_json::to_value(PagesResult { outcomes })
            .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))
    }

    /// The scheduler's auto-approve identity; the actor is part of the job, so
    /// a manual and a scheduled request for one session still share one job.
    fn scheduled(&self) -> bool {
        self.actor.agent.as_deref() == Some(AUTO_IMPROVE_AUTO_APPROVE_AGENT)
    }

    fn approval_actor(&self) -> ActorContext {
        if self.scheduled() {
            return ActorContext {
                agent: Some(AUTO_IMPROVE_AUTO_APPROVE_AGENT.into()),
                ..ActorContext::default()
            };
        }
        let mut actor = self.actor.clone();
        actor.agent = Some("auto_improve_auto_approve".into());
        actor
    }

    async fn publish_stage(&self, plan: &StagePlan) -> Result<Value, Abort> {
        let inner = &self.svc.inner;
        let mut effects = self.effects().await?;
        // 1. Stage exactly once: recorded effect, else a run already stamped
        //    with this job (crash after the commit), else stage now.
        let staged: StageEffect = if let Some(e) = effects.iter().find(|e| e.key == "stage") {
            serde_json::from_value(e.json.clone())
                .map_err(|_| failed("the recorded staging effect is unreadable"))?
        } else {
            let found = inner
                .reader
                .auto_improve_run_for_processing_job(self.ws, self.proj, self.job_id)
                .await
                .map_err(store_abort)?;
            let effect = if let Some((run_id, ids)) = found {
                // The adopted run's skips are not stored: they are exactly the
                // planned targets that did not become proposals.
                let mut staged_paths = std::collections::BTreeSet::new();
                for id in &ids {
                    if let Some(detail) = inner
                        .reader
                        .auto_improve_proposal_detail(self.ws, self.proj, *id)
                        .await
                        .map_err(store_abort)?
                    {
                        staged_paths.insert(detail.summary.target_path.to_string());
                    }
                }
                let skipped = plan
                    .proposals
                    .iter()
                    .filter(|p| !staged_paths.contains(&p.target_path))
                    .map(|p| SkippedProposal {
                        target_path: p.target_path.clone(),
                        reason: "not staged: another proposal was already pending for this target"
                            .to_string(),
                    })
                    .collect();
                StageEffect {
                    run_id: run_id.to_string(),
                    proposal_ids: ids.iter().map(ToString::to_string).collect(),
                    skipped,
                }
            } else {
                let report = self
                    .writer
                    .stage_auto_improve_run_for_owner(plan.stage_input(self)?, self.owner.clone())
                    .await
                    .map_err(store_abort)?;
                StageEffect {
                    run_id: report.run_id.to_string(),
                    proposal_ids: report
                        .proposal_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    skipped: report.skipped,
                }
            };
            self.writer
                .record_processing_effect(
                    &self.claim,
                    "stage".into(),
                    serde_json::to_value(&effect)
                        .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))?,
                )
                .await
                .map_err(store_abort)?;
            effect
        };
        let run_id = staged
            .run_id
            .parse::<AutoImproveRunId>()
            .map_err(|_| failed("the staged run id is unreadable"))?;
        // 2. Sidecars and the approval tail, per proposal, each confirmed.
        let approval_required = inner.require_approval;
        let admission = self
            .overrides
            .admission_ctx
            .clone()
            .unwrap_or(AdmissionContext {
                op: AdmissionOp::WritePage,
                ..AdmissionContext::default()
            });
        let mut proposals = Vec::with_capacity(staged.proposal_ids.len());
        for raw in &staged.proposal_ids {
            let id = raw
                .parse::<AutoImproveProposalId>()
                .map_err(|_| failed("a staged proposal id is unreadable"))?;
            let sidecar = self
                .wiki
                .write_auto_improve_sidecar(self.ws, self.proj, id)
                .await
                .map_err(wiki_abort)?
                .display()
                .to_string();
            let key = format!("approve:{id}");
            let (status, page_id) = if let Some(e) = effects.iter().find(|e| e.key == key) {
                (
                    e.json
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending")
                        .to_string(),
                    e.json
                        .get("page_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                )
            } else if approval_required {
                ("pending".to_string(), None)
            } else {
                let detail = inner
                    .reader
                    .auto_improve_proposal_detail(self.ws, self.proj, id)
                    .await
                    .map_err(store_abort)?;
                let decided = match detail {
                    Some(d) if d.summary.status != AutoImproveProposalStatus::Pending => Some((
                        d.summary.status.as_str().to_string(),
                        d.applied_page_id.map(|p| p.to_string()),
                    )),
                    _ => None,
                };
                let outcome = if let Some(decided) = decided {
                    decided
                } else {
                    match self
                        .wiki
                        .approve_auto_improve_proposal(
                            self.ws,
                            self.proj,
                            id,
                            self.approval_actor(),
                            self.author_id,
                            Some(admission.clone()),
                        )
                        .await
                        .map_err(wiki_abort)?
                    {
                        ApproveAutoImproveProposalResult::Approved { page_id } => {
                            ("approved".to_string(), Some(page_id.to_string()))
                        }
                        ApproveAutoImproveProposalResult::Conflict => {
                            ("conflict".to_string(), None)
                        }
                    }
                };
                let effect = json!({ "status": outcome.0, "page_id": outcome.1 });
                self.writer
                    .record_processing_effect(&self.claim, key, effect.clone())
                    .await
                    .map_err(store_abort)?;
                effects.push(ai_memory_store::ProcessingEffect {
                    key: format!("approve:{id}"),
                    json: effect,
                    recorded_at: jiff::Timestamp::now(),
                });
                outcome
            };
            proposals.push(AutoImproveStagedProposal {
                id,
                sidecar_path: sidecar,
                status,
                page_id,
            });
        }
        serde_json::to_value(AutoImproveStaged {
            run_id,
            approval_required,
            summary: plan.summary.clone(),
            warnings: plan.warnings.clone(),
            rejected_candidates_count: plan.rejected_count,
            proposals,
            skipped: staged.skipped,
        })
        .map_err(|e| Abort::Error(ProcessingError::Other(e.to_string())))
    }
}

// ---- result conversions ---------------------------------------------------

/// The legacy outcomes of a completed consolidation job.
pub(super) fn outcomes_from_result(
    result: Option<&Value>,
) -> ConsolidatorResult<Vec<ConsolidationOutcome>> {
    let parsed: PagesResult = result
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| ConsolidatorError::Processing("the job result is unreadable".into()))?;
    parsed
        .outcomes
        .into_iter()
        .map(|o| {
            Ok(ConsolidationOutcome {
                path: PagePath::new(o.path)?,
                dry_run: false,
                new_title: o.new_title,
                new_body_markdown: o.new_body_markdown,
                page_id: o.page_id.as_deref().map(str::parse::<PageId>).transpose()?,
                tags: o.tags,
            })
        })
        .collect()
}

/// The legacy review report of a completed report-only job.
pub(super) fn report_from_result(result: Option<&Value>) -> Result<AutoImproveReport, String> {
    let value = result.ok_or("the job result is missing")?;
    let mut report: AutoImproveReport = serde_json::from_value(
        value
            .get("report")
            .cloned()
            .ok_or("the job result has no report")?,
    )
    .map_err(|_| "the job report is unreadable".to_string())?;
    if let Some(evals) = value.get("eval_results").cloned() {
        report.eval_results = serde_json::from_value(evals).unwrap_or_default();
    }
    Ok(report)
}

/// What a completed staging job staged.
pub(super) fn staged_from_result(result: Option<&Value>) -> Result<AutoImproveStaged, String> {
    result
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| "the job result is unreadable".to_string())
}
