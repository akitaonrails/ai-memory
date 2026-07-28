//! `ai-memory search` — run an FTS5 query against the wiki index.
//!
//! Thin HTTP client. Calls `GET /admin/search?q=…&limit=…`; renders
//! the hits as human text or JSON. Never opens the store directly.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::SearchArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// FTS5 hit, mirrors `ai_memory_store::PageHit` fields used here.
#[derive(Debug, Deserialize, Serialize)]
struct Hit {
    path: String,
    title: String,
    snippet: String,
    rank: f64,
}

/// Run the `search` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or returns non-2xx.
pub async fn run(config: &Config, args: SearchArgs) -> Result<()> {
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let limit_str = args.limit.to_string();
    let hits: Vec<Hit> = get_json(
        &ep,
        "/admin/search",
        &[
            ("q", args.query.as_str()),
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
            ("limit", limit_str.as_str()),
        ],
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else if hits.is_empty() {
        println!("no results for {:?}", args.query);
    } else {
        println!("{} result(s) for {:?}:", hits.len(), args.query);
        for hit in &hits {
            println!("  {}  rank={:.4}", hit.path, hit.rank);
            println!("    {}", hit.title);
            println!("    {}", hit.snippet);
        }
    }
    Ok(())
}
