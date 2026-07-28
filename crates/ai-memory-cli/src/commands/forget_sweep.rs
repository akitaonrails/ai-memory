//! `ai-memory forget-sweep` — thin HTTP client for the M8 retention sweep.

use anyhow::Result;
use serde::Serialize;

use crate::cli::ForgetSweepArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/forget-sweep`.
#[derive(Serialize)]
struct ForgetSweepRequest {
    workspace: String,
    project: String,
    dry_run: bool,
}

/// Run the `forget-sweep` subcommand.
///
/// Sends the retention-sweep request to the server over HTTP and
/// prints the JSON response.
///
/// # Errors
/// Returns an error if the server is unreachable or returns a non-2xx
/// response.
pub async fn run(config: &Config, args: ForgetSweepArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/forget-sweep",
        &ForgetSweepRequest {
            workspace,
            project,
            dry_run: args.dry_run,
        },
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
