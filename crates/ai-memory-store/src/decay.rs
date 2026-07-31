//! Retention-formula math + tunable parameters.
//!
//! Adapted from agentmemory's `salience · exp(−λΔt) + Σ(σ/days_since_access)`.
//! We simplify the Σ term to `σ · log(1 + access_count) · exp(−μ · days_since_access)`
//! so we don't have to materialise a full access-history table on the
//! hot read path -- the `access_count` + `last_accessed_at` columns on
//! `pages` (V03 migration) are enough.
//!
//! The formula is pure: pass in everything you need, get a score out.
//! The forget-sweep job computes it from store rows; the property tests
//! pin the math without touching the database.

use serde::{Deserialize, Serialize};

/// Tunable retention coefficients.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DecayParams {
    /// Per-day exponential decay rate applied to "age since updated_at".
    /// `0.02` ≈ 35-day half-life.
    pub lambda: f64,
    /// Magnitude of the access-reinforcement boost.
    pub sigma: f64,
    /// Per-day exponential decay applied to "days since last access" --
    /// so a recent hit boosts more than an old one.
    pub mu: f64,
    /// Default salience used when a page doesn't have an explicit one.
    pub salience_default: f64,
    /// Below this score, an episodic page is a soft-delete candidate.
    pub cold_threshold: f64,
    /// Days a soft-deleted (sweep-evicted) page must survive before
    /// hard-delete, *with* zero subsequent access.
    pub hard_delete_after_days: i64,
    /// How much a page being reinforced by MANY distinct operators should
    /// count for, beyond the raw hit count.
    ///
    /// `0.0` (the default) makes [`retention_score_with_breadth`] identical to
    /// the historical formula, so existing deployments keep byte-identical
    /// eviction decisions until this is deliberately raised. Only meaningful
    /// once requests carry distinct identities: with one shared credential
    /// every read lands under the same actor.
    pub breadth_weight: f64,
}

impl Default for DecayParams {
    fn default() -> Self {
        Self {
            lambda: 0.02,
            sigma: 0.6,
            mu: 0.04,
            // Off: the score is identical to the pre-breadth formula.
            breadth_weight: 0.0,
            salience_default: 1.0,
            cold_threshold: 0.20,
            hard_delete_after_days: 180,
        }
    }
}

/// Compute the retention score. Higher = "keep this page".
///
/// * `age_days` — days since the page's `updated_at`.
/// * `access_count` — total number of search hits.
/// * `days_since_access` — `Some(N)` if the page has ever been
///   accessed; `None` if it never has.
/// * `salience` — the page's own salience when explicit feedback has
///   moved it (`pages.salience`, V37); `None` falls back to
///   [`DecayParams::salience_default`], which is what every page
///   without feedback uses.
#[must_use]
pub fn retention_score(
    params: &DecayParams,
    age_days: f64,
    access_count: u32,
    days_since_access: Option<f64>,
    salience: Option<f64>,
) -> f64 {
    retention_score_with_breadth(
        params,
        age_days,
        access_count,
        days_since_access,
        salience,
        0,
    )
}

/// Retention score that also accounts for HOW MANY distinct operators reinforced
/// the page.
///
/// `access_count` cannot tell "50 reads by one person" from "one read by each of
/// 50 people", although only the second says the page is load-bearing for a
/// team. `distinct_actors` supplies that, weighted by
/// [`DecayParams::breadth_weight`].
///
/// Identity at the default weight of `0.0`, and identity again for
/// `distinct_actors` of 0 (no per-actor rows recorded — every page written
/// before this existed) or 1 (a single reader). So enabling the table changes
/// no score until an operator deliberately turns the weight up, and there is no
/// eviction cliff to migrate around.
///
/// `salience` is the page's own feedback-moved salience; it scales the time
/// term independently of breadth, which scales the access term.
#[must_use]
pub fn retention_score_with_breadth(
    params: &DecayParams,
    age_days: f64,
    access_count: u32,
    days_since_access: Option<f64>,
    salience: Option<f64>,
    distinct_actors: u32,
) -> f64 {
    let salience = salience.unwrap_or(params.salience_default);
    let time_term = salience * (-params.lambda * age_days).exp();
    // g(0) = g(1) = 1, monotonically non-decreasing afterwards.
    let breadth = 1.0 + params.breadth_weight * (f64::from(distinct_actors.max(1)) - 1.0).ln_1p();
    let access_term = days_since_access.map_or(0.0, |d| {
        params.sigma * (1.0 + f64::from(access_count)).ln() * (-params.mu * d).exp() * breadth
    });
    time_term + access_term
}

/// Salience bounds and step for explicit feedback. Deliberately narrow:
/// feedback should tilt retention, not let one agent judgement pin a
/// page forever (`SALIENCE_MAX`) or evict it on the next sweep
/// (`SALIENCE_MIN` keeps a fresh page above `cold_threshold`).
pub const SALIENCE_MIN: f64 = 0.25;
/// Upper salience bound; see [`SALIENCE_MIN`].
pub const SALIENCE_MAX: f64 = 2.0;
/// Per-signal salience step; see [`SALIENCE_MIN`].
pub const SALIENCE_STEP: f64 = 0.25;

/// Apply one feedback signal to a page's salience, clamped to
/// `[SALIENCE_MIN, SALIENCE_MAX]`. `current` is `None` for a page that
/// has never received feedback, which starts from
/// [`DecayParams::salience_default`].
///
/// `Stale` / `Wrong` drop straight to the floor rather than stepping:
/// they assert the content is no longer trustworthy, so retention
/// should stop propping it up while it waits for a lint pass.
#[must_use]
pub fn salience_after_feedback(
    params: &DecayParams,
    current: Option<f64>,
    kind: ai_memory_core::FeedbackKind,
) -> f64 {
    use ai_memory_core::FeedbackKind as K;
    let current = current.unwrap_or(params.salience_default);
    let next = match kind {
        K::Helpful => current + SALIENCE_STEP,
        K::NotHelpful => current - SALIENCE_STEP,
        K::Stale | K::Wrong => SALIENCE_MIN,
    };
    next.clamp(SALIENCE_MIN, SALIENCE_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::FeedbackKind;

    #[test]
    fn fresh_unused_page_starts_near_salience() {
        let p = DecayParams::default();
        let score = retention_score(&p, 0.0, 0, None, None);
        assert!((score - p.salience_default).abs() < 1e-9);
    }

    #[test]
    fn ancient_page_with_no_access_decays_below_threshold() {
        let p = DecayParams::default();
        let score = retention_score(&p, 365.0, 0, None, None);
        assert!(score < p.cold_threshold, "got {score}");
    }

    #[test]
    fn frequently_accessed_page_stays_above_threshold_even_old() {
        let p = DecayParams::default();
        let aged_unused = retention_score(&p, 200.0, 0, None, None);
        let aged_hot = retention_score(&p, 200.0, 50, Some(2.0), None);
        assert!(aged_unused < p.cold_threshold);
        assert!(
            aged_hot > p.cold_threshold,
            "hot page should survive: {aged_hot} (cold {aged_unused})",
        );
    }

    #[test]
    fn recent_access_boosts_more_than_old_access() {
        let p = DecayParams::default();
        let recent = retention_score(&p, 100.0, 10, Some(2.0), None);
        let stale = retention_score(&p, 100.0, 10, Some(120.0), None);
        assert!(recent > stale, "recent {recent} vs stale {stale}");
    }

    #[test]
    fn score_decreases_as_age_increases_without_access() {
        let p = DecayParams::default();
        let young = retention_score(&p, 10.0, 0, None, None);
        let old = retention_score(&p, 20.0, 0, None, None);
        assert!(young > old, "young {young} vs old {old}");
    }

    #[test]
    fn score_increases_with_access_count_when_access_age_matches() {
        let p = DecayParams::default();
        let low = retention_score(&p, 100.0, 1, Some(5.0), None);
        let high = retention_score(&p, 100.0, 20, Some(5.0), None);
        assert!(high > low, "high {high} vs low {low}");
    }

    #[test]
    fn explicit_salience_scales_the_time_term_only() {
        let p = DecayParams::default();
        // No access term: the score is purely salience · exp(−λt).
        let default = retention_score(&p, 30.0, 0, None, None);
        let boosted = retention_score(&p, 30.0, 0, None, Some(2.0));
        let dropped = retention_score(&p, 30.0, 0, None, Some(SALIENCE_MIN));
        assert!((boosted - 2.0 * default).abs() < 1e-9);
        assert!(dropped < default, "floor salience must score below default");

        // With an access term, only the time half scales.
        let with_access_default = retention_score(&p, 30.0, 10, Some(1.0), None);
        let with_access_boosted = retention_score(&p, 30.0, 10, Some(1.0), Some(2.0));
        assert!((with_access_boosted - with_access_default - default).abs() < 1e-9);
    }

    #[test]
    fn salience_none_matches_explicit_default() {
        let p = DecayParams::default();
        assert!(
            (retention_score(&p, 42.0, 3, Some(7.0), None)
                - retention_score(&p, 42.0, 3, Some(7.0), Some(p.salience_default)))
            .abs()
                < 1e-12,
            "NULL salience must read exactly as salience_default",
        );
    }

    #[test]
    fn feedback_steps_salience_within_bounds() {
        let p = DecayParams::default();
        // Helpful steps up and saturates at the ceiling.
        let mut s = salience_after_feedback(&p, None, FeedbackKind::Helpful);
        assert!((s - (p.salience_default + SALIENCE_STEP)).abs() < 1e-9);
        for _ in 0..20 {
            s = salience_after_feedback(&p, Some(s), FeedbackKind::Helpful);
        }
        assert!((s - SALIENCE_MAX).abs() < 1e-9, "got {s}");

        // Not-helpful steps down and saturates at the floor.
        let mut s = salience_after_feedback(&p, None, FeedbackKind::NotHelpful);
        assert!((s - (p.salience_default - SALIENCE_STEP)).abs() < 1e-9);
        for _ in 0..20 {
            s = salience_after_feedback(&p, Some(s), FeedbackKind::NotHelpful);
        }
        assert!((s - SALIENCE_MIN).abs() < 1e-9, "got {s}");

        // Stale/wrong drop straight to the floor from anywhere.
        for kind in [FeedbackKind::Stale, FeedbackKind::Wrong] {
            let s = salience_after_feedback(&p, Some(SALIENCE_MAX), kind);
            assert!((s - SALIENCE_MIN).abs() < 1e-9, "{kind:?} → {s}");
        }
    }

    #[test]
    fn floored_salience_keeps_a_fresh_page_above_the_cold_threshold() {
        // The floor must not make an actively-used page sweep-eligible:
        // feedback lowers confidence, the sweep decides eviction.
        let p = DecayParams::default();
        let fresh_floored = retention_score(&p, 0.0, 0, None, Some(SALIENCE_MIN));
        assert!(
            fresh_floored > p.cold_threshold,
            "fresh floored page should survive: {fresh_floored}",
        );
    }
}
