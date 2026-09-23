//! `ai-memory-eval` — offline evaluation harnesses for ai-memory.
//!
//! Two subcommands:
//!
//! - `ab` — live A/B comparison of two LLM providers on the production
//!   consolidation prompt (see [`ab`] and `evals/README.md`).
//! - `retrieval` — LongMemEval retrieval benchmark driven end-to-end
//!   through a real `ai-memory serve` subprocess: hook-shaped ingestion,
//!   real search stack, R@k / hit@k per question category (see
//!   [`retrieval`] and `docs/benchmarks/`).

mod ab;
mod retrieval;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ai-memory-eval",
    about = "Evaluation harnesses for ai-memory (LLM A/B + retrieval benchmark)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Live A/B harness comparing two LLM providers on the ai-memory
    /// consolidation prompt.
    Ab(Box<ab::AbArgs>),
    /// LongMemEval retrieval benchmark through the real server stack.
    Retrieval(Box<retrieval::RetrievalArgs>),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ai_memory_eval=info,warn".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Ab(args) => ab::run(*args).await,
        Command::Retrieval(args) => retrieval::run(*args).await,
    }
}
