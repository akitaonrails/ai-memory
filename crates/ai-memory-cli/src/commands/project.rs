//! `ai-memory project` — project settings (#708).
//!
//! Thin HTTP client over `/admin/projects/*`, root-only like `ai-memory
//! grant`: access is an operator decision, made against the server rather than
//! a database the operator's laptop usually cannot open.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cli::{ProjectAccessArgs, ProjectArgs, ProjectCommand};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Dispatch a `project` subcommand.
///
/// # Errors
/// Transport failures, a non-root token, an unknown project or mode.
pub async fn run(config: &Config, args: ProjectArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    match args.command {
        ProjectCommand::Access(args) => access(&ep, args).await,
        ProjectCommand::Grants(args) => crate::commands::grant::list_for_project(&ep, &args).await,
    }
}

#[derive(Debug, Deserialize)]
struct AccessResponse {
    mode: String,
    previous: String,
    changed: bool,
    without_access: Vec<String>,
}

async fn access(ep: &ServerEndpoint, args: ProjectAccessArgs) -> Result<()> {
    let resp: AccessResponse = post_json(
        ep,
        "/admin/projects/access",
        &serde_json::json!({
            "workspace": args.workspace,
            "project": args.project,
            "mode": args.mode,
        }),
    )
    .await
    .context("setting project access")?;
    let repo = format!("{}/{}", args.workspace, args.project);
    if resp.changed {
        println!("{repo} is now {} (was {}).", resp.mode, resp.previous);
    } else {
        println!("{repo} was already {}; nothing changed.", resp.mode);
    }
    if !resp.without_access.is_empty() {
        println!();
        println!("These users have written to {repo} and hold no grant, so they are now refused:");
        for name in &resp.without_access {
            println!("  {name}");
        }
        println!(
            "Grant the ones who should keep access: ai-memory user grant --user <name> \
             --workspace {} --project {} --level write",
            args.workspace, args.project
        );
    }
    Ok(())
}
