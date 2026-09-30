//! `ai-memory list-projects` — plain read-only listing of every
//! workspace/project pair the server knows about.
//!
//! Before this command, the only way to see this from the client was to hit
//! `GET /api/v1/projects` directly with curl/Invoke-RestMethod, because
//! `show`'s own use of that endpoint is folded into an interactive picker
//! (and silently falls back to client-local scan results when the server
//! call fails). This is the plain, scriptable equivalent.

use anyhow::Result;

use crate::cli::ListProjectsArgs;
use crate::commands::show::ProjectRow;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

pub async fn run(config: &Config, args: ListProjectsArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let query = args
        .workspace
        .as_deref()
        .filter(|value| !value.is_empty())
        .map_or_else(Vec::new, |workspace| vec![("workspace", workspace)]);
    let mut rows: Vec<ProjectRow> = get_json(&endpoint, "/api/v1/projects", &query).await?;
    rows.sort_by(|a, b| {
        a.workspace_name
            .cmp(&b.workspace_name)
            .then_with(|| a.project_name.cmp(&b.project_name))
    });

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No projects found.");
        return Ok(());
    }

    let workspace_width = rows
        .iter()
        .map(|row| row.workspace_name.len())
        .max()
        .unwrap_or(0)
        .max("WORKSPACE".len());
    let project_width = rows
        .iter()
        .map(|row| row.project_name.len())
        .max()
        .unwrap_or(0)
        .max("PROJECT".len());

    println!(
        "{:workspace_width$}  {:project_width$}  {:>6}  LAST UPDATED",
        "WORKSPACE", "PROJECT", "PAGES"
    );
    for row in &rows {
        println!(
            "{:workspace_width$}  {:project_width$}  {:>6}  {}",
            row.workspace_name,
            row.project_name,
            row.page_count,
            row.last_updated.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}
