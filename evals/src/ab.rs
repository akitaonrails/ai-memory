//! Live A/B harness for the ai-memory consolidation prompt.
//!
//! ## What it does
//!
//! For each fixture under `evals/fixtures/*.json` (each one a small,
//! synthetic session log), the runner:
//!
//! 1. Calls [`ai_memory_consolidate::build_batch_request`] to build
//!    the EXACT ChatRequest the production consolidator would send.
//! 2. Sends that request to two providers concurrently — a
//!    *baseline* and a *candidate* — via
//!    [`ai_memory_llm::complete_structured`], which is the same
//!    parse-extract-and-validate path the live system uses.
//! 3. Saves the deserialised `ConsolidatedBatch`, the raw JSON, a
//!    flattened markdown rendering, and a `meta.json` (timing,
//!    update count, parse status) per fixture per provider under
//!    `evals/runs/<timestamp>/{baseline,candidate}/`.
//! 4. Scores literal fidelity (see [`crate::fidelity`]): which of the
//!    fixture's `must_mention` facts the projection dropped, which ones
//!    the model dropped after seeing them, invented tokens and wikilinks,
//!    and `must_not_mention` hits.
//! 5. Prints a side-by-side summary table to stdout and writes
//!    `report.json` with the provenance a published number needs.
//!
//! `--runs N` repeats every side N times; the spread between a side's own
//! runs is the noise band a delta has to clear. `--generated-seed` adds
//! the seeded long session from [`crate::generate`]. Anything subtler than
//! these literal checks (structure, judgement) is still for you to eyeball.
//!
//! ## Running it
//!
//! See `evals/README.md` for canonical invocations. The short
//! version:
//!
//! ```bash
//! cargo run -p ai-memory-eval -- ab \
//!     --baseline-provider openai-compat \
//!     --baseline-base-url https://openrouter.ai/api/v1 \
//!     --baseline-model moonshotai/kimi-k2.6 \
//!     --baseline-api-key-env OPENROUTER_API_KEY \
//!     --candidate-provider openai-compat \
//!     --candidate-base-url http://192.168.0.90:11434/v1 \
//!     --candidate-model qwen3:32b \
//!     --candidate-api-key ollama-local
//! ```

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use ai_memory_consolidate::{ConsolidatedBatch, build_batch_request};
use ai_memory_core::{Observation, ObservationKind, ProjectId, SessionId, WorkspaceId};
use ai_memory_llm::{AuthRequirement, LlmProvider, ProviderAuth, ProviderChoice, ProviderConfig};

use crate::fidelity::{PageFidelity, PromptFidelity, Scorer, request_text};
use crate::generate;

#[derive(clap::Args, Debug)]
pub struct AbArgs {
    /// Fixture directory (each `*.json` is one synthetic session log).
    #[arg(long, default_value = "evals/fixtures")]
    fixtures: PathBuf,

    /// Output root. A timestamped subdir gets created here per run.
    #[arg(long, default_value = "evals/runs")]
    out: PathBuf,

    /// Baseline provider (`anthropic` / `openai` / `openai-compat` /
    /// `openai-oauth` / `codex` / `copilot`).
    #[arg(long)]
    baseline_provider: String,

    /// Baseline base URL (omit for native Anthropic/OpenAI).
    #[arg(long)]
    baseline_base_url: Option<String>,

    /// Baseline model id.
    #[arg(long)]
    baseline_model: String,

    /// Baseline API key (raw value). Prefer `--baseline-api-key-env`.
    #[arg(long)]
    baseline_api_key: Option<String>,

    /// Env var to read the baseline API key from.
    #[arg(long)]
    baseline_api_key_env: Option<String>,

    /// Baseline auth file for `openai-oauth` or `codex`.
    #[arg(long)]
    baseline_token_file: Option<PathBuf>,

    /// Candidate provider.
    #[arg(long)]
    candidate_provider: String,

    /// Candidate base URL.
    #[arg(long)]
    candidate_base_url: Option<String>,

    /// Candidate model id.
    #[arg(long)]
    candidate_model: String,

    /// Candidate API key (raw value).
    #[arg(long)]
    candidate_api_key: Option<String>,

    /// Env var to read the candidate API key from.
    #[arg(long)]
    candidate_api_key_env: Option<String>,

    /// Candidate auth file for `openai-oauth` or `codex`.
    #[arg(long)]
    candidate_token_file: Option<PathBuf>,

    /// Repeat each side this many times; the spread between a side's own
    /// runs is the noise band.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=20))]
    runs: u32,

    /// Also run the seeded long session (`evals/src/generate.rs`); the
    /// published runs use 1215.
    #[arg(long)]
    generated_seed: Option<u64>,

    /// Compare the byte-compatible default request on the baseline side with
    /// the opt-in verbatim-residue request on the candidate side.
    #[arg(long)]
    residue: bool,
}

/// JSON shape of an eval fixture file. We only care about
/// observations the consolidation prompt actually consumes; the
/// other Observation fields get populated with dummies at runtime.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Fixture {
    pub(crate) name: String,
    /// Free-text describing the scenario; not consumed by the runner
    /// but useful when reading fixture files by hand.
    #[serde(default)]
    pub(crate) description: String,
    pub(crate) observations: Vec<FixtureObservation>,
    /// Literal strings the pages should carry (exact substring).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) must_mention: Vec<String>,
    /// Literal strings no page may carry: injected instructions,
    /// credential-shaped values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) must_not_mention: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FixtureObservation {
    pub(crate) kind: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
}

/// One result of one provider on one fixture.
#[derive(Debug, Serialize)]
struct CaseRun {
    provider: String,
    fixture: String,
    run: u32,
    elapsed_ms: u128,
    parsed_ok: bool,
    error: Option<String>,
    update_count: Option<usize>,
    fidelity: Option<PageFidelity>,
}

/// One fixture's projection plus every run of both sides.
#[derive(Debug, Serialize)]
struct FixtureReport {
    fixture: String,
    projection: PromptFidelity,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_projection: Option<PromptFidelity>,
    baseline: Vec<CaseRun>,
    candidate: Vec<CaseRun>,
}

#[derive(Debug, Serialize)]
struct SideMeta {
    provider: String,
    model: String,
    base_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct AbReport {
    commit: String,
    runs: u32,
    generated_seed: Option<u64>,
    generated_sha256: Option<String>,
    verbatim_residue_ab: bool,
    baseline: SideMeta,
    candidate: SideMeta,
    fixtures: Vec<FixtureReport>,
}

pub async fn run(args: AbArgs) -> Result<()> {
    let baseline = resolve_provider("baseline", &args)?;
    let candidate = resolve_provider("candidate", &args)?;

    let baseline_provider =
        make_provider(baseline.clone()).context("building baseline provider")?;
    let candidate_provider =
        make_provider(candidate.clone()).context("building candidate provider")?;

    let mut fixtures = load_fixtures(&args.fixtures)
        .with_context(|| format!("loading fixtures from {}", args.fixtures.display()))?;
    let generated = match args.generated_seed {
        Some(seed) => {
            let fx = generate::session(seed);
            let (json, sha) = generate::dump(&fx)?;
            fixtures.push((PathBuf::from(format!("{}.json", fx.name)), fx));
            Some((json, sha))
        }
        None => None,
    };
    if fixtures.is_empty() {
        bail!("no *.json fixtures found in {}", args.fixtures.display());
    }

    // Timestamped run dir; filesystem-safe (colons + dots → dashes).
    let stamp: String = Timestamp::now()
        .to_string()
        .chars()
        .map(|c| if c == ':' || c == '.' { '-' } else { c })
        .collect();
    let run_dir = args.out.join(&stamp);
    std::fs::create_dir_all(run_dir.join("baseline"))?;
    std::fs::create_dir_all(run_dir.join("candidate"))?;
    let generated_sha256 = match generated {
        Some((json, sha)) => {
            std::fs::write(run_dir.join("generated-session.json"), json)?;
            Some(sha)
        }
        None => None,
    };

    println!();
    println!("ai-memory eval — {stamp}");
    println!(
        "  fixtures : {} ({})",
        fixtures.len(),
        args.fixtures.display()
    );
    println!(
        "  baseline : {} {} @ {}",
        baseline.provider_name(),
        baseline.model,
        baseline.base_url.as_deref().unwrap_or("(default)")
    );
    println!(
        "  candidate: {} {} @ {}",
        candidate.provider_name(),
        candidate.model,
        candidate.base_url.as_deref().unwrap_or("(default)")
    );
    println!("  runs     : {}", args.runs);
    println!("  out      : {}", run_dir.display());
    println!();

    let mut reports: Vec<FixtureReport> = Vec::with_capacity(fixtures.len());
    for (path, fx) in fixtures {
        println!("→ {} ({} observations)", fx.name, fx.observations.len());
        let observations = synthesise_observations(&fx);
        let session_id = SessionId::new();
        let baseline_request = build_batch_request(session_id, &observations, false);
        let candidate_request = build_batch_request(session_id, &observations, args.residue);
        let baseline_scorer = Scorer::new(
            session_id,
            &observations,
            &baseline_request,
            &fx.must_mention,
            &fx.must_not_mention,
        );
        let candidate_scorer = Scorer::new(
            session_id,
            &observations,
            &candidate_request,
            &fx.must_mention,
            &fx.must_not_mention,
        );
        let projection = baseline_scorer.prompt(observations.len());
        let candidate_projection = args
            .residue
            .then(|| candidate_scorer.prompt(observations.len()));
        println!("  projection: {}", describe_projection(&projection));
        if let Some(candidate) = &candidate_projection {
            println!("  + residue: {}", describe_projection(candidate));
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&fx.name)
            .to_string();
        std::fs::create_dir_all(run_dir.join("requests"))?;
        std::fs::write(
            run_dir.join("requests").join(format!("{stem}.txt")),
            request_text(&baseline_request),
        )?;
        if args.residue {
            std::fs::write(
                run_dir.join("requests").join(format!("{stem}.residue.txt")),
                request_text(&candidate_request),
            )?;
        }

        let mut report = FixtureReport {
            fixture: fx.name.clone(),
            projection,
            candidate_projection,
            baseline: Vec::new(),
            candidate: Vec::new(),
        };
        for run in 1..=args.runs {
            // Run both providers concurrently per fixture so a slow one
            // doesn't double the wall-clock.
            let (a_res, b_res) = tokio::join!(
                run_one(
                    baseline_provider.clone(),
                    "baseline",
                    baseline_request.clone()
                ),
                run_one(
                    candidate_provider.clone(),
                    "candidate",
                    candidate_request.clone()
                ),
            );
            let case_stem = if args.runs > 1 {
                format!("{stem}.r{run}")
            } else {
                stem.clone()
            };
            let a = persist_case(
                "baseline",
                &fx.name,
                &case_stem,
                run,
                &run_dir,
                a_res,
                &baseline_scorer,
            )?;
            let b = persist_case(
                "candidate",
                &fx.name,
                &case_stem,
                run,
                &run_dir,
                b_res,
                &candidate_scorer,
            )?;
            for case in [&a, &b] {
                println!(
                    "  {:<9} r{run}: {:>5} ms  parsed={} updates={}  {}",
                    case.provider,
                    case.elapsed_ms,
                    case.parsed_ok,
                    case.update_count.map_or("-".into(), |n| n.to_string()),
                    case.fidelity.as_ref().map_or(String::new(), describe_page)
                );
            }
            // Force-flush per run so progress is visible when stdout
            // is piped (e.g. via `tee` or backgrounded runs). Without
            // this, full-buffering swallows every println! until exit.
            std::io::stdout().flush().ok();
            report.baseline.push(a);
            report.candidate.push(b);
        }
        reports.push(report);
    }

    let ab_report = AbReport {
        commit: crate::retrieval::report::commit_sha(),
        runs: args.runs,
        generated_seed: args.generated_seed,
        generated_sha256,
        verbatim_residue_ab: args.residue,
        baseline: SideMeta {
            provider: baseline.provider_name().to_string(),
            model: baseline.model.clone(),
            base_url: baseline.base_url.clone(),
        },
        candidate: SideMeta {
            provider: candidate.provider_name().to_string(),
            model: candidate.model.clone(),
            base_url: candidate.base_url.clone(),
        },
        fixtures: reports,
    };
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_string_pretty(&ab_report)?,
    )?;

    // Tail summary so the user can grok the run at a glance.
    println!();
    println!("=== summary ===");
    print!("{}", fidelity_summary(&ab_report.fixtures));
    let baseline_runs: Vec<&CaseRun> = ab_report
        .fixtures
        .iter()
        .flat_map(|f| &f.baseline)
        .collect();
    let candidate_runs: Vec<&CaseRun> = ab_report
        .fixtures
        .iter()
        .flat_map(|f| &f.candidate)
        .collect();
    let n = baseline_runs.len();
    let baseline_total: u128 = baseline_runs.iter().map(|c| c.elapsed_ms).sum();
    let candidate_total: u128 = candidate_runs.iter().map(|c| c.elapsed_ms).sum();
    let baseline_ok = baseline_runs.iter().filter(|c| c.parsed_ok).count();
    let candidate_ok = candidate_runs.iter().filter(|c| c.parsed_ok).count();
    println!(
        "baseline : {baseline_ok}/{n} parsed ok  total {baseline_total} ms  avg {} ms",
        baseline_total / n.max(1) as u128
    );
    println!(
        "candidate: {candidate_ok}/{n} parsed ok  total {candidate_total} ms  avg {} ms",
        candidate_total / n.max(1) as u128
    );
    println!();
    println!("outputs under: {}", run_dir.display());
    println!(
        "compare with:  diff -ru {}/baseline {}/candidate",
        run_dir.display(),
        run_dir.display()
    );
    Ok(())
}

/// Resolve `ProviderConfig` for one side from CLI args + env.
fn resolve_provider(side: &str, args: &AbArgs) -> Result<ResolvedConfig> {
    let (provider_str, base_url, model, api_key_inline, api_key_env, token_file) = match side {
        "baseline" => (
            args.baseline_provider.as_str(),
            args.baseline_base_url.clone(),
            args.baseline_model.clone(),
            args.baseline_api_key.clone(),
            args.baseline_api_key_env.clone(),
            args.baseline_token_file.clone(),
        ),
        "candidate" => (
            args.candidate_provider.as_str(),
            args.candidate_base_url.clone(),
            args.candidate_model.clone(),
            args.candidate_api_key.clone(),
            args.candidate_api_key_env.clone(),
            args.candidate_token_file.clone(),
        ),
        _ => bail!("unknown side {side}"),
    };

    let provider = match provider_str {
        "anthropic" => ProviderChoice::Anthropic,
        "openai" => ProviderChoice::OpenAi,
        "openai-compat" | "openai_compat" => ProviderChoice::OpenAiCompat,
        "openai-oauth" | "openai_oauth" => ProviderChoice::OpenAiOAuth,
        "codex" => ProviderChoice::Codex,
        "copilot" | "github-copilot" | "github_copilot" => ProviderChoice::Copilot,
        other => {
            bail!(
                "{side}: provider {other} not one of anthropic|openai|openai-compat|openai-oauth|codex|copilot"
            )
        }
    };

    let api_key = match (api_key_inline, api_key_env) {
        (Some(s), _) if !s.is_empty() => Some(SecretString::from(s)),
        (_, Some(env)) => match std::env::var(&env) {
            Ok(v) if !v.is_empty() => Some(SecretString::from(v)),
            _ => bail!("{side}: env var {env} is not set or empty"),
        },
        _ => None,
    };

    let auth = match provider.auth_requirement() {
        AuthRequirement::RequiredApiKey { env_var } => {
            ProviderAuth::required_api_key_from_env(env_var, api_key)
        }
        AuthRequirement::OptionalApiKey { env_var } => {
            ProviderAuth::optional_api_key_from_env(env_var, api_key)
        }
        AuthRequirement::OpenAiOAuthToken => {
            ProviderAuth::openai_oauth_token_file(token_file.ok_or_else(|| {
                anyhow::anyhow!("{side}: --{side}-token-file is required for openai-oauth")
            })?)
        }
        AuthRequirement::CodexAuthFile => ProviderAuth::codex(
            token_file.ok_or_else(|| {
                anyhow::anyhow!("{side}: --{side}-token-file is required for codex")
            })?,
            "codex",
        ),
        AuthRequirement::CopilotToken => ProviderAuth::copilot(
            token_file.ok_or_else(|| {
                anyhow::anyhow!("{side}: --{side}-token-file is required for copilot")
            })?,
            None,
            None,
            base_url.clone(),
        ),
        AuthRequirement::AnthropicOAuthToken => ProviderAuth::anthropic_oauth_token(api_key),
        AuthRequirement::CursorCli => ProviderAuth::cursor(ai_memory_llm::CursorAuth::new(
            "agent",
            std::env::temp_dir(),
            &std::env::vars_os().collect::<Vec<_>>(),
        )),
    };

    Ok(ResolvedConfig {
        provider,
        provider_str: provider_str.to_string(),
        model,
        base_url,
        auth,
    })
}

#[derive(Clone)]
struct ResolvedConfig {
    provider: ProviderChoice,
    provider_str: String,
    model: String,
    base_url: Option<String>,
    auth: ProviderAuth,
}

impl ResolvedConfig {
    fn provider_name(&self) -> &str {
        self.provider_str.as_str()
    }
}

impl From<ResolvedConfig> for ProviderConfig {
    fn from(r: ResolvedConfig) -> Self {
        Self {
            provider: r.provider,
            model: r.model,
            auth: r.auth,
            base_url: r.base_url,
            // Match the product default so provider comparisons exercise the
            // same schema-constrained path operators receive.
            compat_strict: true,
            // Comparisons must not differ by engine-side thinking behaviour;
            // keep the product default (off) for every candidate.
            compat_disable_thinking: false,
            compat_stream: None,
            request_timeout_secs: ai_memory_llm::DEFAULT_REQUEST_TIMEOUT_SECS,
            // The A/B harness compares consolidation quality; reasoning
            // effort stays at each model's own default unless a future
            // flag threads it through.
            reasoning_effort: None,
            // No operator header list here: `build_provider` still layers
            // the default user agent, which is what a gateway needs to
            // attribute harness traffic.
            extra_headers: ai_memory_llm::ExtraHeaders::default(),
        }
    }
}

fn make_provider(r: ResolvedConfig) -> Result<Arc<dyn LlmProvider>> {
    ai_memory_llm::build_provider(ProviderConfig::from(r))
        .map_err(anyhow::Error::from)
        .context("building provider")
}

pub(crate) fn load_fixtures(dir: &Path) -> Result<Vec<(PathBuf, Fixture)>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        let fx: Fixture =
            serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", p.display()))?;
        out.push((p, fx));
    }
    Ok(out)
}

pub(crate) fn synthesise_observations(fx: &Fixture) -> Vec<Observation> {
    let workspace_id = WorkspaceId::new();
    let project_id = ProjectId::new();
    let session_id = SessionId::new();
    fx.observations
        .iter()
        .map(|f| Observation {
            id: ai_memory_core::ObservationId::new(),
            session_id,
            workspace_id,
            project_id,
            kind: f
                .kind
                .parse::<ObservationKind>()
                .unwrap_or(ObservationKind::Other),
            extension: None,
            source_event: None,
            title: f.title.clone(),
            body: f.body.clone(),
            importance: 5,
            created_at: Timestamp::now(),
        })
        .collect()
}

struct ProviderResult {
    elapsed: Duration,
    /// Raw text the model emitted — always saved, even when parsing
    /// fails. This is the load-bearing artifact for debugging
    /// schema mismatches: you can see exactly what the model said.
    raw_text: String,
    /// Parsed-from-text JSON, if the response was valid JSON at all.
    raw_json: Option<serde_json::Value>,
    /// Schema-validated batch, if the JSON also satisfied the schema.
    batch: Option<ConsolidatedBatch>,
    error: Option<String>,
}

async fn run_one(
    provider: Arc<dyn LlmProvider>,
    side: &str,
    request: ai_memory_llm::ChatRequest,
) -> ProviderResult {
    let start = Instant::now();
    // Call `complete` directly — bypassing `complete_structured` —
    // so we can capture the raw text the model emitted, even if it
    // doesn't deserialise. The cost is that we re-implement the
    // "extract first balanced {…}" fallback locally, but knowing
    // exactly what the model said is what makes the harness useful.
    let chat_res = provider.complete(request).await;
    let elapsed = start.elapsed();
    let raw_text = match &chat_res {
        Ok(r) => r.text.clone(),
        Err(e) => format!("(provider error before any response: {e})"),
    };
    let (raw_json, batch, error) = match chat_res {
        Ok(_) => parse_response(&raw_text),
        Err(e) => (None, None, Some(e.to_string())),
    };
    if let Some(err) = &error {
        tracing::warn!(side, error = %err, "model output did not satisfy schema");
    } else {
        tracing::info!(
            side,
            ms = elapsed.as_millis() as u64,
            updates = batch.as_ref().map_or(0, |b| b.updates.len()),
            "parsed ok"
        );
    }
    ProviderResult {
        elapsed,
        raw_text,
        raw_json,
        batch,
        error,
    }
}

/// Parse a model response into (json, batch, error).
///
/// 1. Try the whole response as JSON.
/// 2. If that fails, search for the first balanced `{…}` block.
/// 3. If we have JSON, try to deserialise as `ConsolidatedBatch`.
fn parse_response(
    text: &str,
) -> (
    Option<serde_json::Value>,
    Option<ConsolidatedBatch>,
    Option<String>,
) {
    let json = serde_json::from_str::<serde_json::Value>(text.trim())
        .ok()
        .or_else(|| {
            extract_first_balanced_object(text).and_then(|s| serde_json::from_str(&s).ok())
        });
    let Some(json) = json else {
        return (None, None, Some("response is not valid JSON".into()));
    };
    match serde_json::from_value::<ConsolidatedBatch>(json.clone()) {
        Ok(batch) => (Some(json), Some(batch), None),
        Err(e) => (Some(json), None, Some(format!("schema mismatch: {e}"))),
    }
}

/// Best-effort `{…}` extractor — ignores braces inside strings.
fn extract_first_balanced_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        if b == b'"' {
            in_string = true;
            continue;
        }
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(text[start..=i].to_string());
            }
        }
    }
    None
}

fn persist_case(
    side: &str,
    fixture_name: &str,
    safe_name: &str,
    run: u32,
    run_dir: &Path,
    res: ProviderResult,
    scorer: &Scorer<'_>,
) -> Result<CaseRun> {
    let dir = run_dir.join(side);
    std::fs::create_dir_all(&dir)?;

    // 1. Raw model text — always saved, even on parse failure.
    //    This is the load-bearing debug artifact: a side-by-side
    //    diff of these files tells you exactly what each model said.
    let raw_path = dir.join(format!("{safe_name}.raw.txt"));
    std::fs::write(&raw_path, &res.raw_text)?;

    // 2. Parsed JSON (if response was valid JSON at all).
    if let Some(json) = &res.raw_json {
        let json_path = dir.join(format!("{safe_name}.json"));
        std::fs::write(&json_path, serde_json::to_string_pretty(json)?)?;
    }

    // 3. Flattened markdown rendering for easy eyeballing.
    let md_path = dir.join(format!("{safe_name}.md"));
    let md = render_markdown(side, fixture_name, &res);
    std::fs::write(&md_path, md)?;

    // 4. Per-case meta.
    let case = CaseRun {
        provider: side.to_string(),
        fixture: fixture_name.to_string(),
        run,
        elapsed_ms: res.elapsed.as_millis(),
        parsed_ok: res.batch.is_some(),
        error: res.error.clone(),
        update_count: res.batch.as_ref().map(|b| b.updates.len()),
        fidelity: res.batch.as_ref().map(|b| scorer.page(b)),
    };
    let meta_path = dir.join(format!("{safe_name}.meta.json"));
    std::fs::write(&meta_path, serde_json::to_string_pretty(&case)?)?;
    Ok(case)
}

fn render_markdown(side: &str, fixture_name: &str, res: &ProviderResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {side} — {fixture_name}\n\n"));
    out.push_str(&format!("- elapsed: {} ms\n", res.elapsed.as_millis()));
    if let Some(err) = &res.error {
        out.push_str(&format!("- error: `{err}`\n"));
    }
    // Always include the raw response so the human reviewer can
    // judge structure/style even when schema validation failed.
    if !res.raw_text.is_empty() {
        out.push_str("\n## Raw response\n\n```\n");
        out.push_str(&res.raw_text);
        out.push_str("\n```\n");
    }
    let Some(batch) = &res.batch else {
        return out;
    };
    out.push_str(&format!("\n- updates: {}\n", batch.updates.len()));
    if !batch.rationale.trim().is_empty() {
        out.push_str("\n## Rationale\n\n");
        out.push_str(batch.rationale.trim());
        out.push('\n');
    }
    for (i, u) in batch.updates.iter().enumerate() {
        out.push_str(&format!("\n## Update {} — `{}`\n\n", i + 1, u.path));
        out.push_str(&format!("**Title:** {}  \n", u.title));
        out.push_str(&format!("**Kind:** `{}`  \n", u.kind.as_str()));
        out.push_str(&format!("**Tier:** `{}`  \n", u.tier.as_str()));
        if !u.tags.is_empty() {
            out.push_str(&format!("**Tags:** {}  \n", u.tags.join(", ")));
        }
        out.push('\n');
        out.push_str(u.body_markdown.trim());
        out.push('\n');
    }
    out
}

/// One line per fixture: what the projection kept.
pub(crate) fn describe_projection(p: &PromptFidelity) -> String {
    let mut out = format!(
        "selected {}/{} truncated {}",
        p.selected, p.observations, p.truncated_bodies
    );
    if p.must_mention_total > 0 {
        let _ = write!(
            out,
            "  facts lost {}/{}",
            p.lost_in_projection.len(),
            p.must_mention_total
        );
        if !p.lost_in_projection.is_empty() {
            let _ = write!(out, " {:?}", p.lost_in_projection);
        }
    }
    if let Some(r) = p.recall_prompt {
        let _ = write!(out, "  recall_prompt {r:.3}");
    }
    if let Some(coverage) = p.error_line_observation_coverage {
        let _ = write!(out, "  error_obs {coverage:.3}");
    }
    if let Some(share) = p.file_error_line_share {
        let _ = write!(out, "  file_error_share {share:.3}");
    }
    if !p.must_not_mention_hits.is_empty() {
        let _ = write!(out, "  forbidden {:?}", p.must_not_mention_hits);
    }
    out
}

fn describe_page(f: &PageFidelity) -> String {
    let mut out = String::new();
    if let Some(loss) = f.llm_loss {
        let _ = write!(out, "llm_loss {loss:.2} ");
    }
    if let Some(r) = f.recall_page {
        let _ = write!(out, "recall_page {r:.3} ");
    }
    let _ = write!(
        out,
        "invented tokens {} links {} forbidden {}",
        f.hallucinated_tokens.len(),
        f.invented_wikilinks.len(),
        f.must_not_mention_hits.len()
    );
    out
}

/// Per fixture and side: mean and min-max of `llm_loss` and invented
/// tokens over the runs. A delta inside a side's own spread is noise.
fn fidelity_summary(fixtures: &[FixtureReport]) -> String {
    let mut out = String::new();
    for f in fixtures {
        for (side, cases) in [("baseline", &f.baseline), ("candidate", &f.candidate)] {
            let scored: Vec<&PageFidelity> =
                cases.iter().filter_map(|c| c.fidelity.as_ref()).collect();
            let losses: Vec<f64> = scored.iter().filter_map(|s| s.llm_loss).collect();
            let invented: Vec<f64> = scored
                .iter()
                .map(|s| s.hallucinated_tokens.len() as f64)
                .collect();
            let _ = writeln!(
                out,
                "{:<28} {:<9} parsed {}/{}  llm_loss {}  invented tokens {}",
                f.fixture,
                side,
                scored.len(),
                cases.len(),
                spread(&losses),
                spread(&invented)
            );
        }
    }
    out
}

fn spread(values: &[f64]) -> String {
    if values.is_empty() {
        return "-".into();
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    format!("{mean:.2} ({min:.2}-{max:.2})")
}
