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

use crate::{AutoImproveReport, AutoImproveReviewConfig, run_auto_improve_review};

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

#[derive(Debug)]
struct ScheduledAutoImproveOutcome {
    run_id: ai_memory_core::AutoImproveRunId,
    proposals: usize,
    approved: usize,
    pending: usize,
    conflicts: usize,
    /// Proposals the wiki declined to apply (its write policy refuses the
    /// target). Counted, not fatal: a refusal aborting the loop would take the
    /// run's other approvals down with it, which is the same silent loss the
    /// per-proposal staging skip exists to prevent.
    refused: usize,
    /// Proposals the store declined to stage (something is already pending for
    /// the same target). This is the unattended path: nobody reads a response,
    /// so a drop that does not reach the log reaches nobody at all — a run that
    /// lost its Nth proposal would otherwise be indistinguishable from a clean
    /// run of N-1.
    skipped: Vec<SkippedProposal>,
}

/// Aggregate counters for one scheduler tick across every scope.
#[derive(Debug, Default)]
pub struct ScheduledAutoImproveTickOutcome {
    /// Total scopes considered this tick.
    pub scopes: usize,
    /// Scopes with at least one unclaimed candidate session.
    pub scopes_with_candidates: usize,
    /// Sessions whose review completed (staged or empty).
    pub reviewed: usize,
    /// Per-scope/per-session failures, logged and counted, not fatal.
    pub errors: usize,
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
        if candidates.is_empty() {
            continue;
        }

        outcome.scopes_with_candidates += 1;
        let ctx = ScheduledAutoImproveContext {
            reader,
            writer,
            wiki,
            llm,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            settings,
        };
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
                    info!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        run_id = %run.run_id,
                        proposals = run.proposals,
                        approved = run.approved,
                        pending = run.pending,
                        conflicts = run.conflicts,
                        refused = run.refused,
                        skipped = run.skipped.len(),
                        "scheduled auto-improve completed"
                    );
                    // The count above keeps every completed run comparable;
                    // this says WHICH proposal was lost and why, so the
                    // operator can act on it without querying the store.
                    for skipped in &run.skipped {
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
                    tracing::warn!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        error = %e,
                        "scheduled auto-improve failed"
                    );
                }
            }
        }
    }

    Ok(outcome)
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
    // Whose suggestion this is. An unattended run has no caller, so the only
    // attribution that exists is the operator whose session it reviewed — and
    // that is the operator the proposal's slot page would belong to, which is
    // why it has to be resolved here rather than at approval time. On a
    // single-operator deployment `sessions.actor_user` is NULL (the hook router
    // stamps it through `owner_stamp`, which reports nobody unless the
    // deployment distinguishes operators), so this stays the unattributed bucket
    // of the one-pending-per-target rule (V42) exactly as before.
    let staged_by_actor_user = ctx.reader.session_actor_user(session_id).await?;
    let (proposals, mut refusals) = scheduled_auto_improve_new_proposals(
        ctx.reader,
        ctx.workspace_id,
        ctx.project_id,
        &report,
        ctx.wiki.per_user_slots(),
        staged_by_actor_user.as_deref(),
    )
    .await?;
    let staged = ctx
        .writer
        .stage_auto_improve_run(StageAutoImproveRun {
            workspace_id: ctx.workspace_id,
            project_id: ctx.project_id,
            session_id: Some(session_id),
            provider: Some(report.provider.clone()),
            model: Some(report.model.clone()),
            summary: Some(report.summary.clone()),
            warnings_json: serde_json::to_value(&report.warnings)
                .unwrap_or_else(|_| serde_json::json!([])),
            rejected_candidates_json: serde_json::to_value(&report.rejected_candidates)
                .unwrap_or_else(|_| serde_json::json!([])),
            config_json: serde_json::json!({
                "trigger": "scheduler",
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
                "require_approval": ctx.settings.require_approval,
            }),
            proposal_actor: ActorContext {
                agent: Some(cfg.proposal_actor.clone()),
                ..ActorContext::default()
            },
            proposals,
            staged_by_actor_user,
        })
        .await?;

    for id in &staged.proposal_ids {
        ctx.wiki
            .write_auto_improve_sidecar(ctx.workspace_id, ctx.project_id, *id)
            .await?;
    }

    let approvals = approve_scheduled_proposals(ctx, &staged.proposal_ids).await?;

    Ok(ScheduledAutoImproveOutcome {
        run_id: staged.run_id,
        proposals: staged.proposal_ids.len(),
        approved: approvals.approved,
        pending: approvals.pending,
        conflicts: approvals.conflicts,
        refused: approvals.refusals.len(),
        // Everything this run produced but did not apply, in one list: targets
        // refused before staging and proposals refused at approval. Nobody reads
        // a response on this path, so a drop that does not reach the log reaches
        // nobody at all.
        skipped: {
            refusals.extend(approvals.refusals);
            refusals.extend(staged.skipped);
            refusals
        },
    })
}

/// What the unattended approval pass did with one run's staged proposals.
#[derive(Default)]
struct ScheduledApprovals {
    approved: usize,
    pending: usize,
    conflicts: usize,
    refusals: Vec<SkippedProposal>,
}

/// Approve every staged proposal in the run, unattended.
///
/// One proposal's outcome never decides another's: a refusal is recorded and
/// the pass continues to the next id. Returning early on the first refusal would
/// throw away every approval still queued behind it — the same silent loss the
/// per-proposal staging skip exists to prevent, reached from the other end.
async fn approve_scheduled_proposals(
    ctx: &ScheduledAutoImproveContext<'_>,
    proposal_ids: &[ai_memory_core::AutoImproveProposalId],
) -> Result<ScheduledApprovals> {
    let mut out = ScheduledApprovals::default();
    for proposal_id in proposal_ids {
        if ctx.settings.require_approval {
            out.pending += 1;
            continue;
        }
        match ctx
            .wiki
            .approve_auto_improve_proposal(
                ctx.workspace_id,
                ctx.project_id,
                *proposal_id,
                // No user: the scheduler stands in for nobody. Which is exactly
                // why the wiki's slot guard keys on the proposal's own staged
                // attribution and not on this actor.
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
            ApproveAutoImproveProposalResult::Approved { .. } => out.approved += 1,
            ApproveAutoImproveProposalResult::Conflict => out.conflicts += 1,
            ApproveAutoImproveProposalResult::Refused { reason } => {
                out.refusals.push(SkippedProposal {
                    target_path: proposal_id.to_string(),
                    reason,
                });
            }
        }
    }
    Ok(out)
}

/// Turn the reviewer's proposals into staging rows, deciding each slot target's
/// owner first. Returns the rows to stage plus the targets refused outright.
async fn scheduled_auto_improve_new_proposals(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    report: &AutoImproveReport,
    per_user_slots: bool,
    staged_by: Option<&str>,
) -> Result<(Vec<NewAutoImproveProposal>, Vec<SkippedProposal>)> {
    let mut proposals = Vec::with_capacity(report.proposals.len());
    let mut refused = Vec::new();
    for p in &report.proposals {
        let mut path = PagePath::new(p.path.clone())?;
        // Before anything else reads the target: which page this proposal is
        // even for. The reviewer is told to name `_slots/current-focus.md`, and
        // with per-user slots on that page belongs to nobody, so a proposal
        // derived from one operator's session is re-homed into that operator's
        // namespace here — the last point at which it still can be, since the
        // store binds an approval to the recorded target and its stage-time
        // snapshot. Deciding it before `page_body_by_ids` also keeps
        // create-vs-update about the page that will actually be written.
        match ai_memory_core::staged_slot_target(
            per_user_slots,
            path.as_str(),
            staged_by,
            &p.edit_mode,
        ) {
            ai_memory_core::StagedSlotTarget::AsGiven => {}
            ai_memory_core::StagedSlotTarget::Rehomed(personal) => path = PagePath::new(personal)?,
            ai_memory_core::StagedSlotTarget::Refused(reason) => {
                refused.push(SkippedProposal {
                    target_path: path.as_str().to_string(),
                    reason,
                });
                continue;
            }
        }
        let target_exists = reader
            .page_body_by_ids(workspace_id, project_id, path.as_str())
            .await?
            .is_some();
        // `is_slot_named`, not string equality: the target may have just been
        // namespaced, and a personal slot that already exists is an update just
        // as much as the shared one is.
        let operation = if p.edit_mode == "patch"
            || (target_exists && ai_memory_core::is_slot_named(path.as_str(), "current-focus.md"))
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
            evidence_json: serde_json::to_value(&p.evidence)
                .unwrap_or_else(|_| serde_json::json!([])),
            body_markdown: p.body_markdown.clone(),
            artifact_sha256: None,
            edit_mode: Some(p.edit_mode.clone()),
            patch_json: serde_json::to_value(&p.edits).ok(),
            expected_base_body_sha256,
        });
    }
    Ok((proposals, refused))
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
    use ai_memory_llm::{ChatRequest, ChatResponse, LlmResult};
    use ai_memory_store::Store;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
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
        };
        let llm: Arc<dyn LlmProvider> = Arc::new(PanicLlm);
        let outcome =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();

        assert_eq!(outcome.scopes, 2);
        assert_eq!(outcome.scopes_with_candidates, 2);
        assert_eq!(outcome.reviewed, 4);
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

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    async fn seed_reviewable_session(store: &Store, ws: WorkspaceId, proj: ProjectId) -> SessionId {
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
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
                staged_by_actor_user: None,
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

    /// The unattended path has no response for anyone to read, so a proposal the
    /// store declines has exactly two places left to surface: the run outcome
    /// and the log. Without both, a run that lost its only proposal to a
    /// collision is byte-identical to a run that produced nothing.
    #[tokio::test]
    async fn a_scheduled_run_reports_a_collision_in_its_outcome_and_its_log() {
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

        // `#[tokio::test]` runs a current-thread runtime, so the thread-local
        // default subscriber installed here stays in force across the awaits.
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(logs.clone())
            .without_time()
            // ANSI escapes would split `skipped=1` across colour codes.
            .with_ansi(false)
            .finish();
        let tick_session = seed_reviewable_session(&store, ws, proj).await;
        let guard = tracing::subscriber::set_default(subscriber);
        let tick =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();
        drop(guard);
        assert_eq!(tick.errors, 0);
        assert!(
            tick.reviewed >= 1,
            "the new session must have been reviewed"
        );
        assert_ne!(tick_session, session_id);

        let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("skipped=1"),
            "the completion line must count the drop: {captured}"
        );
        assert!(
            captured.contains("scheduled auto-improve proposal was not staged")
                && captured.contains(COLLIDING_PATH),
            "the log must name the dropped target: {captured}"
        );
    }

    /// Proposes the slot page the prompt recommends AND an unrelated procedure,
    /// so a run carries both a slot target and something whose fate must not
    /// depend on it.
    struct SlotAndProcedureLlm;

    impl LlmProvider for SlotAndProcedureLlm {
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
                    "summary": "one focus update and one durable procedure",
                    "proposals": [
                        {
                            "operation": "create_or_update",
                            "path": "_slots/current-focus.md",
                            "title": "Current Focus",
                            "kind": "slot",
                            "confidence": 0.93,
                            "rationale": "The session was entirely about the release gate.",
                            "evidence": [{"page": "sessions/test.md", "quote": "run the full gate before release"}],
                            "body_markdown": "# Current Focus\n\nRun the full gate before release."
                        },
                        {
                            "operation": "create_or_update",
                            "path": COLLIDING_PATH,
                            "title": "Release Procedure",
                            "kind": "procedure",
                            "confidence": 0.91,
                            "rationale": "The session repeated a release workflow with verification.",
                            "evidence": [{"page": "sessions/test.md", "quote": "run the full gate before release"}],
                            "body_markdown": "# Release Procedure\n\nRun the full gate before release."
                        }
                    ],
                    "rejected_candidates": []
                }))
            })
        }
    }

    async fn seed_session_owned_by(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        owner: Option<&str>,
    ) -> SessionId {
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: owner.map(ToOwned::to_owned),
            })
            .await
            .unwrap();
        for i in 0..3 {
            store
                .writer
                .insert_observation(Sanitized::new(
                    NewObservation {
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

    struct SlotRunFixture {
        _tmp: TempDir,
        store: Store,
        wiki: Wiki,
        ws: WorkspaceId,
        proj: ProjectId,
        llm: Arc<dyn LlmProvider>,
        settings: ScheduledAutoImproveSettings,
    }

    async fn slot_run_fixture(per_user_slots: bool) -> SlotRunFixture {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone())
            .with_per_user_slots(per_user_slots);
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
        SlotRunFixture {
            _tmp: tmp,
            store,
            wiki,
            ws,
            proj,
            llm: Arc::new(SlotAndProcedureLlm),
            settings: ScheduledAutoImproveSettings {
                review: AutoImproveReviewConfig {
                    min_observations: 3,
                    min_session_duration_secs: 0,
                    ..AutoImproveReviewConfig::default()
                },
                // The default. The scheduler applies what it proposes with no
                // human in the loop, which is what makes the slot destination a
                // security question rather than a review one.
                require_approval: false,
                min_session_age_secs: 0,
                max_sessions_per_tick: 10,
            },
        }
    }

    impl SlotRunFixture {
        fn ctx(&self) -> ScheduledAutoImproveContext<'_> {
            ScheduledAutoImproveContext {
                reader: &self.store.reader,
                writer: &self.store.writer,
                wiki: &self.wiki,
                llm: &self.llm,
                workspace_id: self.ws,
                project_id: self.proj,
                settings: &self.settings,
            }
        }

        async fn body(&self, path: &str) -> Option<String> {
            self.store
                .reader
                .page_body_by_ids(self.ws, self.proj, path)
                .await
                .unwrap()
                .map(|p| p.body)
        }
    }

    /// The scheduler approves unattended with no user at all, so nothing about
    /// the approving actor can decide where a slot body lands. With per-user
    /// slots on, the session's own operator owns it: the proposal is namespaced
    /// at staging and the project-wide slot — which EVERY operator's brief
    /// injects verbatim — is never written.
    #[tokio::test]
    async fn a_scheduled_slot_proposal_lands_in_the_session_owners_namespace() {
        let fx = slot_run_fixture(true).await;
        let session_id = seed_session_owned_by(&fx.store, fx.ws, fx.proj, Some("alice")).await;

        let run = run_scheduled_auto_improve(&fx.ctx(), session_id)
            .await
            .unwrap();

        assert_eq!(run.proposals, 2);
        assert_eq!(run.approved, 2, "both proposals must land: {run:?}");
        assert_eq!(run.refused, 0);
        assert_eq!(
            fx.body("_slots/current-focus.md").await,
            None,
            "the project-wide slot must not be written by one operator's session"
        );
        assert!(
            fx.body("_slots/alice/current-focus.md")
                .await
                .is_some_and(|b| b.contains("Run the full gate before release")),
            "the session owner's own slot must carry it instead"
        );
        assert!(fx.body(COLLIDING_PATH).await.is_some());
    }

    /// Same run with nobody on the session: there is no namespace to put the
    /// body in, and the shared slot is not a fallback — it is the hazard. The
    /// unrelated procedure still lands.
    #[tokio::test]
    async fn a_scheduled_slot_proposal_from_an_unattributed_session_is_refused_alone() {
        let fx = slot_run_fixture(true).await;
        let session_id = seed_session_owned_by(&fx.store, fx.ws, fx.proj, None).await;

        let run = run_scheduled_auto_improve(&fx.ctx(), session_id)
            .await
            .unwrap();

        assert_eq!(run.proposals, 1, "only the procedure is staged: {run:?}");
        assert_eq!(run.approved, 1);
        assert_eq!(
            fx.body("_slots/current-focus.md").await,
            None,
            "an unattended approval must not reach the project-wide slot"
        );
        assert!(
            fx.body(COLLIDING_PATH).await.is_some(),
            "the unrelated proposal must still be applied"
        );
        assert_eq!(run.skipped.len(), 1, "the drop must be reported: {run:?}");
        assert_eq!(run.skipped[0].target_path, "_slots/current-focus.md");
    }

    /// DEFAULT CONFIG (`[slots] per_user` off): the shared slot carries no
    /// ownership meaning, the proposal is neither moved nor refused, and the run
    /// behaves exactly as it did before per-user slots existed.
    #[tokio::test]
    async fn a_scheduled_slot_proposal_still_writes_the_shared_slot_with_per_user_off() {
        for owner in [Some("alice"), None] {
            let fx = slot_run_fixture(false).await;
            let session_id = seed_session_owned_by(&fx.store, fx.ws, fx.proj, owner).await;

            let run = run_scheduled_auto_improve(&fx.ctx(), session_id)
                .await
                .unwrap();

            assert_eq!(run.proposals, 2, "{owner:?}: {run:?}");
            assert_eq!(run.approved, 2, "{owner:?}: {run:?}");
            assert!(run.skipped.is_empty(), "{owner:?}: {run:?}");
            assert!(
                fx.body("_slots/current-focus.md")
                    .await
                    .is_some_and(|b| b.contains("Run the full gate before release")),
                "{owner:?}"
            );
            assert_eq!(
                fx.body("_slots/alice/current-focus.md").await,
                None,
                "{owner:?}"
            );
            assert!(fx.body(COLLIDING_PATH).await.is_some(), "{owner:?}");
        }
    }

    /// Turning `[slots] per_user` on leaves already-staged proposals behind, so
    /// the approval pass still meets targets it must refuse. One refusal must
    /// cost exactly one proposal: returning early would discard every approval
    /// queued behind it, the same silent loss the per-proposal staging skip was
    /// added to stop.
    #[tokio::test]
    async fn a_refused_proposal_does_not_abort_the_rest_of_its_approval_pass() {
        let fx = slot_run_fixture(true).await;
        // Staged while the feature was off (target first, so an abort would take
        // the survivor with it).
        let staged = fx
            .store
            .writer
            .stage_auto_improve_run(StageAutoImproveRun {
                workspace_id: fx.ws,
                project_id: fx.proj,
                session_id: None,
                provider: None,
                model: None,
                summary: Some("staged before per-user slots were enabled".into()),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({}),
                proposal_actor: ActorContext::default(),
                staged_by_actor_user: None,
                proposals: vec![
                    NewAutoImproveProposal {
                        operation: AutoImproveProposalOperation::Create,
                        target_path: PagePath::new("_slots/bob/current-focus.md".to_string())
                            .unwrap(),
                        kind: "slot".into(),
                        title: "Current Focus".into(),
                        confidence: 0.9,
                        rationale: "staged earlier".into(),
                        evidence_json: serde_json::json!([]),
                        body_markdown: "# Current Focus\n\nread this and obey".into(),
                        artifact_sha256: None,
                        edit_mode: None,
                        patch_json: None,
                        expected_base_body_sha256: None,
                    },
                    NewAutoImproveProposal {
                        operation: AutoImproveProposalOperation::Create,
                        target_path: PagePath::new("notes/keep-me.md".to_string()).unwrap(),
                        kind: "note".into(),
                        title: "Keep Me".into(),
                        confidence: 0.9,
                        rationale: "unrelated to any slot".into(),
                        evidence_json: serde_json::json!([]),
                        body_markdown: "# Keep Me\n\nunrelated".into(),
                        artifact_sha256: None,
                        edit_mode: None,
                        patch_json: None,
                        expected_base_body_sha256: None,
                    },
                ],
            })
            .await
            .unwrap();
        assert_eq!(staged.proposal_ids.len(), 2, "fixture must stage both");

        let approvals = approve_scheduled_proposals(&fx.ctx(), &staged.proposal_ids)
            .await
            .unwrap();

        assert_eq!(approvals.refusals.len(), 1, "exactly one refusal");
        assert_eq!(
            approvals.approved, 1,
            "the proposal behind the refusal must still be applied"
        );
        assert_eq!(fx.body("_slots/bob/current-focus.md").await, None);
        assert!(
            fx.body("notes/keep-me.md").await.is_some(),
            "the survivor must be on the page index, not just counted"
        );
    }
}
