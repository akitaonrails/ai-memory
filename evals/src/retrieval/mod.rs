//! LongMemEval retrieval benchmark, end to end through the real stack.
//!
//! Per question: replay its haystack sessions through `POST /hook/batch`
//! (production hook cadence), query with `memory_query` over MCP, and
//! score session-level hit@k / recall@k against the dataset's evidence
//! labels. See `docs/benchmarks/` for published baselines and
//! `evals/README.md` for how to run.
//!
//! ## R2: A/B + the accuracy/latency/context triple
//!
//! With a candidate config (`--candidate` plus any `--candidate-*` knob),
//! the same question set runs through TWO server configs — a baseline and
//! a candidate — and the report gains a baseline→candidate delta. Every
//! run, single or A/B, now reports a triple per slice: retrieval accuracy
//! (hit@k / recall@k), `memory_query` latency (p50/p95), and the context
//! tokens a result would cost the agent (chars/4 over returned snippets).
//! Without a candidate config the single-config path is unchanged.

mod dataset;
mod ingest;
mod qa;
mod query;
mod report;
mod score;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;

use dataset::Question;
use ingest::{project_for, stored_session_uuid};
use server::{EvalEmbeddings, EvalServer, LaunchConfig};

mod server;

#[derive(clap::Args, Debug)]
pub struct RetrievalArgs {
    /// Dataset file (LongMemEval-S JSON).
    #[arg(long, default_value = "evals/datasets/longmemeval_s.json")]
    dataset: PathBuf,

    /// Download the dataset first if the file is missing.
    #[arg(long)]
    fetch: bool,

    /// `ai-memory` server binary to benchmark. Build it first:
    /// `cargo build --release -p ai-memory-cli`.
    #[arg(long, default_value = "target/release/ai-memory")]
    server_bin: PathBuf,

    /// Score only the first N questions (deterministic prefix) — smoke
    /// runs and iteration. Omit for the full 500.
    #[arg(long)]
    sample: Option<usize>,

    /// Cutoffs for hit@k / recall@k.
    #[arg(long, value_delimiter = ',', default_values_t = vec![1, 3, 5, 10])]
    ks: Vec<usize>,

    /// Questions ingested+queried concurrently.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

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

    // ---- QA-accuracy mode (R2b, opt-in, live LLM) ----
    /// Enable end-to-end QA-accuracy grading (real LLM API calls). OFF by
    /// default. Per scored question the harness synthesizes an answer from
    /// the retrieved snippets (or uses a server-provided `answer` field when
    /// present) and grades it against the gold answer with an LLM judge.
    /// Requires `--qa-provider` + `--qa-model` and a resolvable key; if the
    /// key/provider cannot be resolved the run SKIPS QA (retrieval metrics
    /// are unaffected) instead of failing.
    #[arg(long)]
    qa: bool,

    /// QA answerer/judge provider
    /// (`anthropic|openai|openai-compat|openai-oauth|codex|copilot|gemini`).
    #[arg(long)]
    qa_provider: Option<String>,

    /// QA answerer model id.
    #[arg(long)]
    qa_model: Option<String>,

    /// QA answerer base URL (omit for native vendors).
    #[arg(long)]
    qa_base_url: Option<String>,

    /// QA answerer API key (raw value). Prefer `--qa-api-key-env` or the
    /// provider's default env var.
    #[arg(long)]
    qa_api_key: Option<String>,

    /// Env var to read the QA answerer API key from (defaults to the
    /// provider's canonical env var, e.g. `GEMINI_API_KEY`).
    #[arg(long)]
    qa_api_key_env: Option<String>,

    /// Auth file for a QA answerer using `openai-oauth`/`codex`/`copilot`.
    #[arg(long)]
    qa_token_file: Option<PathBuf>,

    /// QA grader provider; defaults to `--qa-provider`.
    #[arg(long)]
    qa_grader_provider: Option<String>,

    /// QA grader model; defaults to `--qa-model`.
    #[arg(long)]
    qa_grader_model: Option<String>,

    /// QA grader base URL; defaults to `--qa-base-url`.
    #[arg(long)]
    qa_grader_base_url: Option<String>,

    /// QA grader API key (raw); defaults to `--qa-api-key`.
    #[arg(long)]
    qa_grader_api_key: Option<String>,

    /// Env var for the QA grader API key; defaults to `--qa-api-key-env`.
    #[arg(long)]
    qa_grader_api_key_env: Option<String>,

    /// Auth file for the QA grader; defaults to `--qa-token-file`.
    #[arg(long)]
    qa_grader_token_file: Option<PathBuf>,
}

impl RetrievalArgs {
    /// A/B mode is on when `--candidate` is set or any candidate knob is.
    fn ab_mode(&self) -> bool {
        self.candidate
            || self.candidate_embeddings.is_some()
            || self.candidate_reranker
            || !self.candidate_server_env.is_empty()
            || !self.candidate_query_arg.is_empty()
    }
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
    /// Human-readable knob summary for the report.
    summary: String,
}

impl Knobs {
    fn parse(
        side: &'static str,
        embeddings: &str,
        reranker: bool,
        server_env: &[String],
        query_arg: &[String],
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

        Ok(Self {
            side,
            embeddings: embeddings_kind,
            mode,
            models_root,
            reranker,
            env,
            query_args,
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
    if args.fetch && !args.dataset.exists() {
        dataset::fetch(&args.dataset).await?;
    }
    let mut questions = dataset::load(&args.dataset)?;
    let total_loaded = questions.len();
    if let Some(n) = args.sample {
        questions.truncate(n);
    }
    let abstention: Vec<Question> = questions
        .iter()
        .filter(|q| q.is_abstention())
        .cloned()
        .collect();
    questions.retain(|q| !q.is_abstention());
    tracing::info!(
        loaded = total_loaded,
        scored = questions.len(),
        abstention_excluded = abstention.len(),
        "dataset ready"
    );

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
        )?)
    } else {
        None
    };

    // Fetch the local embedding model once if any config needs it.
    let needs_local = baseline.embeddings == EvalEmbeddings::Local
        || candidate
            .as_ref()
            .is_some_and(|c| c.embeddings == EvalEmbeddings::Local);
    if needs_local {
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
    let qa_engine = if args.qa {
        match qa::QaEngine::resolve(&args) {
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

    let abstention_n = abstention.len();
    let baseline_report = run_config(
        &args,
        &baseline,
        &questions,
        abstention_n,
        &commit,
        qa_engine.clone(),
    )
    .await?;

    // Timestamped run dir shared by single-config and A/B outputs.
    let stamp: String = Timestamp::now()
        .to_string()
        .chars()
        .map(|c| if c == ':' || c == '.' { '-' } else { c })
        .collect();
    let run_dir = args.out.join(format!("{stamp}-retrieval"));
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
            let candidate_report = run_config(
                &args,
                &candidate,
                &questions,
                abstention_n,
                &commit,
                qa_engine.clone(),
            )
            .await?;
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

/// Launch one server config, replay + query + score the whole question
/// set through it, and build its [`report::Report`]. Each config gets a
/// fresh server (and fresh data dir) so ingestion never bleeds between
/// baseline and candidate.
async fn run_config(
    args: &RetrievalArgs,
    knobs: &Knobs,
    questions: &[Question],
    abstention_n: usize,
    commit: &str,
    qa: Option<Arc<qa::QaEngine>>,
) -> Result<report::Report> {
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
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let max_k = args.ks.iter().copied().max().unwrap_or(10);
    let ks = Arc::new(args.ks.clone());
    let base_url = Arc::new(server.base_url.clone());
    let client = Arc::new(client);
    let query_args = Arc::new(knobs.query_args.clone());

    let semaphore = Arc::new(tokio::sync::Semaphore::new(args.concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for q in questions.iter().cloned() {
        let permit = semaphore.clone().acquire_owned().await?;
        let (client, base_url, ks, query_args, qa) = (
            client.clone(),
            base_url.clone(),
            ks.clone(),
            query_args.clone(),
            qa.clone(),
        );
        tasks.spawn(async move {
            let _permit = permit;
            let events = ingest::ingest_question(&client, &base_url, &q)
                .await
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
                .map(|s| stored_session_uuid(s))
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
                events,
                retrieved = outcome.retrieved.len(),
                latency_ms = outcome.latency.as_millis() as u64,
                context_tokens = outcome.context_tokens,
                hit_at_max = score.hit_at.values().next_back().copied().unwrap_or(0.0),
                qa_correct = score.qa.as_ref().map(|o| o.correct),
                "scored"
            );
            anyhow::Ok(score)
        });
    }

    // Collect everything before judging: one bad question must not waste
    // the other 499, but a partial run must never masquerade as a
    // publishable number either — any failure aborts before reporting.
    let mut scores = Vec::new();
    let mut failures = Vec::new();
    while let Some(res) = tasks.join_next().await {
        match res.context("task panicked")? {
            Ok(score) => scores.push(score),
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
    scores.sort_by(|a, b| a.question_id.cmp(&b.question_id));

    let slices = score::aggregate(&scores, &args.ks);
    Ok(report::Report {
        generated_at: Timestamp::now().to_string(),
        commit: commit.to_string(),
        hardware: report::hardware(),
        dataset: "longmemeval_s",
        dataset_sha256: dataset::LONGMEMEVAL_S_SHA256,
        mode: knobs.mode,
        config: knobs.summary.clone(),
        questions_scored: scores.len(),
        abstention_excluded: abstention_n,
        ks: args.ks.clone(),
        qa: qa.as_ref().map(|e| e.report_meta()),
        slices,
        per_question: scores,
    })
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
        )
        .unwrap();
        assert!(k.summary.contains("embeddings=local"));
        assert!(k.summary.contains("reranker=on"));
        assert!(k.summary.contains("env:AI_MEMORY_LLM_PROVIDER=openai"));
        assert!(k.summary.contains("arg:include_expired=true"));
        assert_eq!(k.mode, "local-embeddings");
    }
}
