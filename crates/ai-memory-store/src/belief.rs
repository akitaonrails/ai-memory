//! Read-time belief-strength confidence (P2,
//! `docs/design-hindsight-borrowings.md` §3).
//!
//! Pure, deterministic, zero-LLM math — mirroring [`crate::decay`] — so the
//! monotonicity and anti-entrenchment guarantees are unit-testable without a
//! database. Given the evidence a page *version* has accrued (`page_evidence`,
//! V63) plus its count of live contradictions, it derives a bounded
//! `confidence` in `[0.0, CONFIDENCE_CAP]`.
//!
//! Confidence is derived at *read time* rather than stored: there is no mutable
//! scalar to write on every access and no second source of truth to keep in
//! sync — the append-only evidence rows are the only truth, and the number is
//! recomputed from them each query.
//!
//! # Anti-entrenchment (Hindsight's documented failure mode)
//! A popular-but-wrong belief must not pin itself at the top forever
//! (`research-hindsight.md` §5). Three guards are baked into [`confidence`]:
//!
//! * **Distinct sessions, not raw count.** Breadth is driven by the number of
//!   *distinct* supporting sessions; a page cited 50 times by one session is
//!   worth far less than one cited by 50 sessions, so a single loud operator
//!   cannot manufacture confidence.
//! * **Recency weighting.** The age of the *newest* sighting shades the score
//!   down toward a floor, so an old belief nobody has reaffirmed loses standing
//!   to a freshly reinforced one.
//! * **A hard cap below 1.0.** The support curve saturates and the result is
//!   clamped to [`CONFIDENCE_CAP`], so evidence *alone* can never pin a page's
//!   confidence — and thus its ranking authority — at the ceiling.
//!
//! A fourth guard lives in the caller, not here: a supersession always wins
//! regardless of count (invariant #16). Confidence only shades *ranking*; it
//! never gates whether a correction is written or returned, and the ranker
//! declines to apply the boost to superseded (stale) versions at all.

/// Distinct supporting sessions at which the support curve reaches ~63% of its
/// ceiling. Deliberately small: a belief backed by a handful of independent
/// sessions is already strongly supported, and the saturating shape means
/// each further session adds less — the diminishing-returns half of the
/// anti-entrenchment guard.
const SUPPORT_SATURATION: f64 = 3.0;

/// Credit a non-session evidence row (a directly-cited observation or a
/// reaffirming feedback row) contributes to breadth, relative to a distinct
/// session. Heavily discounted because those rows carry no independent-session
/// guarantee, so they cannot substitute for genuine cross-session breadth.
const NON_SESSION_CREDIT: f64 = 0.2;

/// Ceiling on the breadth non-session evidence can add, in distinct-session
/// equivalents. The anti-entrenchment core: a page cited by one session and a
/// hundred observations must not reach the confidence of one backed by many
/// independent sessions — raw non-session volume is capped so distinct breadth
/// always dominates.
const RESIDUAL_BREADTH_CAP: f64 = 1.0;

/// Recency time-constant: evidence this many seconds old has decayed most of
/// the way to [`RECENCY_FLOOR`]. 30 days — long enough that steady work keeps a
/// belief "fresh", short enough that a belief abandoned for a month visibly
/// loses standing.
const RECENCY_TAU_SECS: f64 = 60.0 * 60.0 * 24.0 * 30.0;

/// Floor of the recency multiplier: even the oldest evidence keeps this share
/// of its support. Recency *shades* standing; it never erases a real belief
/// (that is supersession's job, not aging's).
const RECENCY_FLOOR: f64 = 0.5;

/// Hard ceiling on derived confidence. Below 1.0 by construction so that
/// evidence can never pin a page at maximum authority — the confidence cap of
/// the anti-entrenchment guard.
pub const CONFIDENCE_CAP: f64 = 0.95;

/// The evidence a single page version has accrued, as read from the store for
/// one candidate page. All fields default to zero / `None`, which
/// [`confidence`] reads as "no evidence" → confidence `0.0` (neutral, i.e. no
/// ranking effect), never "unsupported".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BeliefInputs {
    /// Total `page_evidence` rows citing this page version.
    pub evidence_count: u32,
    /// Distinct supporting sessions — `COUNT(DISTINCT source_id)` over the
    /// `session` and `reconsolidation` evidence kinds. The breadth signal.
    pub distinct_sessions: u32,
    /// `MAX(created_at)` over the page's evidence rows, in Unix microseconds;
    /// `None` when the page has no evidence.
    pub newest_evidence_us: Option<i64>,
    /// Number of live (`contradicts` → latest page) contradictions this page
    /// declares. Each one weakens the belief.
    pub unresolved_contradictions: u32,
}

/// Derive a page version's belief-strength confidence in `[0.0, CONFIDENCE_CAP]`
/// from its evidence and contradictions, evaluated against `now_us`.
///
/// Monotone by construction: strictly increasing in distinct sessions and in
/// evidence count, strictly decreasing in the age of the newest evidence and in
/// the contradiction count. A page with no evidence returns `0.0`.
#[must_use]
pub fn confidence(inputs: &BeliefInputs, now_us: i64) -> f64 {
    // Breadth is driven by distinct sessions; non-session rows beyond that add
    // only a discounted residual. `saturating_sub` guards the (normal) case
    // where distinct sessions is a subset of the total row count, and the
    // (pathological) case where a caller reports more distinct sessions than
    // rows — the residual is then simply zero, never negative.
    let residual = f64::from(
        inputs
            .evidence_count
            .saturating_sub(inputs.distinct_sessions),
    );
    let residual_breadth = (NON_SESSION_CREDIT * residual).min(RESIDUAL_BREADTH_CAP);
    let breadth = f64::from(inputs.distinct_sessions) + residual_breadth;
    if breadth <= 0.0 {
        // No evidence at all: neutral, no ranking effect. Returning here also
        // keeps a page whose only "evidence" is an unresolved contradiction
        // from going negative.
        return 0.0;
    }

    // Saturating support: 1 - e^(-breadth/k) ∈ [0, 1), diminishing returns per
    // additional session — evidence alone cannot reach the ceiling.
    let support = 1.0 - (-breadth / SUPPORT_SATURATION).exp();

    // Recency: newest sighting shades the score toward RECENCY_FLOOR. `None`
    // is unreachable once breadth > 0 (a row implies a timestamp), but we treat
    // it as "unknown, not old" (multiplier 1.0) rather than penalizing.
    let recency = match inputs.newest_evidence_us {
        Some(ts) => {
            let age_secs = (now_us.saturating_sub(ts)).max(0) as f64 / 1_000_000.0;
            RECENCY_FLOOR + (1.0 - RECENCY_FLOOR) * (-age_secs / RECENCY_TAU_SECS).exp()
        }
        None => 1.0,
    };

    // Each live contradiction weakens the belief. 1/(1+n) is monotone
    // decreasing and bottoms out toward 0 without ever driving the whole score
    // negative.
    let contradiction = 1.0 / (1.0 + f64::from(inputs.unresolved_contradictions));

    (support * recency * contradiction).clamp(0.0, CONFIDENCE_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "now" so age-based terms are deterministic.
    const NOW: i64 = 1_900_000_000_000_000;
    const DAY_US: i64 = 60 * 60 * 24 * 1_000_000;

    fn inputs(distinct: u32, count: u32, age_days: i64, contradictions: u32) -> BeliefInputs {
        BeliefInputs {
            evidence_count: count,
            distinct_sessions: distinct,
            newest_evidence_us: Some(NOW - age_days * DAY_US),
            unresolved_contradictions: contradictions,
        }
    }

    #[test]
    fn no_evidence_is_zero_and_thus_neutral() {
        assert_eq!(confidence(&BeliefInputs::default(), NOW), 0.0);
        // An unresolved contradiction with no supporting evidence stays 0.0,
        // never negative.
        let only_contradiction = BeliefInputs {
            unresolved_contradictions: 3,
            ..BeliefInputs::default()
        };
        assert_eq!(confidence(&only_contradiction, NOW), 0.0);
    }

    #[test]
    fn more_distinct_sessions_is_strictly_higher() {
        // Stay in the unsaturated region; the cap is asserted separately.
        let mut prev = confidence(&inputs(1, 1, 0, 0), NOW);
        for n in 2..=8 {
            let c = confidence(&inputs(n, n, 0, 0), NOW);
            assert!(c > prev, "distinct={n}: {c} !> {prev}");
            prev = c;
        }
    }

    #[test]
    fn raw_count_matters_far_less_than_distinct_sessions() {
        // 50 sightings from ONE session must be worth much less than 50 from
        // 50 sessions — the core anti-entrenchment property.
        let one_loud = confidence(&inputs(1, 50, 0, 0), NOW);
        let many_distinct = confidence(&inputs(50, 50, 0, 0), NOW);
        assert!(
            many_distinct > one_loud,
            "distinct breadth {many_distinct} must beat one loud session {one_loud}"
        );
    }

    #[test]
    fn older_newest_evidence_is_strictly_lower() {
        let mut prev = confidence(&inputs(4, 4, 0, 0), NOW);
        for age in [1_i64, 7, 30, 90, 365, 3650] {
            let c = confidence(&inputs(4, 4, age, 0), NOW);
            assert!(c < prev, "age={age}d: {c} !< {prev}");
            prev = c;
        }
    }

    #[test]
    fn more_contradictions_is_strictly_lower() {
        let mut prev = confidence(&inputs(4, 4, 0, 0), NOW);
        for n in 1..=5 {
            let c = confidence(&inputs(4, 4, 0, n), NOW);
            assert!(c < prev, "contradictions={n}: {c} !< {prev}");
            prev = c;
        }
    }

    #[test]
    fn confidence_cap_is_respected_even_with_overwhelming_evidence() {
        let overwhelming = confidence(&inputs(100_000, 1_000_000, 0, 0), NOW);
        assert!(overwhelming <= CONFIDENCE_CAP, "{overwhelming} exceeds cap");
        assert!(overwhelming < 1.0, "confidence must never reach 1.0");
    }

    #[test]
    fn always_bounded() {
        for &distinct in &[0_u32, 1, 5, 100] {
            for &count in &[distinct, distinct + 10, distinct.saturating_mul(3)] {
                for &age in &[0_i64, 30, 1000] {
                    for &contra in &[0_u32, 1, 10] {
                        let c = confidence(&inputs(distinct, count, age, contra), NOW);
                        assert!((0.0..=CONFIDENCE_CAP).contains(&c), "out of range: {c}");
                    }
                }
            }
        }
    }

    #[test]
    fn recency_floor_keeps_ancient_evidence_supported() {
        // Even 100 years old, a broadly-supported belief keeps at least the
        // floor share of its support (aging shades; it does not erase).
        let ancient = confidence(&inputs(50, 50, 365 * 100, 0), NOW);
        let fresh = confidence(&inputs(50, 50, 0, 0), NOW);
        assert!(ancient > 0.0);
        assert!(ancient < fresh);
        assert!(ancient >= fresh * RECENCY_FLOOR * 0.99);
    }

    #[test]
    fn missing_timestamp_is_treated_as_unknown_not_old() {
        let no_ts = BeliefInputs {
            evidence_count: 4,
            distinct_sessions: 4,
            newest_evidence_us: None,
            unresolved_contradictions: 0,
        };
        // Same as a brand-new sighting: recency multiplier 1.0.
        assert!((confidence(&no_ts, NOW) - confidence(&inputs(4, 4, 0, 0), NOW)).abs() < 1e-12);
    }
}
