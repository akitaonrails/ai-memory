//! Pinned retrieval benchmarks, end to end through the real stack.
//!
//! Per question: replay its haystack sessions through `POST /hook/batch`
//! (production hook cadence), query with `memory_query` over MCP, and
//! score session-level hit@k / recall@k / recall-all@k against the dataset's
//! evidence labels. See `docs/benchmarks/` for published baselines and
//! `evals/README.md` for how to run.
//!
//! ## R2: A/B + the accuracy/latency/context triple
//!
//! With a candidate config (`--candidate` plus any `--candidate-*` knob),
//! the same question set runs through TWO server configs — a baseline and
//! a candidate — and the report gains a baseline→candidate delta. Every
//! run, single or A/B, now reports a triple per slice: retrieval accuracy
//! (hit@k / recall@k / recall-all@k), `memory_query` latency (p50/p95), and
//! the context tokens a result would cost the agent (chars/4 over returned
//! snippets).
//! Without a candidate config the single-config path is unchanged.
//!
//! ## Decomposing the retrieval gap
//!
//! Four opt-in instruments split a hit@k number into capture loss and
//! ranking loss (defaults reproduce the published baselines):
//!
//! - `--reachability` checks whether each answer-bearing turn survives
//!   into the page, the observations, and the embedder's 512-token window;
//! - `--engine grep` scores a whole-session term-overlap baseline with no
//!   server, calibrated against agentmemory's grep adapter;
//! - `--ingest whole-session|captured-session` writes one page per session
//!   through the real page write path instead of replaying hooks;
//! - `--date-prefix turn|session-start|none` moves the per-turn date.

pub(crate) mod compare;
pub(crate) mod dataset;
pub(crate) mod grep;
pub(crate) mod ingest;
pub(crate) mod qa;
pub(crate) mod query;
mod reachability;
pub(crate) mod report;
mod score;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;

use dataset::{DatasetVariant, Question};
use ingest::{DatePrefix, IngestMode, project_for, stored_session_uuid};
use server::{EvalEmbeddings, EvalServer, LaunchConfig};

mod server;

#[derive(clap::Args, Debug)]
pub struct RetrievalArgs {
    /// Dataset file in LongMemEval JSON format. Defaults to the selected
    /// variant's pinned path.
    #[arg(long)]
    dataset: Option<PathBuf>,

    /// Pinned dataset: `s` or `s-cleaned` for LongMemEval, or the bundled
    /// offline coding-session corpus `coding-v1`.
    #[arg(long, value_enum, default_value_t = Variant::S)]
    dataset_variant: Variant,

    /// Download the dataset first if the file is missing.
    #[arg(long)]
    fetch: bool,

    /// `ai-memory` server binary to benchmark. Build it first:
    /// `cargo build --release -p ai-memory-cli`.
    #[arg(long, default_value = "target/release/ai-memory")]
    server_bin: PathBuf,

    /// Score only the first N questions (deterministic prefix) — smoke
    /// runs and iteration. Omit for the entire selected dataset.
    #[arg(long)]
    sample: Option<usize>,

    /// Score only the first N scored (non-abstention) questions of each
    /// question type: a deterministic sample that covers every slice.
    /// Applied after `--sample`.
    #[arg(long)]
    stratify: Option<usize>,

    /// Score only these question ids (comma-separated or repeated), in
    /// dataset order. Applied before `--sample` and `--stratify`: a
    /// targeted re-run of a few questions without a full pass.
    #[arg(long = "question-id", value_delimiter = ',')]
    question_ids: Vec<String>,

    /// `server` (the default) drives a real `ai-memory serve`; `grep`
    /// scores the whole-session term-overlap baseline with no server.
    #[arg(long, value_enum, default_value_t = Engine::Server)]
    engine: Engine,

    /// After ingest, report how much answer-bearing evidence reaches the
    /// page, the observations, and the local embedder's 512-token window.
    /// Needs the local model's tokenizer (fetched into `evals/models/`).
    #[arg(long)]
    reachability: bool,

    /// Cutoffs for hit@k / recall@k / recall-all@k.
    #[arg(long, value_delimiter = ',', default_values_t = vec![1, 3, 5, 10])]
    ks: Vec<usize>,

    /// Questions ingested+queried concurrently.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Per-request HTTP timeout, seconds. Hook batches slow down as the
    /// store grows, and a timed-out batch cannot be retried safely (the
    /// server may have applied it), so long full runs need headroom.
    #[arg(long, default_value_t = 120)]
    http_timeout_secs: u64,

    /// Output root; a timestamped run dir is created under it.
    #[arg(long, default_value = "evals/runs")]
    out: PathBuf,

    /// Keep the server's temp data dir for post-mortem inspection.
    #[arg(long)]
    keep_data_dir: bool,

    // ---- baseline config knobs ----
    /// Baseline embedding mode: `none` (zero-LLM, vectors off, the
    /// default) or `local` (in-process all-MiniLM-L6-v2, vectors on; the
    /// model is fetched once into `evals/models/`, checksum-pinned).
    #[arg(long, default_value = "none")]
    embeddings: String,

    /// Turn the post-RRF reranker on for the baseline
    /// (`AI_MEMORY_RERANKER=llm`). Requires an LLM provider, which you
    /// must supply via `--server-env` — otherwise the server ignores it.
    #[arg(long)]
    reranker: bool,

    /// Extra server env for the baseline, `KEY=VALUE`, repeatable. The
    /// general escape hatch for any server knob (reranker provider,
    /// retrieval flags, future toggles).
    #[arg(long = "server-env", value_name = "KEY=VALUE")]
    server_env: Vec<String>,

    /// Extra `memory_query` argument for the baseline, `KEY=JSON`,
    /// repeatable (e.g. `--query-arg include_expired=true`). The value is
    /// parsed as JSON, falling back to a string. Lets future query knobs
    /// (`pin_first`, `include_superseded`, …) be A/B'd without new flags.
    /// `query`/`workspace`/`project`/`limit` are reserved by the harness.
    #[arg(long = "query-arg", value_name = "KEY=JSON")]
    query_arg: Vec<String>,

    /// How the haystack reaches the baseline server: `hooks` (the shipped
    /// capture path), `whole-session` (one page per raw session), or
    /// `captured-session` (one page per session holding the captured text).
    #[arg(long, value_enum, default_value_t = IngestMode::Hooks)]
    ingest: IngestMode,

    /// Where the baseline replay puts each session's date: `turn` (every
    /// turn, the published baselines), `session-start`, or `none`.
    #[arg(long, value_enum, default_value_t = DatePrefix::Turn)]
    date_prefix: DatePrefix,

    // ---- candidate config knobs (A/B mode) ----
    /// Enable A/B mode with a candidate config. Implied by any
    /// `--candidate-*` knob. With no other candidate knob the candidate
    /// mirrors the baseline — the determinism check (delta ≈ 0).
    #[arg(long)]
    candidate: bool,

    /// Candidate embedding mode (`none`/`local`); defaults to the
    /// baseline's `--embeddings`.
    #[arg(long)]
    candidate_embeddings: Option<String>,

    /// Turn the reranker on for the candidate.
    #[arg(long)]
    candidate_reranker: bool,

    /// Extra server env for the candidate, `KEY=VALUE`, repeatable.
    #[arg(long = "candidate-server-env", value_name = "KEY=VALUE")]
    candidate_server_env: Vec<String>,

    /// Extra `memory_query` argument for the candidate, `KEY=JSON`,
    /// repeatable.
    #[arg(long = "candidate-query-arg", value_name = "KEY=JSON")]
    candidate_query_arg: Vec<String>,

    /// Candidate ingest mode; defaults to the baseline's `--ingest`.
    #[arg(long, value_enum)]
    candidate_ingest: Option<IngestMode>,

    /// Candidate date placement; defaults to the baseline's `--date-prefix`.
    #[arg(long, value_enum)]
    candidate_date_prefix: Option<DatePrefix>,

    // ---- QA-accuracy mode (R2b, opt-in, live LLM) ----
    #[command(flatten)]
    qa: qa::QaArgs,
}

impl RetrievalArgs {
    /// A/B mode is on when `--candidate` is set or any candidate knob is.
    fn ab_mode(&self) -> bool {
        self.candidate
            || self.candidate_embeddings.is_some()
            || self.candidate_reranker
            || !self.candidate_server_env.is_empty()
            || !self.candidate_query_arg.is_empty()
            || self.candidate_ingest.is_some()
            || self.candidate_date_prefix.is_some()
    }
}

/// `--dataset-variant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Variant {
    S,
    SCleaned,
    CodingV1,
}

impl Variant {
    fn pinned(self) -> &'static DatasetVariant {
        match self {
            Self::S => &dataset::VARIANT_S,
            Self::SCleaned => &dataset::VARIANT_S_CLEANED,
            Self::CodingV1 => &dataset::VARIANT_CODING_V1,
        }
    }
}

/// `--engine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Engine {
    Server,
    Grep,
}

/// One resolved server + query configuration.
struct Knobs {
    /// Short label for reports (`baseline` / `candidate`).
    side: &'static str,
    embeddings: EvalEmbeddings,
    /// `zero-llm` or `local-embeddings`, for report provenance.
    mode: &'static str,
    models_root: Option<PathBuf>,
    reranker: bool,
    env: Vec<(String, String)>,
    query_args: serde_json::Map<String, serde_json::Value>,
    ingest: IngestMode,
    date_prefix: DatePrefix,
    /// Human-readable knob summary for the report.
    summary: String,
}

impl Knobs {
    #[allow(clippy::too_many_arguments)]
    fn parse(
        side: &'static str,
        embeddings: &str,
        reranker: bool,
        server_env: &[String],
        query_arg: &[String],
        ingest: IngestMode,
        date_prefix: DatePrefix,
    ) -> Result<Self> {
        let (embeddings_kind, mode, models_root) = match embeddings {
            "none" => (EvalEmbeddings::None, "zero-llm", None),
            "local" => (
                EvalEmbeddings::Local,
                "local-embeddings",
                Some(PathBuf::from("evals/models")),
            ),
            other => bail!("{side}: --embeddings must be `none` or `local`, got {other}"),
        };
        let env = parse_env(side, server_env)?;
        let query_args = parse_query_args(side, query_arg)?;

        let mut summary = format!(
            "embeddings={embeddings} reranker={}",
            if reranker { "on" } else { "off" }
        );
        for (k, v) in &env {
            summary.push_str(&format!(" env:{k}={v}"));
        }
        for (k, v) in &query_args {
            summary.push_str(&format!(" arg:{k}={v}"));
        }
        // Only non-default ablations are named, so a default run's summary
        // reads exactly as the published baselines do.
        if ingest != IngestMode::Hooks {
            summary.push_str(&format!(" ingest={}", ingest.as_str()));
        }
        if date_prefix != DatePrefix::Turn {
            summary.push_str(&format!(" date_prefix={}", date_prefix.as_str()));
        }

        Ok(Self {
            side,
            embeddings: embeddings_kind,
            mode,
            models_root,
            reranker,
            env,
            query_args,
            ingest,
            date_prefix,
            summary,
        })
    }

    fn launch_config(&self) -> LaunchConfig<'_> {
        LaunchConfig {
            embeddings: self.embeddings,
            models_root: self.models_root.as_deref(),
            reranker: self.reranker,
            env: &self.env,
        }
    }
}

fn parse_env(side: &str, items: &[String]) -> Result<Vec<(String, String)>> {
    items
        .iter()
        .map(|item| {
            let (k, v) = item.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("{side}: --server-env `{item}` must be KEY=VALUE")
            })?;
            if k.is_empty() {
                bail!("{side}: --server-env `{item}` has an empty key");
            }
            Ok((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Reserved args the harness always sets; a config may not override them.
const RESERVED_QUERY_ARGS: [&str; 4] = ["query", "workspace", "project", "limit"];

fn parse_query_args(
    side: &str,
    items: &[String],
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut map = serde_json::Map::new();
    for item in items {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("{side}: --query-arg `{item}` must be KEY=JSON"))?;
        if k.is_empty() {
            bail!("{side}: --query-arg `{item}` has an empty key");
        }
        if RESERVED_QUERY_ARGS.contains(&k) {
            bail!("{side}: --query-arg key `{k}` is reserved by the harness");
        }
        // Parse as JSON, else treat the raw text as a string value.
        let value = serde_json::from_str::<serde_json::Value>(v)
            .unwrap_or_else(|_| serde_json::Value::String(v.to_string()));
        map.insert(k.to_string(), value);
    }
    Ok(map)
}

pub async fn run(args: RetrievalArgs) -> Result<()> {
    // Captured at launch: a long run must not stamp its report with a
    // commit that landed while it was in flight.
    let commit = report::commit_sha();
    let variant = args.dataset_variant.pinned();
    let dataset_path = args
        .dataset
        .clone()
        .unwrap_or_else(|| PathBuf::from(variant.default_path));
    if args.fetch && !dataset_path.exists() {
        dataset::fetch(&dataset_path, variant).await?;
    }
    let mut questions = dataset::load(&dataset_path, variant)?;
    let total_loaded = questions.len();
    if !args.question_ids.is_empty() {
        let unknown: Vec<&String> = args
            .question_ids
            .iter()
            .filter(|id| !questions.iter().any(|q| &q.question_id == *id))
            .collect();
        if !unknown.is_empty() {
            bail!("--question-id not in the dataset: {unknown:?}");
        }
        questions.retain(|q| args.question_ids.contains(&q.question_id));
    }
    if let Some(n) = args.sample {
        questions.truncate(n);
    }
    let abstention: Vec<Question> = questions
        .iter()
        .filter(|q| q.is_abstention())
        .cloned()
        .collect();
    questions.retain(|q| !q.is_abstention());
    if let Some(per_type) = args.stratify {
        questions = dataset::stratify(questions, per_type);
    }
    tracing::info!(
        loaded = total_loaded,
        scored = questions.len(),
        abstention_excluded = abstention.len(),
        "dataset ready"
    );

    let stamp: String = Timestamp::now()
        .to_string()
        .chars()
        .map(|c| if c == ':' || c == '.' { '-' } else { c })
        .collect();
    let run_dir = args.out.join(format!("{stamp}-retrieval"));

    if args.engine == Engine::Grep {
        if args.ab_mode() || args.reachability || args.qa.enabled {
            bail!("--engine grep runs alone: no candidate, --reachability, or --qa");
        }
        let report = run_grep(&args, variant, &questions, abstention.len(), &commit);
        std::fs::create_dir_all(&run_dir)?;
        std::fs::write(
            run_dir.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        let md = report::to_markdown(&report);
        std::fs::write(run_dir.join("report.md"), &md)?;
        println!("{md}");
        println!("run dir: {}", run_dir.display());
        return Ok(());
    }

    if !args.server_bin.exists() {
        bail!(
            "server binary {} not found — run `cargo build --release -p ai-memory-cli` first",
            args.server_bin.display()
        );
    }

    let baseline = Knobs::parse(
        "baseline",
        &args.embeddings,
        args.reranker,
        &args.server_env,
        &args.query_arg,
        args.ingest,
        args.date_prefix,
    )?;
    let candidate = if args.ab_mode() {
        let embeddings = args
            .candidate_embeddings
            .clone()
            .unwrap_or_else(|| args.embeddings.clone());
        Some(Knobs::parse(
            "candidate",
            &embeddings,
            args.candidate_reranker,
            &args.candidate_server_env,
            &args.candidate_query_arg,
            args.candidate_ingest.unwrap_or(args.ingest),
            args.candidate_date_prefix.unwrap_or(args.date_prefix),
        )?)
    } else {
        None
    };

    // Fetch the local embedding model once if any config needs it.
    let needs_local = baseline.embeddings == EvalEmbeddings::Local
        || candidate
            .as_ref()
            .is_some_and(|c| c.embeddings == EvalEmbeddings::Local);
    if needs_local || args.reachability {
        let root = PathBuf::from("evals/models");
        if !ai_memory_llm::model_present(&root) {
            tracing::info!("fetching the local embedding model into evals/models (~87 MB)");
            ai_memory_llm::fetch_model(&root).await?;
        }
    }

    // QA-accuracy mode (R2b): opt-in, live LLM. Resolve the answerer +
    // grader once and share them across both configs. If QA is requested but
    // a provider/key can't be resolved, SKIP cleanly (retrieval metrics are
    // unaffected) rather than failing the run — mirroring live-LLM tests.
    let qa_engine = if args.qa.enabled {
        match qa::QaEngine::resolve(&args.qa) {
            Ok(engine) => {
                println!("QA-accuracy mode ON (live LLM): {}", engine.summary());
                tracing::info!(qa = engine.summary(), "QA-accuracy mode enabled");
                Some(Arc::new(engine))
            }
            Err(e) => {
                println!("QA-accuracy mode requested but SKIPPED: {e:#}");
                tracing::warn!(error = %format!("{e:#}"), "QA-accuracy mode skipped");
                None
            }
        }
    } else {
        None
    };

    // The embedder-window check reads the model's own tokenizer.
    let embed_window = if args.reachability {
        Some(Arc::new(reachability::EmbedWindow::load(
            std::path::Path::new("evals/models"),
        )?))
    } else {
        None
    };

    let abstention_n = abstention.len();
    let run = RunContext {
        args: &args,
        variant,
        questions: &questions,
        abstention_n,
        commit: &commit,
        qa: qa_engine.clone(),
        embed_window,
    };
    let baseline_report = run_config(&run, &baseline).await?;

    // Timestamped run dir shared by single-config and A/B outputs.
    std::fs::create_dir_all(&run_dir)?;

    match candidate {
        None => {
            std::fs::write(
                run_dir.join("report.json"),
                serde_json::to_vec_pretty(&baseline_report)?,
            )?;
            let md = report::to_markdown(&baseline_report);
            std::fs::write(run_dir.join("report.md"), &md)?;
            println!("{md}");
        }
        Some(candidate) => {
            let candidate_report = run_config(&run, &candidate).await?;
            let ab = report::AbReport::new(baseline_report, candidate_report);
            std::fs::write(run_dir.join("report.json"), serde_json::to_vec_pretty(&ab)?)?;
            let md = report::ab_to_markdown(&ab);
            std::fs::write(run_dir.join("report.md"), &md)?;
            println!("{md}");
        }
    }
    println!("run dir: {}", run_dir.display());
    Ok(())
}

/// Everything a config run shares with its A/B twin.
struct RunContext<'a> {
    args: &'a RetrievalArgs,
    variant: &'static DatasetVariant,
    questions: &'a [Question],
    abstention_n: usize,
    commit: &'a str,
    qa: Option<Arc<qa::QaEngine>>,
    embed_window: Option<Arc<reachability::EmbedWindow>>,
}

/// What one question's task hands back for aggregation.
struct QuestionRun {
    score: score::QuestionScore,
    ingest: ingest::IngestStats,
    reach: Option<reachability::QuestionReach>,
}

/// Launch one server config, replay + query + score the whole question
/// set through it, and build its [`report::Report`]. Each config gets a
/// fresh server (and fresh data dir) so ingestion never bleeds between
/// baseline and candidate.
async fn run_config(run: &RunContext<'_>, knobs: &Knobs) -> Result<report::Report> {
    let args = run.args;
    let server = EvalServer::launch(&args.server_bin, args.keep_data_dir, &knobs.launch_config())
        .await
        .with_context(|| format!("launching {} server", knobs.side))?;
    tracing::info!(
        side = knobs.side,
        url = server.base_url,
        data_dir = %server.data_dir_path.display(),
        config = knobs.summary,
        "eval server up"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(args.http_timeout_secs))
        .build()?;
    let max_k = args.ks.iter().copied().max().unwrap_or(10);
    let ks = Arc::new(args.ks.clone());
    let base_url = Arc::new(server.base_url.clone());
    let client = Arc::new(client);
    let query_args = Arc::new(knobs.query_args.clone());
    let (ingest_mode, date_prefix) = (knobs.ingest, knobs.date_prefix);
    let reach_on = args.reachability;

    let started = std::time::Instant::now();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(args.concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for q in run.questions.iter().cloned() {
        let permit = semaphore.clone().acquire_owned().await?;
        let (client, base_url, ks, query_args, qa, embed_window) = (
            client.clone(),
            base_url.clone(),
            ks.clone(),
            query_args.clone(),
            run.qa.clone(),
            run.embed_window.clone(),
        );
        tasks.spawn(async move {
            let _permit = permit;
            let ingest_stats = match ingest_mode {
                IngestMode::Hooks => {
                    ingest::ingest_question(&client, &base_url, &q, date_prefix).await
                }
                mode => {
                    ingest::write_session_pages(&client, &base_url, &q, mode, date_prefix).await
                }
            }
            .with_context(|| format!("ingesting {}", q.question_id))?;
            let outcome = query::memory_query(
                &client,
                &base_url,
                ingest::EVAL_WORKSPACE,
                &project_for(&q),
                &q.question,
                max_k,
                &query_args,
            )
            .await
            .with_context(|| format!("querying {}", q.question_id))?;
            let evidence: HashSet<uuid::Uuid> = q
                .answer_session_ids
                .iter()
                .map(|s| stored_session_uuid(&q, s))
                .collect();
            let mut score = score::score_question(
                &q.question_id,
                &q.question_type,
                &evidence,
                &outcome.retrieved,
                &ks,
                outcome.latency.as_millis(),
                outcome.context_tokens,
            );
            score.ingest_ms = ingest_stats.elapsed.as_millis();
            score.raw_hits = outcome.raw_hits;
            // Read back only after the query, so the measurement never
            // changes what the query saw.
            let reach = if reach_on {
                Some(
                    reachability::measure_question(
                        &client,
                        &base_url,
                        &q,
                        ingest_mode,
                        date_prefix,
                        embed_window.as_deref(),
                    )
                    .await
                    .with_context(|| format!("reachability for {}", q.question_id))?,
                )
            } else {
                None
            };
            // QA-accuracy (R2b): answer + grade. A QA failure for one
            // question is logged and left ungraded — it must not waste the
            // whole retrieval run (retrieval numbers are already collected).
            if let Some(engine) = qa.as_ref() {
                match engine
                    .evaluate(&q.question, &q.gold_answer_text(), &outcome)
                    .await
                {
                    Ok(qa_outcome) => score.qa = Some(qa_outcome),
                    Err(e) => tracing::warn!(
                        question = q.question_id,
                        error = %format!("{e:#}"),
                        "QA grading failed; question left ungraded"
                    ),
                }
            }
            tracing::info!(
                question = q.question_id,
                items = ingest_stats.items,
                retrieved = outcome.retrieved.len(),
                latency_ms = outcome.latency.as_millis() as u64,
                context_tokens = outcome.context_tokens,
                hit_at_max = score.hit_at.values().next_back().copied().unwrap_or(0.0),
                qa_correct = score.qa.as_ref().map(|o| o.correct),
                "scored"
            );
            anyhow::Ok(QuestionRun {
                score,
                ingest: ingest_stats,
                reach,
            })
        });
    }

    // Collect everything before judging: one bad question must not waste
    // the other 499, but a partial run must never masquerade as a
    // publishable number either — any failure aborts before reporting.
    let mut scores = Vec::new();
    let mut ingests = Vec::new();
    let mut reaches = Vec::new();
    let mut failures = Vec::new();
    while let Some(res) = tasks.join_next().await {
        match res.context("task panicked")? {
            Ok(done) => {
                scores.push(done.score);
                ingests.push(done.ingest);
                reaches.extend(done.reach);
            }
            Err(e) => failures.push(format!("{e:#}")),
        }
    }
    if !failures.is_empty() {
        for f in &failures {
            tracing::error!("{f}");
        }
        bail!(
            "{} config: {} of {} questions failed; no report written",
            knobs.side,
            failures.len(),
            failures.len() + scores.len()
        );
    }
    let wall = started.elapsed();
    scores.sort_by(|a, b| a.question_id.cmp(&b.question_id));
    reaches.sort_by(|a, b| a.question_id.cmp(&b.question_id));

    let slices = score::aggregate(&scores, &args.ks);
    Ok(report::Report {
        generated_at: Timestamp::now().to_string(),
        commit: run.commit.to_string(),
        hardware: report::hardware(),
        dataset: run.variant.name,
        dataset_sha256: run.variant.sha256,
        mode: knobs.mode,
        config: knobs.summary.clone(),
        questions_scored: scores.len(),
        abstention_excluded: run.abstention_n,
        ks: args.ks.clone(),
        qa: run.qa.as_ref().map(|e| e.report_meta()),
        ingest: Some(report::IngestSummary::new(
            ingest_mode,
            date_prefix,
            args.concurrency,
            &ingests,
            wall,
        )),
        reachability: (!reaches.is_empty()).then(|| report::ReachabilityReport {
            slices: reachability::aggregate(&reaches),
            per_question: reaches,
        }),
        slices,
        per_question: scores,
    })
}

/// `--engine grep`: rank every question's raw haystack in-process.
fn run_grep(
    args: &RetrievalArgs,
    variant: &'static DatasetVariant,
    questions: &[Question],
    abstention_n: usize,
    commit: &str,
) -> report::Report {
    let scores: Vec<score::QuestionScore> = questions
        .iter()
        .map(|q| {
            let started = std::time::Instant::now();
            let ranked = grep::rank_question(q);
            let latency = started.elapsed().as_millis();
            let retrieved: Vec<query::Retrieved> = ranked
                .into_iter()
                .map(|id| query::Retrieved {
                    session_uuid: Some(id),
                })
                .collect();
            let evidence: HashSet<uuid::Uuid> = q
                .answer_session_ids
                .iter()
                .map(|s| stored_session_uuid(q, s))
                .collect();
            score::score_question(
                &q.question_id,
                &q.question_type,
                &evidence,
                &retrieved,
                &args.ks,
                latency,
                0,
            )
        })
        .collect();
    let slices = score::aggregate(&scores, &args.ks);
    report::Report {
        generated_at: Timestamp::now().to_string(),
        commit: commit.to_string(),
        hardware: report::hardware(),
        dataset: variant.name,
        dataset_sha256: variant.sha256,
        mode: "grep",
        config: "engine=grep (whole raw session, term overlap, no server)".into(),
        questions_scored: scores.len(),
        abstention_excluded: abstention_n,
        ks: args.ks.clone(),
        qa: None,
        ingest: None,
        reachability: None,
        slices,
        per_question: scores,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_args_parse_json_and_string_and_reject_reserved() {
        let m = parse_query_args(
            "baseline",
            &[
                "include_expired=true".into(),
                "note=hello world".into(),
                "top=5".into(),
            ],
        )
        .unwrap();
        assert_eq!(m["include_expired"], serde_json::json!(true));
        assert_eq!(m["note"], serde_json::json!("hello world"));
        assert_eq!(m["top"], serde_json::json!(5));

        let err = parse_query_args("baseline", &["query=x".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
        let err = parse_query_args("baseline", &["bad".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("KEY=JSON"), "{err}");
    }

    #[test]
    fn server_env_parses_key_value_pairs() {
        let e = parse_env(
            "baseline",
            &["AI_MEMORY_RERANKER=llm".into(), "A=b=c".into()],
        )
        .unwrap();
        assert_eq!(e[0], ("AI_MEMORY_RERANKER".into(), "llm".into()));
        // split_once keeps everything after the first `=` in the value.
        assert_eq!(e[1], ("A".into(), "b=c".into()));
        assert!(parse_env("baseline", &["nope".into()]).is_err());
    }

    #[test]
    fn knobs_summary_reflects_the_matrix() {
        let k = Knobs::parse(
            "candidate",
            "local",
            true,
            &["AI_MEMORY_LLM_PROVIDER=openai".into()],
            &["include_expired=true".into()],
            IngestMode::Hooks,
            DatePrefix::Turn,
        )
        .unwrap();
        assert!(k.summary.contains("embeddings=local"));
        assert!(k.summary.contains("reranker=on"));
        assert!(k.summary.contains("env:AI_MEMORY_LLM_PROVIDER=openai"));
        assert!(k.summary.contains("arg:include_expired=true"));
        assert_eq!(k.mode, "local-embeddings");
        // Default ablations stay out of the summary (baseline reports
        // read as before); non-default ones are named.
        assert!(!k.summary.contains("ingest="));
        assert!(!k.summary.contains("date_prefix="));
        let k = Knobs::parse(
            "candidate",
            "none",
            false,
            &[],
            &[],
            IngestMode::WholeSession,
            DatePrefix::SessionStart,
        )
        .unwrap();
        assert!(k.summary.contains("ingest=whole-session"));
        assert!(k.summary.contains("date_prefix=session-start"));
    }
}
