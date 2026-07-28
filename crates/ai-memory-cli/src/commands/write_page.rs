//! `ai-memory write-page` — write or update a wiki page via the server.
//!
//! Sends a `POST /admin/write-page` request to the running server.
//! The server handles workspace/project resolution, tier parsing,
//! frontmatter framing, and the atomic wiki write.

use std::io::Read;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::WritePageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct WritePageBody {
    workspace: String,
    project: String,
    path: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    tier: String,
    tags: Vec<String>,
    pinned: bool,
}

#[derive(Deserialize)]
struct WritePageResponseBody {
    page_id: String,
    path: String,
}

/// Run the `write-page` subcommand.
///
/// # Errors
/// Returns an error if stdin cannot be read (when `body == "-"`), or if
/// the POST to `/admin/write-page` fails.
pub async fn run(config: &Config, args: WritePageArgs) -> Result<()> {
    let body_text = if args.body == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading body from stdin")?;
        buf
    } else {
        args.body
    };

    // Resolve the project the same way read-page/search do: explicit flag wins,
    // otherwise derive the current project from host cwd / repo root. This keeps
    // write + read-back pairs from silently targeting different projects.
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let resp: WritePageResponseBody = post_json(
        &endpoint,
        "/admin/write-page",
        &WritePageBody {
            workspace: workspace.clone(),
            project: project.clone(),
            path: args.path.clone(),
            body: body_text,
            title: args.title,
            kind: args.kind,
            tier: args.tier,
            tags: args.tag,
            pinned: args.pinned,
        },
    )
    .await
    .context("writing page via server")?;

    let short_id = &resp.page_id[..resp.page_id.len().min(8)];
    println!(
        "✓ wrote {} (page_id={}) under {}/{}",
        resp.path, short_id, workspace, project
    );
    Ok(())
}
