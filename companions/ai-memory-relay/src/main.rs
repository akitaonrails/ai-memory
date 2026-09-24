//! Thin CLI over [`ai_memory_relay::relay`]. Parse, call, print, exit.

use std::path::PathBuf;

use ai_memory_relay::relay::{self, FlushOptions, Report};
use clap::{Parser, Subcommand};

/// Queue external lifecycle events for ai-memory.
///
/// Exit codes: 0 success, 2 failure, 3 flush ended with events pending.
#[derive(Debug, Parser)]
#[command(name = "ai-memory-relay", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Bind a queue directory to one destination, producer, actor and scope.
    Init {
        #[arg(long)]
        queue_dir: PathBuf,
        /// http(s) base URL of the ai-memory server. No userinfo, query or fragment.
        #[arg(long)]
        server_url: String,
        /// Producer namespace, sent as `extension` (also your `AI_MEMORY_CAPTURE_OWNER`).
        #[arg(long)]
        producer: String,
        /// Stable adapter-side operator namespace. Never a bearer token.
        #[arg(long)]
        actor: String,
        #[arg(long)]
        workspace: String,
        #[arg(long)]
        project: String,
    },
    /// Add events from a JSON array file. All-or-nothing.
    Enqueue {
        #[arg(long)]
        queue_dir: PathBuf,
        /// `[{"event_id","agent","event","body"}]`; body needs explicit `session_id` and `cwd`.
        #[arg(long)]
        file: PathBuf,
    },
    /// Deliver pending events through POST /hook/batch.
    Flush {
        #[arg(long)]
        queue_dir: PathBuf,
        /// Batches attempted in one flush. Finite by default: a flush never loops
        /// forever, and what it does not deliver stays queued for the next run.
        #[arg(long, default_value_t = 64)]
        max_batches: usize,
    },
    /// Payload-free counters for the queue, as JSON.
    Status {
        #[arg(long)]
        queue_dir: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Command::Init {
            queue_dir,
            server_url,
            producer,
            actor,
            workspace,
            project,
        } => relay::init(
            &queue_dir,
            &server_url,
            &producer,
            &actor,
            &workspace,
            &project,
        ),
        Command::Enqueue { queue_dir, file } => relay::enqueue(&queue_dir, &file),
        Command::Flush {
            queue_dir,
            max_batches,
        } => relay::flush(
            &queue_dir,
            &FlushOptions {
                max_batches: max_batches.max(1),
            },
        ),
        Command::Status { queue_dir } => relay::status(&queue_dir),
    };
    std::process::exit(finish(outcome));
}

fn finish(outcome: anyhow::Result<Report>) -> i32 {
    match outcome {
        Ok(report) => {
            for line in &report.summary {
                println!("{line}");
            }
            if let Some(failure) = &report.failure {
                eprintln!("error: {failure}");
            }
            report.exit_code()
        }
        Err(error) => {
            // `{error:#}` prints the context chain, which is built from paths,
            // counts and classes only.
            eprintln!("error: {error:#}");
            2
        }
    }
}
