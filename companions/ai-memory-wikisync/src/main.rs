//! Thin CLI: parse arguments once, call the library, render output.

use std::path::PathBuf;

use ai_memory_wikisync::bidi::{self, DEFAULT_MAX_DELETES, Prefer, SyncArgs, SyncMode};
use ai_memory_wikisync::client::DEFAULT_SERVER_URL;
use ai_memory_wikisync::sync::{Mode, RunArgs, run};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Team-wiki sync companion for ai-memory (#986): export and two-way sync"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List pages to create/update/unchanged against the destination.
    /// Never writes files or state.
    Plan(CommonArgs),
    /// Export pages into --dest. Dry-run unless --apply is passed.
    Export(ExportArgs),
    /// Sync both ways: repository edits go to the server through MCP,
    /// server edits come to --dest. Dry-run unless --apply is passed.
    Sync(SyncCliArgs),
}

#[derive(Parser, Debug, Clone)]
struct SyncCliArgs {
    /// Perform the writes. Without it, sync stays a dry-run.
    #[arg(long)]
    apply: bool,
    /// Which side wins a page changed in the repository and on the server.
    /// Without it, such a page is a conflict and nothing is written.
    #[arg(long, value_enum)]
    prefer: Option<PreferArg>,
    /// Propagate deletes: a file deleted in the repository deletes its
    /// server page, a page deleted on the server deletes its file, as long
    /// as the other side is unchanged since the last sync. Without it,
    /// deletes are only reported.
    #[arg(long)]
    propagate_deletes: bool,
    /// Refuse the whole run when it would delete more than this many pages
    /// and files.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_DELETES)]
    max_deletes: usize,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum PreferArg {
    Repo,
    Server,
}

#[derive(Parser, Debug, Clone)]
struct ExportArgs {
    /// Perform the writes. Without it, export stays a dry-run.
    #[arg(long)]
    apply: bool,
    /// Overwrite files edited locally since the last export.
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug, Clone)]
struct CommonArgs {
    /// ai-memory server origin, for example http://127.0.0.1:49374.
    #[arg(long, env = "AI_MEMORY_SERVER_URL", default_value = DEFAULT_SERVER_URL)]
    server: String,
    /// Bearer token; reads AI_MEMORY_AUTH_TOKEN when omitted. Never logged.
    #[arg(long, env = "AI_MEMORY_AUTH_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Source workspace on the server.
    #[arg(long)]
    workspace: String,
    /// Source project on the server.
    #[arg(long)]
    project: String,
    /// Destination directory inside the repository.
    #[arg(long)]
    dest: PathBuf,
    /// Top-level wiki directory (family) to export; repeatable. The
    /// allowlist is explicit: '*' is refused.
    #[arg(long = "include", value_name = "FAMILY")]
    include: Vec<String>,
}

impl From<ExportArgs> for RunArgs {
    fn from(args: ExportArgs) -> Self {
        Self {
            server: args.common.server,
            token: args.common.token,
            workspace: args.common.workspace,
            project: args.common.project,
            dest: args.common.dest,
            include: args.common.include,
            force: args.force,
        }
    }
}

impl From<CommonArgs> for RunArgs {
    fn from(args: CommonArgs) -> Self {
        Self {
            server: args.server,
            token: args.token,
            workspace: args.workspace,
            project: args.project,
            dest: args.dest,
            include: args.include,
            force: false,
        }
    }
}

impl From<SyncCliArgs> for SyncArgs {
    fn from(args: SyncCliArgs) -> Self {
        Self {
            run: RunArgs::from(args.common),
            prefer: args.prefer.map(|prefer| match prefer {
                PreferArg::Repo => Prefer::Repo,
                PreferArg::Server => Prefer::Server,
            }),
            propagate_deletes: args.propagate_deletes,
            max_deletes: args.max_deletes,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Plan(args) => run(&RunArgs::from(args), Mode::Plan).await,
        Commands::Export(args) => {
            let mode = if args.apply {
                Mode::Apply
            } else {
                Mode::DryRun
            };
            run(&RunArgs::from(args), mode).await
        }
        Commands::Sync(args) => {
            let mode = if args.apply {
                SyncMode::Apply
            } else {
                SyncMode::DryRun
            };
            bidi::run(&SyncArgs::from(args), mode).await.map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// A token in AI_MEMORY_AUTH_TOKEN must never surface in --help: clap
    /// prints live env values for `env` args unless they are hidden.
    #[test]
    fn token_env_value_is_hidden_from_help() {
        let mut command = Cli::command();
        let tokens: Vec<_> = command
            .get_subcommands_mut()
            .flat_map(|sub| {
                let name = sub.get_name().to_string();
                sub.get_arguments()
                    .filter(|arg| arg.get_id() == "token")
                    .map(|arg| (name.clone(), arg.is_hide_env_values_set()))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(!tokens.is_empty(), "subcommands expose --token");
        for (name, hidden) in tokens {
            assert!(hidden, "--token on {name} must set hide_env_values");
        }
        let plan_help = command
            .get_subcommands_mut()
            .find(|sub| sub.get_name() == "plan")
            .expect("plan subcommand")
            .render_help()
            .to_string();
        assert!(plan_help.contains("AI_MEMORY_AUTH_TOKEN"));
    }
}
