//! Oracle for the bounded session-observation load (equivalence tests).
//!
//! This file holds verbatim copies of the pre-change auto-improve prompt
//! path, `build_prompt_input`, `preflight_rejection` and
//! `session_duration_secs` as of the pre-change commit, byte-identical apart
//! from the renamed entry points, so the equivalence tests in
//! `tests/suite/bounded_session_load.rs` can compare the old full-load path
//! against the bounded-snapshot path on the same data. The projection the
//! builder calls (`project_observations` and its config) is the frozen base
//! copy (`crate::consolidator::frozen_projection_oracle`), not the live
//! `crate::projection`: the oracles must not share the rule under test. The
//! other shared helpers it calls are the current ones.

use super::*;
use crate::consolidator::frozen_projection_oracle::{
    ObservationProjectionConfig, project_observations,
};
use ai_memory_core::Observation;

/// Old prompt builder, copied verbatim (pre-change).
// The signature mirrors the pre-change production builder verbatim, which
// took eight arguments; the lint is waived rather than reshaped.
#[allow(clippy::too_many_arguments)]
pub(crate) fn oracle_build_prompt_input(
    session_id: SessionId,
    observations: &[Observation],
    duration_secs: u64,
    session_page: Option<&StoredPageBody>,
    recent_pages: &[BriefingPage],
    patchable_pages: &[PatchablePageContext],
    rejection_context: &[AutoImproveRejectionSummary],
    cfg: &AutoImproveReviewConfig,
) -> PromptInput {
    let mut warnings = Vec::new();
    let mut rejected_candidates = Vec::new();
    // Shares production's budget helper so the equivalence tests keep comparing
    // the bounded projection on an identical input budget; the frozen part of
    // this oracle is the projection, not the token arithmetic.
    let usable_chars = usable_input_chars(cfg.max_input_tokens);

    let recent = render_recent_pages(recent_pages);
    let patchable_budget = usable_chars / 4;
    let patchable = render_patchable_pages(
        patchable_pages,
        cfg.max_patchable_body_chars,
        patchable_budget,
        &mut warnings,
    );
    let session_page_budget = session_page
        .map(|_| usable_chars / 3)
        .unwrap_or(0)
        .min(MAX_SESSION_PAGE_CHARS);
    let session_page_section =
        render_session_page(session_page, session_page_budget, &mut warnings);
    let rejection_budget = rejection_context_char_budget(usable_chars);
    let rejection_text = render_rejection_context(rejection_context, rejection_budget);
    let observation_budget = usable_chars
        .saturating_sub(recent.len())
        .saturating_sub(patchable.text.len())
        .saturating_sub(rejection_text.len())
        .saturating_sub(session_page_section.len())
        .saturating_sub(PROMPT_SCAFFOLD_RESERVE_CHARS);
    let selected_limit = if session_page.is_some() {
        SAMPLE_LIMIT_WITH_SESSION_PAGE
    } else {
        SAMPLE_LIMIT_WITHOUT_SESSION_PAGE
    };
    let projected_observations = project_observations(
        observations,
        &ObservationProjectionConfig::new(
            observation_budget,
            selected_limit,
            MAX_OBSERVATION_BODY_CHARS,
        )
        .with_context_label("auto-improve"),
    );
    warnings.extend(projected_observations.warnings.iter().cloned());
    let rendered_observations = projected_observations.text;
    let selected_count = projected_observations.selected_count;
    let patchable_text = &patchable.text;

    if selected_count < observations.len() {
        rejected_candidates.push(AutoImproveRejectedCandidate {
            reason: "input_budget_sampled".into(),
            evidence: format!(
                "selected {selected_count} of {} observations for scalable review",
                observations.len()
            ),
            target_path: None,
            kind: None,
            operation: None,
            edit_mode: None,
        });
    }

    let prompt = format!(
        "Review one completed ai-memory session and propose only durable wiki edits.\n\
         Session: {session_id}\n\
         Observation count: {}\n\
         Session duration seconds: {duration_secs}\n\
         Minimum confidence: {}\n\
         Max proposals: {}\n\
         Include raw fallback: {}\n\
         Pending proposal sidecar path: {}\n\n\
         Existing project pages, for duplicate avoidance. Full-page proposals must not target these existing paths or titles except _slots/current-focus.md:\n{recent}\n\n\
         Patchable existing target context. Patch mode is allowed only for these listed _rules/ and procedures/ paths. Use exact markdown heading anchors including markers. Do not request patches for pages, anchors, or bodies not included here. delete_section is unsupported. Budgets: max edits/proposal {}, max patch edits/run {}, max content chars/edit {}, max changed chars/proposal {}, max final body chars {}, max rule page tokens {}, max procedure page tokens {}.\n{patchable_text}\n\n\
         Recent rejected auto-improvement attempts for this workspace/project. Avoid repeating these target/reason/fingerprint patterns unless the session provides clearly new evidence:\n{rejection_text}\n\n\
         Consolidated session page, primary source when present:\n{session_page_section}\n\n\
         Selected observations, sampled for scale and supporting evidence:\n{rendered_observations}\n\n\
         Return proposals only for durable lessons. Prefer gotchas/, decisions/, concepts/, procedures/, _rules/, _slots/current-focus.md, or notes/. Use _rules/ exactly for rules; never rules/. Full-page proposal body markdown must begin with '# <title>'; patch proposals use edits and are materialized by validation. Full-page and materialized patch final bodies for _rules/ and procedures/ must stay within their page token budgets. Reject no-activity sessions, smoke tests, release markers, one-off task narratives, transient failures, generic routing snippets, and agent instruction text unless the session explicitly changed project policy.",
        observations.len(),
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
    );

    PromptInput {
        prompt,
        patchable_paths: patchable.included_paths,
        rejected_candidates,
        warnings,
    }
}

/// Old preflight gate, copied verbatim (pre-change).
pub(crate) fn oracle_preflight_rejection(
    observations: &[Observation],
    duration_secs: u64,
    cfg: &AutoImproveReviewConfig,
) -> Option<AutoImproveRejectedCandidate> {
    if observations.len() < cfg.min_observations {
        return Some(AutoImproveRejectedCandidate {
            reason: "too_few_observations".into(),
            evidence: format!(
                "{} observations below configured minimum {}",
                observations.len(),
                cfg.min_observations
            ),
            target_path: None,
            kind: None,
            operation: None,
            edit_mode: None,
        });
    }
    if duration_secs < cfg.min_session_duration_secs {
        return Some(AutoImproveRejectedCandidate {
            reason: "session_too_short".into(),
            evidence: format!(
                "{duration_secs}s below configured minimum {}s",
                cfg.min_session_duration_secs
            ),
            target_path: None,
            kind: None,
            operation: None,
            edit_mode: None,
        });
    }
    None
}

/// Old session duration, copied verbatim (pre-change).
pub(crate) fn oracle_session_duration_secs(observations: &[Observation]) -> u64 {
    let (Some(first), Some(last)) = (observations.first(), observations.last()) else {
        return 0;
    };
    let diff_us = last
        .created_at
        .as_microsecond()
        .saturating_sub(first.created_at.as_microsecond());
    u64::try_from(diff_us / 1_000_000).unwrap_or(0)
}
