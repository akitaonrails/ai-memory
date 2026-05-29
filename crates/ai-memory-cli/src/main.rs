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

mod auth;
mod cli;
mod commands;
mod config;
mod http_client;
mod logging;
mod process_guard;

use cli::{Cli, Command};
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone();

    let config = Arc::new(Config::load(cli.config.as_deref(), cli.data_dir.clone())?);
    let _logging_guard = logging::init(&config)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %config.data_dir.display(),
        bind = %config.bind,
        "ai-memory starting",
    );

    match cli.command {
        Command::Init(args) => commands::init::run(&config, args, config_path.as_deref()),
        Command::Status(args) => commands::status::run(&config, args).await,
        Command::Search(args) => commands::search::run(&config, args).await,
        Command::ReadPage(args) => commands::read_page::run(&config, args).await,
        Command::WritePage(args) => commands::write_page::run(&config, args).await,
        Command::Serve(args) => commands::serve::run(&config, args).await,
        Command::Reset(args) => commands::reset::run(&config, args),
        Command::Backup(args) => commands::backup::run(&config, args).await,
        Command::Restore(args) => commands::restore::run(&config, args),
        Command::InstallHooks(args) => commands::install_hooks::run(&config, args),
        Command::InstallMcp(args) => commands::install_mcp::run(&config, args),
        Command::Commit(args) => commands::commit::run(&config, args).await,
        Command::LlmTest(args) => commands::llm_test::run(&config, args).await,
        Command::ForgetSweep(args) => commands::forget_sweep::run(&config, args).await,
        Command::Lint(args) => commands::lint::run(&config, args).await,
        Command::Embed(args) => commands::embed::run(&config, args).await,
        Command::GenerateAuthToken(args) => commands::generate_auth_token::run(&config, args),
        Command::SetupAgent(args) => commands::setup_agent::run(&config, args),
        Command::Bootstrap(args) => commands::bootstrap::run(&config, args).await,
        Command::InstallInstructions(args) => commands::install_instructions::run(&config, args),
        Command::Reorg(args) => commands::reorg::run(&config, args).await,
        Command::PurgeProject(args) => commands::purge_project::run(&config, args).await,
        Command::RenameProject(args) => commands::rename_project::run(&config, args).await,
        Command::Uninstall(args) => commands::uninstall::run(&config, args),
        Command::Auth(args) => commands::auth::run(&config, args).await,
    }
}
