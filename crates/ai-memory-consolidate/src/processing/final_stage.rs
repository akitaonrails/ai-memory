//! Final-stage prompts, context and review assembly of chunked processing.
//!
//! The final call sees only the reduced, server-validated digest plus a
//! bounded slice of context (project preferences, the current page, titles,
//! slots, or the review pages). It answers in the existing structured types
//! (`ConsolidatedPage`, `ConsolidatedBatch`, `AutoImproveLlmResponse`), and
//! those outputs are then validated by the existing guards before any effect.

use std::collections::{BTreeMap, HashMap};

use ai_memory_core::{
    ActorContext, AgentKind, OwnerFilter, PagePath, ProcessingJobKind, ProjectId, SessionId,
    WorkspaceId,
};
use ai_memory_llm::{ChatMessage, ChatRequest, Role};
use ai_memory_store::{BriefingPage, ReaderPool};
use ai_memory_wiki::Wiki;
use serde::{Deserialize, Serialize};

use super::budget::{StageBudgets, final_prompt, fingerprint};
use super::chunk_digest::render_digest;
use super::chunk_pipeline::Output;
use super::chunk_plan::text_cost;
use crate::auto_improve::{
    AutoImproveError, AutoImproveEvalConfig, AutoImproveEvalResult, AutoImproveLlmResponse,
    AutoImproveProposal, AutoImproveRejectedCandidate, AutoImproveReport, AutoImproveReviewConfig,
    ExistingPageIndex, MAX_REJECTION_CONTEXT_CHARS, apply_eval_gate_with_before_bodies,
    estimate_tokens, eval_targets_path, load_patchable_pages, load_rejection_context,
    normalize_edit_mode, render_patchable_pages, render_recent_pages, render_rejection_context,
    validate_response,
};
use crate::consolidator::{
    Consolidator, ConsolidatorResult, render_current_body_section, render_instructions_block,
    render_slot_snapshots, render_title_uniqueness_section,
};
use crate::path_sanitize::slugify_page_path;
use crate::types::{ConsolidatedBatch, ConsolidatedPage};

/// The final call's persisted answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(super) enum FinalRecord {
    Page {
        page: ConsolidatedPage,
    },
    Batch {
        batch: ConsolidatedBatch,
    },
    Review {
        response: AutoImproveLlmResponse,
    },
    /// Persisted before the external eval command starts; without a matching
    /// [`FinalRecord::Evaluated`] the eval may have run.
    EvalStarted,
    /// The validated report and the server's eval observations, persisted
    /// right after the eval so a restart never re-runs it. The results are
    /// stored explicitly: the report type does not deserialize them.
    Evaluated {
        report: AutoImproveReport,
        eval_results: Vec<AutoImproveEvalResult>,
    },
}

impl FinalRecord {
    pub(super) fn evaluated(report: &AutoImproveReport) -> Self {
        let mut report = report.clone();
        let eval_results = std::mem::take(&mut report.eval_results);
        Self::Evaluated {
            report,
            eval_results,
        }
    }
}

/// Fit a rendering into `budget` cost by shrinking its character allowance.
pub(super) fn fit_cost(render: impl Fn(usize) -> String, budget: usize) -> String {
    let mut chars = budget;
    for _ in 0..8 {
        if chars == 0 {
            break;
        }
        let out = render(chars);
        let cost = text_cost(&out);
        if cost <= budget {
            return out;
        }
        chars = (chars.saturating_mul(budget) / cost.max(1)).saturating_sub(1);
    }
    String::new()
}

fn clip_lines(text: &str, budget: usize) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if text_cost(&out) + text_cost(line) + 1 > budget {
            out.push_str("[list truncated]\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Digest groups as the reduce and final prompts show them.
pub(super) fn render_groups(outputs: &[Output]) -> String {
    let mut out = String::new();
    for (i, o) in outputs.iter().enumerate() {
        out.push_str(&format!(
            "== g{} (observations o{}-o{}) ==\n{}",
            i + 1,
            o.lo,
            o.hi,
            render_digest(&o.digest)
        ));
    }
    out
}

fn one_request(system: &str, user: String, max_tokens: u32) -> ChatRequest {
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

/// Context of a consolidation final call.
pub(super) struct ConsolidateContext {
    pub agent_kind: AgentKind,
    pub anchor: PagePath,
    pub existing_titles: Vec<String>,
    /// Instructions, current body or slots, and titles, already budgeted.
    pub blocks: String,
    pub hash: String,
    /// Latest page id per path, captured with the content the final call
    /// sees (never afterwards): `None` = the page was absent.
    pub bases: BTreeMap<String, Option<String>>,
}

/// Read the bounded context a consolidation final call needs.
///
/// # Errors
/// Store, scope or wiki failures.
#[allow(clippy::too_many_arguments)]
pub(super) async fn gather_consolidate_context(
    legacy: &Consolidator,
    wiki: &Wiki,
    reader: &ReaderPool,
    ws: WorkspaceId,
    proj: ProjectId,
    session_id: SessionId,
    multi: bool,
    actor: &ActorContext,
    instructions: Option<&str>,
    cap: usize,
) -> ConsolidatorResult<ConsolidateContext> {
    let agent_kind = legacy.resolve_agent_origin(session_id).await?;
    let anchor = PagePath::new(format!("sessions/{session_id}.md"))?;
    let instructions = legacy.resolve_instructions(ws, proj, instructions).await;
    let titles = legacy
        .existing_page_titles(ws, proj, actor, session_id)
        .await;
    let instructions_block = fit_cost(
        |n| render_instructions_block(instructions.as_deref(), n),
        cap / 4,
    );
    let titles_block = fit_cost(|n| render_title_uniqueness_section(&titles, n), cap / 4);
    let rest = cap
        .saturating_sub(text_cost(&instructions_block))
        .saturating_sub(text_cost(&titles_block));
    let latest = |path: String| async move {
        reader
            .latest_page_id_by_ids(ws, proj, path)
            .await
            .map(|id| id.map(|id| id.to_string()))
            .map_err(|e| crate::consolidator::ConsolidatorError::Processing(e.to_string()))
    };
    let mut bases: BTreeMap<String, Option<String>> = BTreeMap::new();
    let middle = if multi {
        // The anchor page is part of the batch's target set (hooks have
        // already created it), so its id and body are captured like the
        // slots': ids are read around the bodies and must not move, so each
        // id belongs to the rendered content or the read is repeated.
        let anchor_key = anchor.as_str().to_string();
        let mut stable = None;
        for _ in 0..3 {
            let anchor_id = latest(anchor_key.clone()).await?;
            let anchor_body = wiki
                .read_page(ws, proj, &anchor)
                .map(|md| md.body)
                .unwrap_or_default();
            let first = legacy.slot_snapshots(ws, proj, actor).await?;
            let mut ids = BTreeMap::new();
            for slot in &first {
                ids.insert(slot.path.clone(), latest(slot.path.clone()).await?);
            }
            let second = legacy.slot_snapshots(ws, proj, actor).await?;
            let anchor_after = latest(anchor_key.clone()).await?;
            let anchor_block = fit_cost(|n| render_current_body_section(&anchor_body, n), rest / 2);
            let slot_budget = rest.saturating_sub(text_cost(&anchor_block));
            let a = fit_cost(|n| render_slot_snapshots(&first, n), slot_budget);
            let b = fit_cost(|n| render_slot_snapshots(&second, n), slot_budget);
            if a == b && anchor_id == anchor_after {
                ids.insert(anchor_key.clone(), anchor_id);
                stable = Some((format!("{anchor_block}{a}"), ids));
                break;
            }
        }
        let (rendered, ids) = stable.ok_or_else(|| {
            crate::consolidator::ConsolidatorError::Processing(
                "the target pages kept changing while the context was read".into(),
            )
        })?;
        bases = ids;
        rendered
    } else {
        // The id first, the body after: a commit in between leaves the base
        // older than the body, so the guarded write conflicts, never clobbers.
        bases.insert(
            anchor.as_str().to_string(),
            latest(anchor.as_str().to_string()).await?,
        );
        let body = wiki
            .read_page(ws, proj, &anchor)
            .map(|md| md.body)
            .unwrap_or_default();
        fit_cost(|n| render_current_body_section(&body, n), rest)
    };
    let blocks = format!("{instructions_block}{middle}{titles_block}");
    let hash = fingerprint(&[
        &blocks,
        &titles.join("\n"),
        &format!("{agent_kind:?}"),
        &format!("{bases:?}"),
    ]);
    Ok(ConsolidateContext {
        agent_kind,
        anchor,
        existing_titles: titles,
        blocks,
        hash,
        bases,
    })
}

/// The final consolidation request.
pub(super) fn consolidate_request(
    kind: ProcessingJobKind,
    ctx: &ConsolidateContext,
    session_id: SessionId,
    outputs: &[Output],
    total_observations: u32,
    budgets: &StageBudgets,
) -> ChatRequest {
    let (system, _) = final_prompt(kind);
    let last = total_observations.saturating_sub(1);
    let user = format!(
        "Session id: {session_id}\nSession page path: {}\n{}\n\
         Verified digest of the whole session ({total_observations} observations, refs o0-o{last}); \
         every ref was checked by the server:\n{}",
        ctx.anchor,
        ctx.blocks,
        render_groups(outputs)
    );
    one_request(system, user, budgets.final_out_tokens)
}

/// Context of an auto-improve final call.
pub(super) struct ReviewContext {
    recent_text: String,
    patchable_text: String,
    rejection_text: String,
    pub warnings: Vec<String>,
    pub existing_index: ExistingPageIndex,
    pub hash: String,
}

/// Read the bounded review context.
///
/// # Errors
/// Store errors.
pub(super) async fn gather_review_context(
    reader: &ReaderPool,
    ws: WorkspaceId,
    proj: ProjectId,
    cfg: &AutoImproveReviewConfig,
    cap: usize,
) -> Result<ReviewContext, AutoImproveError> {
    let briefing = reader
        .briefing_for_project(ws, proj, 100, OwnerFilter::Any, false)
        .await?;
    // Session pages are never review targets: they come from consolidation.
    let recent_pages: Vec<BriefingPage> = briefing
        .recent_pages
        .iter()
        .filter(|page| !page.path.starts_with("sessions/"))
        .cloned()
        .collect();
    let patchable = load_patchable_pages(reader, ws, proj, &recent_pages, cfg).await?;
    let rejections = load_rejection_context(reader, ws, proj, cfg).await?;
    let recent_text = clip_lines(&render_recent_pages(&recent_pages), cap / 4);
    let mut warnings = Vec::new();
    // The legacy renderers budget in bytes, which can undercount a cost that
    // charges non-ASCII characters three: stay two thirds inside the cap.
    let rendered = render_patchable_pages(
        &patchable,
        cfg.max_patchable_body_chars,
        cap / 2 * 2 / 3,
        &mut warnings,
    );
    let rejection_text = render_rejection_context(
        &rejections,
        (cap / 4 * 2 / 3).min(MAX_REJECTION_CONTEXT_CHARS),
    );
    let included: Vec<_> = patchable
        .iter()
        .filter(|p| rendered.included_paths.contains(&p.path))
        .cloned()
        .collect();
    let existing_index = ExistingPageIndex::from_pages(&recent_pages, &included);
    // Everything the review's validation and eval read, not only the clipped
    // text: the whole recent listing (the index's paths and titles) and the
    // exact version and body hash of every patchable page a patch is
    // materialized against.
    let mut listing: Vec<String> = recent_pages
        .iter()
        .map(|p| format!("{}\0{}", p.path, p.title))
        .collect();
    listing.sort();
    let bases: Vec<String> = included
        .iter()
        .map(|p| {
            let at = recent_pages
                .iter()
                .find(|r| r.path == p.path)
                .map_or("", |r| r.updated_at.as_str());
            format!("{}\0{}\0{at}", p.path, p.body_sha256)
        })
        .collect();
    let hash = fingerprint(&[
        &recent_text,
        &rendered.text,
        &rejection_text,
        &listing.join("\n"),
        &bases.join("\n"),
    ]);
    Ok(ReviewContext {
        recent_text,
        patchable_text: rendered.text,
        rejection_text,
        warnings,
        existing_index,
        hash,
    })
}

/// The final auto-improve request.
pub(super) fn review_request(
    cfg: &AutoImproveReviewConfig,
    ctx: &ReviewContext,
    session_id: SessionId,
    outputs: &[Output],
    total_observations: u32,
    duration_secs: u64,
    budgets: &StageBudgets,
) -> ChatRequest {
    let (system, _) = final_prompt(ProcessingJobKind::AutoImprove);
    let last = total_observations.saturating_sub(1);
    let user = format!(
        "Review one completed ai-memory session and propose only durable wiki edits.\n\
         Session: {session_id}\n\
         Observations covered: {total_observations} (refs o0-o{last}; the verified digest below covers all of them)\n\
         Session duration seconds: {duration_secs}\n\
         Minimum confidence: {}\nMax proposals: {}\nInclude raw fallback: {}\n\
         Pending proposal sidecar path: {}\n\
         Limits: max edits/proposal {}, max patch edits/run {}, max content chars/edit {}, \
         max changed chars/proposal {}, max final body chars {}, max rule page tokens {}, \
         max procedure page tokens {}.\n\n\
         Existing project pages (full-page proposals must not target these paths or titles, except _slots/current-focus.md):\n{}\n\
         Patchable existing targets (patch mode only for these listed paths, exact heading anchors):\n{}\n\n\
         Recent rejected attempts (avoid repeating unless the session has clearly new evidence):\n{}\n\n\
         Verified digest of the whole session; every ref was checked by the server:\n{}",
        cfg.min_confidence,
        cfg.max_proposals_per_run,
        cfg.include_raw_fallback,
        cfg.pending_path,
        cfg.max_edits_per_proposal,
        cfg.max_patch_edits_per_run,
        cfg.max_edit_content_chars,
        cfg.max_changed_chars_per_proposal,
        cfg.max_final_body_chars,
        cfg.max_rule_page_tokens,
        cfg.max_procedure_page_tokens,
        ctx.recent_text,
        ctx.patchable_text,
        ctx.rejection_text,
        render_groups(outputs),
    );
    one_request(system, user, budgets.final_out_tokens)
}

/// One rejected candidate, recorded rather than dropped.
fn rejection(
    reason: &str,
    evidence: String,
    p: &AutoImproveProposal,
) -> AutoImproveRejectedCandidate {
    AutoImproveRejectedCandidate {
        reason: reason.into(),
        evidence,
        target_path: Some(p.path.clone()),
        kind: Some(p.kind.clone()),
        operation: Some(p.operation.clone()),
        edit_mode: Some(p.edit_mode.clone()),
    }
}

/// Why the validator would refuse this one proposal on its own.
fn verdict(
    p: &AutoImproveProposal,
    cfg: &AutoImproveReviewConfig,
    index: &ExistingPageIndex,
) -> Result<(), String> {
    let one = AutoImproveLlmResponse {
        summary: "check".into(),
        proposals: vec![p.clone()],
        rejected_candidates: Vec::new(),
    };
    let (accepted, mut rejected, _) = validate_response(one, cfg, index);
    if accepted.is_empty() {
        Err(rejected.pop().map(|r| r.reason).unwrap_or_default())
    } else {
        Ok(())
    }
}

/// Two patch proposals of one base as one: their edits in order, both
/// rationales and evidence, and the weaker confidence. `None` when either is
/// not a patch (a page cannot be both replaced and patched).
fn combine(a: &AutoImproveProposal, b: &AutoImproveProposal) -> Option<AutoImproveProposal> {
    if normalize_edit_mode(&a.edit_mode) != "patch" || normalize_edit_mode(&b.edit_mode) != "patch"
    {
        return None;
    }
    let mut merged = a.clone();
    merged.confidence = a.confidence.min(b.confidence);
    merged.edits.extend(b.edits.iter().cloned());
    let extra = b.rationale.trim();
    if !extra.is_empty() && !merged.rationale.contains(extra) {
        merged.rationale = format!("{}; {extra}", merged.rationale.trim_end());
    }
    for e in &b.evidence {
        if !merged
            .evidence
            .iter()
            .any(|x| x.page == e.page && x.quote == e.quote)
        {
            merged.evidence.push(e.clone());
        }
    }
    Some(merged)
}

/// One proposal per target path, so caps and the eval gate count each target
/// once. Candidates of a target that each pass validation alone are combined
/// when they are patches of the same base and the combination still passes;
/// every other candidate is rejected with a reason, never dropped silently.
/// Candidates that fail alone are left for the validator to reject.
fn merge_targets(
    raw: &mut AutoImproveLlmResponse,
    cfg: &AutoImproveReviewConfig,
    index: &ExistingPageIndex,
) {
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<AutoImproveProposal>> = BTreeMap::new();
    for p in std::mem::take(&mut raw.proposals) {
        let key = slugify_page_path(p.path.trim()).to_lowercase();
        groups
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push(p);
    }
    for key in order {
        let mut group = groups.remove(&key).unwrap_or_default();
        if group.len() == 1 {
            raw.proposals.append(&mut group);
            continue;
        }
        group.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        let mut acc: Option<AutoImproveProposal> = None;
        let mut invalid = Vec::new();
        for c in group {
            if verdict(&c, cfg, index).is_err() {
                invalid.push(c);
                continue;
            }
            let Some(a) = acc.take() else {
                acc = Some(c);
                continue;
            };
            acc = Some(match combine(&a, &c) {
                None => {
                    let why = format!("{}: incompatible edit modes or duplicate full page", c.path);
                    raw.rejected_candidates
                        .push(rejection("duplicate_target", why, &c));
                    a
                }
                Some(merged) => match verdict(&merged, cfg, index) {
                    Ok(()) => merged,
                    Err(reason) => {
                        let why = format!("{}: {reason}", c.path);
                        raw.rejected_candidates
                            .push(rejection("patch_merge_conflict", why, &c));
                        a
                    }
                },
            });
        }
        raw.proposals.extend(acc);
        raw.proposals.extend(invalid);
    }
}

/// The validated review before its eval gate runs.
///
/// The gate may start the operator's external command, which has effects of
/// its own, so the caller persists a marker before [`ReviewDraft::finish`]
/// and the result right after it (see [`eval_progress`]).
pub(super) struct ReviewDraft {
    report: AutoImproveReport,
    /// Bodies the eval compares against, fixed here and never re-read.
    before_bodies: BTreeMap<String, String>,
    external_eval: bool,
}

impl ReviewDraft {
    /// Whether finishing starts the external eval command.
    pub(super) fn runs_external_eval(&self) -> bool {
        self.external_eval
    }

    /// Run the eval gate once and complete the report.
    pub(super) async fn finish(mut self, eval: &AutoImproveEvalConfig) -> AutoImproveReport {
        let r = &mut self.report;
        r.eval_results = apply_eval_gate_with_before_bodies(
            eval,
            &mut r.proposals,
            &mut r.rejected_candidates,
            &mut r.warnings,
            &self.before_bodies,
        )
        .await;
        r.summary = if r.proposals.is_empty() {
            "review completed; no validated proposals".into()
        } else {
            format!(
                "review completed; {} proposal(s) validated",
                r.proposals.len()
            )
        };
        self.report
    }
}

/// Validate the final response once for the whole session: candidates are
/// merged per target, caps apply to the merged proposals, and the eval's
/// before-bodies are fixed (the context's own bodies for patched pages).
///
/// # Errors
/// Store failures reading a before-body.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_review(
    reader: &ReaderPool,
    provider: &str,
    model: &str,
    ws: WorkspaceId,
    proj: ProjectId,
    session_id: SessionId,
    cfg: &AutoImproveReviewConfig,
    ctx: &ReviewContext,
    mut raw: AutoImproveLlmResponse,
    total_observations: usize,
    duration_secs: u64,
    estimated_input_text: &str,
) -> Result<ReviewDraft, AutoImproveError> {
    merge_targets(&mut raw, cfg, &ctx.existing_index);
    let (proposals, rejected, mut warnings) = validate_response(raw, cfg, &ctx.existing_index);
    warnings.extend(ctx.warnings.iter().cloned());
    let mut before_bodies = BTreeMap::new();
    if cfg.eval.enabled {
        for p in proposals
            .iter()
            .filter(|p| eval_targets_path(&cfg.eval, &p.path))
        {
            let body = match ctx.existing_index.patchable_body(&p.path) {
                Some(body) => body.to_string(),
                None => reader
                    .page_body_by_ids(ws, proj, &p.path)
                    .await?
                    .map(|page| page.body)
                    .unwrap_or_default(),
            };
            before_bodies.insert(p.path.clone(), body);
        }
    }
    let external_eval = !cfg.eval.command.trim().is_empty() && !before_bodies.is_empty();
    Ok(ReviewDraft {
        report: AutoImproveReport {
            session_id: session_id.to_string(),
            observations_considered: total_observations,
            session_duration_secs: duration_secs,
            estimated_input_tokens: estimate_tokens(estimated_input_text),
            provider: provider.to_string(),
            model: model.to_string(),
            min_confidence: cfg.min_confidence,
            proposal_actor: cfg.proposal_actor.clone(),
            pending_path: cfg.pending_path.clone(),
            summary: String::new(),
            proposals,
            rejected_candidates: rejected,
            warnings,
            eval_results: Vec::new(),
        },
        before_bodies,
        external_eval,
    })
}

/// Node key of the marker persisted before the external eval starts.
pub(super) fn started_key(key: &str) -> String {
    format!("{key}:s")
}

const REPORT_SUFFIX: &str = ":r";

/// Node key of the report persisted right after the eval returned.
pub(super) fn report_key(key: &str) -> String {
    format!("{key}{REPORT_SUFFIX}")
}

/// Where a job's external eval stands, from its persisted final nodes.
pub(super) enum EvalProgress {
    /// No eval was started: build the report.
    Fresh,
    /// An eval started and left no result. It may have run, so it is never
    /// repeated automatically.
    Started,
    /// The eval finished: publish this report. `key` is the final's key.
    Done {
        key: String,
        report: Box<AutoImproveReport>,
    },
}

/// Read the eval state back from persisted nodes. The eval results come from
/// the trusted stored record, since the report type never deserializes them.
pub(super) fn eval_progress(finals: &HashMap<String, FinalRecord>) -> EvalProgress {
    let mut done: Option<(&String, &AutoImproveReport, &Vec<AutoImproveEvalResult>)> = None;
    let mut started = false;
    for (key, record) in finals {
        match record {
            FinalRecord::Evaluated {
                report,
                eval_results,
            } if done.is_none_or(|(k, _, _)| key < k) => {
                done = Some((key, report, eval_results));
            }
            FinalRecord::EvalStarted => started = true,
            _ => {}
        }
    }
    match done {
        Some((key, report, eval_results)) => {
            let mut report = report.clone();
            report.eval_results = eval_results.clone();
            EvalProgress::Done {
                key: key.strip_suffix(REPORT_SUFFIX).unwrap_or(key).to_string(),
                report: Box::new(report),
            }
        }
        None if started => EvalProgress::Started,
        None => EvalProgress::Fresh,
    }
}

/// The report of a session the preflight filters skipped (no LLM call).
pub(super) fn skipped_report(
    session_id: SessionId,
    cfg: &AutoImproveReviewConfig,
    total: usize,
    duration_secs: u64,
    rejection: AutoImproveRejectedCandidate,
) -> AutoImproveReport {
    AutoImproveReport {
        session_id: session_id.to_string(),
        observations_considered: total,
        session_duration_secs: duration_secs,
        estimated_input_tokens: 0,
        provider: "none".into(),
        model: "none".into(),
        min_confidence: cfg.min_confidence,
        proposal_actor: cfg.proposal_actor.clone(),
        pending_path: cfg.pending_path.clone(),
        summary: "session skipped by preflight filters".into(),
        proposals: Vec::new(),
        rejected_candidates: vec![rejection],
        warnings: Vec::new(),
        eval_results: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_improve::{AutoImproveEvalConfig, PatchablePageContext};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    fn proposal(path: &str, confidence: f32) -> AutoImproveProposal {
        serde_json::from_value(json!({
            "path": path, "title": "t", "kind": "gotcha", "confidence": confidence,
            "rationale": "why",
            "evidence": [{"page": "sessions/s.md", "quote": "q"}],
            "body_markdown": "# t\n\nbody"
        }))
        .unwrap()
    }

    const BASE: &str = "# Release\n\n## Steps\nOne\n\n## Verify\nCheck\n";

    fn index() -> ExistingPageIndex {
        let page = PatchablePageContext::new(
            "procedures/release.md".into(),
            "Release".into(),
            "procedure".into(),
            BASE.into(),
        );
        ExistingPageIndex::from_pages(&[], &[page])
    }

    fn patch(
        op: &str,
        anchor: &str,
        content: &str,
        section: Option<&str>,
        quote: &str,
    ) -> AutoImproveProposal {
        serde_json::from_value(json!({
            "path": "procedures/release.md", "title": "Release", "kind": "procedure",
            "confidence": 0.9, "rationale": format!("because {quote}"),
            "evidence": [{"page": "sessions/s.md", "quote": quote}],
            "edit_mode": "patch",
            "edits": [{"op": op, "anchor": anchor, "content": content,
                       "section_sha256": section.map(|s| format!("{:x}", Sha256::digest(s.as_bytes())))}]
        }))
        .unwrap()
    }

    fn raw(proposals: Vec<AutoImproveProposal>) -> AutoImproveLlmResponse {
        AutoImproveLlmResponse {
            summary: "s".into(),
            proposals,
            rejected_candidates: Vec::new(),
        }
    }

    fn review_ctx() -> ReviewContext {
        ReviewContext {
            recent_text: String::new(),
            patchable_text: String::new(),
            rejection_text: String::new(),
            warnings: Vec::new(),
            existing_index: index(),
            hash: String::new(),
        }
    }

    #[test]
    fn two_full_page_candidates_for_one_target_keep_the_more_confident_and_reject_the_other() {
        let mut r = raw(vec![
            proposal("gotchas/a.md", 0.9),
            proposal("gotchas/a.md", 0.95),
            proposal("gotchas/b.md", 0.9),
        ]);
        for candidate in &r.proposals {
            verdict(candidate, &AutoImproveReviewConfig::default(), &index()).unwrap();
        }
        merge_targets(&mut r, &AutoImproveReviewConfig::default(), &index());
        assert_eq!(r.proposals.len(), 2);
        assert_eq!(r.proposals[0].confidence, 0.95);
        assert_eq!(r.rejected_candidates.len(), 1);
        assert_eq!(r.rejected_candidates[0].reason, "duplicate_target");
    }

    #[test]
    fn disjoint_patches_of_one_page_merge_into_one_proposal_with_both_edits() {
        let cfg = AutoImproveReviewConfig::default();
        let mut r = raw(vec![
            patch("append", "## Steps", "Two", None, "first"),
            patch("append", "## Verify", "Run it", None, "second"),
        ]);
        merge_targets(&mut r, &cfg, &index());
        assert_eq!(r.proposals.len(), 1);
        assert!(r.rejected_candidates.is_empty());
        assert_eq!(r.proposals[0].edits.len(), 2);
        assert_eq!(r.proposals[0].evidence.len(), 2);
        let (accepted, rejected, _) = validate_response(r, &cfg, &index());
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(accepted.len(), 1);
        assert!(accepted[0].body_markdown.contains("Two"));
        assert!(accepted[0].body_markdown.contains("Run it"));
    }

    #[test]
    fn conflicting_patches_keep_the_first_and_reject_the_other_explicitly() {
        let cfg = AutoImproveReviewConfig::default();
        let steps = "## Steps\nOne\n\n";
        let mut r = raw(vec![
            patch(
                "replace_section",
                "## Steps",
                "## Steps\nA\n",
                Some(steps),
                "first",
            ),
            {
                let mut p = patch(
                    "replace_section",
                    "## Steps",
                    "## Steps\nB\n",
                    Some(steps),
                    "second",
                );
                p.confidence = 0.8;
                p
            },
        ]);
        merge_targets(&mut r, &cfg, &index());
        assert_eq!(r.proposals.len(), 1);
        assert_eq!(r.proposals[0].edits.len(), 1);
        assert_eq!(r.rejected_candidates.len(), 1);
        assert_eq!(r.rejected_candidates[0].reason, "patch_merge_conflict");
        assert!(
            r.rejected_candidates[0]
                .evidence
                .contains("replace_section_hash_mismatch")
        );
    }

    #[test]
    fn a_patch_and_a_full_page_for_one_target_are_incompatible() {
        let cfg = AutoImproveReviewConfig::default();
        let mut full = proposal("procedures/release.md", 0.8);
        full.kind = "procedure".into();
        let mut r = raw(vec![
            patch("append", "## Steps", "Two", None, "first"),
            full,
        ]);
        merge_targets(&mut r, &cfg, &index());
        assert_eq!(r.proposals.len(), 1);
        assert_eq!(r.rejected_candidates.len(), 1);
        assert_eq!(r.rejected_candidates[0].reason, "duplicate_target");
    }

    /// The eval command is an external effect: two disjoint edits of one page
    /// are evaluated once, and the persisted result is what a restart reuses.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_eval_command_runs_once_for_the_merged_proposal_and_its_result_is_reusable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("count");
        let script = dir.path().join("eval.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat >/dev/null\necho run >> {}\n\
                 echo '{{\"passed\":true,\"score_before\":0.5,\"score_after\":0.9}}'\n",
                counter.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = AutoImproveReviewConfig {
            eval: AutoImproveEvalConfig {
                enabled: true,
                command: script.display().to_string(),
                ..AutoImproveEvalConfig::default()
            },
            ..AutoImproveReviewConfig::default()
        };
        let store = ai_memory_store::Store::open(dir.path()).unwrap();
        async fn prepare(reader: &ReaderPool, cfg: &AutoImproveReviewConfig) -> ReviewDraft {
            prepare_review(
                reader,
                "p",
                "m",
                WorkspaceId::new(),
                ProjectId::new(),
                SessionId::new(),
                cfg,
                &review_ctx(),
                raw(vec![
                    patch("append", "## Steps", "Two", None, "first"),
                    patch("append", "## Verify", "Run it", None, "second"),
                ]),
                3,
                100,
                "prompt",
            )
            .await
            .unwrap()
        }

        let draft = prepare(&store.reader, &cfg).await;
        assert!(draft.runs_external_eval());
        let report = draft.finish(&cfg.eval).await;
        let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs, 1, "one proposal, one eval");
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].edits.len(), 2);
        assert_eq!(report.eval_results.len(), 1);

        // Restart: the stored record gives the report back with its eval
        // results, and nothing runs again.
        let stored = serde_json::to_value(FinalRecord::evaluated(&report)).unwrap();
        let mut finals = HashMap::new();
        finals.insert(started_key("f:k"), FinalRecord::EvalStarted);
        finals.insert(report_key("f:k"), serde_json::from_value(stored).unwrap());
        match eval_progress(&finals) {
            EvalProgress::Done { key, report } => {
                assert_eq!(key, "f:k");
                assert_eq!(report.eval_results.len(), 1);
                assert_eq!(report.proposals.len(), 1);
            }
            _ => panic!("a stored result is reused"),
        }
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1
        );

        // A start without a result is refused, not repeated.
        finals.remove(&report_key("f:k"));
        assert!(matches!(eval_progress(&finals), EvalProgress::Started));
        // Control: nothing started, nothing to refuse.
        assert!(matches!(
            eval_progress(&HashMap::new()),
            EvalProgress::Fresh
        ));
        // Control: no external command, no marker needed.
        cfg.eval.command.clear();
        assert!(!prepare(&store.reader, &cfg).await.runs_external_eval());
    }

    #[test]
    fn fit_cost_shrinks_a_multibyte_rendering_under_its_budget() {
        let text = "ação 🚀 ".repeat(500);
        let out = fit_cost(|n| text.chars().take(n).collect(), 400);
        assert!(text_cost(&out) <= 400);
        assert!(!out.is_empty());
        assert_eq!(fit_cost(|_| "x".repeat(10), 0), "");
    }

    #[test]
    fn clip_lines_keeps_whole_lines_and_marks_the_cut() {
        let text = (0..50)
            .map(|i| format!("- page {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = clip_lines(&text, 60);
        assert!(out.ends_with("[list truncated]\n"));
        assert!(out.starts_with("- page 0\n"));
    }
}
