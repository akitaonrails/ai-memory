//! `ai-memory lint` — thin HTTP client for the M8 lint pass.

use anyhow::Result;
use serde::Serialize;

use crate::cli::LintArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/lint`.
#[derive(Serialize)]
struct LintRequest {
    workspace: String,
    project: String,
    dry_run: bool,
    no_llm: bool,
}

/// Run the `lint` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or returns a non-2xx
/// response.
pub async fn run(config: &Config, args: LintArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/lint",
        &LintRequest {
            workspace,
            project,
            dry_run: args.dry_run,
            no_llm: args.no_llm,
        },
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
