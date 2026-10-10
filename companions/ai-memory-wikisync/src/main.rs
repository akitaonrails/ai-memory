//! Thin CLI: parse arguments once, call the library, render output.

use std::path::PathBuf;
use std::process::ExitCode;

use ai_memory_wikisync::bidi::{
    self, CheckStatus, DEFAULT_MAX_DELETES, Prefer, SyncArgs, SyncMode,
};
use ai_memory_wikisync::client::DEFAULT_SERVER_URL;
use ai_memory_wikisync::hook::{self, HookSpec, InstallArgs, OnMerge};
use ai_memory_wikisync::sync::{Mode, RunArgs, run};
use clap::{Parser, Subcommand, ValueEnum};

/// Exit codes. 2 is left to clap's usage errors.
const EXIT_ERROR: u8 = 1;
/// `sync --check`: imports, exports or deletes are pending.
const EXIT_DRIFT: u8 = 3;
/// `sync --check`: conflicts or refusals need a person; wins over drift.
const EXIT_BLOCKED: u8 = 4;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Team-wiki sync companion for ai-memory (#986): export, two-way sync, CI check and post-merge hook"
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
    /// With --check, exits 0 in sync, 3 on drift, 4 on conflicts or refusals.
    Sync(SyncCliArgs),
    /// Install a git post-merge hook that runs `sync` after each merge: a
    /// dry-run report unless --on-merge apply. Never stores a token.
    InstallHook(InstallHookArgs),
    /// Remove the post-merge hook block install-hook wrote, and nothing else.
    UninstallHook(UninstallHookArgs),
}

#[derive(Parser, Debug, Clone)]
struct SyncCliArgs {
    /// Perform the writes. Without it, sync stays a dry-run.
    #[arg(long)]
    apply: bool,
    /// Read-only CI check: write nothing (not even state) and exit 0 in
    /// sync, 3 when changes are pending, 4 on conflicts or refusals. A clone
    /// without sync state compares the repository with the server directly.
    #[arg(long, conflicts_with = "apply")]
    check: bool,
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

#[derive(Parser, Debug, Clone)]
struct InstallHookArgs {
    /// Print the hook block instead of writing it.
    #[arg(long)]
    print: bool,
    /// What the hook does after a merge.
    #[arg(long, value_enum, default_value = "report")]
    on_merge: OnMergeArg,
    /// Have the hook pass --propagate-deletes.
    #[arg(long)]
    propagate_deletes: bool,
    /// Hooks directory to write to, instead of the repository's .git/hooks.
    #[arg(long, value_name = "DIR")]
    hooks_dir: Option<PathBuf>,
    /// Add the block to an existing post-merge hook (POSIX shell only).
    #[arg(long)]
    append: bool,
    /// Server origin to bake into the hook. Without it the hook uses
    /// AI_MEMORY_SERVER_URL or the default when it runs.
    #[arg(long)]
    server: Option<String>,
    /// Refused: the hook reads AI_MEMORY_AUTH_TOKEN when it runs, so no
    /// token is ever written to disk.
    #[arg(long, hide = true)]
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
    /// Top-level wiki directory (family) to sync; repeatable.
    #[arg(long = "include", value_name = "FAMILY")]
    include: Vec<String>,
}

#[derive(Parser, Debug, Clone)]
struct UninstallHookArgs {
    /// A directory inside the repository whose hook to remove.
    #[arg(long, default_value = ".")]
    dest: PathBuf,
    /// Hooks directory to edit, instead of the repository's .git/hooks.
    #[arg(long, value_name = "DIR")]
    hooks_dir: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum OnMergeArg {
    Report,
    Apply,
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

impl InstallHookArgs {
    fn into_install(self) -> anyhow::Result<InstallArgs> {
        if self.token.is_some() {
            anyhow::bail!(
                "install-hook never stores a token; the hook reads AI_MEMORY_AUTH_TOKEN from \
                 the environment git runs it in"
            );
        }
        Ok(InstallArgs {
            spec: HookSpec {
                server: self.server,
                workspace: self.workspace,
                project: self.project,
                dest: self.dest,
                include: self.include,
                on_merge: match self.on_merge {
                    OnMergeArg::Report => OnMerge::Report,
                    OnMergeArg::Apply => OnMerge::Apply,
                },
                propagate_deletes: self.propagate_deletes,
            },
            print: self.print,
            hooks_dir: self.hooks_dir,
            append: self.append,
        })
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match dispatch(Cli::parse()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:?}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

async fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Commands::Plan(args) => run(&RunArgs::from(args), Mode::Plan).await?,
        Commands::Export(args) => {
            let mode = if args.apply {
                Mode::Apply
            } else {
                Mode::DryRun
            };
            run(&RunArgs::from(args), mode).await?
        }
        Commands::Sync(args) => {
            let mode = match (args.check, args.apply) {
                (true, _) => SyncMode::Check,
                (false, true) => SyncMode::Apply,
                (false, false) => SyncMode::DryRun,
            };
            let outcome = bidi::run(&SyncArgs::from(args), mode).await?;
            if mode == SyncMode::Check {
                return Ok(match outcome.status() {
                    CheckStatus::InSync => ExitCode::SUCCESS,
                    CheckStatus::Drift => ExitCode::from(EXIT_DRIFT),
                    CheckStatus::Blocked => ExitCode::from(EXIT_BLOCKED),
                });
            }
        }
        Commands::InstallHook(args) => hook::install(&args.into_install()?)?,
        Commands::UninstallHook(args) => hook::uninstall(&args.dest, args.hooks_dir.as_deref())?,
    }
    Ok(ExitCode::SUCCESS)
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
                    // install-hook's --token reads no env var; it is refused.
                    .filter(|arg| arg.get_id() == "token" && arg.get_env().is_some())
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
