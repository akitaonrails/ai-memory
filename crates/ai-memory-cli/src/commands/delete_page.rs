//! `ai-memory delete-page` — delete a wiki page via the server.
//!
//! Sends a `POST /admin/delete-page` request to the running server.
//! The server resolves `(workspace, project)` via the same path the
//! read tools use (`resolve_ws_proj`) so a delete targeting a project
//! that exists in multiple workspaces can never silently land in the
//! wrong slot — closes the structural gap that `memory_delete_page`
//! (MCP) had until this milestone.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::DeletePageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct DeletePageBody {
    workspace: String,
    project: String,
    path: String,
}

#[derive(Deserialize)]
struct DeletePageResponseBody {
    path: String,
    deleted: bool,
}

/// Run the `delete-page` subcommand.
///
/// # Errors
/// Returns an error if the POST to `/admin/delete-page` fails (network
/// failure, scope resolution failure, admission webhook rejecting the
/// delete, or filesystem error).
pub async fn run(config: &Config, args: DeletePageArgs) -> Result<()> {
    // Resolve the project the same way write-page/read-page do: explicit
    // flag wins, otherwise derive the current project from host cwd / repo
    // root. Keeps delete + read-back pairs targeting the same project.
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let resp: DeletePageResponseBody = post_json(
        &endpoint,
        "/admin/delete-page",
        &DeletePageBody {
            workspace: workspace.clone(),
            project: project.clone(),
            path: args.path.clone(),
        },
    )
    .await
    .context("deleting page via server")?;

    let status = if resp.deleted { "✓ deleted" } else { "no-op" };
    println!("{} {} under {}/{}", status, resp.path, workspace, project);
    Ok(())
}
