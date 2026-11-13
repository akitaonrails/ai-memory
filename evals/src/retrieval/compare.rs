//! `retrieval-compare`: pair two saved retrieval reports question by question.
//!
//! A release comparison publishes slice averages, and an average that moves by
//! a few questions cannot say whether the ranking changed or whether questions
//! near a cutoff traded places (#1854). This reads the `report.json` of two
//! single-config `retrieval` runs (the previous release and the candidate, or
//! two runs of one build) and lists every question whose first hit moved, with
//! the cutoffs it moved between. Moves of one cutoff in both directions,
//! spread across question types, look like boundary noise; moves that skip
//! cutoffs, go one way, or cluster in one type point at a ranking change.
//! With `--fts`, the candidate build's FTS report marks moved questions whose
//! row equals the FTS row exactly, which is what a query that ran without its
//! vector looks like.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(clap::Args, Debug)]
pub struct CompareArgs {
    /// The earlier run: its run directory or its `report.json`.
    #[arg(long)]
    baseline: PathBuf,

    /// The later run: its run directory or its `report.json`.
    #[arg(long)]
    candidate: PathBuf,

    /// The candidate build's FTS run, to mark moved questions whose candidate
    /// row matches it exactly.
    #[arg(long)]
    fts: Option<PathBuf>,
}

pub fn run(args: CompareArgs) -> Result<()> {
    let baseline = load(&args.baseline)?;
    let candidate = load(&args.candidate)?;
    let fts = args.fts.as_deref().map(load).transpose()?;
    print!(
        "{}",
        to_markdown(&compare(&baseline, &candidate, fts.as_ref())?)
    );
    Ok(())
}

/// The part of a saved single-config report a comparison reads. Reports from
/// earlier releases lack later fields (`recall_all_at`, `ingest_ms`), so only
/// what every release report carries is required.
#[derive(Debug, Deserialize)]
struct SavedReport {
    #[serde(default)]
    commit: String,
    #[serde(default)]
    config: String,
    #[serde(default)]
    mode: String,
    per_question: Vec<SavedQuestion>,
}

#[derive(Debug, Deserialize)]
struct SavedQuestion {
    question_id: String,
    question_type: String,
    hit_at: BTreeMap<usize, f64>,
    recall_at: BTreeMap<usize, f64>,
    #[serde(default)]
    retrieved_sessions: usize,
    #[serde(default)]
    latency_ms: u64,
    #[serde(default)]
    context_tokens: usize,
}

fn load(path: &Path) -> Result<SavedReport> {
    let file = if path.is_dir() {
        path.join("report.json")
    } else {
        path.to_path_buf()
    };
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    if value.get("per_question").is_none() && value.get("baseline").is_some() {
        bail!(
            "{} is an A/B report; compare the single-config runs instead",
            file.display()
        );
    }
    serde_json::from_value(value).with_context(|| format!("reading {}", file.display()))
}

/// The smallest cutoff a question hit at, or `None` when it missed them all.
fn first_hit(question: &SavedQuestion) -> Option<usize> {
    question
        .hit_at
        .iter()
        .find(|(_, hit)| **hit > 0.0)
        .map(|(k, _)| *k)
}

#[derive(Debug)]
struct Comparison {
    baseline: String,
    candidate: String,
    ks: Vec<usize>,
    paired: usize,
    only_baseline: Vec<String>,
    only_candidate: Vec<String>,
    cutoffs: Vec<CutoffRow>,
    moved: Vec<Moved>,
    by_type: BTreeMap<String, TypeCounts>,
    fts_checked: bool,
}

#[derive(Debug)]
struct CutoffRow {
    k: usize,
    baseline_hit: f64,
    candidate_hit: f64,
    gained: usize,
    lost: usize,
    baseline_recall: f64,
    candidate_recall: f64,
}

#[derive(Debug)]
struct Moved {
    question_id: String,
    question_type: String,
    before: Option<usize>,
    after: Option<usize>,
    /// How many cutoffs the first hit crossed; a miss counts as the cutoff
    /// after the largest.
    crossed: usize,
    improved: bool,
    recall_before: f64,
    recall_after: f64,
    latency_before: u64,
    latency_after: u64,
    same_as_fts: Option<bool>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TypeCounts {
    paired: usize,
    improved: usize,
    worsened: usize,
}

fn label(report: &SavedReport) -> String {
    let commit: String = report.commit.chars().take(8).collect();
    format!("{} ({}) at {commit}", report.config, report.mode)
}

fn measured_cutoffs(report: &SavedReport, side: &str) -> Result<Vec<usize>> {
    let expected: Vec<usize> = report
        .per_question
        .first()
        .map(|question| question.hit_at.keys().copied().collect())
        .unwrap_or_default();
    for question in &report.per_question {
        let hit: Vec<usize> = question.hit_at.keys().copied().collect();
        let recall: Vec<usize> = question.recall_at.keys().copied().collect();
        if hit != recall {
            bail!(
                "{side} question {} has different hit and recall cutoff grids: {hit:?} vs {recall:?}",
                question.question_id
            );
        }
        if hit != expected {
            bail!(
                "{side} question {} has cutoff grid {hit:?}, different from the report's {expected:?}",
                question.question_id
            );
        }
    }
    Ok(expected)
}

fn compare(
    baseline: &SavedReport,
    candidate: &SavedReport,
    fts: Option<&SavedReport>,
) -> Result<Comparison> {
    let baseline_cutoffs = measured_cutoffs(baseline, "baseline")?;
    let candidate_cutoffs = measured_cutoffs(candidate, "candidate")?;
    let fts_cutoffs = fts
        .map(|report| measured_cutoffs(report, "FTS"))
        .transpose()?;
    let by_id = |report: &SavedReport| -> BTreeMap<String, usize> {
        report
            .per_question
            .iter()
            .enumerate()
            .map(|(index, question)| (question.question_id.clone(), index))
            .collect()
    };
    let (base_ids, cand_ids) = (by_id(baseline), by_id(candidate));
    let fts_ids = fts.map(by_id);
    let mut ks: Vec<usize> = baseline_cutoffs
        .iter()
        .chain(&candidate_cutoffs)
        .copied()
        .collect();
    ks.sort_unstable();
    ks.dedup();
    let position = |hit: Option<usize>| hit.map_or(ks.len(), |k| ks.partition_point(|c| *c < k));
    let max_k = ks.last().copied().unwrap_or_default();

    let pairs: Vec<(&SavedQuestion, &SavedQuestion)> = baseline
        .per_question
        .iter()
        .filter_map(|b| Some((b, &candidate.per_question[*cand_ids.get(&b.question_id)?])))
        .collect();
    let paired = pairs.len();
    // A coarser grid makes identical first hits look like ranking moves, so
    // both reports must measure the same cutoffs.
    if paired > 0 && baseline_cutoffs != candidate_cutoffs {
        bail!(
            "baseline and candidate have incompatible cutoff grids: {baseline_cutoffs:?} vs {candidate_cutoffs:?}; compare reports generated with the same --ks"
        );
    }
    if let (Some(grid), Some(ids)) = (&fts_cutoffs, &fts_ids)
        && grid != &candidate_cutoffs
        && pairs
            .iter()
            .any(|(_, question)| ids.contains_key(&question.question_id))
    {
        bail!(
            "candidate and FTS have incompatible cutoff grids: {candidate_cutoffs:?} vs {grid:?}; compare reports generated with the same --ks"
        );
    }
    let mean = |values: &mut dyn Iterator<Item = f64>| {
        if paired == 0 {
            0.0
        } else {
            values.sum::<f64>() / paired as f64
        }
    };
    let hit = |question: &SavedQuestion, k: usize| question.hit_at[&k];
    let hits = |question: &SavedQuestion, k: usize| hit(question, k) > 0.0;
    let recall = |question: &SavedQuestion, k: usize| question.recall_at[&k];
    let cutoffs = ks
        .iter()
        .map(|&k| CutoffRow {
            k,
            baseline_hit: mean(&mut pairs.iter().map(|(b, _)| hit(b, k))),
            candidate_hit: mean(&mut pairs.iter().map(|(_, c)| hit(c, k))),
            gained: pairs
                .iter()
                .filter(|(b, c)| !hits(b, k) && hits(c, k))
                .count(),
            lost: pairs
                .iter()
                .filter(|(b, c)| hits(b, k) && !hits(c, k))
                .count(),
            baseline_recall: mean(&mut pairs.iter().map(|(b, _)| recall(b, k))),
            candidate_recall: mean(&mut pairs.iter().map(|(_, c)| recall(c, k))),
        })
        .collect();

    let mut moved = Vec::new();
    let mut by_type: BTreeMap<String, TypeCounts> = BTreeMap::new();
    for (b, c) in &pairs {
        let counts = by_type.entry(c.question_type.clone()).or_default();
        counts.paired += 1;
        let (before, after) = (first_hit(b), first_hit(c));
        if before == after {
            continue;
        }
        let (from, to) = (position(before), position(after));
        let improved = to < from;
        if improved {
            counts.improved += 1;
        } else {
            counts.worsened += 1;
        }
        let same_as_fts = fts.zip(fts_ids.as_ref()).map(|(fts, ids)| {
            ids.get(&c.question_id)
                .map(|&index| &fts.per_question[index])
                .is_some_and(|f| {
                    f.hit_at == c.hit_at
                        && f.recall_at == c.recall_at
                        && f.retrieved_sessions == c.retrieved_sessions
                        && f.context_tokens == c.context_tokens
                })
        });
        moved.push(Moved {
            question_id: c.question_id.clone(),
            question_type: c.question_type.clone(),
            before,
            after,
            crossed: from.abs_diff(to),
            improved,
            recall_before: recall(b, max_k),
            recall_after: recall(c, max_k),
            latency_before: b.latency_ms,
            latency_after: c.latency_ms,
            same_as_fts,
        });
    }
    // The widest moves first, losses before gains: those are the leads.
    moved.sort_by(|a, b| {
        b.crossed
            .cmp(&a.crossed)
            .then(a.improved.cmp(&b.improved))
            .then(a.question_type.cmp(&b.question_type))
            .then(a.question_id.cmp(&b.question_id))
    });
    let unpaired = |from: &SavedReport, other: &BTreeMap<String, usize>| -> Vec<String> {
        from.per_question
            .iter()
            .filter(|question| !other.contains_key(&question.question_id))
            .map(|question| question.question_id.clone())
            .collect()
    };
    Ok(Comparison {
        baseline: label(baseline),
        candidate: label(candidate),
        only_baseline: unpaired(baseline, &cand_ids),
        only_candidate: unpaired(candidate, &base_ids),
        ks,
        paired,
        cutoffs,
        moved,
        by_type,
        fts_checked: fts.is_some(),
    })
}

fn cutoff(hit: Option<usize>) -> String {
    hit.map_or_else(|| "miss".to_owned(), |k| format!("@{k}"))
}

fn to_markdown(c: &Comparison) -> String {
    let mut md = String::from("# Retrieval comparison\n\n");
    md.push_str(&format!(
        "- baseline: {}\n- candidate: {}\n- {} questions in both runs",
        c.baseline, c.candidate, c.paired
    ));
    for (side, ids) in [
        ("baseline", &c.only_baseline),
        ("candidate", &c.only_candidate),
    ] {
        if !ids.is_empty() {
            md.push_str(&format!("; {} only in the {side}", ids.len()));
        }
    }
    md.push_str("\n\n| metric | baseline | candidate | delta | gained | lost |\n");
    md.push_str("|---|---:|---:|---:|---:|---:|\n");
    for row in &c.cutoffs {
        md.push_str(&format!(
            "| hit@{} | {:.3} | {:.3} | {:+.3} | {} | {} |\n",
            row.k,
            row.baseline_hit,
            row.candidate_hit,
            row.candidate_hit - row.baseline_hit,
            row.gained,
            row.lost
        ));
    }
    for row in &c.cutoffs {
        md.push_str(&format!(
            "| recall@{} | {:.3} | {:.3} | {:+.3} | | |\n",
            row.k,
            row.baseline_recall,
            row.candidate_recall,
            row.candidate_recall - row.baseline_recall
        ));
    }

    let improved = c.moved.iter().filter(|m| m.improved).count();
    let one_cutoff = c.moved.iter().filter(|m| m.crossed == 1).count();
    md.push_str(&format!(
        "\n## Questions whose first hit moved\n\n{} moved: {improved} earlier, {} later; \
         {one_cutoff} by one cutoff.\n",
        c.moved.len(),
        c.moved.len() - improved
    ));
    if !c.moved.is_empty() {
        let max_k = c.ks.last().copied().unwrap_or_default();
        let fts_header = if c.fts_checked {
            " candidate row = FTS row |"
        } else {
            ""
        };
        md.push_str(&format!(
            "\n| question | type | first hit | cutoffs crossed | recall@{max_k} | \
             latency ms |{fts_header}\n|---|---|---|---:|---|---|{}\n",
            if c.fts_checked { "---|" } else { "" }
        ));
        for m in &c.moved {
            let fts_cell = match m.same_as_fts {
                Some(true) => " yes |",
                Some(false) => " no |",
                None => "",
            };
            md.push_str(&format!(
                "| {} | {} | {} → {} | {} | {:.2} → {:.2} | {} → {} |{fts_cell}\n",
                m.question_id,
                m.question_type,
                cutoff(m.before),
                cutoff(m.after),
                m.crossed,
                m.recall_before,
                m.recall_after,
                m.latency_before,
                m.latency_after,
            ));
        }
    }

    md.push_str("\n## By question type\n\n| type | questions | earlier | later |\n");
    md.push_str("|---|---:|---:|---:|\n");
    for (question_type, counts) in &c.by_type {
        md.push_str(&format!(
            "| {question_type} | {} | {} | {} |\n",
            counts.paired, counts.improved, counts.worsened
        ));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(id: &str, question_type: &str, first_hit: Option<usize>) -> serde_json::Value {
        let hit: serde_json::Map<String, serde_json::Value> = [1, 3, 5, 10]
            .into_iter()
            .map(|k| {
                let hit = first_hit.is_some_and(|first| k >= first);
                (
                    k.to_string(),
                    serde_json::json!(if hit { 1.0 } else { 0.0 }),
                )
            })
            .collect();
        serde_json::json!({
            "question_id": id,
            "question_type": question_type,
            "hit_at": hit.clone(),
            "recall_at": hit,
            "retrieved_sessions": 9,
            "latency_ms": 120,
            "context_tokens": 500,
        })
    }

    fn report(commit: &str, questions: Vec<serde_json::Value>) -> SavedReport {
        serde_json::from_value(serde_json::json!({
            "commit": commit,
            "config": "embeddings=local",
            "mode": "server",
            "per_question": questions,
        }))
        .unwrap()
    }

    fn measured_at(mut question: serde_json::Value, ks: &[usize]) -> serde_json::Value {
        for metric in ["hit_at", "recall_at"] {
            question[metric]
                .as_object_mut()
                .unwrap()
                .retain(|k, _| ks.contains(&k.parse::<usize>().unwrap()));
        }
        question
    }

    fn compare_files(
        baseline: Vec<serde_json::Value>,
        candidate: Vec<serde_json::Value>,
        fts: Option<Vec<serde_json::Value>>,
    ) -> Result<()> {
        let tmp = tempfile::TempDir::new().unwrap();
        let write = |name: &str, questions: Vec<serde_json::Value>| {
            let path = tmp.path().join(name);
            std::fs::write(
                &path,
                serde_json::json!({
                    "commit": "historical",
                    "config": name,
                    "mode": "server",
                    "per_question": questions,
                })
                .to_string(),
            )
            .unwrap();
            path
        };
        run(CompareArgs {
            baseline: write("baseline.json", baseline),
            candidate: write("candidate.json", candidate),
            fts: fts.map(|questions| write("fts.json", questions)),
        })
    }

    #[test]
    fn incompatible_cutoffs_do_not_become_zero_measurements() {
        let q = question("q", "user", Some(1));
        let error = compare_files(
            vec![measured_at(q.clone(), &[5])],
            vec![measured_at(q, &[1, 5])],
            None,
        )
        .expect_err("an unmeasured cutoff must not become a zero or a first-hit move");
        assert!(error.to_string().contains("cutoff"), "{error}");
    }

    #[test]
    fn incomplete_hit_or_recall_grids_are_refused_in_every_report() {
        let q = question("q", "user", Some(1));
        for side in 0..3 {
            for metric in ["hit_at", "recall_at"] {
                let mut malformed = q.clone();
                malformed[metric].as_object_mut().unwrap().remove("3");
                let mut rows = [vec![q.clone()], vec![q.clone()], vec![q.clone()]];
                rows[side] = vec![malformed];
                let [baseline, candidate, fts] = rows;
                let error = compare_files(baseline, candidate, Some(fts))
                    .expect_err("a missing metric cell must not be padded with zero");
                assert!(error.to_string().contains("q"), "{error}");
                assert!(error.to_string().contains("cutoff"), "{error}");
            }
        }
    }

    #[test]
    fn inconsistent_question_grids_are_refused_in_every_report() {
        let q1 = question("q1", "user", Some(1));
        let q2 = question("q2", "user", Some(1));
        for side in 0..3 {
            let mut rows = [
                vec![q1.clone(), q2.clone()],
                vec![q1.clone(), q2.clone()],
                vec![q1.clone(), q2.clone()],
            ];
            rows[side][1] = measured_at(q2.clone(), &[5]);
            let [baseline, candidate, fts] = rows;
            let error = compare_files(baseline, candidate, Some(fts))
                .expect_err("each row must contain the report's complete cutoff grid");
            assert!(error.to_string().contains("q2"), "{error}");
            assert!(error.to_string().contains("cutoff"), "{error}");
        }
    }

    #[test]
    fn an_unpaired_row_cannot_add_unmeasured_cutoffs_to_paired_rows() {
        let q = measured_at(question("paired", "user", Some(1)), &[5]);
        let extra = measured_at(question("unpaired", "user", Some(1)), &[1, 5]);
        compare_files(vec![q.clone(), extra], vec![q], None)
            .expect_err("unpaired rows must not introduce zero-filled paired measurements");
    }

    #[test]
    fn fts_marking_refuses_incompatible_cutoffs_for_overlapping_questions() {
        let q = question("q", "user", Some(1));
        compare_files(
            vec![question("q", "user", Some(5))],
            vec![q.clone()],
            Some(vec![measured_at(q, &[5])]),
        )
        .expect_err("an unmeasured FTS cutoff must not look like a different retrieval row");
    }

    #[test]
    fn compatible_historical_reports_keep_partial_and_disjoint_question_support() {
        // No `ks`, `recall_all_at`, or `ingest_ms`: older saved reports still
        // compare, and the config labels are deliberately different.
        let q1 = measured_at(question("q1", "user", Some(1)), &[5]);
        let q2 = measured_at(question("q2", "user", Some(1)), &[5]);
        compare_files(vec![q1.clone()], vec![q1.clone()], None).unwrap();
        compare_files(vec![q1.clone(), q2.clone()], vec![q1.clone()], None).unwrap();
        compare_files(
            vec![q1.clone()],
            vec![measured_at(question("q2", "user", Some(1)), &[1, 5])],
            None,
        )
        .unwrap();
        compare_files(vec![], vec![], None).unwrap();
        compare_files(vec![], vec![q1.clone()], None).unwrap();
        compare_files(vec![q1.clone()], vec![], None).unwrap();
        // An FTS run with no overlapping questions makes no paired claim.
        compare_files(
            vec![q1.clone()],
            vec![q1],
            Some(vec![measured_at(question("q2", "user", Some(1)), &[1, 5])]),
        )
        .unwrap();
    }

    #[test]
    fn moved_questions_carry_their_cutoffs_and_direction() {
        let baseline = report(
            "aaaaaaaaaaaa",
            vec![
                question("q1", "user", Some(5)),
                question("q2", "user", Some(10)),
                question("q3", "preference", None),
                question("q4", "temporal", Some(1)),
                question("q5", "temporal", Some(1)),
            ],
        );
        let candidate = report(
            "bbbbbbbbbbbb",
            vec![
                question("q1", "user", Some(10)),
                question("q2", "user", None),
                question("q3", "preference", Some(10)),
                question("q4", "temporal", None),
            ],
        );
        let c = compare(&baseline, &candidate, None).unwrap();

        assert_eq!(c.ks, vec![1, 3, 5, 10]);
        assert_eq!(c.paired, 4);
        assert_eq!(c.only_baseline, vec!["q5"]);
        assert!(c.only_candidate.is_empty());
        let summary: Vec<_> = c
            .moved
            .iter()
            .map(|m| {
                (
                    m.question_id.as_str(),
                    m.before,
                    m.after,
                    m.crossed,
                    m.improved,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("q4", Some(1), None, 4, false),
                ("q1", Some(5), Some(10), 1, false),
                ("q2", Some(10), None, 1, false),
                ("q3", None, Some(10), 1, true),
            ],
            "the widest move leads, then losses before gains"
        );
        let at10 = c.cutoffs.iter().find(|row| row.k == 10).unwrap();
        assert_eq!((at10.gained, at10.lost), (1, 2));
        assert!((at10.baseline_hit - 0.75).abs() < 1e-9);
        assert!((at10.candidate_hit - 0.5).abs() < 1e-9);
        assert_eq!(
            c.by_type["user"],
            TypeCounts {
                paired: 2,
                improved: 0,
                worsened: 2
            }
        );
        assert_eq!(c.by_type["preference"].improved, 1);

        let md = to_markdown(&c);
        assert!(md.contains("4 questions in both runs; 1 only in the baseline"));
        assert!(md.contains("4 moved: 1 earlier, 3 later; 3 by one cutoff."));
        assert!(md.contains("| hit@10 | 0.750 | 0.500 | -0.250 | 1 | 2 |"));
        assert!(md.contains("| q1 | user | @5 → @10 | 1 | 1.00 → 1.00 | 120 → 120 |"));
        assert!(md.contains("| q3 | preference | miss → @10 | 1 | 0.00 → 1.00 | 120 → 120 |"));
        assert!(md.contains("embeddings=local (server) at aaaaaaaa"));
    }

    #[test]
    fn the_fts_run_marks_moved_rows_it_matches_exactly() {
        let baseline = report(
            "a",
            vec![
                question("q1", "user", Some(5)),
                question("q2", "user", Some(5)),
            ],
        );
        let candidate = report(
            "b",
            vec![
                question("q1", "user", Some(10)),
                question("q2", "user", Some(10)),
            ],
        );
        let mut matching = question("q1", "user", Some(10));
        matching["latency_ms"] = serde_json::json!(15);
        let mut differing = question("q2", "user", Some(10));
        differing["retrieved_sessions"] = serde_json::json!(8);
        let fts = report("b", vec![matching, differing]);

        let c = compare(&baseline, &candidate, Some(&fts)).unwrap();
        let marks: Vec<(&str, Option<bool>)> = c
            .moved
            .iter()
            .map(|m| (m.question_id.as_str(), m.same_as_fts))
            .collect();
        assert_eq!(marks, vec![("q1", Some(true)), ("q2", Some(false))]);
        let md = to_markdown(&c);
        assert!(md.contains("candidate row = FTS row |"));
        assert!(md.contains("| q1 | user | @5 → @10 | 1 | 1.00 → 1.00 | 120 → 120 | yes |"));
        assert!(
            compare(&baseline, &candidate, None).unwrap().moved[0]
                .same_as_fts
                .is_none()
        );
    }

    #[test]
    fn saved_reports_load_from_a_run_dir_and_refuse_an_ab_report() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A report from before `recall_all_at` and `ingest_ms` existed.
        std::fs::write(
            tmp.path().join("report.json"),
            serde_json::json!({
                "commit": "c0ffee",
                "config": "embeddings=off",
                "mode": "server",
                "per_question": [question("q1", "user", Some(3))],
            })
            .to_string(),
        )
        .unwrap();
        let loaded = load(tmp.path()).unwrap();
        assert_eq!(loaded.per_question.len(), 1);
        assert_eq!(first_hit(&loaded.per_question[0]), Some(3));

        let ab = tmp.path().join("ab.json");
        std::fs::write(
            &ab,
            serde_json::json!({ "baseline": {}, "candidate": {}, "delta": {} }).to_string(),
        )
        .unwrap();
        let error = load(&ab).unwrap_err().to_string();
        assert!(error.contains("is an A/B report"), "{error}");
    }
}
