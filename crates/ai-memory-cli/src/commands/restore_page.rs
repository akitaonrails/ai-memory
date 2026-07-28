//! `ai-memory restore-page` — restore one wiki page from a git checkpoint.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::RestorePageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct RestorePageRequest {
    workspace: String,
    project: String,
    path: String,
    rev: String,
}

#[derive(Deserialize, Serialize)]
struct RestorePageResponse {
    page_id: String,
    path: String,
    restored_from: String,
    #[serde(default)]
    pre_checkpoint: Option<String>,
    #[serde(default)]
    checkpoint: Option<String>,
}

/// Run the `restore-page` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable, the checkpoint/path cannot be
/// restored, or the restored page cannot be reindexed.
pub async fn run(config: &Config, args: RestorePageArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let resp: RestorePageResponse = post_json(
        &endpoint,
        "/admin/restore-page",
        &RestorePageRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            path: args.path.clone(),
            rev: args.from.clone(),
        },
    )
    .await
    .context("restoring page via server")?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    println!(
        "restored {} under {}/{} from {} (page_id={})",
        resp.path,
        workspace,
        project,
        resp.restored_from,
        &resp.page_id[..resp.page_id.len().min(8)]
    );
    if let Some(pre) = resp.pre_checkpoint {
        println!("pre-restore checkpoint: {pre}");
    }
    if let Some(cp) = resp.checkpoint {
        println!("restore checkpoint: {cp}");
    }
    Ok(())
}
