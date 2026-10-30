//! `ai-memory-eval` — offline evaluation harnesses for ai-memory.
//!
//! Five subcommands:
//!
//! - `ab` — live A/B comparison of two LLM providers on the production
//!   consolidation prompt, with literal-fidelity scoring (see [`ab`],
//!   [`fidelity`] and `evals/README.md`).
//! - `projection` — the zero-LLM half of that scoring: which fixture facts
//!   and literal tokens the observation projection lets into the prompt.
//! - `retrieval` — pinned retrieval benchmarks driven end-to-end
//!   through a real `ai-memory serve` subprocess: hook-shaped ingestion,
//!   real search stack, R@k / hit@k per question category (see
//!   [`retrieval`] and `docs/benchmarks/`).
//! - `retrieval-compare` — pairs two saved `retrieval` reports by question
//!   and lists the questions whose first hit moved.
//! - `abstention` — offline evidence-sufficiency signals, threshold sweeps,
//!   and optional structured LLM answerability evaluation.

mod ab;
mod abstention;
mod fidelity;
mod generate;
mod projection;
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
    /// Zero-LLM: which facts the consolidation prompt's observation
    /// projection keeps, per fixture.
    Projection(projection::ProjectionArgs),
    /// Pinned retrieval benchmarks through the real server stack.
    Retrieval(Box<retrieval::RetrievalArgs>),
    /// Pair two saved retrieval reports by question: which questions' first
    /// hit moved, by how many cutoffs, and in which question types.
    RetrievalCompare(retrieval::compare::CompareArgs),
    /// Offline evidence-sufficiency and abstention benchmark.
    Abstention(Box<abstention::AbstentionArgs>),
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
        Command::Projection(args) => projection::run(args),
        Command::Retrieval(args) => retrieval::run(*args).await,
        Command::RetrievalCompare(args) => retrieval::compare::run(args),
        Command::Abstention(args) => abstention::run(*args).await,
    }
}
