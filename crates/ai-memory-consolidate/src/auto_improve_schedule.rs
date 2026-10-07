//! Scheduled auto-improvement orchestration.
//!
//! The server-side scheduler (started by `ai-memory serve`) drives one
//! non-overlapping tick per configured interval; this module owns what a
//! tick *does*: seed per-scope watermarks at startup, claim newly
//! completed sessions (at-most-once per session), run
//! [`run_auto_improve_review`], stage the validated proposals, write the
//! human-reviewable sidecars, and auto-approve them through the wiki
//! mutation path unless the operator requires manual approval.
//!
//! Approval-gate semantics are deliberately identical to the manual
//! CLI/admin/MCP path: proposals are always staged first, and
//! `require_approval` only decides whether they are applied immediately
//! or left pending — see `docs/auto-improvement-loop.md`.

use std::sync::Arc;

use ai_memory_core::{ActorContext, PagePath, ProjectId, SessionId, WorkspaceId};
use ai_memory_llm::LlmProvider;
use ai_memory_store::{
    ApproveAutoImproveProposalResult, AutoImproveProposalOperation, NewAutoImproveProposal,
    ReaderPool, SkippedProposal, StageAutoImproveRun, WriterHandle,
};
use ai_memory_wiki::Wiki;
use anyhow::Result;
use tracing::info;

use crate::{
    AutoImproveError, AutoImproveReport, AutoImproveReviewConfig, redacted_auto_improve_summary,
    run_auto_improve_review,
};

/// Settings for the scheduled auto-improvement loop, already mapped from
/// the host's configuration. Bundles the review config with the
/// scheduler-only knobs so the tick driver takes a single value.
#[derive(Debug, Clone)]
pub struct ScheduledAutoImproveSettings {
    /// Full review configuration (`[auto_improve]`).
    pub review: AutoImproveReviewConfig,
    /// When true, validated proposals stay pending for manual review
    /// instead of being auto-approved (`[auto_improve] require_approval`).
    pub require_approval: bool,
    /// Minimum session age before a completed session becomes a
    /// candidate (`[auto_improve.scheduler] min_session_age_secs`).
    pub min_session_age_secs: u64,
    /// Maximum sessions reviewed per scope per tick
    /// (`[auto_improve.scheduler] max_sessions_per_tick`).
    pub max_sessions_per_tick: usize,
    /// Cross-session ("experience") pass settings; `None` = disabled
    /// (`[auto_improve.experience]`, docs/experience.md).
    pub experience: Option<crate::ExperienceConfig>,
}

/// Seed the per-scope scheduler watermark for every known scope at
/// startup, so historical sessions are never auto-reviewed on upgrade.
/// Returns `(scopes, errors)`.
///
/// # Errors
/// Fails only when the scope list itself cannot be read; per-scope
/// state-init failures are logged and counted, not fatal.
pub async fn initialize_auto_improve_scheduler_scopes(
    reader: &ReaderPool,
    writer: &WriterHandle,
) -> Result<(usize, usize)> {
    let scopes = reader.list_all_scopes().await?;
    let total = scopes.len();
    let mut errors = 0usize;
    for scope in scopes {
        if let Err(e) = writer
            .ensure_auto_improve_scheduler_state(scope.workspace_id, scope.project_id)
            .await
        {
            errors += 1;
            tracing::warn!(
                workspace = %scope.workspace_name,
                project = %scope.project_name,
                error = %e,
                "auto-improve scheduler startup state init failed"
            );
        }
    }
    Ok((total, errors))
}

struct ScheduledAutoImproveOutcome {
    run_id: ai_memory_core::AutoImproveRunId,
    proposals: usize,
    approved: usize,
    pending: usize,
    conflicts: usize,
    /// Proposals the store declined to stage (something is already pending for
    /// the same target). This is the unattended path: nobody reads a response,
    /// so a drop that does not reach the log reaches nobody at all — a run that
    /// lost its Nth proposal would otherwise be indistinguishable from a clean
    /// run of N-1.
    skipped: Vec<SkippedProposal>,
}

/// Aggregate counters for one scheduler tick across every scope.
///
/// The trailing record fields mirror exactly what the tick's warnings
/// carried (each is set in the same arm that emits the warning) so the
/// tests observe the warning contracts through typed values instead of
/// log capture: a `warn!` callsite's cached `Interest` is computed
/// process-wide by whichever thread first executes it, and under a
/// single-process harness a sibling test can register the callsite with
/// no subscriber installed, silencing every later capture (#1116).
/// Production reads the counters only, hence the explicit allows.
#[derive(Debug, Default)]
pub struct ScheduledAutoImproveTickOutcome {
    /// Total scopes considered this tick.
    pub scopes: usize,
    /// Scopes with at least one unclaimed candidate session.
    pub scopes_with_candidates: usize,
    /// Sessions whose review completed (staged or empty).
    pub reviewed: usize,
    /// Proposals that were reviewed but could not be staged, usually because
    /// another proposal is already pending for the same target.
    pub skipped: usize,
    /// Per-scope/per-session failures, logged and counted, not fatal.
    pub errors: usize,
    /// Sessions whose claim spent its last attempt this tick and is now parked.
    /// Counted separately from `errors` because it is the terminal state an
    /// operator has to act on, not just another retryable failure (#833).
    pub parked: usize,
    /// Cross-session ("experience") passes that ran this tick.
    pub experience_runs: usize,
    /// Redacted summaries of the review failures the tick warned about
    /// ("scheduled auto-improve failed"), in warn order. Read by the tick
    /// tests (compiled only under `cfg(test)`).
    #[allow(dead_code)]
    review_failures: Vec<String>,
    /// Redacted summaries of the experience-pass failures the tick warned
    /// about ("experience pass failed"), in warn order. Read by the tick
    /// tests (compiled only under `cfg(test)`).
    #[allow(dead_code)]
    experience_failures: Vec<String>,
    /// The proposals the per-session review path warned were not staged
    /// ("scheduled auto-improve proposal was not staged"), in warn order.
    /// The experience path counts its skips into `skipped` without a
    /// per-proposal warning, so they appear here only through the review
    /// path. Read by the tick tests (compiled only under `cfg(test)`).
    #[allow(dead_code)]
    skipped_proposals: Vec<SkippedProposal>,
}

struct ScheduledAutoImproveContext<'a> {
    reader: &'a ReaderPool,
    writer: &'a WriterHandle,
    wiki: &'a Wiki,
    llm: &'a Arc<dyn LlmProvider>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    settings: &'a ScheduledAutoImproveSettings,
}

/// One scheduler tick: claim newly completed sessions in every scope
/// (at-most-once via the persisted watermark) and run the auto-improve
/// review + staging pipeline for each. Failures are logged and counted
/// in the outcome; they never abort the tick.
///
/// # Errors
/// Fails only when the scope list itself cannot be read.
pub async fn run_auto_improve_scheduler_tick(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    llm: &Arc<dyn LlmProvider>,
    settings: &ScheduledAutoImproveSettings,
) -> Result<ScheduledAutoImproveTickOutcome> {
    let scopes = reader.list_all_scopes().await?;
    let mut outcome = ScheduledAutoImproveTickOutcome {
        scopes: scopes.len(),
        ..ScheduledAutoImproveTickOutcome::default()
    };

    for scope in scopes {
        if let Err(e) = writer
            .ensure_auto_improve_scheduler_state(scope.workspace_id, scope.project_id)
            .await
        {
            outcome.errors += 1;
            tracing::warn!(
                workspace = %scope.workspace_name,
                project = %scope.project_name,
                error = %e,
                "scheduled auto-improve state init failed"
            );
            continue;
        }

        let candidates = match reader
            .auto_improve_candidate_sessions(
                scope.workspace_id,
                scope.project_id,
                settings.min_session_age_secs,
                settings.max_sessions_per_tick,
            )
            .await
        {
            Ok(candidates) => candidates,
            Err(e) => {
                outcome.errors += 1;
                tracing::warn!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    error = %e,
                    "scheduled auto-improve candidate query failed"
                );
                continue;
            }
        };
        let ctx = ScheduledAutoImproveContext {
            reader,
            writer,
            wiki,
            llm,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            settings,
        };

        // Cross-session ("experience") pass — cadence-gated per scope,
        // independent of whether this tick has per-session candidates.
        if let Some(experience) = &settings.experience {
            match reader
                .experience_pass_due(scope.workspace_id, scope.project_id)
                .await
            {
                Ok((newer, _)) if newer >= experience.min_new_sessions => {
                    match run_scheduled_experience(&ctx, experience).await {
                        Ok(None) => {
                            tracing::debug!(
                                workspace = %scope.workspace_name,
                                project = %scope.project_name,
                                "experience pass skipped (too few session pages); \
                                 cadence anchor kept"
                            );
                        }
                        Ok(Some(run)) => {
                            outcome.experience_runs += 1;
                            outcome.skipped += run.skipped.len();
                            if let Err(e) = writer
                                .mark_experience_pass_run(scope.workspace_id, scope.project_id)
                                .await
                            {
                                outcome.errors += 1;
                                tracing::warn!(
                                    workspace = %scope.workspace_name,
                                    project = %scope.project_name,
                                    error = %e,
                                    "experience pass mark failed"
                                );
                            }
                            info!(
                                workspace = %scope.workspace_name,
                                project = %scope.project_name,
                                new_sessions = newer,
                                run_id = %run.run_id,
                                proposals = run.proposals,
                                approved = run.approved,
                                pending = run.pending,
                                "experience pass completed"
                            );
                        }
                        Err(e) => {
                            outcome.errors += 1;
                            // Redacted summary only: the `Display` of this
                            // anyhow chain is transparent to the provider
                            // error and would print the response body.
                            let error_summary = redacted_scheduler_error_summary(&e);
                            outcome.experience_failures.push(error_summary.clone());
                            tracing::warn!(
                                workspace = %scope.workspace_name,
                                project = %scope.project_name,
                                error_summary = %error_summary,
                                "experience pass failed"
                            );
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    outcome.errors += 1;
                    tracing::warn!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        error = %e,
                        "experience cadence probe failed"
                    );
                }
            }
        }

        if candidates.is_empty() {
            continue;
        }

        outcome.scopes_with_candidates += 1;
        for candidate in candidates {
            let claimed = match ctx
                .writer
                .claim_auto_improve_scheduler_session(
                    ctx.workspace_id,
                    ctx.project_id,
                    candidate.session_id,
                    candidate.ended_at,
                )
                .await
            {
                Ok(claimed) => claimed,
                Err(e) => {
                    outcome.errors += 1;
                    tracing::warn!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        error = %e,
                        "scheduled auto-improve claim failed"
                    );
                    continue;
                }
            };
            if !claimed {
                tracing::debug!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    session_id = %candidate.session_id,
                    "scheduled auto-improve candidate already claimed or reviewed"
                );
                continue;
            }
            match run_scheduled_auto_improve(&ctx, candidate.session_id).await {
                Ok(run) => {
                    outcome.reviewed += 1;
                    outcome.skipped += run.skipped.len();
                    info!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        run_id = %run.run_id,
                        proposals = run.proposals,
                        approved = run.approved,
                        pending = run.pending,
                        conflicts = run.conflicts,
                        skipped = run.skipped.len(),
                        "scheduled auto-improve completed"
                    );
                    // The count above keeps every completed run comparable;
                    // this says WHICH proposal was lost and why, so the
                    // operator can act on it without querying the store.
                    for skipped in &run.skipped {
                        outcome.skipped_proposals.push(skipped.clone());
                        tracing::warn!(
                            workspace = %scope.workspace_name,
                            project = %scope.project_name,
                            session_id = %candidate.session_id,
                            run_id = %run.run_id,
                            target_path = %skipped.target_path,
                            reason = %skipped.reason,
                            "scheduled auto-improve proposal was not staged"
                        );
                    }
                }
                Err(e) => {
                    outcome.errors += 1;
                    // #833: the claim is this session's only in-flight marker and the
                    // candidate query excludes on it, so leaving it behind dropped the
                    // session from every future tick — silently, because the tick
                    // reports `errors=1` once and clean runs forever after. Release it
                    // so the next tick retries, and let the attempt counter park it
                    // once a deterministic failure has proved it will not recover.
                    // Redacted summary only: this exact string is persisted
                    // to the claim's `last_error` and logged — the `Display`
                    // of the anyhow chain would leak the provider body to
                    // both.
                    let error_summary = redacted_scheduler_error_summary(&e);
                    let attempts = match ctx
                        .writer
                        .record_auto_improve_claim_failure(
                            ctx.workspace_id,
                            ctx.project_id,
                            candidate.session_id,
                            &error_summary,
                        )
                        .await
                    {
                        Ok(attempts) => Some(attempts),
                        Err(release_err) => {
                            // The review already failed; a failed release is a second,
                            // separate fault and must not mask the first.
                            tracing::warn!(
                                workspace = %scope.workspace_name,
                                project = %scope.project_name,
                                session_id = %candidate.session_id,
                                error = %release_err,
                                "scheduled auto-improve claim release failed"
                            );
                            None
                        }
                    };
                    let parked = attempts
                        .is_some_and(|n| n >= ai_memory_store::AUTO_IMPROVE_CLAIM_MAX_ATTEMPTS);
                    if parked {
                        outcome.parked += 1;
                    }
                    outcome.review_failures.push(error_summary.clone());
                    tracing::warn!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        error_summary = %error_summary,
                        attempts = attempts.unwrap_or(0),
                        parked,
                        "scheduled auto-improve failed"
                    );
                }
            }
        }
    }

    Ok(outcome)
}

/// Redacted one-line summary of a scheduler failure, for the persisted claim
/// `last_error` and the tick/experience log events.
///
/// `run_scheduled_auto_improve` and `run_scheduled_experience` convert the
/// typed [`AutoImproveError`] into `anyhow::Error` on the way out, and
/// `AutoImproveError::Llm` is transparent to `LlmError` — a provider 400
/// would otherwise persist its response body to the claim and print it in
/// the tick warnings. This is the scheduler's single redaction boundary:
/// classify by the typed chain, never by its `Display` or `Debug`. A typed
/// review error reuses [`redacted_auto_improve_summary`]; a bare
/// [`LlmError`] is summarized by its class and status; anything else gets a
/// generic class so an unrecognized cause cannot smuggle free text through.
#[must_use]
fn redacted_scheduler_error_summary(error: &anyhow::Error) -> String {
    for cause in error.chain() {
        if let Some(review_error) = cause.downcast_ref::<AutoImproveError>() {
            return redacted_auto_improve_summary(review_error);
        }
        if let Some(llm_error) = cause.downcast_ref::<ai_memory_llm::LlmError>() {
            return format!(
                "auto-improve failed: class={} status={}",
                llm_error.class(),
                llm_error
                    .http_status()
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "none".into())
            );
        }
    }
    "auto-improve failed: class=unrecognized status=none".to_string()
}

async fn run_scheduled_auto_improve(
    ctx: &ScheduledAutoImproveContext<'_>,
    session_id: SessionId,
) -> Result<ScheduledAutoImproveOutcome> {
    let cfg = ctx.settings.review.clone();
    let report = run_auto_improve_review(
        ctx.reader,
        &**ctx.llm,
        ctx.workspace_id,
        ctx.project_id,
        session_id,
        cfg.clone(),
    )
    .await?;
    stage_and_apply(ctx, Some(session_id), &report, "scheduler").await
}

/// Run one cross-session ("experience") review and stage it through the
/// identical proposal path. `docs/experience.md`.
async fn run_scheduled_experience(
    ctx: &ScheduledAutoImproveContext<'_>,
    experience: &crate::ExperienceConfig,
) -> Result<Option<ScheduledAutoImproveOutcome>> {
    let cfg = ctx.settings.review.clone();
    let report = crate::run_experience_review(
        ctx.reader,
        &**ctx.llm,
        ctx.workspace_id,
        ctx.project_id,
        cfg,
        experience,
    )
    .await?;
    // A preflight skip (enough ENDED sessions but too few summary
    // pages — consolidation lag, purges) must not stage an empty run or
    // burn the cadence window (post-audit finding): report it as
    // not-run so the anchor stays put and the next tick retries.
    if report.provider == "none" {
        return Ok(None);
    }
    stage_and_apply(ctx, None, &report, "experience-scheduler")
        .await
        .map(Some)
}

/// Stage a report's proposals and (unless approval is required) apply
/// them — the shared tail of both the per-session and the experience
/// scheduler paths, so their behaviour cannot drift.
async fn stage_and_apply(
    ctx: &ScheduledAutoImproveContext<'_>,
    session_id: Option<SessionId>,
    report: &AutoImproveReport,
    trigger: &str,
) -> Result<ScheduledAutoImproveOutcome> {
    let cfg = ctx.settings.review.clone();
    let proposals =
        scheduled_auto_improve_new_proposals(ctx.reader, ctx.workspace_id, ctx.project_id, report)
            .await?;
    let staged = ctx
        .writer
        .stage_auto_improve_run_for_owner(
            StageAutoImproveRun {
                workspace_id: ctx.workspace_id,
                project_id: ctx.project_id,
                session_id,
                provider: Some(report.provider.clone()),
                model: Some(report.model.clone()),
                summary: Some(report.summary.clone()),
                warnings_json: serde_json::to_value(&report.warnings)
                    .unwrap_or_else(|_| serde_json::json!([])),
                rejected_candidates_json: serde_json::to_value(&report.rejected_candidates)
                    .unwrap_or_else(|_| serde_json::json!([])),
                config_json: serde_json::json!({
                    "trigger": trigger,
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
                    "eval_results": report.eval_results(),
                    "require_approval": ctx.settings.require_approval,
                }),
                proposal_actor: ActorContext {
                    agent: Some(cfg.proposal_actor.clone()),
                    ..ActorContext::default()
                },
                proposals,
            },
            None,
        )
        .await?;
    for id in &staged.proposal_ids {
        ctx.wiki
            .write_auto_improve_sidecar(ctx.workspace_id, ctx.project_id, *id)
            .await?;
    }

    let mut approved = 0usize;
    let mut pending = 0usize;
    let mut conflicts = 0usize;
    for proposal_id in &staged.proposal_ids {
        if ctx.settings.require_approval {
            pending += 1;
            continue;
        }
        match ctx
            .wiki
            .approve_auto_improve_proposal(
                ctx.workspace_id,
                ctx.project_id,
                *proposal_id,
                ActorContext {
                    agent: Some("auto_improve_scheduler_auto_approve".into()),
                    ..ActorContext::default()
                },
                None,
                Some(ai_memory_wiki::AdmissionContext {
                    op: ai_memory_wiki::AdmissionOp::WritePage,
                    ..ai_memory_wiki::AdmissionContext::default()
                }),
            )
            .await?
        {
            ApproveAutoImproveProposalResult::Approved { .. } => approved += 1,
            ApproveAutoImproveProposalResult::Conflict => conflicts += 1,
        }
    }

    Ok(ScheduledAutoImproveOutcome {
        run_id: staged.run_id,
        proposals: staged.proposal_ids.len(),
        approved,
        pending,
        conflicts,
        skipped: staged.skipped,
    })
}

async fn scheduled_auto_improve_new_proposals(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    report: &AutoImproveReport,
) -> Result<Vec<NewAutoImproveProposal>> {
    let mut proposals = Vec::with_capacity(report.proposals.len());
    for p in &report.proposals {
        let path = PagePath::new(p.path.clone())?;
        let target_exists = reader
            .page_body_by_ids(workspace_id, project_id, path.as_str())
            .await?
            .is_some();
        let operation = if p.edit_mode == "patch"
            || (target_exists && path.as_str() == "_slots/current-focus.md")
        {
            AutoImproveProposalOperation::Update
        } else {
            AutoImproveProposalOperation::Create
        };
        let expected_base_body_sha256 = p
            .expected_base_body_sha256
            .as_deref()
            .map(hex_to_sha256)
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid expected_base_body_sha256: {e}"))?;
        proposals.push(NewAutoImproveProposal {
            operation,
            target_path: path,
            kind: p.kind.clone(),
            title: p.title.clone(),
            confidence: f64::from(p.confidence),
            rationale: p.rationale.clone(),
            evidence_json: report.proposal_evidence_json(p)?,
            body_markdown: p.body_markdown.clone(),
            artifact_sha256: None,
            edit_mode: Some(p.edit_mode.clone()),
            patch_json: serde_json::to_value(&p.edits).ok(),
            expected_base_body_sha256,
        });
    }
    Ok(proposals)
}

fn hex_to_sha256(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err("expected 64 hex chars".into());
    }
    let mut out = [0_u8; 32];
    for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        out[idx] = u8::from_str_radix(s, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{
        AgentKind, NewObservation, NewSession, ObservationKind, Sanitized, Sanitizer,
    };
    use ai_memory_llm::{ChatRequest, ChatResponse, LlmError, LlmResult};
    use ai_memory_store::Store;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct PanicLlm;

    impl LlmProvider for PanicLlm {
        fn name(&self) -> &'static str {
            "panic"
        }

        fn model(&self) -> &str {
            "panic"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { panic!("preflight-skipped scheduler test must not call LLM") })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { panic!("preflight-skipped scheduler test must not call LLM") })
        }
    }

    /// A provider that rejects the scheduled review with a private body:
    /// the claim's persisted `last_error` and the tick warnings must carry
    /// class/status only, never the body.
    struct SchedulerSentinelLlm {
        calls: Arc<AtomicUsize>,
    }

    impl LlmProvider for SchedulerSentinelLlm {
        fn name(&self) -> &'static str {
            "scheduler-sentinel"
        }

        fn model(&self) -> &str {
            "sentinel-model"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Err(LlmError::Provider {
                    status: 400,
                    body: "SENTINEL_PRIVATE_BODY".into(),
                })
            })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(LlmError::Provider {
                    status: 400,
                    body: "SENTINEL_PRIVATE_BODY".into(),
                })
            })
        }
    }

    /// Cross-session fake that fails the experience review with a private
    /// body: the experience warnings must carry class/status only.
    struct ExperienceSentinelLlm;

    impl LlmProvider for ExperienceSentinelLlm {
        fn name(&self) -> &'static str {
            "experience-sentinel"
        }

        fn model(&self) -> &str {
            "sentinel-model"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(ChatResponse {
                    text: "unused".into(),
                    usage: None,
                    model: "sentinel-model".into(),
                })
            })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Err(LlmError::Provider {
                    status: 400,
                    body: "SENTINEL_PRIVATE_BODY".into(),
                })
            })
        }
    }

    /// Read the persisted claim row directly. A retryable claim is not
    /// parked, so the typed parked-claims listing never sees it — the only
    /// way to assert what a single retryable failure persisted.
    fn claim_row(
        db_path: &std::path::Path,
        ws: WorkspaceId,
        proj: ProjectId,
        session: SessionId,
    ) -> (String, u32) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT last_error, attempts FROM auto_improve_scheduler_claims \
             WHERE workspace_id = ?1 AND project_id = ?2 AND session_id = ?3",
            rusqlite::params![ws.as_bytes(), proj.as_bytes(), session.as_bytes()],
            |row| {
                let last_error: Option<String> = row.get(0)?;
                Ok((last_error.unwrap(), row.get(1)?))
            },
        )
        .expect("the claim row must exist after a failed review")
    }

    /// A retryable provider failure must persist only the redacted
    /// class/status summary to the claim's `last_error` on every attempt,
    /// keep the existing attempts/park policy, and leave a fresh store
    /// handle (process restart) unable to find the body in the DB.
    #[tokio::test]
    async fn scheduled_retryable_failure_persists_only_class_status_summary() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "retry-sentinel", None)
            .await
            .unwrap();
        initialize_auto_improve_scheduler_scopes(&store.reader, &store.writer)
            .await
            .unwrap();
        let session_id = seed_reviewable_session(&store, ws, project).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let llm: Arc<dyn LlmProvider> = Arc::new(SchedulerSentinelLlm {
            calls: Arc::clone(&calls),
        });
        let settings = ScheduledAutoImproveSettings {
            review: AutoImproveReviewConfig {
                min_observations: 3,
                min_session_duration_secs: 0,
                ..AutoImproveReviewConfig::default()
            },
            require_approval: true,
            min_session_age_secs: 0,
            max_sessions_per_tick: 10,
            experience: None,
        };

        let expected = "auto-improve failed: class=provider status=400";

        // Attempt 1: retryable provider 400 → the claim records the attempt
        // (not parked). The persisted column carries only class/status.
        let first =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(first.errors, 1, "the retryable failure must be reported");
        assert_eq!(
            first.parked, 0,
            "a retryable failure must not park the claim"
        );
        // The failure record is set in the same arm that emits the warning,
        // from the same summary, so this proves the warning fired carrying
        // class/status only — never the body — without a log capture.
        assert_eq!(
            first.review_failures,
            [expected],
            "the failure warning must fire with the redacted summary"
        );
        let (last_error, attempts) = claim_row(store.db_path(), ws, project, session_id);
        assert_eq!(attempts, 1);
        assert_eq!(
            last_error, expected,
            "the persisted last_error must be the redacted summary"
        );
        assert!(!last_error.contains("SENTINEL_PRIVATE_BODY"));

        // Attempt 2: the claim re-arms and each attempt writes only the safe
        // summary.
        let second =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(second.errors, 1);
        assert_eq!(second.parked, 0);
        let (last_error, attempts) = claim_row(store.db_path(), ws, project, session_id);
        assert_eq!(attempts, 2);
        assert_eq!(last_error, expected);

        // Attempt 3 exhausts the budget: the claim parks and stops being a
        // candidate — the policy is unchanged by the redaction.
        let third =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(third.errors, 1);
        assert_eq!(third.parked, 1, "the third attempt must park the claim");
        assert_eq!(third.review_failures, [expected]);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let fourth =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(fourth.errors, 0, "a parked claim must not be retried");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "the parked claim must not call the LLM"
        );
        assert!(
            fourth.review_failures.is_empty(),
            "a parked claim must not warn again: {:?}",
            fourth.review_failures
        );

        // Crash contract: a process death right after the write (before the
        // warning) leaves only the redacted value. A fresh handle on the
        // same DB file finds class/status, never the body.
        drop(wiki);
        drop(store);
        let restarted = Store::open(tmp.path()).unwrap();
        let (last_error, attempts) = claim_row(restarted.db_path(), ws, project, session_id);
        assert_eq!((attempts, last_error.as_str()), (3, expected));
        let parked = restarted
            .reader
            .auto_improve_parked_claims(ws, project)
            .await
            .unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].last_error.as_deref(), Some(expected));
    }

    /// An experience pass failure must warn with the redacted class/status
    /// and never the provider body.
    #[tokio::test]
    async fn experience_pass_failure_warns_class_status_not_body() {
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
        let project = store
            .writer
            .get_or_create_project(ws, "experience-sentinel", None)
            .await
            .unwrap();
        assert_eq!(
            initialize_auto_improve_scheduler_scopes(&store.reader, &store.writer)
                .await
                .unwrap(),
            (1, 0)
        );

        // Three completed sessions AFTER init, each with a summary page —
        // the cadence gate is due (same fixture as the cadence-gating test).
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        for _ in 0..3 {
            let session_id = SessionId::new();
            store
                .writer
                .begin_session(ai_memory_core::NewSession {
                    occurred_at: None,
                    id: session_id,
                    workspace_id: ws,
                    project_id: project,
                    agent_kind: ai_memory_core::AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            store.writer.end_session(session_id, None).await.unwrap();
            wiki.write_page(ai_memory_wiki::WritePageRequest {
                workspace_id: ws,
                project_id: project,
                path: PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
                frontmatter: serde_json::json!({"title": "session"}),
                body: "tag main then deploy; restart stack".into(),
                tier: ai_memory_core::Tier::Episodic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::anonymous(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
        }

        let settings = ScheduledAutoImproveSettings {
            review: AutoImproveReviewConfig::default(),
            require_approval: true,
            min_session_age_secs: 0,
            max_sessions_per_tick: 10,
            experience: Some(crate::ExperienceConfig {
                sessions: 10,
                min_new_sessions: 3,
                ..crate::ExperienceConfig::default()
            }),
        };
        let llm: Arc<dyn LlmProvider> = Arc::new(ExperienceSentinelLlm);

        let outcome =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();

        assert_eq!(outcome.experience_runs, 0, "{outcome:?}");
        assert_eq!(
            outcome.errors, 1,
            "the experience failure must be reported once: {outcome:?}"
        );
        // The failure record is set in the same arm that emits the warning,
        // from the same summary, so this proves the warning fired carrying
        // class/status only — never the body — without a log capture.
        assert_eq!(
            outcome.experience_failures,
            ["auto-improve failed: class=provider status=400"],
            "the experience failure warning must fire with the redacted summary"
        );
    }

    /// The scheduler redaction boundary classifies by the typed chain: a
    /// typed review error reuses the redacted summary, a bare `LlmError`
    /// keeps the same shape, and an unrecognized cause (including a non-LLM
    /// one whose `Display` carries the sentinel) gets a generic safe class
    /// with no free text.
    #[test]
    fn redacted_scheduler_error_summary_classifies_the_typed_chain() {
        let provider_400 = anyhow::Error::new(AutoImproveError::Llm(LlmError::Provider {
            status: 400,
            body: "SENTINEL_PRIVATE_BODY".into(),
        }));
        assert_eq!(
            redacted_scheduler_error_summary(&provider_400),
            "auto-improve failed: class=provider status=400"
        );

        // The same failure without the typed wrapper: identical shape.
        let bare = anyhow::Error::new(LlmError::Provider {
            status: 400,
            body: "SENTINEL_PRIVATE_BODY".into(),
        });
        assert_eq!(
            redacted_scheduler_error_summary(&bare),
            redacted_scheduler_error_summary(&provider_400)
        );

        // Non-LLM control: a string-only chain whose `Display` carries the
        // sentinel must not leak it.
        let opaque = anyhow::anyhow!("store blip with SENTINEL_PRIVATE_BODY details");
        assert_eq!(
            redacted_scheduler_error_summary(&opaque),
            "auto-improve failed: class=unrecognized status=none"
        );

        // A non-LLM typed variant keeps its fixed label and `none` status.
        let missing = anyhow::Error::new(AutoImproveError::SessionNotFound(SessionId::new()));
        assert_eq!(
            redacted_scheduler_error_summary(&missing),
            "auto-improve failed: class=session-not-found status=none"
        );
    }

    #[tokio::test]
    async fn auto_improve_scheduler_startup_init_preserves_first_interval_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let first_project = store
            .writer
            .get_or_create_project(ws, "first", None)
            .await
            .unwrap();
        let second_project = store
            .writer
            .get_or_create_project(ws, "second", None)
            .await
            .unwrap();

        for project_id in [first_project, second_project] {
            let before_startup_init = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    occurred_at: None,
                    id: before_startup_init,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            store
                .writer
                .end_session(before_startup_init, None)
                .await
                .unwrap();
        }

        assert_eq!(
            initialize_auto_improve_scheduler_scopes(&store.reader, &store.writer)
                .await
                .unwrap(),
            (2, 0)
        );

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let mut first_interval_sessions = Vec::new();
        for project_id in [first_project, second_project] {
            let session_id = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    occurred_at: None,
                    id: session_id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            store.writer.end_session(session_id, None).await.unwrap();
            first_interval_sessions.push((project_id, session_id));
        }

        let settings = ScheduledAutoImproveSettings {
            review: AutoImproveReviewConfig::default(),
            require_approval: false,
            min_session_age_secs: 0,
            max_sessions_per_tick: 10,
            experience: None,
        };
        let llm: Arc<dyn LlmProvider> = Arc::new(PanicLlm);
        let outcome =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();

        assert_eq!(outcome.scopes, 2);
        assert_eq!(outcome.scopes_with_candidates, 2);
        assert_eq!(outcome.reviewed, 4);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(outcome.errors, 0);

        for (project_id, session_id) in first_interval_sessions {
            let candidates = store
                .reader
                .auto_improve_candidate_sessions(ws, project_id, 0, 10)
                .await
                .unwrap();
            assert!(
                candidates.iter().all(|c| c.session_id != session_id),
                "first-interval session should have been reviewed or claimed"
            );
        }
    }

    /// Cross-session fake: proposes one procedure citing two sessions.
    struct ExperienceLlm;

    impl LlmProvider for ExperienceLlm {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-experience"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(ChatResponse {
                    text: "unused".into(),
                    usage: None,
                    model: "fake-experience".into(),
                })
            })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(serde_json::json!({
                    "summary": "a workflow repeats across sessions",
                    "proposals": [{
                        "operation": "create_or_update",
                        "path": "procedures/cross-session-release.md",
                        "title": "Cross-Session Release Workflow",
                        "kind": "procedure",
                        "confidence": 0.9,
                        "rationale": "Two sessions independently ran the same release steps.",
                        "evidence": [{"page": "sessions/a.md", "quote": "tag main then deploy"}],
                        "body_markdown": "# Cross-Session Release Workflow\n\nTag main, then deploy."
                    }],
                    "rejected_candidates": []
                }))
            })
        }
    }

    /// End to end through the scheduler: the experience pass runs only
    /// when its cadence says enough NEW sessions completed, stages its
    /// proposal through the identical pending path, and the cadence
    /// anchor advances so the next tick is a no-op (docs/experience.md).
    #[tokio::test]
    async fn experience_pass_is_cadence_gated_and_stages_pending() {
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
        let project = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        assert_eq!(
            initialize_auto_improve_scheduler_scopes(&store.reader, &store.writer)
                .await
                .unwrap(),
            (1, 0)
        );

        // Three completed sessions AFTER init, each with a summary page.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        for _ in 0..3 {
            let session_id = SessionId::new();
            store
                .writer
                .begin_session(ai_memory_core::NewSession {
                    occurred_at: None,
                    id: session_id,
                    workspace_id: ws,
                    project_id: project,
                    agent_kind: ai_memory_core::AgentKind::OpenCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
            store.writer.end_session(session_id, None).await.unwrap();
            wiki.write_page(ai_memory_wiki::WritePageRequest {
                workspace_id: ws,
                project_id: project,
                path: PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
                frontmatter: serde_json::json!({"title": "session"}),
                body: "tag main then deploy; restart stack".into(),
                tier: ai_memory_core::Tier::Episodic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::anonymous(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
        }

        let settings = ScheduledAutoImproveSettings {
            review: AutoImproveReviewConfig::default(),
            require_approval: true,
            min_session_age_secs: 0,
            // Per-session path effectively off: the fake sessions have no
            // observations, so preflight rejects them anyway.
            max_sessions_per_tick: 10,
            experience: Some(crate::ExperienceConfig {
                sessions: 10,
                min_new_sessions: 3,
                ..crate::ExperienceConfig::default()
            }),
        };
        let llm: Arc<dyn LlmProvider> = Arc::new(ExperienceLlm);
        let outcome =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(outcome.experience_runs, 1, "{outcome:?}");
        assert_eq!(outcome.errors, 0, "{outcome:?}");

        // The proposal is staged pending (require_approval), through the
        // same table the per-session path uses.
        let pending = store
            .reader
            .list_auto_improve_proposals(
                ws,
                project,
                Some(ai_memory_store::AutoImproveProposalStatus::Pending),
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert_eq!(
            pending[0].target_path.as_str(),
            "procedures/cross-session-release.md"
        );

        // Cadence anchored: an immediate second tick runs nothing.
        let outcome2 =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(outcome2.experience_runs, 0, "{outcome2:?}");

        // Post-audit regression: approving the proposal must land an
        // OKF-conformant FILE (type + generated), like every other write
        // path — the approve emit used to skip disk conformance, which
        // blocked export-okf and phantom-superseded on the next reindex
        // after a binary upgrade.
        wiki.approve_auto_improve_proposal(
            ws,
            project,
            pending[0].id,
            ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .unwrap();
        let approved = std::fs::read_to_string(
            tmp.path()
                .join("wiki")
                .join(ws.to_string())
                .join(project.to_string())
                .join("procedures/cross-session-release.md"),
        )
        .unwrap();
        assert!(approved.contains("type: Procedure"), "{approved}");
        assert!(approved.contains("generated:"), "{approved}");
    }

    /// Below the cadence floor nothing runs at all — no LLM call, no
    /// staging (the PanicLlm proves the LLM is never touched).
    #[tokio::test]
    async fn experience_pass_stays_quiet_below_the_cadence_floor() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        initialize_auto_improve_scheduler_scopes(&store.reader, &store.writer)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(ai_memory_core::NewSession {
                occurred_at: None,
                id: session_id,
                workspace_id: ws,
                project_id: project,
                agent_kind: ai_memory_core::AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store.writer.end_session(session_id, None).await.unwrap();

        let settings = ScheduledAutoImproveSettings {
            review: AutoImproveReviewConfig::default(),
            require_approval: true,
            min_session_age_secs: 0,
            max_sessions_per_tick: 10,
            experience: Some(crate::ExperienceConfig {
                sessions: 10,
                min_new_sessions: 3,
                ..crate::ExperienceConfig::default()
            }),
        };
        let llm: Arc<dyn LlmProvider> = Arc::new(PanicLlm);
        let outcome =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(outcome.experience_runs, 0, "{outcome:?}");
    }

    /// Proposes exactly one page, so a pre-existing pending proposal for that
    /// same page is guaranteed to collide.
    struct OneProposalLlm;

    impl LlmProvider for OneProposalLlm {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-model"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(ChatResponse {
                    text: "unused".into(),
                    usage: None,
                    model: "fake-model".into(),
                })
            })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                Ok(serde_json::json!({
                    "summary": "found one durable procedure",
                    "proposals": [{
                        "operation": "create_or_update",
                        "path": COLLIDING_PATH,
                        "title": "Release Procedure",
                        "kind": "procedure",
                        "confidence": 0.91,
                        "rationale": "The session repeated a release workflow with verification.",
                        "evidence": [{"page": "sessions/test.md", "quote": "run the full gate before release"}],
                        "body_markdown": "# Release Procedure\n\nRun the full gate before release."
                    }],
                    "rejected_candidates": []
                }))
            })
        }
    }

    const COLLIDING_PATH: &str = "procedures/release.md";

    async fn seed_reviewable_session(store: &Store, ws: WorkspaceId, proj: ProjectId) -> SessionId {
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        for i in 0..3 {
            store
                .writer
                .insert_observation(Sanitized::new(
                    NewObservation {
                        occurred_at: None,
                        session_id,
                        workspace_id: ws,
                        project_id: proj,
                        kind: if i == 0 {
                            ObservationKind::SessionStart
                        } else {
                            ObservationKind::UserPrompt
                        },
                        extension: None,
                        source_event: None,
                        title: format!("event {i}"),
                        body: "run the full gate before release".into(),
                        importance: 5,
                    },
                    &Sanitizer::builtin(),
                ))
                .await
                .unwrap();
        }
        store.writer.end_session(session_id, None).await.unwrap();
        session_id
    }

    /// Stage a pending proposal for `COLLIDING_PATH` in the same unattributed
    /// bucket the scheduler stages into, so the scheduler's own proposal hits
    /// the one-pending-per-target rule.
    async fn stage_blocking_proposal(store: &Store, ws: WorkspaceId, proj: ProjectId) {
        let staged = store
            .writer
            .stage_auto_improve_run(StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: None,
                provider: None,
                model: None,
                summary: Some("pre-existing pending proposal".into()),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({}),
                proposal_actor: ActorContext::default(),
                proposals: vec![NewAutoImproveProposal {
                    operation: AutoImproveProposalOperation::Create,
                    target_path: PagePath::new(COLLIDING_PATH.to_string()).unwrap(),
                    kind: "procedure".into(),
                    title: "Release Procedure".into(),
                    confidence: 0.9,
                    rationale: "already awaiting review".into(),
                    evidence_json: serde_json::json!([]),
                    body_markdown: "# Release Procedure\n".into(),
                    artifact_sha256: None,
                    edit_mode: None,
                    patch_json: None,
                    expected_base_body_sha256: None,
                }],
            })
            .await
            .unwrap();
        assert_eq!(staged.proposal_ids.len(), 1, "fixture must actually stage");
    }

    /// The unattended path has no response for anyone to read, so a proposal
    /// the store declines has exactly two places left to surface: the typed
    /// tick outcome's counter and the warning that names the target. The tick
    /// records each warned drop in `skipped_proposals`, set in the same arm
    /// that emits the warning, so the identity is asserted through the typed
    /// record instead of a log capture — without both surfaces, a run that
    /// lost its only proposal to a collision is byte-identical to a run that
    /// produced nothing.
    #[tokio::test]
    async fn a_scheduled_run_reports_a_collision_in_its_outcome() {
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
            .get_or_create_project(ws, "proj", None)
            .await
            .unwrap();
        let session_id = seed_reviewable_session(&store, ws, proj).await;
        stage_blocking_proposal(&store, ws, proj).await;

        let settings = ScheduledAutoImproveSettings {
            review: AutoImproveReviewConfig {
                // The fixture session is short and small; the preflight gates
                // are not what this test is about.
                min_observations: 3,
                min_session_duration_secs: 0,
                ..AutoImproveReviewConfig::default()
            },
            require_approval: true,
            min_session_age_secs: 0,
            max_sessions_per_tick: 10,
            experience: None,
        };
        let llm: Arc<dyn LlmProvider> = Arc::new(OneProposalLlm);
        let ctx = ScheduledAutoImproveContext {
            reader: &store.reader,
            writer: &store.writer,
            wiki: &wiki,
            llm: &llm,
            workspace_id: ws,
            project_id: proj,
            settings: &settings,
        };

        let run = run_scheduled_auto_improve(&ctx, session_id).await.unwrap();
        assert_eq!(run.proposals, 0, "the only proposal collided");
        assert_eq!(
            run.skipped.len(),
            1,
            "the outcome must carry the drop, not just the surviving count"
        );
        assert_eq!(run.skipped[0].target_path, COLLIDING_PATH);

        let tick_session = seed_reviewable_session(&store, ws, proj).await;
        let tick =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        assert_eq!(tick.errors, 0);
        assert!(
            tick.reviewed >= 1,
            "the new session must have been reviewed"
        );
        assert_eq!(tick.skipped, 1, "the tick must count the dropped proposal");
        assert_ne!(tick_session, session_id);
        assert_eq!(
            tick.skipped_proposals.len(),
            1,
            "the tick must record the warned drop, not just count it"
        );
        assert_eq!(
            tick.skipped_proposals[0].target_path, COLLIDING_PATH,
            "the warning must name the dropped target"
        );
    }
}
