//! Deterministic observation projection for bounded internal LLM prompts.

use std::collections::BTreeSet;

use ai_memory_core::{Observation, ObservationKind};

const DEFAULT_EVEN_SAMPLE_BUCKETS: usize = 16;
const MAX_RENDERED_TITLE_CHARS: usize = 500;
const MAX_RENDERED_SOURCE_CHARS: usize = 128;
const EVEN_SAMPLE_SCORE: i32 = 20;
const MAX_RECENCY_SCORE: i32 = 80;

/// Budget and rendering controls for observation projection.
#[derive(Debug, Clone)]
pub struct ObservationProjectionConfig {
    /// Target maximum rendered characters. If ordinary pruning cannot fit the
    /// text, the final projection is clipped with a visible fallback marker.
    pub max_total_chars: usize,
    /// Maximum number of observations selected before character-budget pruning.
    pub max_selected_observations: usize,
    /// Maximum body excerpt characters per selected observation.
    pub per_body_excerpt_chars: usize,
    /// Optional label included in omission warnings/markers.
    pub context_label: Option<String>,
}

impl ObservationProjectionConfig {
    /// Construct a projection config.
    #[must_use]
    pub fn new(
        max_total_chars: usize,
        max_selected_observations: usize,
        per_body_excerpt_chars: usize,
    ) -> Self {
        Self {
            max_total_chars,
            max_selected_observations,
            per_body_excerpt_chars,
            context_label: None,
        }
    }

    /// Attach a context label for human-readable warnings.
    #[must_use]
    pub fn with_context_label(mut self, label: impl Into<String>) -> Self {
        self.context_label = Some(label.into());
        self
    }
}

/// Rendered observation projection plus accounting useful to callers.
#[derive(Debug, Clone)]
pub struct ProjectedObservations {
    /// Prompt-ready text.
    pub text: String,
    /// Total observations considered.
    pub total_count: usize,
    /// Number of observations rendered.
    pub selected_count: usize,
    /// Number of observations not rendered.
    pub omitted_count: usize,
    /// Number of selected observation bodies truncated.
    pub truncated_bodies: usize,
    /// Selected observation indices in chronological order.
    pub selected_indices: Vec<usize>,
    /// Non-fatal budget and truncation notes.
    pub warnings: Vec<String>,
}

/// Cap one user-visible string with a visible marker.
#[must_use]
pub fn cap_text_with_marker(input: &str, max_chars: usize, label: &str) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max_chars).collect();
    let omitted = input.chars().count().saturating_sub(max_chars);
    out.push_str(&format!("\n[{label} truncated; {omitted} chars omitted]"));
    out
}

/// Project raw observations into deterministic, budgeted prompt text without
/// mutating or compressing the raw observations in SQLite.
#[must_use]
pub fn project_observations(
    observations: &[Observation],
    cfg: &ObservationProjectionConfig,
) -> ProjectedObservations {
    if observations.is_empty() {
        return ProjectedObservations {
            text: "(none)".into(),
            total_count: 0,
            selected_count: 0,
            omitted_count: 0,
            truncated_bodies: 0,
            selected_indices: Vec::new(),
            warnings: Vec::new(),
        };
    }

    if cfg.max_selected_observations == 0 || cfg.max_total_chars == 0 {
        let text = format!(
            "[{} observations omitted: no projection budget; full originals remain in SQLite by observation id]\n",
            observations.len()
        );
        return ProjectedObservations {
            text,
            total_count: observations.len(),
            selected_count: 0,
            omitted_count: observations.len(),
            truncated_bodies: 0,
            selected_indices: Vec::new(),
            warnings: vec![format!(
                "{} observation projection omitted by input budget",
                context_label(cfg)
            )],
        };
    }

    let mut selected = select_observation_indices(observations, cfg.max_selected_observations);
    let mut rendered = render_projection(observations, &selected, cfg.per_body_excerpt_chars);

    while rendered.text.chars().count() > cfg.max_total_chars && selected.len() > 1 {
        let Some(remove_idx) = lowest_prunable_index(observations, &selected) else {
            break;
        };
        selected.retain(|idx| *idx != remove_idx);
        rendered = render_projection(observations, &selected, cfg.per_body_excerpt_chars);
    }

    let omitted_count = observations.len().saturating_sub(selected.len());
    let mut text = rendered.text;
    let mut warnings = Vec::new();
    if omitted_count > 0 {
        let marker = format!(
            "\n[{} observations omitted from {} projection due to sample/count/character budget; selected {} of {}; full originals remain in SQLite by observation id]\n",
            omitted_count,
            context_label(cfg),
            selected.len(),
            observations.len()
        );
        text.push_str(&marker);
        warnings.push(format!(
            "{} observation input sampled {} of {} observations",
            context_label(cfg),
            selected.len(),
            observations.len()
        ));
    }
    if rendered.truncated_bodies > 0 {
        warnings.push(format!(
            "{} truncated {} observation bod{} to {} chars",
            context_label(cfg),
            rendered.truncated_bodies,
            if rendered.truncated_bodies == 1 {
                "y"
            } else {
                "ies"
            },
            cfg.per_body_excerpt_chars
        ));
    }
    if text.chars().count() > cfg.max_total_chars {
        warnings.push(format!(
            "{} projection exceeded max_total_chars after mandatory markers/anchors ({} > {})",
            context_label(cfg),
            text.chars().count(),
            cfg.max_total_chars
        ));
        text = fit_text_to_budget(
            &text,
            cfg.max_total_chars,
            "[projection text truncated to budget; full originals remain in SQLite by observation id]",
        );
    }

    ProjectedObservations {
        text,
        total_count: observations.len(),
        selected_count: selected.len(),
        omitted_count,
        truncated_bodies: rendered.truncated_bodies,
        selected_indices: selected,
        warnings,
    }
}

fn context_label(cfg: &ObservationProjectionConfig) -> &str {
    cfg.context_label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .unwrap_or("observation")
}

struct RenderedProjection {
    text: String,
    truncated_bodies: usize,
}

fn render_projection(
    observations: &[Observation],
    selected: &[usize],
    per_body_excerpt_chars: usize,
) -> RenderedProjection {
    let mut text = String::new();
    let mut truncated_bodies = 0usize;
    for idx in selected {
        let Some(obs) = observations.get(*idx) else {
            continue;
        };
        let (body, truncated, omitted) = excerpt_body(&obs.body, per_body_excerpt_chars);
        if truncated {
            truncated_bodies += 1;
        }
        let title = cap_text_with_marker(&obs.title, MAX_RENDERED_TITLE_CHARS, "observation title");
        text.push_str(&format!(
            "\n--- observation {}/{} ---\nid: {}\nkind: {}\ntitle: {}\nimportance: {}\ncreated_at: {}\n",
            idx + 1,
            observations.len(),
            obs.id,
            obs.kind.as_str(),
            title,
            obs.importance,
            obs.created_at,
        ));
        if let Some(extension) = obs.extension.as_deref().filter(|s| !s.trim().is_empty()) {
            let extension = cap_text_with_marker(extension, MAX_RENDERED_SOURCE_CHARS, "extension");
            text.push_str(&format!("extension: {extension}\n"));
        }
        if let Some(source_event) = obs.source_event.as_deref().filter(|s| !s.trim().is_empty()) {
            let source_event =
                cap_text_with_marker(source_event, MAX_RENDERED_SOURCE_CHARS, "source event");
            text.push_str(&format!("source_event: {source_event}\n"));
        }
        text.push_str(&format!("body:\n{body}"));
        if truncated {
            text.push_str(&format!(
                "\n[observation body truncated; {omitted} chars omitted; full original remains in SQLite as observation id {}]",
                obs.id
            ));
        }
        text.push('\n');
    }
    RenderedProjection {
        text,
        truncated_bodies,
    }
}

fn excerpt_body(body: &str, max_chars: usize) -> (String, bool, usize) {
    let total = body.chars().count();
    if total <= max_chars {
        return (body.to_string(), false, 0);
    }
    let excerpt: String = body.chars().take(max_chars).collect();
    (excerpt, true, total.saturating_sub(max_chars))
}

fn fit_text_to_budget(text: &str, max_chars: usize, marker: &str) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let marker = format!("\n{marker}");
    let marker_len = marker.chars().count();
    if marker_len >= max_chars {
        return marker.chars().take(max_chars).collect();
    }
    let keep = max_chars.saturating_sub(marker_len);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(&marker);
    out
}

fn select_observation_indices(observations: &[Observation], limit: usize) -> Vec<usize> {
    if observations.len() <= limit {
        return (0..observations.len()).collect();
    }
    let mut selected: BTreeSet<usize> = BTreeSet::new();
    if limit == 1 {
        selected.insert(observations.len() - 1);
        return selected.into_iter().collect();
    }
    selected.insert(0);
    selected.insert(observations.len() - 1);
    let even = even_sample_indices(observations.len());
    let mut scored: Vec<(i32, usize)> = observations
        .iter()
        .enumerate()
        .filter(|(idx, _)| !selected.contains(idx))
        .map(|(idx, obs)| {
            let mut score = observation_score(obs, idx, observations.len());
            if even.contains(&idx) {
                score += EVEN_SAMPLE_SCORE;
            }
            (score, idx)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    for (_, idx) in scored {
        if selected.len() >= limit {
            break;
        }
        selected.insert(idx);
    }
    selected.into_iter().collect()
}

fn even_sample_indices(total: usize) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    if total == 0 {
        return out;
    }
    if total == 1 {
        out.insert(0);
        return out;
    }
    let buckets = DEFAULT_EVEN_SAMPLE_BUCKETS.min(total);
    for bucket in 0..buckets {
        let idx = bucket.saturating_mul(total - 1) / (buckets - 1).max(1);
        out.insert(idx);
    }
    out
}

fn lowest_prunable_index(observations: &[Observation], selected: &[usize]) -> Option<usize> {
    selected
        .iter()
        .copied()
        .filter(|idx| !is_hard_anchor(observations, *idx))
        .map(|idx| {
            (
                observation_score(&observations[idx], idx, observations.len()),
                idx,
            )
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, idx)| idx)
}

fn is_hard_anchor(observations: &[Observation], idx: usize) -> bool {
    idx == 0 || idx + 1 == observations.len()
}

fn observation_score(obs: &Observation, idx: usize, total: usize) -> i32 {
    let mut score = i32::from(obs.importance);
    score += recency_score(idx, total);
    score += match obs.kind {
        ObservationKind::UserPrompt => 100,
        ObservationKind::SessionEnd => 95,
        ObservationKind::PreCompact => 90,
        ObservationKind::PostCompaction => 85,
        // A non-empty Stop carries the opt-in assistant excerpt (#196) — a
        // high-signal end-of-turn summary or late correction. Score it just
        // below PreCompact (90), above PostCompaction (85), so it competes for a
        // sampling slot without TYING any kind (ties resolve by position). An
        // empty Stop (capture off, or gated out) keeps its low prior.
        ObservationKind::Stop if !obs.body.trim().is_empty() => 88,
        ObservationKind::Stop => 55,
        ObservationKind::PostToolUse => 30,
        ObservationKind::Notification => 25,
        ObservationKind::SessionStart => 20,
        ObservationKind::Other => 15,
        ObservationKind::PreToolUse => 5,
    };
    if idx == 0 || idx + 1 == total {
        score += 100;
    }
    if has_high_signal_terms(obs) {
        score += 45;
    }
    if obs.importance >= 9 {
        score += 45;
    }
    if obs.body.contains("```") {
        score += 8;
    }
    let body_prefix = obs.body.chars().take(4_000).collect::<String>();
    let text = format!("{}\n{}", obs.title, body_prefix).to_ascii_lowercase();
    if text.contains("long-term memory (ai-memory)")
        || text.contains("install ai-memory routing")
        || text.contains("memory_query searches only one project")
    {
        score -= 80;
    }
    score
}

fn recency_score(idx: usize, total: usize) -> i32 {
    if total <= 1 {
        return 0;
    }
    (idx.saturating_mul(MAX_RECENCY_SCORE as usize) / (total - 1)) as i32
}

fn has_high_signal_terms(obs: &Observation) -> bool {
    let body_prefix = obs.body.chars().take(4_000).collect::<String>();
    let text = format!("{}\n{}", obs.title, body_prefix).to_ascii_lowercase();
    [
        "root cause",
        "fix",
        "fixed",
        "failed",
        "failure",
        "error",
        "bug",
        "regression",
        "decision",
        "decided",
        "gotcha",
        "rule",
        "always",
        "never",
        "migration",
        "scope",
        "workspace",
        "project",
        "auth",
        "test",
        "clippy",
        "release",
    ]
    .iter()
    .any(|keyword| text.contains(keyword))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{ObservationId, ProjectId, SessionId, WorkspaceId};
    use jiff::Timestamp;

    fn obs(
        idx: usize,
        kind: ObservationKind,
        title: &str,
        body: &str,
        importance: u8,
    ) -> Observation {
        Observation {
            id: ObservationId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            session_id: SessionId::new(),
            kind,
            title: format!("{title} {idx}"),
            body: body.into(),
            created_at: Timestamp::UNIX_EPOCH,
            importance,
            extension: None,
            source_event: None,
        }
    }

    #[test]
    fn small_sessions_render_all_observations() {
        let observations = vec![
            obs(0, ObservationKind::SessionStart, "start", "cwd", 5),
            obs(1, ObservationKind::UserPrompt, "prompt", "do work", 7),
            obs(2, ObservationKind::SessionEnd, "end", "done", 5),
        ];
        let projected = project_observations(
            &observations,
            &ObservationProjectionConfig::new(10_000, 10, 1_000),
        );
        assert_eq!(projected.selected_count, 3);
        assert_eq!(projected.omitted_count, 0);
        assert!(projected.text.contains("observation 1/3"));
        assert!(projected.text.contains("observation 3/3"));
        assert!(projected.warnings.is_empty());
    }

    #[test]
    fn long_sessions_preserve_anchors_and_mark_omissions() {
        let mut observations: Vec<_> = (0..80)
            .map(|idx| obs(idx, ObservationKind::PostToolUse, "routine", "boring", 3))
            .collect();
        observations[5] = obs(
            5,
            ObservationKind::UserPrompt,
            "user prompt",
            "important request",
            5,
        );
        observations[20] = obs(
            20,
            ObservationKind::PreCompact,
            "pre compact",
            "context pressure",
            5,
        );
        observations[35] = obs(
            35,
            ObservationKind::PostToolUse,
            "error",
            "failed with regression",
            5,
        );
        observations[50] = obs(
            50,
            ObservationKind::Other,
            "high importance",
            "key decision",
            10,
        );
        observations[79] = obs(79, ObservationKind::SessionEnd, "session end", "done", 5);

        let projected = project_observations(
            &observations,
            &ObservationProjectionConfig::new(20_000, 12, 200),
        );
        assert!(projected.selected_indices.contains(&0));
        assert!(projected.selected_indices.contains(&79));
        assert!(projected.selected_indices.contains(&5));
        assert!(projected.selected_indices.contains(&20));
        assert!(projected.selected_indices.contains(&35));
        assert!(projected.selected_indices.contains(&50));
        assert!(projected.text.contains("observations omitted"));
    }

    #[test]
    fn many_high_signal_observations_still_respect_selection_cap() {
        let observations: Vec<_> = (0..40)
            .map(|idx| {
                obs(
                    idx,
                    ObservationKind::UserPrompt,
                    "user prompt",
                    "fix failed error regression decision",
                    9,
                )
            })
            .collect();
        let projected = project_observations(
            &observations,
            &ObservationProjectionConfig::new(20_000, 8, 200),
        );
        assert_eq!(projected.selected_count, 8);
        assert_eq!(projected.selected_indices.first().copied(), Some(0));
        assert_eq!(projected.selected_indices.last().copied(), Some(39));
        assert!(projected.text.contains("observations omitted"));
    }

    #[test]
    fn long_session_selection_prefers_later_corrections_without_losing_anchors_or_sampling() {
        let mut observations: Vec<_> = (0..80)
            .map(|idx| obs(idx, ObservationKind::PostToolUse, "routine", "boring", 3))
            .collect();
        observations[78] = obs(
            78,
            ObservationKind::PostToolUse,
            "final correction",
            "tool_family: file\noutcome: unknown\n---\nfinal state supersedes earlier draft",
            3,
        );
        observations[79] = obs(79, ObservationKind::SessionEnd, "session end", "done", 5);

        let projected = project_observations(
            &observations,
            &ObservationProjectionConfig::new(20_000, 8, 200),
        );

        assert_eq!(projected.selected_indices.first().copied(), Some(0));
        assert_eq!(projected.selected_indices.last().copied(), Some(79));
        assert!(projected.selected_indices.contains(&78));
        let even = even_sample_indices(observations.len());
        assert!(
            projected
                .selected_indices
                .iter()
                .any(|idx| !is_hard_anchor(&observations, *idx) && even.contains(idx))
        );
    }

    #[test]
    fn body_truncation_marker_includes_omitted_chars_and_id() {
        let observations = vec![obs(
            0,
            ObservationKind::UserPrompt,
            "prompt",
            &"x".repeat(30),
            5,
        )];
        let id = observations[0].id.to_string();
        let projected = project_observations(
            &observations,
            &ObservationProjectionConfig::new(10_000, 10, 10),
        );
        assert!(projected.text.contains("20 chars omitted"));
        assert!(projected.text.contains(&id));
        assert!(projected.text.contains("full original remains in SQLite"));
        assert_eq!(projected.truncated_bodies, 1);
    }

    #[test]
    fn huge_title_is_capped_and_projection_respects_budget() {
        let mut observations = vec![obs(
            0,
            ObservationKind::Notification,
            &"title".repeat(1_000),
            "small body",
            5,
        )];
        observations[0].extension = Some("ext".into());
        observations[0].source_event = Some("source".into());
        let projected = project_observations(
            &observations,
            &ObservationProjectionConfig::new(900, 10, 100),
        );
        assert!(projected.text.chars().count() <= 900);
        assert!(projected.text.contains("observation title truncated"));
        assert!(projected.text.contains("extension: ext"));
        assert!(projected.text.contains("source_event: source"));
    }

    #[test]
    fn tiny_budget_uses_hard_fallback_marker() {
        let observations = vec![obs(
            0,
            ObservationKind::UserPrompt,
            "prompt",
            &"x".repeat(1_000),
            5,
        )];
        let cfg = ObservationProjectionConfig::new(80, 10, 500);
        let projected = project_observations(&observations, &cfg);
        assert!(projected.text.chars().count() <= cfg.max_total_chars);
        assert!(projected.text.contains("projection text truncated"));
    }

    #[test]
    fn output_respects_max_chars_except_marker_overhead() {
        let observations: Vec<_> = (0..40)
            .map(|idx| {
                obs(
                    idx,
                    ObservationKind::PostToolUse,
                    "routine",
                    &"x".repeat(200),
                    3,
                )
            })
            .collect();
        let cfg = ObservationProjectionConfig::new(3_000, 10, 50);
        let projected = project_observations(&observations, &cfg);
        assert!(projected.text.chars().count() <= cfg.max_total_chars);
        assert!(projected.text.contains("observations omitted"));
    }

    /// Lock the base reviewer ordering for the opt-in Stop excerpt (#196):
    /// a non-empty Stop scores below PreCompact (no tie — ties resolve by
    /// position), above PostCompaction, well above an empty Stop, and below a
    /// UserPrompt. Same idx/total/importance means only the kind term (and body
    /// emptiness) differs; neutral text avoids high-signal and anchor bonuses.
    #[test]
    fn non_empty_stop_score_sits_below_precompact_above_postcompaction() {
        let idx = 50;
        let total = 100;
        let imp = 5;
        let mk = |kind, body| observation_score(&obs(idx, kind, "note", body, imp), idx, total);

        let stop_full = mk(ObservationKind::Stop, "the assistant said hello");
        let stop_empty = mk(ObservationKind::Stop, "");
        let precompact = mk(ObservationKind::PreCompact, "the assistant said hello");
        let postcompaction = mk(ObservationKind::PostCompaction, "the assistant said hello");
        let user_prompt = mk(ObservationKind::UserPrompt, "the assistant said hello");

        // Kind terms: Stop-nonempty 88, PreCompact 90, PostCompaction 85,
        // Stop-empty 55, UserPrompt 100. Everything else cancels.
        assert_eq!(stop_full, precompact - 2, "must sit just below PreCompact");
        assert_eq!(
            stop_full,
            postcompaction + 3,
            "must sit above PostCompaction"
        );
        assert_eq!(stop_full, stop_empty + 33, "non-empty must beat empty Stop");
        assert!(
            user_prompt > stop_full,
            "the UserPrompt base priority must remain higher"
        );
    }

    /// A late non-empty Stop (the opt-in assistant excerpt with a correction) is
    /// selected out of a long routine session AND its text reaches the projection.
    #[test]
    fn non_empty_stop_late_correction_is_selected_and_rendered() {
        let mut observations: Vec<_> = (0..60)
            .map(|idx| obs(idx, ObservationKind::PostToolUse, "routine", "x", 1))
            .collect();
        // The assistant's final turn retracts an earlier claim — the exact signal
        // PR3 wants the reviewer to see. Kept within the excerpt window.
        observations[40] = obs(
            40,
            ObservationKind::Stop,
            "stop",
            "CORRECTION_SENTINEL: the earlier fix was wrong; use config.rs instead",
            6,
        );
        let cfg = ObservationProjectionConfig::new(20_000, 12, 2_000);
        let projected = project_observations(&observations, &cfg);

        assert!(
            projected.selected_indices.contains(&40),
            "non-empty Stop must win a sampling slot: {:?}",
            projected.selected_indices
        );
        assert!(
            projected.text.contains("CORRECTION_SENTINEL"),
            "the correction must reach the reviewer projection"
        );
    }

    /// A sampled multi-turn session retains representative prompts and
    /// non-empty Stops together. This is not a claim that every prompt fits in
    /// every bounded projection.
    #[test]
    fn sampled_session_retains_prompts_and_non_empty_stops() {
        let mut observations: Vec<_> = (0..60)
            .map(|idx| obs(idx, ObservationKind::PostToolUse, "routine", "x", 1))
            .collect();
        let prompts = [10, 25, 45];
        let stops = [15, 30, 50];
        for &i in &prompts {
            observations[i] = obs(i, ObservationKind::UserPrompt, "prompt", "a request", 5);
        }
        for &i in &stops {
            observations[i] = obs(i, ObservationKind::Stop, "stop", "assistant summary", 5);
        }
        let cfg = ObservationProjectionConfig::new(20_000, 12, 2_000);
        let projected = project_observations(&observations, &cfg);

        for i in prompts {
            assert!(
                projected.selected_indices.contains(&i),
                "expected UserPrompt at {i} to be selected: {:?}",
                projected.selected_indices
            );
        }
        for i in stops {
            assert!(
                projected.selected_indices.contains(&i),
                "non-empty Stop at {i} was not selected: {:?}",
                projected.selected_indices
            );
        }
    }
}
