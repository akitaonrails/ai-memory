//! Deterministic observation projection for bounded internal LLM prompts.

use std::collections::BTreeSet;

use ai_memory_core::{Observation, ObservationKind};

const DEFAULT_EVEN_SAMPLE_BUCKETS: usize = 16;
const MAX_RENDERED_TITLE_CHARS: usize = 500;
const MAX_RENDERED_SOURCE_CHARS: usize = 128;
const MAX_RECENT_TAIL_RESERVATION: usize = 128;
const RECENT_TAIL_RESERVATION_DIVISOR: usize = 2;
const RECENCY_SCORE_WEIGHT: i32 = 30;
const HIGH_SIGNAL_SCORE_BONUS: i32 = 45;

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
    project_observations_with_preference(observations, cfg, false)
}

/// Project observations while preferring later in-session facts. This is kept
/// crate-internal so the public projection config stays source-compatible.
#[must_use]
pub(crate) fn project_observations_prefer_recent(
    observations: &[Observation],
    cfg: &ObservationProjectionConfig,
) -> ProjectedObservations {
    project_observations_with_preference(observations, cfg, true)
}

fn project_observations_with_preference(
    observations: &[Observation],
    cfg: &ObservationProjectionConfig,
    prefer_recent: bool,
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

    let mut selected =
        select_observation_indices(observations, cfg.max_selected_observations, prefer_recent);
    let mut rendered = render_projection(observations, &selected, cfg.per_body_excerpt_chars);

    while rendered.text.chars().count() > cfg.max_total_chars && selected.len() > 1 {
        let Some(remove_idx) = lowest_prunable_index(observations, &selected, prefer_recent) else {
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

fn select_observation_indices(
    observations: &[Observation],
    limit: usize,
    prefer_recent: bool,
) -> Vec<usize> {
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
    if prefer_recent {
        for idx in recent_tail_indices(observations.len(), limit) {
            if selected.len() >= limit {
                break;
            }
            selected.insert(idx);
        }
    }
    let even = even_sample_indices(observations.len());
    let mut scored: Vec<(i32, usize)> = observations
        .iter()
        .enumerate()
        .filter(|(idx, _)| !selected.contains(idx))
        .map(|(idx, obs)| {
            let mut score = observation_score(obs, idx, observations.len(), prefer_recent);
            if even.contains(&idx) {
                score += 40;
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

fn recent_tail_indices(total: usize, limit: usize) -> Vec<usize> {
    if total == 0 || limit == 0 {
        return Vec::new();
    }
    let tail_len = if limit <= 2 {
        1
    } else {
        (limit / RECENT_TAIL_RESERVATION_DIVISOR).clamp(2, MAX_RECENT_TAIL_RESERVATION)
    }
    .min(total);
    let start = total.saturating_sub(tail_len);
    (start..total).collect()
}

fn recent_pruning_indices(total: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    let tail_len = if total <= 2 {
        1
    } else {
        (total / RECENT_TAIL_RESERVATION_DIVISOR).clamp(2, MAX_RECENT_TAIL_RESERVATION)
    }
    .min(total);
    let start = total.saturating_sub(tail_len);
    (start..total).collect()
}

fn lowest_prunable_index(
    observations: &[Observation],
    selected: &[usize],
    prefer_recent: bool,
) -> Option<usize> {
    let recent_tail: BTreeSet<_> = if prefer_recent {
        recent_pruning_indices(observations.len())
            .into_iter()
            .collect()
    } else {
        BTreeSet::new()
    };
    let candidates: Vec<_> = selected
        .iter()
        .copied()
        .filter(|idx| !is_hard_anchor(observations, *idx))
        .collect();

    lowest_scored_index(
        observations,
        candidates
            .iter()
            .copied()
            .filter(|idx| !recent_tail.contains(idx)),
        prefer_recent,
    )
    .or_else(|| {
        if prefer_recent {
            selected
                .iter()
                .copied()
                .find(|idx| idx + 1 != observations.len() && !recent_tail.contains(idx))
        } else {
            None
        }
    })
    .or_else(|| lowest_scored_index(observations, candidates.iter().copied(), prefer_recent))
}

fn lowest_scored_index(
    observations: &[Observation],
    indices: impl Iterator<Item = usize>,
    prefer_recent: bool,
) -> Option<usize> {
    indices
        .map(|idx| {
            (
                observation_score(&observations[idx], idx, observations.len(), prefer_recent),
                idx,
            )
        })
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, idx)| idx)
}

fn is_hard_anchor(observations: &[Observation], idx: usize) -> bool {
    idx == 0 || idx + 1 == observations.len()
}

fn observation_score(obs: &Observation, idx: usize, total: usize, prefer_recent: bool) -> i32 {
    let mut score = i32::from(obs.importance);
    score += match obs.kind {
        ObservationKind::UserPrompt => 100,
        ObservationKind::SessionEnd => 95,
        ObservationKind::Stop => 55,
        ObservationKind::PreCompact => 90,
        ObservationKind::PostToolUse => 30,
        ObservationKind::Notification => 25,
        ObservationKind::SessionStart => 20,
        ObservationKind::Other => 15,
        ObservationKind::PreToolUse => 5,
    };
    if prefer_recent {
        score += recency_score(idx, total);
    }
    if idx == 0 || idx + 1 == total {
        score += 100;
    }
    if has_high_signal_terms(obs, prefer_recent) {
        score += HIGH_SIGNAL_SCORE_BONUS;
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
    let denominator = total.saturating_sub(1);
    if denominator == 0 {
        return 0;
    }
    let score = (idx as u128).saturating_mul(RECENCY_SCORE_WEIGHT as u128) / denominator as u128;
    i32::try_from(score).unwrap_or(i32::MAX)
}

fn has_high_signal_terms(obs: &Observation, prefer_recent: bool) -> bool {
    let body_prefix = obs.body.chars().take(4_000).collect::<String>();
    let text = format!("{}\n{}", obs.title, body_prefix).to_ascii_lowercase();
    let has_base_term = [
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
    .any(|keyword| text.contains(keyword));
    has_base_term
        || (prefer_recent
            && ["correction", "corrected", "confirmed"]
                .iter()
                .any(|keyword| text.contains(keyword)))
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
    fn long_sessions_keep_late_plain_correction_over_stale_early_observation() {
        let mut observations: Vec<_> = (0..30)
            .map(|idx| obs(idx, ObservationKind::PreToolUse, "routine", "routine", 5))
            .collect();
        observations[1] = obs(
            1,
            ObservationKind::PostToolUse,
            "deploy plan",
            "Deploy is manual.",
            5,
        );
        observations[28] = obs(
            28,
            ObservationKind::PostToolUse,
            "deploy update",
            "Deploy is Coolify plus Traefik.",
            5,
        );

        let projected = project_observations_prefer_recent(
            &observations,
            &ObservationProjectionConfig::new(20_000, 3, 200),
        );

        assert_eq!(projected.selected_indices, vec![0, 28, 29]);
        assert!(projected.text.contains("Coolify plus Traefik"));
        assert!(!projected.text.contains("Deploy is manual."));
    }

    #[test]
    fn count_cap_keeps_recent_tail_correction_in_long_sessions() {
        let mut observations: Vec<_> = (0..300)
            .map(|idx| {
                obs(
                    idx,
                    ObservationKind::UserPrompt,
                    "high signal filler",
                    "fix failed error regression decision",
                    9,
                )
            })
            .collect();
        observations[1] = obs(
            1,
            ObservationKind::UserPrompt,
            "deploy plan",
            "Deploy is manual.",
            9,
        );
        observations[220] = obs(
            220,
            ObservationKind::PostToolUse,
            "deploy update",
            "Deploy uses Coolify plus Traefik.",
            5,
        );

        let projected = project_observations_prefer_recent(
            &observations,
            &ObservationProjectionConfig::new(200_000, 256, 200),
        );

        assert_eq!(projected.selected_count, 256);
        assert!(projected.selected_indices.contains(&220));
        assert!(projected.text.contains("Coolify plus Traefik"));
    }

    #[test]
    fn recent_tail_reservation_does_not_evict_older_high_signal_observations() {
        let mut observations: Vec<_> = (0..300)
            .map(|idx| obs(idx, ObservationKind::PreToolUse, "routine", "routine", 5))
            .collect();
        for idx in [5, 20, 35, 50, 65, 80, 95, 110] {
            observations[idx] = obs(
                idx,
                ObservationKind::UserPrompt,
                "high signal",
                "fix failed error regression decision",
                9,
            );
        }

        let projected = project_observations_prefer_recent(
            &observations,
            &ObservationProjectionConfig::new(200_000, 20, 200),
        );

        for idx in [5, 20, 35, 50, 65, 80, 95, 110] {
            assert!(projected.selected_indices.contains(&idx));
        }
    }

    #[test]
    fn character_budget_pruning_keeps_late_correction() {
        let mut observations: Vec<_> = (0..60)
            .map(|idx| obs(idx, ObservationKind::PreToolUse, "routine", "routine", 5))
            .collect();
        observations[1] = obs(
            1,
            ObservationKind::UserPrompt,
            "deploy plan",
            "Deploy is manual.",
            5,
        );
        observations[52] = obs(
            52,
            ObservationKind::PostToolUse,
            "deploy update",
            "Deploy uses Coolify plus Traefik.",
            5,
        );

        let projected = project_observations_prefer_recent(
            &observations,
            &ObservationProjectionConfig::new(1_400, 20, 80),
        );

        assert!(projected.selected_indices.contains(&52));
        assert!(projected.text.contains("Coolify plus Traefik"));
        assert!(!projected.text.contains("Deploy is manual."));
    }

    #[test]
    fn small_limit_recent_mode_reserves_plain_late_correction() {
        let mut observations: Vec<_> = (0..30)
            .map(|idx| obs(idx, ObservationKind::PreToolUse, "routine", "routine", 5))
            .collect();
        observations[1] = obs(
            1,
            ObservationKind::UserPrompt,
            "stale deploy claim",
            "fix failed error regression decision; deploy is manual.",
            9,
        );
        observations[28] = obs(
            28,
            ObservationKind::PostToolUse,
            "deploy correction",
            "Deploy uses Coolify plus Traefik.",
            5,
        );

        let projected = project_observations_prefer_recent(
            &observations,
            &ObservationProjectionConfig::new(20_000, 3, 200),
        );

        assert_eq!(projected.selected_indices, vec![0, 28, 29]);
        assert!(projected.text.contains("Coolify plus Traefik"));
        assert!(!projected.text.contains("deploy is manual"));
    }

    #[test]
    fn tiny_recent_budget_prefers_latest_anchor_over_first_anchor() {
        let observations = vec![
            obs(
                0,
                ObservationKind::UserPrompt,
                "stale deploy claim",
                &format!("Deploy is manual. {}", "x".repeat(1_000)),
                9,
            ),
            obs(1, ObservationKind::PreToolUse, "routine", "routine", 5),
            obs(
                2,
                ObservationKind::SessionEnd,
                "final deploy state",
                "Deploy uses Coolify plus Traefik.",
                5,
            ),
        ];

        let projected = project_observations_prefer_recent(
            &observations,
            &ObservationProjectionConfig::new(650, 2, 500),
        );

        assert_eq!(projected.selected_indices, vec![2]);
        assert!(projected.text.contains("Coolify plus Traefik"));
        assert!(!projected.text.contains("Deploy is manual"));
    }

    #[test]
    fn tight_recent_budget_keeps_penultimate_correction_before_first_anchor() {
        let observations = vec![
            obs(
                0,
                ObservationKind::UserPrompt,
                "stale deploy claim",
                &format!("Deploy is manual. {}", "x".repeat(1_000)),
                9,
            ),
            obs(
                1,
                ObservationKind::PostToolUse,
                "deploy correction",
                "Deploy uses Coolify plus Traefik.",
                5,
            ),
            obs(2, ObservationKind::SessionEnd, "session end", "done", 5),
        ];

        let projected = project_observations_prefer_recent(
            &observations,
            &ObservationProjectionConfig::new(650, 3, 500),
        );

        assert_eq!(projected.selected_indices, vec![1, 2]);
        assert!(projected.text.contains("Coolify plus Traefik"));
        assert!(!projected.text.contains("Deploy is manual"));
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
}
