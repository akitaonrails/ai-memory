//! `ai-memory read-page` — display the full body of a wiki page.
//!
//! Two resolution modes:
//! - `--path <path>`: fetch the page at that exact wiki path.
//! - positional `<query>`: FTS5 search scoped to the current project;
//!   fetches the top-ranking hit's full body.
//!
//! The project is auto-detected from the current directory (same heuristic
//! as `search`). Pass `--project` to target a different project explicitly.
//!
//! Thin HTTP client. Never opens the store or wiki directly.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::ReadPageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Mirrors the `ReadPageResponse` struct on the server side.
#[derive(Debug, Deserialize, Serialize)]
struct PageContent {
    path: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    project: String,
    title: Option<String>,
    body: String,
    frontmatter: serde_json::Value,
}

/// Run the `read-page` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable, returns non-2xx,
/// or neither `--path` nor a query is supplied.
pub async fn run(config: &Config, args: ReadPageArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config(config);
    let project = super::resolve_project_name(config, args.project.as_deref())?;

    let page: PageContent = if let Some(ref raw_path) = args.path {
        get_json(
            &ep,
            "/admin/read-page",
            &[
                ("workspace", args.workspace.as_str()),
                ("project", project.as_str()),
                ("path", raw_path.as_str()),
            ],
        )
        .await?
    } else if let Some(ref query) = args.query {
        get_json(
            &ep,
            "/admin/read-page",
            &[
                ("workspace", args.workspace.as_str()),
                ("project", project.as_str()),
                ("q", query.as_str()),
            ],
        )
        .await?
    } else {
        bail!("provide a query (positional) or --path <wiki-path>");
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&page)?);
    } else {
        if let Some(ref title) = page.title {
            println!("# {title}");
        }
        println!("path: {}", page.path);
        println!();
        print!("{}", page.body);
        if !page.body.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}
