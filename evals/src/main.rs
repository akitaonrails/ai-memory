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
//! 4. Prints a side-by-side summary table to stdout.
//!
//! Quality judgement is left to the human reading the outputs — the
//! harness only reports the OBJECTIVE deltas (latency, update
//! counts, schema-parse success). Anything subtler (faithfulness,
//! hallucination, structure) is for you to eyeball.
//!
//! ## Running it
//!
//! See `evals/README.md` for canonical invocations. The short
//! version:
//!
//! ```bash
//! cargo run -p ai-memory-eval -- \
//!     --baseline-provider openai-compat \
//!     --baseline-base-url https://openrouter.ai/api/v1 \
//!     --baseline-model moonshotai/kimi-k2.6 \
//!     --baseline-api-key-env OPENROUTER_API_KEY \
//!     --candidate-provider openai-compat \
//!     --candidate-base-url http://192.168.0.90:11434/v1 \
//!     --candidate-model qwen3:32b \
//!     --candidate-api-key ollama-local
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use jiff::Timestamp;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use ai_memory_consolidate::{ConsolidatedBatch, build_batch_request};
use ai_memory_core::{Observation, ObservationKind, ProjectId, SessionId, WorkspaceId};
use ai_memory_llm::{AuthRequirement, LlmProvider, ProviderAuth, ProviderChoice, ProviderConfig};

#[derive(Parser, Debug)]
#[command(
    name = "ai-memory-eval",
    about = "Live A/B harness comparing two LLM providers on the ai-memory consolidation prompt"
)]
struct Args {
    /// Fixture directory (each `*.json` is one synthetic session log).
    #[arg(long, default_value = "evals/fixtures")]
    fixtures: PathBuf,

    /// Output root. A timestamped subdir gets created here per run.
    #[arg(long, default_value = "evals/runs")]
    out: PathBuf,

    /// Baseline provider (`anthropic` / `openai` / `openai-compat` / `openai-oauth` / `copilot`).
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

    /// Baseline OpenAI OAuth token file for `--baseline-provider openai-oauth`.
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

    /// Candidate OpenAI OAuth token file for `--candidate-provider openai-oauth`.
    #[arg(long)]
    candidate_token_file: Option<PathBuf>,
}

/// JSON shape of an eval fixture file. We only care about
/// observations the consolidation prompt actually consumes; the
/// other Observation fields get populated with dummies at runtime.
#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    /// Free-text describing the scenario; not consumed by the runner
    /// but useful when reading fixture files by hand.
    #[allow(dead_code)]
    #[serde(default)]
    description: String,
    observations: Vec<FixtureObservation>,
}

#[derive(Debug, Deserialize)]
struct FixtureObservation {
    kind: String,
    title: String,
    #[serde(default)]
    body: String,
}

/// One result of one provider on one fixture.
#[derive(Debug, Serialize)]
struct CaseRun {
    provider: String,
    fixture: String,
    elapsed_ms: u128,
    parsed_ok: bool,
    error: Option<String>,
    update_count: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ai_memory_eval=info,warn".into()),
        )
        .init();

    let args = Args::parse();

    let baseline = resolve_provider("baseline", &args)?;
    let candidate = resolve_provider("candidate", &args)?;

    let baseline_provider =
        make_provider(baseline.clone()).context("building baseline provider")?;
    let candidate_provider =
        make_provider(candidate.clone()).context("building candidate provider")?;

    let fixtures = load_fixtures(&args.fixtures)
        .with_context(|| format!("loading fixtures from {}", args.fixtures.display()))?;
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
    println!("  out      : {}", run_dir.display());
    println!();

    let mut results: Vec<(CaseRun, CaseRun)> = Vec::with_capacity(fixtures.len());
    for (path, fx) in fixtures {
        println!("→ {} ({} observations)", fx.name, fx.observations.len());
        let observations = synthesise_observations(&fx);
        let session_id = SessionId::new();

        // Run both providers concurrently per fixture so a slow one
        // doesn't double the wall-clock.
        let req_a = build_batch_request(session_id, &observations);
        let req_b = build_batch_request(session_id, &observations);
        let (a_res, b_res) = tokio::join!(
            run_one(baseline_provider.clone(), "baseline", req_a),
            run_one(candidate_provider.clone(), "candidate", req_b),
        );

        let a = persist_case("baseline", &fx.name, &path, &run_dir, a_res)?;
        let b = persist_case("candidate", &fx.name, &path, &run_dir, b_res)?;

        println!(
            "  baseline : {:>5} ms  parsed={} updates={}",
            a.elapsed_ms,
            a.parsed_ok,
            a.update_count.map_or("-".into(), |n| n.to_string())
        );
        println!(
            "  candidate: {:>5} ms  parsed={} updates={}",
            b.elapsed_ms,
            b.parsed_ok,
            b.update_count.map_or("-".into(), |n| n.to_string())
        );
        // Force-flush per fixture so progress is visible when stdout
        // is piped (e.g. via `tee` or backgrounded runs). Without
        // this, full-buffering swallows every println! until exit.
        std::io::stdout().flush().ok();
        results.push((a, b));
    }

    // Tail summary so the user can grok the run at a glance.
    println!();
    println!("=== summary ===");
    let n = results.len();
    let baseline_total: u128 = results.iter().map(|(a, _)| a.elapsed_ms).sum();
    let candidate_total: u128 = results.iter().map(|(_, b)| b.elapsed_ms).sum();
    let baseline_ok = results.iter().filter(|(a, _)| a.parsed_ok).count();
    let candidate_ok = results.iter().filter(|(_, b)| b.parsed_ok).count();
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
fn resolve_provider(side: &str, args: &Args) -> Result<ResolvedConfig> {
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
        "copilot" | "github-copilot" | "github_copilot" => ProviderChoice::Copilot,
        other => {
            bail!(
                "{side}: provider {other} not one of anthropic|openai|openai-compat|openai-oauth|copilot"
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
        AuthRequirement::CopilotToken => ProviderAuth::copilot(
            token_file.ok_or_else(|| {
                anyhow::anyhow!("{side}: --{side}-token-file is required for copilot")
            })?,
            None,
            None,
            base_url.clone(),
        ),
        AuthRequirement::AnthropicOAuthToken => ProviderAuth::anthropic_oauth_token(api_key),
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
        }
    }
}

fn make_provider(r: ResolvedConfig) -> Result<Arc<dyn LlmProvider>> {
    ai_memory_llm::build_provider(ProviderConfig::from(r))
        .map_err(anyhow::Error::from)
        .context("building provider")
}

fn load_fixtures(dir: &Path) -> Result<Vec<(PathBuf, Fixture)>> {
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

fn synthesise_observations(fx: &Fixture) -> Vec<Observation> {
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
    fixture_path: &Path,
    run_dir: &Path,
    res: ProviderResult,
) -> Result<CaseRun> {
    let safe_name = fixture_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(fixture_name);
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
        elapsed_ms: res.elapsed.as_millis(),
        parsed_ok: res.batch.is_some(),
        error: res.error.clone(),
        update_count: res.batch.as_ref().map(|b| b.updates.len()),
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
