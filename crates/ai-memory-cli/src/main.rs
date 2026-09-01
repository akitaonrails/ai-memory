//! `ai-memory` binary entry point.
//!
//! Loads configuration once at startup, initialises tracing, then dispatches
//! to the requested subcommand. Domain crates take `&Config` by reference;
//! there is no global state, no `lazy_static`, no second config-read path
//! (lesson from agentmemory #456 / #469).

#![doc(html_no_source)]

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::info;

mod auth_bearer;
mod cli;
mod commands;
mod config;
mod http_client;
mod logging;
mod marker;
mod process_guard;

use cli::{Cli, Command};
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let Cli {
        data_dir,
        config: config_path,
        command,
    } = Cli::parse();

    // Hooks fire on every tool call: they must be cheap and must emit ONLY
    // their JSON object to stdout. Short-circuit before config load and
    // tracing init (added latency + possible stdout noise). The hook reads
    // its server URL + token from flags; it only needs the data-dir to locate
    // a stored OIDC token when no explicit `--auth-token` is given, so we pass
    // the bare path rather than loading the full config.
    let command = match command {
        Command::Hook(args) => return commands::hook::run(data_dir, args).await,
        Command::HookDrain(_args) => return commands::hook::run_drain(data_dir).await,
        // Completions are pure text derived from the command tree. Emitting
        // them must not require a loadable config or an initialised data dir
        // (they are typically generated before `init`, or in a packaging
        // step), and tracing must not get the chance to interleave anything
        // into the script on stdout.
        Command::Completions(args) => return commands::completions::run(args),
        other => other,
    };

    let config = Arc::new(Config::load(config_path.as_deref(), data_dir)?);
    // Only the long-running server warns when file logging degrades (an
    // operator wants to know persistent logs moved); one-shot client
    // commands degrade silently — their file logs are irrelevant and the
    // warning read like the command itself had a problem.
    let degrade_warnings = if matches!(command, Command::Serve(_)) {
        logging::DegradeWarnings::Loud
    } else {
        logging::DegradeWarnings::Quiet
    };
    let _logging_guard = logging::init(&config, degrade_warnings)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        server_url = %config.server_url,
        data_dir = %config.data_dir.display(),
        bind = %config.bind,
        "ai-memory starting",
    );

    match command {
        Command::Init(args) => commands::init::run(&config, args, config_path.as_deref()),
        Command::Status(args) => commands::status::run(&config, args).await,
        Command::Run(args) => {
            let exit_code = commands::run::run(&config, args).await?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Command::Show(args) => {
            let exit_code = commands::show::run(&config, args).await?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Command::Continue(args) => {
            let exit_code = commands::continue_session::run(&config, args).await?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Command::Handoffs(args) => commands::handoffs::run(&config, args).await,
        Command::Workstreams(args) => commands::workstreams::run(&config, args).await,
        Command::RenameWorkstream(args) => commands::rename_workstream::run(&config, args).await,
        Command::WorkstreamSearch(args) => commands::workstream_search::run(&config, args).await,
        Command::AuditContamination(args) => {
            commands::audit_contamination::run(&config, args).await
        }
        Command::Search(args) => commands::search::run(&config, args).await,
        Command::ReadPage(args) => commands::read_page::run(&config, args).await,
        Command::WritePage(args) => commands::write_page::run(&config, args).await,
        Command::DeletePage(args) => commands::delete_page::run(&config, args).await,
        Command::Serve(args) => commands::serve::run(&config, args).await,
        Command::Reset(args) => commands::reset::run(&config, args),
        Command::Compact(args) => commands::compact::run(&config, args).await,
        Command::Backup(args) => commands::backup::run(&config, args).await,
        Command::Restore(args) => commands::restore::run(&config, args),
        Command::Reindex(args) => commands::reindex::run(&config, args).await,
        Command::InstallHooks(args) => commands::install_hooks::run(&config, args),
        // `Hook` is handled in the fast-path above (before config/tracing).
        Command::Hook(args) => commands::hook::run(Some(config.data_dir.clone()), args).await,
        // `HookDrain` is handled in the fast-path above (before config/tracing).
        Command::HookDrain(_args) => commands::hook::run_drain(Some(config.data_dir.clone())).await,
        Command::InstallMcp(args) => commands::install_mcp::run(&config, args),
        Command::McpBridge(args) => commands::mcp_bridge::run(&config, args).await,
        Command::Commit(args) => commands::commit::run(&config, args).await,
        Command::Checkpoints(args) => commands::checkpoints::run(&config, args).await,
        Command::RestorePage(args) => commands::restore_page::run(&config, args).await,
        Command::LlmTest(args) => commands::llm_test::run(&config, args).await,
        Command::ForgetSweep(args) => commands::forget_sweep::run(&config, args).await,
        Command::Lint(args) => commands::lint::run(&config, args).await,
        Command::Curator(args) => commands::curator::run(&config, args).await,
        Command::AutoImproveReport(args) => commands::auto_improve_report::run(&config, args).await,
        Command::AutoImprove(args) => commands::auto_improve::run(&config, args).await,
        Command::FinalizeSession(args) => commands::finalize_session::run(&config, args).await,
        Command::PendingWrites(args) => commands::pending_writes::run(&config, args).await,
        Command::Embed(args) => commands::embed::run(&config, args).await,
        Command::GenerateAuthToken(args) => commands::generate_auth_token::run(&config, args),
        Command::SetupAgent(args) => commands::setup_agent::run(&config, args),
        Command::Bootstrap(args) => commands::bootstrap::run(&config, args).await,
        Command::InstallInstructions(args) => commands::install_instructions::run(&config, args),
        Command::InstallSkills(args) => commands::install_skills::run(&config, args),
        Command::Reorg(args) => commands::reorg::run(&config, args).await,
        Command::PurgeProject(args) => commands::purge_project::run(&config, args).await,
        Command::PurgeSession(args) => commands::purge_session::run(&config, args).await,
        Command::RenameProject(args) => commands::rename_project::run(&config, args).await,
        Command::MoveProject(args) => commands::move_project::run(&config, args).await,
        Command::MoveSession(args) => commands::move_session::run(&config, args).await,
        Command::Uninstall(args) => commands::uninstall::run(&config, args),
        Command::Auth(args) => commands::auth::run(&config, args).await,
        Command::User(args) => commands::user::run(&config, args).await,
        // `Completions` is handled in the fast-path above (before config/tracing).
        Command::Completions(args) => commands::completions::run(args),
    }
}
