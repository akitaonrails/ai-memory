//! Session-level retrieval scoring, matching how published LongMemEval
//! retrieval numbers are reported: for each question, did the evidence
//! sessions surface in the top k retrieved results?
//!
//! Two metrics per k:
//!
//! - `hit@k` — 1.0 when ANY evidence session appears in the top k
//!   (the "Recall@k" most memory systems publish);
//! - `recall@k` — the fraction of that question's evidence sessions
//!   found in the top k (stricter on multi-session questions).
//!
//! Abstention questions have no evidence sessions and are excluded
//! from both (reported separately as a count).

use std::collections::{BTreeMap, HashSet};

use serde::Serialize;

use super::qa::QaOutcome;
use super::query::Retrieved;

/// Per-question outcome, ready for aggregation.
#[derive(Debug, Clone, Serialize)]
pub struct QuestionScore {
    pub question_id: String,
    pub question_type: String,
    /// hit@k and recall@k keyed by k.
    pub hit_at: BTreeMap<usize, f64>,
    pub recall_at: BTreeMap<usize, f64>,
    /// Deduped session attributions actually retrieved (for forensics).
    pub retrieved_sessions: usize,
    /// `memory_query` round-trip latency for this question, milliseconds.
    pub latency_ms: u128,
    /// Estimated context tokens the result would cost the agent (chars/4
    /// over returned hit titles + snippets).
    pub context_tokens: usize,
    /// End-to-end QA outcome (R2b, opt-in): a candidate answer graded
    /// against the gold answer. `None` on the default zero-LLM path and for
    /// any question whose QA step failed (logged, left ungraded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa: Option<QaOutcome>,
}

/// Score one question's retrieval run.
///
/// `evidence` holds the stored (uuid-mapped) ids of the answer sessions;
/// `retrieved` is the ranked flattened result list. Duplicate session
/// attributions collapse to their best (first) rank — retrieving three
/// chunks of one session fills one slot, not three.
pub fn score_question(
    question_id: &str,
    question_type: &str,
    evidence: &HashSet<uuid::Uuid>,
    retrieved: &[Retrieved],
    ks: &[usize],
    latency_ms: u128,
    context_tokens: usize,
) -> QuestionScore {
    let mut seen = HashSet::new();
    let mut ranked_sessions = Vec::new();
    for r in retrieved {
        if let Some(sid) = r.session_uuid
            && seen.insert(sid)
        {
            ranked_sessions.push(sid);
        }
    }
    let mut hit_at = BTreeMap::new();
    let mut recall_at = BTreeMap::new();
    for &k in ks {
        let top: HashSet<&uuid::Uuid> = ranked_sessions.iter().take(k).collect();
        let found = evidence.iter().filter(|e| top.contains(e)).count();
        hit_at.insert(k, if found > 0 { 1.0 } else { 0.0 });
        recall_at.insert(
            k,
            if evidence.is_empty() {
                0.0
            } else {
                found as f64 / evidence.len() as f64
            },
        );
    }
    QuestionScore {
        question_id: question_id.to_string(),
        question_type: question_type.to_string(),
        hit_at,
        recall_at,
        retrieved_sessions: ranked_sessions.len(),
        latency_ms,
        context_tokens,
        // R2a scoring is answer-agnostic; the QA path attaches its outcome
        // afterward, keeping this function pure and deterministic.
        qa: None,
    }
}

/// Aggregated metrics for one slice (a category, or overall): the R2
/// accuracy + latency + context-tokens triple.
#[derive(Debug, Clone, Serialize)]
pub struct SliceMetrics {
    pub questions: usize,
    pub hit_at: BTreeMap<usize, f64>,
    pub recall_at: BTreeMap<usize, f64>,
    /// Latency percentiles over the slice's per-question `memory_query`
    /// round trips, milliseconds.
    pub latency_p50_ms: u128,
    pub latency_p95_ms: u128,
    /// Context-token estimate central tendency over the slice.
    pub context_tokens_mean: f64,
    pub context_tokens_median: f64,
    /// End-to-end QA metrics over the slice (R2b). `None` unless QA mode ran
    /// and at least one question in the slice was graded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qa: Option<QaSliceMetrics>,
}

/// Aggregated QA-accuracy metrics for one slice (R2b): fraction of graded
/// answers the LLM judge marked correct, plus the answer-path latency and
/// token estimates so the accuracy/latency/context triple stays meaningful
/// for the synthesized-answer path, not only for retrieval.
#[derive(Debug, Clone, Serialize)]
pub struct QaSliceMetrics {
    /// Questions in the slice that produced a graded answer.
    pub graded: usize,
    /// Of the graded, how many the judge marked correct.
    pub correct: usize,
    /// `correct / graded` (0.0 when nothing was graded).
    pub accuracy: f64,
    /// Answer-step latency percentiles over graded questions, milliseconds.
    pub answer_latency_p50_ms: u128,
    pub answer_latency_p95_ms: u128,
    /// Mean answer-token estimate (chars/4 over synthesis prompt + answer).
    pub answer_tokens_mean: f64,
}

impl QaSliceMetrics {
    /// Aggregate the graded QA outcomes in one slice, or `None` when the
    /// slice has no graded questions.
    fn from_group(group: &[&QuestionScore]) -> Option<Self> {
        let graded: Vec<&QaOutcome> = group.iter().filter_map(|s| s.qa.as_ref()).collect();
        if graded.is_empty() {
            return None;
        }
        let correct = graded.iter().filter(|o| o.correct).count();
        let latencies: Vec<u128> = graded.iter().map(|o| o.answer_latency_ms).collect();
        let tokens_sum: usize = graded.iter().map(|o| o.answer_tokens).sum();
        Some(Self {
            graded: graded.len(),
            correct,
            accuracy: correct as f64 / graded.len() as f64,
            answer_latency_p50_ms: percentile_u128(&latencies, 0.50),
            answer_latency_p95_ms: percentile_u128(&latencies, 0.95),
            answer_tokens_mean: tokens_sum as f64 / graded.len() as f64,
        })
    }
}

/// Nearest-rank percentile (`p` in 0.0..=1.0) over a copy-sorted slice.
/// Empty input yields 0.
fn percentile_u128(values: &[u128], p: f64) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    // Nearest-rank: rank = ceil(p * n), clamped to [1, n], 1-based.
    let n = sorted.len();
    let rank = (p * n as f64).ceil().max(1.0) as usize;
    sorted[rank.min(n) - 1]
}

/// Median of an already-collected list of counts (mean of the two middle
/// elements for an even count). Empty input yields 0.
fn median_f64(values: &[usize]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2] as f64
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) as f64 / 2.0
    }
}

/// Macro-average scores per category plus an `overall` slice.
pub fn aggregate(scores: &[QuestionScore], ks: &[usize]) -> BTreeMap<String, SliceMetrics> {
    let mut slices: BTreeMap<String, Vec<&QuestionScore>> = BTreeMap::new();
    for s in scores {
        slices.entry(s.question_type.clone()).or_default().push(s);
        slices.entry("overall".to_string()).or_default().push(s);
    }
    slices
        .into_iter()
        .map(|(name, group)| {
            let n = group.len() as f64;
            let mut hit_at = BTreeMap::new();
            let mut recall_at = BTreeMap::new();
            for &k in ks {
                hit_at.insert(k, group.iter().map(|s| s.hit_at[&k]).sum::<f64>() / n);
                recall_at.insert(k, group.iter().map(|s| s.recall_at[&k]).sum::<f64>() / n);
            }
            let latencies: Vec<u128> = group.iter().map(|s| s.latency_ms).collect();
            let tokens: Vec<usize> = group.iter().map(|s| s.context_tokens).collect();
            let tokens_sum: usize = tokens.iter().sum();
            (
                name,
                SliceMetrics {
                    questions: group.len(),
                    hit_at,
                    recall_at,
                    latency_p50_ms: percentile_u128(&latencies, 0.50),
                    latency_p95_ms: percentile_u128(&latencies, 0.95),
                    context_tokens_mean: tokens_sum as f64 / n,
                    context_tokens_median: median_f64(&tokens),
                    qa: QaSliceMetrics::from_group(&group),
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retrieved(ids: &[Option<uuid::Uuid>]) -> Vec<Retrieved> {
        ids.iter()
            .map(|session_uuid| Retrieved {
                session_uuid: *session_uuid,
            })
            .collect()
    }

    #[test]
    fn a_top_one_hit_scores_across_all_ks() {
        let e = uuid::Uuid::now_v7();
        let s = score_question(
            "q",
            "single-session-user",
            &HashSet::from([e]),
            &retrieved(&[Some(e)]),
            &[1, 5],
            0,
            0,
        );
        assert_eq!(s.hit_at[&1], 1.0);
        assert_eq!(s.hit_at[&5], 1.0);
        assert_eq!(s.recall_at[&1], 1.0);
    }

    #[test]
    fn evidence_below_the_cutoff_does_not_count() {
        let e = uuid::Uuid::now_v7();
        let others: Vec<Option<uuid::Uuid>> = (0..3).map(|_| Some(uuid::Uuid::now_v7())).collect();
        let mut all = others.clone();
        all.push(Some(e)); // rank 4 (0-based 3)
        let s = score_question(
            "q",
            "multi-session",
            &HashSet::from([e]),
            &retrieved(&all),
            &[1, 3, 5],
            0,
            0,
        );
        assert_eq!(s.hit_at[&1], 0.0);
        assert_eq!(s.hit_at[&3], 0.0);
        assert_eq!(s.hit_at[&5], 1.0);
    }

    #[test]
    fn duplicate_session_chunks_fill_one_slot_not_three() {
        let e = uuid::Uuid::now_v7();
        let noise = uuid::Uuid::now_v7();
        // three chunks of the same noise session ahead of the evidence
        let s = score_question(
            "q",
            "multi-session",
            &HashSet::from([e]),
            &retrieved(&[Some(noise), Some(noise), Some(noise), Some(e)]),
            &[2],
            0,
            0,
        );
        // dedup: noise occupies rank 0, evidence rank 1 → hit@2
        assert_eq!(s.hit_at[&2], 1.0);
    }

    #[test]
    fn partial_multi_session_recall_is_fractional() {
        let e1 = uuid::Uuid::now_v7();
        let e2 = uuid::Uuid::now_v7();
        let s = score_question(
            "q",
            "multi-session",
            &HashSet::from([e1, e2]),
            &retrieved(&[Some(e1)]),
            &[5],
            0,
            0,
        );
        assert_eq!(s.recall_at[&5], 0.5);
        assert_eq!(s.hit_at[&5], 1.0);
    }

    #[test]
    fn unattributed_pages_never_score() {
        let e = uuid::Uuid::now_v7();
        let s = score_question(
            "q",
            "single-session-user",
            &HashSet::from([e]),
            &retrieved(&[None, None]),
            &[5],
            0,
            0,
        );
        assert_eq!(s.hit_at[&5], 0.0);
        assert_eq!(s.retrieved_sessions, 0);
    }

    /// Control: a scorer that credited ANY retrieval would ace a shuffled
    /// result list; the real scorer must not.
    #[test]
    fn random_sessions_score_zero() {
        let e = uuid::Uuid::now_v7();
        let random: Vec<Option<uuid::Uuid>> = (0..10).map(|_| Some(uuid::Uuid::now_v7())).collect();
        let s = score_question(
            "q",
            "knowledge-update",
            &HashSet::from([e]),
            &retrieved(&random),
            &[1, 3, 5, 10],
            0,
            0,
        );
        for k in [1, 3, 5, 10] {
            assert_eq!(s.hit_at[&k], 0.0, "hit@{k}");
            assert_eq!(s.recall_at[&k], 0.0, "recall@{k}");
        }
    }

    #[test]
    fn aggregation_macro_averages_per_category_and_overall() {
        let e = uuid::Uuid::now_v7();
        let hit = score_question(
            "q1",
            "multi-session",
            &HashSet::from([e]),
            &retrieved(&[Some(e)]),
            &[1],
            10,
            100,
        );
        let miss = score_question(
            "q2",
            "multi-session",
            &HashSet::from([uuid::Uuid::now_v7()]),
            &retrieved(&[]),
            &[1],
            30,
            200,
        );
        let agg = aggregate(&[hit, miss], &[1]);
        assert_eq!(agg["multi-session"].hit_at[&1], 0.5);
        assert_eq!(agg["overall"].questions, 2);
        // Latency + context-tokens triple is aggregated alongside accuracy.
        assert_eq!(agg["overall"].context_tokens_mean, 150.0);
        assert_eq!(agg["overall"].context_tokens_median, 150.0);
        // Nearest-rank p50 over [10, 30] → rank ceil(0.5*2)=1 → 10.
        assert_eq!(agg["overall"].latency_p50_ms, 10);
        assert_eq!(agg["overall"].latency_p95_ms, 30);
    }

    fn qa(correct: bool, latency_ms: u128, tokens: usize) -> QaOutcome {
        QaOutcome {
            correct,
            answer_source: "synthesized",
            answer: "a".into(),
            grade_reason: "r".into(),
            answer_latency_ms: latency_ms,
            answer_tokens: tokens,
        }
    }

    #[test]
    fn qa_aggregates_only_over_graded_questions() {
        let e = uuid::Uuid::now_v7();
        let mut graded = score_question(
            "q1",
            "single-session-user",
            &HashSet::from([e]),
            &retrieved(&[Some(e)]),
            &[1],
            10,
            100,
        );
        graded.qa = Some(qa(true, 800, 40));
        let mut also_graded = score_question(
            "q2",
            "single-session-user",
            &HashSet::from([uuid::Uuid::now_v7()]),
            &retrieved(&[]),
            &[1],
            20,
            50,
        );
        also_graded.qa = Some(qa(false, 1200, 60));
        // A third question with QA off (None) must not count toward graded.
        let ungraded = score_question(
            "q3",
            "single-session-user",
            &HashSet::from([uuid::Uuid::now_v7()]),
            &retrieved(&[]),
            &[1],
            30,
            10,
        );

        let agg = aggregate(&[graded, also_graded, ungraded], &[1]);
        let qa = agg["overall"].qa.as_ref().expect("overall has QA");
        assert_eq!(qa.graded, 2);
        assert_eq!(qa.correct, 1);
        assert_eq!(qa.accuracy, 0.5);
        assert_eq!(qa.answer_tokens_mean, 50.0);
        assert_eq!(qa.answer_latency_p50_ms, 800);
        assert_eq!(qa.answer_latency_p95_ms, 1200);
    }

    #[test]
    fn qa_metrics_absent_when_no_question_was_graded() {
        let e = uuid::Uuid::now_v7();
        let s = score_question(
            "q",
            "single-session-user",
            &HashSet::from([e]),
            &retrieved(&[Some(e)]),
            &[1],
            0,
            0,
        );
        let agg = aggregate(&[s], &[1]);
        assert!(agg["overall"].qa.is_none());
    }

    #[test]
    fn percentile_nearest_rank_and_empty() {
        assert_eq!(percentile_u128(&[], 0.5), 0);
        assert_eq!(percentile_u128(&[42], 0.95), 42);
        let v: Vec<u128> = (1..=100).collect();
        assert_eq!(percentile_u128(&v, 0.50), 50);
        assert_eq!(percentile_u128(&v, 0.95), 95);
        assert_eq!(percentile_u128(&v, 1.0), 100);
    }

    #[test]
    fn median_even_and_odd() {
        assert_eq!(median_f64(&[]), 0.0);
        assert_eq!(median_f64(&[7]), 7.0);
        assert_eq!(median_f64(&[1, 2, 3]), 2.0);
        assert_eq!(median_f64(&[1, 2, 3, 10]), 2.5);
    }
}
