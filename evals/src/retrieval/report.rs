//! Render benchmark results as JSON (machine) + markdown (human), with
//! enough provenance (dataset sha, commit, hardware, config) that a
//! published number can be audited later.
//!
//! Two shapes:
//!
//! - [`Report`] — one config's run, reporting the R2 accuracy + latency +
//!   context-tokens triple per slice.
//! - [`AbReport`] — a baseline vs candidate pair over the same question
//!   set, adding a `overall`-slice delta table.

use std::collections::BTreeMap;

use serde::Serialize;

use super::score::{QuestionScore, SliceMetrics};

#[derive(Debug, Serialize)]
pub struct Report {
    pub generated_at: String,
    pub commit: String,
    pub hardware: String,
    pub dataset: &'static str,
    pub dataset_sha256: &'static str,
    pub mode: &'static str,
    /// Human-readable knob summary for this config (e.g.
    /// `embeddings=local reranker=off`).
    pub config: String,
    pub questions_scored: usize,
    pub abstention_excluded: usize,
    pub ks: Vec<usize>,
    /// QA-accuracy provenance (R2b): which provider/model answered and which
    /// graded. Names only — never keys. `None` on the default zero-LLM path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qa: Option<QaReportMeta>,
    pub slices: BTreeMap<String, SliceMetrics>,
    pub per_question: Vec<QuestionScore>,
}

/// QA-accuracy provenance for a run (R2b). Provider + model names only, so a
/// published report never carries a key.
#[derive(Debug, Clone, Serialize)]
pub struct QaReportMeta {
    /// Provider that synthesized/answered (e.g. `gemini`).
    pub answer_provider: String,
    /// Model that answered (e.g. `gemini-2.5-flash`).
    pub answer_model: String,
    /// Provider that graded (LLM-as-judge).
    pub grader_provider: String,
    /// Model that graded.
    pub grader_model: String,
}

pub fn commit_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

pub fn hardware() -> String {
    let model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
        })
        .unwrap_or_else(|| "unknown cpu".into());
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    format!("{model} ({threads} threads)")
}

/// Header block shared by single-config and A/B reports.
fn provenance_md(r: &Report) -> String {
    format!(
        "- commit: `{}`\n- dataset: `{}` (sha256 `{}…`)\n- hardware: {}\n\
         - questions scored: {} ({} abstention questions excluded)\n",
        r.commit,
        r.dataset,
        &r.dataset_sha256[..16],
        r.hardware,
        r.questions_scored,
        r.abstention_excluded,
    )
}

/// The per-slice triple table (accuracy + latency + context tokens).
fn slices_table_md(r: &Report) -> String {
    let mut md = String::new();
    md.push_str("| slice | n |");
    for k in &r.ks {
        md.push_str(&format!(" hit@{k} |"));
    }
    for k in &r.ks {
        md.push_str(&format!(" recall@{k} |"));
    }
    md.push_str(" p50 ms | p95 ms | ctx tok mean | ctx tok med |\n");
    md.push_str("|---|---|");
    for _ in r.ks.iter().chain(r.ks.iter()) {
        md.push_str("---|");
    }
    md.push_str("---|---|---|---|\n");
    for (name, m) in &r.slices {
        md.push_str(&format!("| {name} | {} |", m.questions));
        for k in &r.ks {
            md.push_str(&format!(" {:.3} |", m.hit_at[k]));
        }
        for k in &r.ks {
            md.push_str(&format!(" {:.3} |", m.recall_at[k]));
        }
        md.push_str(&format!(
            " {} | {} | {:.1} | {:.1} |\n",
            m.latency_p50_ms, m.latency_p95_ms, m.context_tokens_mean, m.context_tokens_median,
        ));
    }
    md
}

/// The per-slice QA-accuracy table (R2b). Only slices with graded questions
/// contribute rows; returns an empty string when the run had no QA at all.
fn qa_table_md(r: &Report) -> String {
    let Some(meta) = &r.qa else {
        return String::new();
    };
    let mut md = String::new();
    md.push_str(&format!(
        "\n### QA accuracy (live LLM)\n\n- answered by: {} `{}`\n- graded by: {} `{}`\n\n",
        meta.answer_provider, meta.answer_model, meta.grader_provider, meta.grader_model,
    ));
    md.push_str(
        "| slice | graded | correct | accuracy | ans p50 ms | ans p95 ms | ans tok mean |\n",
    );
    md.push_str("|---|---|---|---|---|---|---|\n");
    for (name, m) in &r.slices {
        if let Some(qa) = &m.qa {
            md.push_str(&format!(
                "| {name} | {} | {} | {:.3} | {} | {} | {:.1} |\n",
                qa.graded,
                qa.correct,
                qa.accuracy,
                qa.answer_latency_p50_ms,
                qa.answer_latency_p95_ms,
                qa.answer_tokens_mean,
            ));
        }
    }
    md
}

/// `overall`-slice QA accuracy, when the run graded any question.
fn overall_qa_accuracy(r: &Report) -> Option<f64> {
    r.slices
        .get("overall")
        .and_then(|m| m.qa.as_ref())
        .map(|q| q.accuracy)
}

const METRIC_NOTES: &str = "\nNotes: hit@k = any evidence session in top k (the \"Recall@k\" most \
     systems publish); recall@k = fraction of evidence sessions found. \
     Latency is the `memory_query` MCP round trip (send→parse); ctx tok is \
     the estimated context an agent ingests from the result = chars/4 over \
     returned hit titles + snippets. Session attribution: `sessions/<id>.md` \
     pages and raw observation hits; unattributable pages never score. \
     Capture is production-shaped: excerpts bounded at the 2 KB privacy boundary.\n";

pub fn to_markdown(r: &Report) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "# LongMemEval-S retrieval — {}\n\n",
        r.generated_at
    ));
    md.push_str(&provenance_md(r));
    md.push_str(&format!("- mode: {}\n- config: {}\n\n", r.mode, r.config));
    md.push_str(&slices_table_md(r));
    md.push_str(&qa_table_md(r));
    md.push_str(METRIC_NOTES);
    md
}

/// A baseline vs candidate run over the same question set.
#[derive(Debug, Serialize)]
pub struct AbReport {
    pub baseline: Report,
    pub candidate: Report,
    /// `overall`-slice baseline→candidate deltas, precomputed for machine
    /// consumers so a reader need not diff the two nested reports.
    pub delta: AbDelta,
}

/// candidate − baseline over the `overall` slice.
#[derive(Debug, Serialize)]
pub struct AbDelta {
    pub hit_at: BTreeMap<usize, f64>,
    pub recall_at: BTreeMap<usize, f64>,
    pub latency_p50_ms: i128,
    pub latency_p95_ms: i128,
    pub context_tokens_mean: f64,
    pub context_tokens_median: f64,
    /// candidate − baseline QA accuracy over the `overall` slice (R2b).
    /// `None` unless both runs graded the overall slice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qa_accuracy: Option<f64>,
}

impl AbReport {
    pub fn new(baseline: Report, candidate: Report) -> Self {
        let delta = compute_delta(&baseline, &candidate);
        Self {
            baseline,
            candidate,
            delta,
        }
    }
}

/// The `overall` slice, present in every non-empty run.
fn overall(r: &Report) -> &SliceMetrics {
    r.slices
        .get("overall")
        .expect("every scored run has an `overall` slice")
}

fn compute_delta(baseline: &Report, candidate: &Report) -> AbDelta {
    let b = overall(baseline);
    let c = overall(candidate);
    let mut hit_at = BTreeMap::new();
    let mut recall_at = BTreeMap::new();
    for k in &candidate.ks {
        hit_at.insert(*k, c.hit_at[k] - b.hit_at[k]);
        recall_at.insert(*k, c.recall_at[k] - b.recall_at[k]);
    }
    AbDelta {
        hit_at,
        recall_at,
        latency_p50_ms: c.latency_p50_ms as i128 - b.latency_p50_ms as i128,
        latency_p95_ms: c.latency_p95_ms as i128 - b.latency_p95_ms as i128,
        context_tokens_mean: c.context_tokens_mean - b.context_tokens_mean,
        context_tokens_median: c.context_tokens_median - b.context_tokens_median,
        qa_accuracy: match (
            overall_qa_accuracy(baseline),
            overall_qa_accuracy(candidate),
        ) {
            (Some(b), Some(c)) => Some(c - b),
            _ => None,
        },
    }
}

pub fn ab_to_markdown(r: &AbReport) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "# LongMemEval-S retrieval A/B — {}\n\n",
        r.candidate.generated_at
    ));
    // Provenance is shared (same dataset/commit/hardware/question set).
    md.push_str(&provenance_md(&r.candidate));
    md.push_str(&format!(
        "- baseline: {} ({})\n- candidate: {} ({})\n\n",
        r.baseline.config, r.baseline.mode, r.candidate.config, r.candidate.mode,
    ));

    md.push_str("## Baseline\n\n");
    md.push_str(&slices_table_md(&r.baseline));
    md.push_str(&qa_table_md(&r.baseline));
    md.push_str("\n## Candidate\n\n");
    md.push_str(&slices_table_md(&r.candidate));
    md.push_str(&qa_table_md(&r.candidate));

    // Delta table over the `overall` slice.
    let b = overall(&r.baseline);
    let c = overall(&r.candidate);
    md.push_str("\n## Delta (overall, candidate − baseline)\n\n");
    md.push_str("| metric | baseline | candidate | delta |\n|---|---|---|---|\n");
    for k in &r.candidate.ks {
        md.push_str(&format!(
            "| hit@{k} | {:.3} | {:.3} | {:+.3} |\n",
            b.hit_at[k], c.hit_at[k], r.delta.hit_at[k]
        ));
    }
    for k in &r.candidate.ks {
        md.push_str(&format!(
            "| recall@{k} | {:.3} | {:.3} | {:+.3} |\n",
            b.recall_at[k], c.recall_at[k], r.delta.recall_at[k]
        ));
    }
    md.push_str(&format!(
        "| latency p50 ms | {} | {} | {:+} |\n",
        b.latency_p50_ms, c.latency_p50_ms, r.delta.latency_p50_ms
    ));
    md.push_str(&format!(
        "| latency p95 ms | {} | {} | {:+} |\n",
        b.latency_p95_ms, c.latency_p95_ms, r.delta.latency_p95_ms
    ));
    md.push_str(&format!(
        "| ctx tok mean | {:.1} | {:.1} | {:+.1} |\n",
        b.context_tokens_mean, c.context_tokens_mean, r.delta.context_tokens_mean
    ));
    md.push_str(&format!(
        "| ctx tok median | {:.1} | {:.1} | {:+.1} |\n",
        b.context_tokens_median, c.context_tokens_median, r.delta.context_tokens_median
    ));
    if let (Some(bq), Some(cq), Some(dq)) = (b.qa.as_ref(), c.qa.as_ref(), r.delta.qa_accuracy) {
        md.push_str(&format!(
            "| qa accuracy | {:.3} | {:.3} | {:+.3} |\n",
            bq.accuracy, cq.accuracy, dq
        ));
    }
    md.push_str(METRIC_NOTES);
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(questions: usize, hit5: f64, recall5: f64) -> SliceMetrics {
        SliceMetrics {
            questions,
            hit_at: BTreeMap::from([(5, hit5)]),
            recall_at: BTreeMap::from([(5, recall5)]),
            latency_p50_ms: 12,
            latency_p95_ms: 34,
            context_tokens_mean: 100.0,
            context_tokens_median: 90.0,
            qa: None,
        }
    }

    fn report(config: &str, hit5: f64, recall5: f64) -> Report {
        let mut slices = BTreeMap::new();
        slices.insert("overall".to_string(), slice(2, hit5, recall5));
        Report {
            generated_at: "2026-09-01".into(),
            commit: "abc".into(),
            hardware: "cpu".into(),
            dataset: "longmemeval_s",
            dataset_sha256: crate::retrieval::dataset::LONGMEMEVAL_S_SHA256,
            mode: "zero-llm",
            config: config.into(),
            questions_scored: 2,
            abstention_excluded: 1,
            ks: vec![5],
            qa: None,
            slices,
            per_question: vec![],
        }
    }

    #[test]
    fn markdown_carries_provenance_and_the_triple() {
        let r = report("embeddings=none reranker=off", 0.5, 0.25);
        let md = to_markdown(&r);
        assert!(md.contains("commit: `abc`"));
        assert!(md.contains("config: embeddings=none reranker=off"));
        // triple row: hit@5 recall@5 p50 p95 ctx-mean ctx-median
        assert!(
            md.contains("| overall | 2 | 0.500 | 0.250 | 12 | 34 | 100.0 | 90.0 |"),
            "{md}"
        );
        assert!(md.contains("abstention questions excluded"));
        assert!(md.contains("ctx tok is"));
    }

    #[test]
    fn ab_delta_is_candidate_minus_baseline_and_zero_for_identical_runs() {
        let baseline = report("embeddings=none", 0.5, 0.25);
        let candidate = report("embeddings=none", 0.5, 0.25);
        let ab = AbReport::new(baseline, candidate);
        assert_eq!(ab.delta.hit_at[&5], 0.0);
        assert_eq!(ab.delta.recall_at[&5], 0.0);
        assert_eq!(ab.delta.latency_p50_ms, 0);
        assert_eq!(ab.delta.context_tokens_mean, 0.0);
        let md = ab_to_markdown(&ab);
        assert!(md.contains("A/B"));
        assert!(md.contains("| hit@5 | 0.500 | 0.500 | +0.000 |"), "{md}");
    }

    #[test]
    fn qa_table_renders_only_when_qa_ran_and_carries_no_key() {
        use super::super::score::QaSliceMetrics;
        let mut r = report("embeddings=none reranker=off", 0.5, 0.25);
        // No QA → no QA section.
        assert!(!to_markdown(&r).contains("QA accuracy"));
        // Attach QA.
        r.qa = Some(QaReportMeta {
            answer_provider: "gemini".into(),
            answer_model: "gemini-2.5-flash".into(),
            grader_provider: "gemini".into(),
            grader_model: "gemini-2.5-flash".into(),
        });
        r.slices.get_mut("overall").unwrap().qa = Some(QaSliceMetrics {
            graded: 2,
            correct: 1,
            accuracy: 0.5,
            answer_latency_p50_ms: 800,
            answer_latency_p95_ms: 1200,
            answer_tokens_mean: 42.0,
        });
        let md = to_markdown(&r);
        assert!(md.contains("QA accuracy (live LLM)"), "{md}");
        assert!(
            md.contains("answered by: gemini `gemini-2.5-flash`"),
            "{md}"
        );
        assert!(
            md.contains("| overall | 2 | 1 | 0.500 | 800 | 1200 | 42.0 |"),
            "{md}"
        );
    }

    #[test]
    fn ab_delta_reports_a_real_difference() {
        let baseline = report("embeddings=none", 0.4, 0.20);
        let candidate = report("embeddings=local", 0.6, 0.30);
        let ab = AbReport::new(baseline, candidate);
        assert!((ab.delta.hit_at[&5] - 0.2).abs() < 1e-9);
        let md = ab_to_markdown(&ab);
        assert!(md.contains("| hit@5 | 0.400 | 0.600 | +0.200 |"), "{md}");
        assert!(md.contains("candidate: embeddings=local"));
    }
}
