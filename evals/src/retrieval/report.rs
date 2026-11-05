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

use super::ingest::{DatePrefix, IngestMode, IngestStats};
use super::reachability::{QuestionReach, ReachSlice};
use super::score::{QuestionScore, SliceMetrics, percentile_u128};

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
    /// Haystack document vectors came from the eval embedding cache when
    /// present (query embeddings are always live).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub embedding_cache: bool,
    pub questions_scored: usize,
    pub abstention_excluded: usize,
    pub ks: Vec<usize>,
    /// QA-accuracy provenance (R2b): which provider/model answered and which
    /// graded. Names only — never keys. `None` on the default zero-LLM path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qa: Option<QaReportMeta>,
    /// Ingest volume and timing (server engine only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingest: Option<IngestSummary>,
    /// Evidence reachability (`--reachability` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachability: Option<ReachabilityReport>,
    /// Protocol, pinned models, stage timings and source-window provenance
    /// for the opt-in Raw-RAG engine. Absent on existing server/grep reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_rag: Option<super::raw_rag::Metadata>,
    pub slices: BTreeMap<String, SliceMetrics>,
    pub per_question: Vec<QuestionScore>,
}

/// `--reachability` results: per-slice aggregates plus every question.
#[derive(Debug, Serialize)]
pub struct ReachabilityReport {
    pub slices: BTreeMap<String, ReachSlice>,
    pub per_question: Vec<QuestionReach>,
}

/// Ingest throughput for one config run.
#[derive(Debug, Clone, Serialize)]
pub struct IngestSummary {
    pub mode: &'static str,
    pub date_prefix: &'static str,
    /// Questions ingested concurrently.
    pub concurrency: usize,
    /// Hook events posted, or pages written.
    pub items: usize,
    /// HTTP requests (batch POSTs or page writes).
    pub requests: usize,
    /// Items per second within one question's sequential ingest stream.
    pub items_per_sec_per_stream: f64,
    pub question_ingest_p50_ms: u128,
    pub question_ingest_p95_ms: u128,
    pub request_p50_ms: u128,
    pub request_p95_ms: u128,
    /// Whole config run (ingest + query + read-back), wall clock.
    pub run_wall_ms: u128,
    /// Page modes: documents larger than the 16 KiB observation backstop.
    pub docs_over_16kib: usize,
}

impl IngestSummary {
    pub fn new(
        mode: IngestMode,
        prefix: DatePrefix,
        concurrency: usize,
        stats: &[IngestStats],
        wall: std::time::Duration,
    ) -> Self {
        let items: usize = stats.iter().map(|s| s.items).sum();
        let stream_secs: f64 = stats.iter().map(|s| s.elapsed.as_secs_f64()).sum();
        let per_question: Vec<u128> = stats.iter().map(|s| s.elapsed.as_millis()).collect();
        let requests: Vec<u128> = stats
            .iter()
            .flat_map(|s| s.request_ms.iter().copied())
            .collect();
        Self {
            mode: mode.as_str(),
            date_prefix: prefix.as_str(),
            concurrency,
            items,
            requests: requests.len(),
            items_per_sec_per_stream: if stream_secs > 0.0 {
                items as f64 / stream_secs
            } else {
                0.0
            },
            question_ingest_p50_ms: percentile_u128(&per_question, 0.50),
            question_ingest_p95_ms: percentile_u128(&per_question, 0.95),
            request_p50_ms: percentile_u128(&requests, 0.50),
            request_p95_ms: percentile_u128(&requests, 0.95),
            run_wall_ms: wall.as_millis(),
            docs_over_16kib: stats.iter().map(|s| s.docs_over_16kib).sum(),
        }
    }
}

/// Ingest and reachability sections for one config, empty when absent.
fn extras_md(r: &Report) -> String {
    let mut md = String::new();
    if let Some(metadata) = &r.raw_rag {
        md.push_str(&super::raw_rag::markdown(metadata));
    }
    if let Some(i) = &r.ingest {
        md.push_str(&format!(
            "\n### Ingest\n\n- mode: {} (date prefix: {}), concurrency {}\n\
             - {} items in {} requests; {:.0} items/s per question stream\n\
             - per question p50/p95: {}/{} ms; per request p50/p95: {}/{} ms\n\
             - run wall clock: {:.1} s; documents over 16 KiB: {}\n",
            i.mode,
            i.date_prefix,
            i.concurrency,
            i.items,
            i.requests,
            i.items_per_sec_per_stream,
            i.question_ingest_p50_ms,
            i.question_ingest_p95_ms,
            i.request_p50_ms,
            i.request_p95_ms,
            i.run_wall_ms as f64 / 1000.0,
            i.docs_over_16kib,
        ));
        let raw = r.per_question.iter().filter(|q| q.raw_hits > 0).count();
        md.push_str(&format!(
            "- questions answered from the raw-observation fallback: {raw}/{}\n",
            r.per_question.len()
        ));
    }
    if let Some(reach) = &r.reachability {
        md.push_str(&super::reachability::to_markdown(&reach.slices));
    }
    md
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
    for k in &r.ks {
        md.push_str(&format!(" recall-all@{k} |"));
    }
    md.push_str(" p50 ms | p95 ms | ctx tok mean | ctx tok med |\n");
    md.push_str("|---|---|");
    for _ in r.ks.iter().chain(r.ks.iter()).chain(r.ks.iter()) {
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
        for k in &r.ks {
            md.push_str(&format!(" {:.3} |", m.recall_all_at[k]));
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
     systems publish); recall@k = fraction of evidence sessions found; \
     recall-all@k = all evidence sessions found in top k. \
     Latency is the `memory_query` MCP round trip (send→parse); ctx tok is \
     the estimated context an agent ingests from the result = chars/4 over \
     returned hit titles + snippets. Session attribution: `sessions/<id>.md` \
     pages and raw observation hits; unattributable pages never score. \
     Capture is production-shaped: excerpts bounded at the 2 KB privacy boundary.\n";

pub fn to_markdown(r: &Report) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "# {} retrieval — {}\n\n",
        r.dataset, r.generated_at
    ));
    md.push_str(&provenance_md(r));
    md.push_str(&format!("- mode: {}\n- config: {}\n", r.mode, r.config));
    if r.embedding_cache {
        md.push_str("- document embeddings: eval cache (`evals/.cache/embeddings`)\n");
    }
    md.push('\n');
    md.push_str(&slices_table_md(r));
    md.push_str(&qa_table_md(r));
    md.push_str(&extras_md(r));
    if r.raw_rag.is_none() {
        md.push_str(METRIC_NOTES);
    }
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
    pub recall_all_at: BTreeMap<usize, f64>,
    pub latency_p50_ms: i128,
    pub latency_p95_ms: i128,
    pub context_tokens_mean: f64,
    pub context_tokens_median: f64,
    /// Per-question hit@k changes keyed by k: `(gained, lost)`, questions
    /// the candidate hits and the baseline misses, and the reverse. A net
    /// delta built from many flips both ways is noisier than its size.
    pub hit_flips: BTreeMap<usize, (usize, usize)>,
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
    let mut recall_all_at = BTreeMap::new();
    for k in &candidate.ks {
        hit_at.insert(*k, c.hit_at[k] - b.hit_at[k]);
        recall_at.insert(*k, c.recall_at[k] - b.recall_at[k]);
        recall_all_at.insert(*k, c.recall_all_at[k] - b.recall_all_at[k]);
    }
    let base_by_id: BTreeMap<&str, &QuestionScore> = baseline
        .per_question
        .iter()
        .map(|q| (q.question_id.as_str(), q))
        .collect();
    let mut hit_flips = BTreeMap::new();
    for k in &candidate.ks {
        let (mut gained, mut lost) = (0, 0);
        for c in &candidate.per_question {
            let Some(b) = base_by_id.get(c.question_id.as_str()) else {
                continue;
            };
            match (b.hit_at.get(k) > Some(&0.0), c.hit_at.get(k) > Some(&0.0)) {
                (false, true) => gained += 1,
                (true, false) => lost += 1,
                _ => {}
            }
        }
        hit_flips.insert(*k, (gained, lost));
    }
    AbDelta {
        hit_flips,
        hit_at,
        recall_at,
        recall_all_at,
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
        "# {} retrieval A/B — {}\n\n",
        r.candidate.dataset, r.candidate.generated_at
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
    md.push_str(&extras_md(&r.baseline));
    md.push_str("\n## Candidate\n\n");
    md.push_str(&slices_table_md(&r.candidate));
    md.push_str(&qa_table_md(&r.candidate));
    md.push_str(&extras_md(&r.candidate));

    // Delta table over the `overall` slice.
    let b = overall(&r.baseline);
    let c = overall(&r.candidate);
    md.push_str("\n## Delta (overall, candidate − baseline)\n\n");
    md.push_str("| metric | baseline | candidate | delta |\n|---|---|---|---|\n");
    for k in &r.candidate.ks {
        let (gained, lost) = r.delta.hit_flips.get(k).copied().unwrap_or((0, 0));
        md.push_str(&format!(
            "| hit@{k} | {:.3} | {:.3} | {:+.3} (+{gained}/-{lost}) |\n",
            b.hit_at[k], c.hit_at[k], r.delta.hit_at[k]
        ));
    }
    for k in &r.candidate.ks {
        md.push_str(&format!(
            "| recall@{k} | {:.3} | {:.3} | {:+.3} |\n",
            b.recall_at[k], c.recall_at[k], r.delta.recall_at[k]
        ));
    }
    for k in &r.candidate.ks {
        md.push_str(&format!(
            "| recall-all@{k} | {:.3} | {:.3} | {:+.3} |\n",
            b.recall_all_at[k], c.recall_all_at[k], r.delta.recall_all_at[k]
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

    fn slice(questions: usize, hit5: f64, recall5: f64, recall_all5: f64) -> SliceMetrics {
        SliceMetrics {
            questions,
            hit_at: BTreeMap::from([(5, hit5)]),
            recall_at: BTreeMap::from([(5, recall5)]),
            recall_all_at: BTreeMap::from([(5, recall_all5)]),
            latency_p50_ms: 12,
            latency_p95_ms: 34,
            context_tokens_mean: 100.0,
            context_tokens_median: 90.0,
            qa: None,
        }
    }

    fn report(config: &str, hit5: f64, recall5: f64, recall_all5: f64) -> Report {
        let mut slices = BTreeMap::new();
        slices.insert("overall".to_string(), slice(2, hit5, recall5, recall_all5));
        Report {
            generated_at: "2026-09-01".into(),
            commit: "abc".into(),
            hardware: "cpu".into(),
            dataset: "longmemeval_s",
            dataset_sha256: crate::retrieval::dataset::LONGMEMEVAL_S_SHA256,
            mode: "zero-llm",
            config: config.into(),
            embedding_cache: false,
            questions_scored: 2,
            abstention_excluded: 1,
            ks: vec![5],
            qa: None,
            ingest: None,
            reachability: None,
            raw_rag: None,
            slices,
            per_question: vec![],
        }
    }

    #[test]
    fn coding_reports_name_the_selected_dataset() {
        let mut baseline = report("embeddings=none", 1.0, 1.0, 1.0);
        baseline.dataset = "coding_v1";
        let mut candidate = report("embeddings=none", 1.0, 1.0, 1.0);
        candidate.dataset = "coding_v1";
        assert!(to_markdown(&baseline).starts_with("# coding_v1 retrieval"));
        assert!(
            ab_to_markdown(&AbReport::new(baseline, candidate))
                .starts_with("# coding_v1 retrieval A/B")
        );
    }

    #[test]
    fn markdown_carries_provenance_and_the_triple() {
        let r = report("embeddings=none reranker=off", 0.5, 0.25, 0.0);
        let md = to_markdown(&r);
        assert!(md.starts_with("# longmemeval_s retrieval"));
        assert!(md.contains("commit: `abc`"));
        assert!(md.contains("config: embeddings=none reranker=off"));
        assert!(md.contains(" hit@5 | recall@5 | recall-all@5 |"));
        // Accuracy, latency and context-token columns remain aligned.
        assert!(
            md.contains("| overall | 2 | 0.500 | 0.250 | 0.000 | 12 | 34 | 100.0 | 90.0 |"),
            "{md}"
        );
        assert!(md.contains("abstention questions excluded"));
        assert!(md.contains("ctx tok is"));
    }

    #[test]
    fn ab_delta_is_candidate_minus_baseline_and_zero_for_identical_runs() {
        let baseline = report("embeddings=none", 0.5, 0.25, 0.0);
        let candidate = report("embeddings=none", 0.5, 0.25, 0.0);
        let ab = AbReport::new(baseline, candidate);
        assert_eq!(ab.delta.hit_at[&5], 0.0);
        assert_eq!(ab.delta.recall_at[&5], 0.0);
        assert_eq!(ab.delta.recall_all_at[&5], 0.0);
        assert_eq!(ab.delta.latency_p50_ms, 0);
        assert_eq!(ab.delta.context_tokens_mean, 0.0);
        let md = ab_to_markdown(&ab);
        assert!(md.contains("A/B"));
        assert!(
            md.contains("| hit@5 | 0.500 | 0.500 | +0.000 (+0/-0) |"),
            "{md}"
        );
        assert!(md.contains("| recall-all@5 | 0.000 | 0.000 | +0.000 |"));
    }

    #[test]
    fn qa_table_renders_only_when_qa_ran_and_carries_no_key() {
        use super::super::score::QaSliceMetrics;
        let mut r = report("embeddings=none reranker=off", 0.5, 0.25, 0.0);
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
        let baseline = report("embeddings=none", 0.4, 0.20, 0.1);
        let candidate = report("embeddings=local", 0.6, 0.30, 0.2);
        let ab = AbReport::new(baseline, candidate);
        assert!((ab.delta.hit_at[&5] - 0.2).abs() < 1e-9);
        assert!((ab.delta.recall_all_at[&5] - 0.1).abs() < 1e-9);
        let md = ab_to_markdown(&ab);
        assert!(md.contains("| hit@5 | 0.400 | 0.600 | +0.200"), "{md}");
        assert!(md.contains("candidate: embeddings=local"));
        assert!(md.contains("| recall-all@5 | 0.100 | 0.200 | +0.100 |"));
        let json = serde_json::to_value(&ab).unwrap();
        assert_eq!(
            json["baseline"]["slices"]["overall"]["recall_all_at"]["5"],
            0.1
        );
        assert_eq!(
            json["candidate"]["slices"]["overall"]["recall_all_at"]["5"],
            0.2
        );
        assert_eq!(json["delta"]["recall_all_at"]["5"], 0.1);

        let reversed = AbReport::new(ab.candidate, ab.baseline);
        assert!(ab_to_markdown(&reversed).contains("| recall-all@5 | 0.200 | 0.100 | -0.100 |"));
        assert_eq!(
            serde_json::to_value(&reversed).unwrap()["delta"]["recall_all_at"]["5"],
            -0.1
        );
    }

    fn question(id: &str, hit5: f64) -> QuestionScore {
        QuestionScore {
            question_id: id.into(),
            question_type: "t".into(),
            hit_at: BTreeMap::from([(5, hit5)]),
            recall_at: BTreeMap::from([(5, hit5)]),
            recall_all_at: BTreeMap::from([(5, hit5)]),
            retrieved_sessions: 1,
            latency_ms: 0,
            context_tokens: 0,
            ingest_ms: 0,
            raw_hits: 0,
            qa: None,
        }
    }

    #[test]
    fn ab_delta_counts_paired_flips_both_ways() {
        let mut baseline = report("b", 0.5, 0.5, 0.5);
        baseline.per_question = vec![
            question("q1", 1.0),
            question("q2", 0.0),
            question("q3", 1.0),
        ];
        let mut candidate = report("c", 0.5, 0.5, 0.5);
        candidate.per_question = vec![
            question("q1", 0.0),
            question("q2", 1.0),
            question("q3", 1.0),
        ];
        let ab = AbReport::new(baseline, candidate);
        assert_eq!(ab.delta.hit_flips[&5], (1, 1));
        assert!(ab_to_markdown(&ab).contains("(+1/-1)"));
    }

    #[test]
    fn json_carries_per_question_and_slice_recall_all() {
        let mut r = report("c", 0.5, 0.5, 0.5);
        r.per_question = vec![question("q1", 1.0), question("q2", 0.0)];
        r.slices = super::super::score::aggregate(&r.per_question, &r.ks);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["per_question"][0]["recall_all_at"]["5"], 1.0);
        assert_eq!(json["per_question"][1]["recall_all_at"]["5"], 0.0);
        assert_eq!(json["slices"]["t"]["recall_all_at"]["5"], 0.5);
        assert_eq!(json["slices"]["overall"]["recall_all_at"]["5"], 0.5);
    }

    #[test]
    fn ingest_summary_aggregates_streams_and_requests() {
        let stats = vec![
            IngestStats {
                items: 10,
                elapsed: std::time::Duration::from_millis(500),
                request_ms: vec![5, 7],
                docs_over_16kib: 1,
            },
            IngestStats {
                items: 30,
                elapsed: std::time::Duration::from_millis(1500),
                request_ms: vec![9],
                docs_over_16kib: 0,
            },
        ];
        let s = IngestSummary::new(
            IngestMode::WholeSession,
            DatePrefix::Turn,
            8,
            &stats,
            std::time::Duration::from_secs(3),
        );
        assert_eq!(s.items, 40);
        assert_eq!(s.requests, 3);
        assert!((s.items_per_sec_per_stream - 20.0).abs() < 1e-9);
        assert_eq!(s.request_p95_ms, 9);
        assert_eq!(s.docs_over_16kib, 1);
        assert_eq!(s.mode, "whole-session");
        let mut r = report("c", 0.5, 0.5, 0.5);
        r.ingest = Some(s);
        let md = to_markdown(&r);
        assert!(md.contains("### Ingest"));
        assert!(md.contains("\n- 40 items in 3 requests"), "{md}");
    }
}
