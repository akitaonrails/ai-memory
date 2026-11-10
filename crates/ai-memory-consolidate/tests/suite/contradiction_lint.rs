//! A5 zero-LLM contradiction detection, end to end through a real Store + Wiki.
//!
//! Invariants guarded (docs/design-memory-aging.md §A5):
//!
//! 1. **Band pair ⇒ candidate.** Two cold pages whose embeddings sit in the
//!    0.4–0.75 cosine-similarity band produce one advisory
//!    `similarity_candidate` naming both paths without claiming conflict or
//!    supersession.
//! 2. **Near-duplicate ⇒ NOT a contradiction.** A pair at ≥0.75 is A3
//!    dedup territory, never flagged here.
//! 3. **No embeddings ⇒ clean no-op** (never an error, never a provider call).
//! 4. **Advisory only (#16).** The detector writes no supersession and deletes
//!    nothing; the source pages are untouched.
//!
//! Embeddings are seeded directly into `page_embeddings` with controlled unit
//! vectors so the cosine similarity between two pages is exact and the band
//! boundaries are testable — the synthetic bag-of-words embedder cannot pin a
//! similarity to a chosen value.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ai_memory_consolidate::{EmbeddingCoord, LintOptions, run_lint};
use ai_memory_core::{ActorContext, PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider, LlmResult};
use ai_memory_store::{Store, f32_vec_to_bytes};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const PROVIDER: &str = "test-embedder";
const MODEL: &str = "unit-2d";
const DIM: u32 = 2;

struct Fixture {
    tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
}

async fn seed_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    Fixture {
        tmp,
        store,
        wiki,
        ws,
        proj,
    }
}

/// Write a semantic page (the tier the contradiction detector considers).
async fn write_semantic(fx: &Fixture, path: &str, body: &str) -> PageId {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": path}),
            body: body.to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some(path.to_string()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap()
}

/// Insert a controlled embedding row for a page under our test triple.
fn seed_embedding(fx: &Fixture, page_id: PageId, vector: &[f32]) {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    conn.execute(
        "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            page_id.as_bytes(),
            f32_vec_to_bytes(vector),
            PROVIDER,
            MODEL,
            DIM,
            jiff::Timestamp::now().as_microsecond(),
        ],
    )
    .unwrap();
}

fn coord() -> Option<EmbeddingCoord> {
    Some(EmbeddingCoord {
        provider: PROVIDER.into(),
        model: MODEL.into(),
        dim: DIM,
    })
}

fn latest_page_count(fx: &Fixture) -> i64 {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
        params![fx.ws.as_bytes(), fx.proj.as_bytes()],
        |row| row.get(0),
    )
    .unwrap()
}

fn options(embedding: Option<EmbeddingCoord>) -> LintOptions {
    options_with_band(
        embedding,
        ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
    )
}

fn options_with_band(
    embedding: Option<EmbeddingCoord>,
    band_min: f32,
    band_max: f32,
) -> LintOptions {
    LintOptions {
        // dry_run so the pass emits findings without writing a report page,
        // keeping the "nothing was mutated" assertion clean.
        dry_run: true,
        // No LLM in these tests — this is the zero-LLM detector.
        use_llm: false,
        decay: ai_memory_store::DecayParams::default(),
        embedding,
        contradiction_band_min: band_min,
        contradiction_band_max: band_max,
    }
}

/// Two cold pages in the band ⇒ one advisory similarity candidate naming
/// both without conflict or supersession claims; nothing is rewritten.
#[tokio::test]
async fn band_pair_yields_advisory_similarity_candidate() {
    let fx = seed_fixture().await;
    let old = write_semantic(&fx, "concepts/old-claim.md", "The retry budget is 3.").await;
    let new = write_semantic(&fx, "concepts/new-claim.md", "The retry budget is 5.").await;
    // dot([1,0],[0.6,0.8]) = 0.6 — squarely in the 0.4–0.75 band.
    seed_embedding(&fx, old, &[1.0, 0.0]);
    seed_embedding(&fx, new, &[0.6, 0.8]);
    let before = latest_page_count(&fx);
    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
    )
    .await
    .unwrap();

    let candidates: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "similarity_candidate")
        .collect();
    assert_eq!(
        candidates.len(),
        1,
        "one band pair ⇒ one similarity candidate: {:?}",
        report.findings
    );
    let f = candidates[0];
    assert_eq!(f.severity, "info", "similarity candidates are advisory");
    assert!(f.pages.contains(&"concepts/old-claim.md".to_string()));
    assert!(f.pages.contains(&"concepts/new-claim.md".to_string()));
    assert!(
        f.message.contains("does not establish a contradiction")
            && f.message.contains("or supersession"),
        "candidate must state the evidence limit: {}",
        f.message
    );
    assert!(!report.findings.iter().any(|f| f.kind == "contradiction"));

    // Advisory (#16): nothing deleted or superseded.
    assert_eq!(
        latest_page_count(&fx),
        before,
        "the detector must not add, delete, or supersede any page"
    );
}

/// A raised floor (as a same-domain / non-English store would configure)
/// suppresses the same 0.6-similarity pair the default 0.4 floor admits.
#[tokio::test]
async fn raised_floor_suppresses_the_band_pair_end_to_end() {
    let fx = seed_fixture().await;
    let old = write_semantic(&fx, "concepts/old-claim.md", "The retry budget is 3.").await;
    let new = write_semantic(&fx, "concepts/new-claim.md", "The retry budget is 5.").await;
    // dot([1,0],[0.6,0.8]) = 0.6 — inside the default band, below a 0.7 floor.
    seed_embedding(&fx, old, &[1.0, 0.0]);
    seed_embedding(&fx, new, &[0.6, 0.8]);

    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options_with_band(
            coord(),
            0.7,
            ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        ),
    )
    .await
    .unwrap();

    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.kind == "similarity_candidate"),
        "a 0.7 floor must drop the 0.6-similarity pair the default 0.4 floor admits: {:?}",
        report.findings
    );
}

/// A near-duplicate pair (cosine ≈ 0.98) is A3's dedup job, not a
/// contradiction — the detector must stay silent.
#[tokio::test]
async fn near_duplicate_is_not_a_contradiction() {
    let fx = seed_fixture().await;
    let a = write_semantic(&fx, "concepts/a.md", "alpha").await;
    let b = write_semantic(&fx, "concepts/b.md", "beta").await;
    seed_embedding(&fx, a, &[1.0, 0.0]);
    // dot ≈ 0.98 — above the band.
    seed_embedding(&fx, b, &[0.98, 0.199]);

    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
    )
    .await
    .unwrap();

    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.kind == "similarity_candidate"),
        "a near-duplicate must not be flagged as a contradiction: {:?}",
        report.findings
    );
}

/// No embeddings for the configured triple ⇒ the detector is a clean no-op
/// (not an error, no provider call). Also covers `embedding: None`.
#[tokio::test]
async fn no_embeddings_is_a_clean_no_op() {
    let fx = seed_fixture().await;
    write_semantic(&fx, "concepts/a.md", "The retry budget is 3.").await;
    write_semantic(&fx, "concepts/b.md", "The retry budget is 5.").await;
    // No embedding rows seeded at all.

    // A triple is configured, but there are no matching vectors → no-op.
    let with_coord = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
    )
    .await
    .unwrap();
    assert!(
        !with_coord
            .findings
            .iter()
            .any(|f| f.kind == "similarity_candidate"),
        "no embeddings ⇒ no contradiction findings: {:?}",
        with_coord.findings
    );

    // No embedder configured at all (embedding: None) → also a no-op.
    let without_coord = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(None),
    )
    .await
    .unwrap();
    assert!(
        !without_coord
            .findings
            .iter()
            .any(|f| f.kind == "similarity_candidate"),
        "embedding: None ⇒ no contradiction findings: {:?}",
        without_coord.findings
    );
}

/// An expired semantic page in the contradiction band must not produce a
/// contradiction finding against an active page.
#[tokio::test]
async fn contradiction_pass_skips_expired_pages() {
    let fx = seed_fixture().await;
    let live = write_semantic(&fx, "concepts/live-claim.md", "The retry budget is 5.").await;
    let expired = write_semantic(&fx, "concepts/expired-claim.md", "The retry budget is 3.").await;
    seed_embedding(&fx, live, &[1.0, 0.0]);
    seed_embedding(&fx, expired, &[0.6, 0.8]);

    {
        let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
        conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
        conn.execute(
            "UPDATE pages SET expires_at = 1 \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                fx.ws.as_bytes(),
                fx.proj.as_bytes(),
                "concepts/expired-claim.md"
            ],
        )
        .unwrap();
    }

    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
    )
    .await
    .unwrap();

    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.kind == "similarity_candidate"),
        "expired pages must not produce contradiction findings: {:?}",
        report.findings
    );
}

/// Records the prompt of every structured call and reports no findings.
struct PromptRecorder(Arc<Mutex<Vec<String>>>);

impl LlmProvider for PromptRecorder {
    fn name(&self) -> &'static str {
        "recorder"
    }
    fn model(&self) -> &str {
        "recorder"
    }
    fn complete<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatRequest,
    ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { unreachable!("the lint pass asks for structured output") })
    }
    fn complete_structured_raw<'life0, 'async_trait>(
        &'life0 self,
        request: ChatRequest,
        _schema: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let prompts = self.0.clone();
        Box::pin(async move {
            for message in &request.messages {
                prompts.lock().unwrap().push(message.content.clone());
            }
            Ok(serde_json::json!({ "findings": [] }))
        })
    }
}

/// The LLM pass audits knowledge pages. The previous run's `_lint/report.md`
/// is a semantic page too, but it is the lint's own output: fed back in, the
/// model can report the report, or re-emit stale findings that cite it.
#[tokio::test]
async fn llm_lint_pass_does_not_audit_the_previous_lint_report() {
    let fx = seed_fixture().await;
    write_semantic(&fx, "concepts/a.md", "Deploys run on Fridays.").await;
    write_semantic(&fx, "concepts/b.md", "Deploys never run on Fridays.").await;
    write_semantic(
        &fx,
        "_lint/report.md",
        "# Lint report\n\n- **warning**: stale finding from the last run",
    )
    .await;

    let prompts = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(PromptRecorder(prompts.clone()));
    run_lint(
        &fx.store.reader,
        &fx.wiki,
        Some(&llm),
        fx.ws,
        fx.proj,
        LintOptions {
            use_llm: true,
            ..options(None)
        },
    )
    .await
    .unwrap();

    let prompts = prompts.lock().unwrap();
    let prompt = prompts.join("\n");
    assert!(prompt.contains("concepts/a.md"), "the pass ran: {prompt}");
    assert!(!prompt.contains("_lint/report.md"), "{prompt}");
    assert!(
        !prompt.contains("stale finding from the last run"),
        "{prompt}"
    );
}

/// Manually labeled, synthetic pairs. Vectors are controlled inputs, not model
/// outputs: these cases measure the classification boundary, not embedding
/// quality or production precision. `expected_findings` records today's
/// behavior independently of the label; update it when semantics change.
///
/// A positive pair contains incompatible simultaneous values for the same
/// setting and scope. Compatible active pairs are negatives. Expired,
/// superseded and foreign-project pairs exercise eligibility and are excluded
/// from the classification metrics. A contradiction prediction is only a
/// `kind = contradiction` finding. A `similarity_candidate` is tracked
/// separately because it is not independent evidence of conflict. The
/// typed-edge control also contains conflicting prose, so its label does not
/// rely on the edge alone.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceCase {
    id: String,
    language: String,
    label: EvidenceLabel,
    setup: EvidenceSetup,
    first: String,
    second: String,
    second_vector: [f32; 2],
    expected_findings: usize,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceLabel {
    Conflict,
    Compatible,
    Ineligible,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceSetup {
    Active,
    Declared,
    Superseded,
    Expired,
    ForeignProject,
}

fn evidence_cases() -> Vec<EvidenceCase> {
    serde_json::from_str(include_str!("../fixtures/contradiction_evidence.json")).unwrap()
}

async fn seed_evidence_case(case: &EvidenceCase) -> Fixture {
    let fx = seed_fixture().await;
    let first = write_semantic(&fx, "decisions/first.md", &case.first).await;
    let project_id = if matches!(case.setup, EvidenceSetup::ForeignProject) {
        fx.store
            .writer
            .get_or_create_project(fx.ws, "foreign", None)
            .await
            .unwrap()
    } else {
        fx.proj
    };
    let path = if matches!(case.setup, EvidenceSetup::Superseded) {
        "decisions/first.md"
    } else {
        "decisions/second.md"
    };
    let frontmatter = if matches!(case.setup, EvidenceSetup::Declared) {
        serde_json::json!({"relations": {"contradicts": ["decisions/first.md"]}})
    } else {
        serde_json::json!({})
    };
    let second = fx
        .wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id,
            path: PagePath::new(path).unwrap(),
            frontmatter,
            body: case.second.clone(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some(path.into()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    if !matches!(case.setup, EvidenceSetup::Declared) {
        seed_embedding(&fx, first, &[1.0, 0.0]);
        seed_embedding(&fx, second, &case.second_vector);
    }
    if matches!(case.setup, EvidenceSetup::Expired) {
        let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
        conn.execute(
            "UPDATE pages SET expires_at = 1 WHERE id = ?1",
            params![first.as_bytes()],
        )
        .unwrap();
    }
    fx
}

async fn evidence_findings(fx: &Fixture) -> Vec<(String, String, Vec<String>)> {
    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
    )
    .await
    .unwrap();
    report
        .findings
        .into_iter()
        .filter(|finding| finding.kind == "contradiction" || finding.kind == "similarity_candidate")
        .map(|finding| (finding.kind, finding.severity, finding.pages))
        .collect()
}

#[derive(Default, Debug)]
struct EvidenceCounts {
    tp: usize,
    fp: usize,
    tn: usize,
    fn_: usize,
    excluded: usize,
    findings: usize,
    similarity_candidates: usize,
}

impl EvidenceCounts {
    fn record(&mut self, case: &EvidenceCase, findings: &[(String, String, Vec<String>)]) {
        self.findings += findings.len();
        let contradictions = findings
            .iter()
            .filter(|(kind, _, _)| kind == "contradiction")
            .count();
        self.similarity_candidates += findings
            .iter()
            .filter(|(kind, _, _)| kind == "similarity_candidate")
            .count();
        match (case.label, contradictions > 0) {
            (EvidenceLabel::Conflict, true) => self.tp += 1,
            (EvidenceLabel::Conflict, false) => self.fn_ += 1,
            (EvidenceLabel::Compatible, true) => self.fp += 1,
            (EvidenceLabel::Compatible, false) => self.tn += 1,
            (EvidenceLabel::Ineligible, _) => self.excluded += 1,
        }
    }

    fn report(&self) {
        // Undefined metrics stay None rather than being reported as zero.
        let ratio = |n: usize, d: usize| (d != 0).then(|| n as f64 / d as f64);
        println!(
            "synthetic classification: {self:?}; precision={:?}; recall={:?}; f1={:?}; fpr={:?}; fnr={:?}",
            ratio(self.tp, self.tp + self.fp),
            ratio(self.tp, self.tp + self.fn_),
            ratio(2 * self.tp, 2 * self.tp + self.fp + self.fn_),
            ratio(self.fp, self.fp + self.tn),
            ratio(self.fn_, self.tp + self.fn_),
        );
    }
}

#[tokio::test]
async fn labeled_evidence_pairs_record_current_classification() {
    let mut counts = EvidenceCounts::default();
    let cases = evidence_cases();
    let mut ids = std::collections::HashSet::new();
    for case in &cases {
        assert!(ids.insert(&case.id), "duplicate fixture id: {}", case.id);
        let fx = seed_evidence_case(case).await;
        let before = latest_page_count(&fx);
        let first = evidence_findings(&fx).await;
        assert_eq!(
            first.len(),
            case.expected_findings,
            "{} ({})",
            case.id,
            case.language
        );
        if matches!(case.label, EvidenceLabel::Ineligible) {
            assert!(first.is_empty(), "excluded pair: {}", case.id);
        }
        if matches!(case.setup, EvidenceSetup::Declared) {
            assert_eq!(first[0].0, "contradiction", "typed edge control kind");
            assert_eq!(first[0].1, "warning", "typed edge control severity");
        } else if matches!(case.setup, EvidenceSetup::Active) && !first.is_empty() {
            assert!(
                first
                    .iter()
                    .all(|(kind, _, _)| kind == "similarity_candidate"),
                "similarity-only findings must remain candidates: {}",
                case.id
            );
        }
        assert_eq!(
            first,
            evidence_findings(&fx).await,
            "repeat parity: {}",
            case.id
        );
        assert_eq!(
            latest_page_count(&fx),
            before,
            "lint mutated pages: {}",
            case.id
        );
        counts.record(case, &first);
    }
    assert_eq!(cases.len(), 11);
    counts.report();
}

/// Timing excludes fixture creation. Each trial is one isolated pair, not a
/// scan over the whole corpus. Run explicitly; no wall-clock assertion enters
/// CI, and the measurements do not imply a production throughput target.
#[tokio::test]
#[ignore = "manual offline lint timing receipt; run with --ignored --nocapture"]
async fn labeled_evidence_pairs_timing_receipt() {
    const TRIALS: usize = 20;
    let mut timings = Vec::new();
    for case in evidence_cases() {
        let fx = seed_evidence_case(&case).await;
        let expected = evidence_findings(&fx).await;
        for _ in 0..TRIALS {
            let started = std::time::Instant::now();
            let actual = evidence_findings(&fx).await;
            timings.push(started.elapsed().as_secs_f64() * 1_000.0);
            assert_eq!(actual, expected, "repeat parity: {}", case.id);
        }
    }
    timings.sort_by(f64::total_cmp);
    let percentile = |pct: usize| timings[(pct * timings.len()).div_ceil(100) - 1];
    println!(
        "synthetic isolated-pair lint: trials={} p50_ms={:.6} p95_ms={:.6}; LLM calls=0",
        timings.len(),
        percentile(50),
        percentile(95),
    );
}
