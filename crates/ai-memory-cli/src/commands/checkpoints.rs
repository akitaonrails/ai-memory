//! `ai-memory checkpoints` — list recent wiki git checkpoints.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::show::terminal_text;
use crate::cli::CheckpointsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

#[derive(Debug, Deserialize, Serialize)]
struct CheckpointResponse {
    oid: String,
    short_oid: String,
    time: i64,
    summary: String,
}

/// Run the `checkpoints` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or returns a non-2xx
/// response.
pub async fn run(config: &Config, args: CheckpointsArgs) -> Result<()> {
    let i18n = &config.i18n;
    // Match `restore-page`, the command this one is paired with in the recovery
    // guide: list the wiki history of the server this checkout routes to, so an
    // OID printed here restores against the same server (#2186). An explicit
    // `AI_MEMORY_SERVER_URL` still lists the install-wide history, and a marker
    // that selects an unresolvable profile fails closed rather than silently
    // listing another server's checkpoints.
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let limit = args.limit.to_string();
    let checkpoints: Vec<CheckpointResponse> = get_json(
        &endpoint,
        "/admin/checkpoints",
        &[("limit", limit.as_str())],
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&checkpoints)?);
        return Ok(());
    }

    if checkpoints.is_empty() {
        println!("{}", i18n.text("cli-checkpoints-no-wiki-checkpoints-found"));
        return Ok(());
    }

    for cp in checkpoints {
        println!(
            "{}  {}  {}",
            terminal_text(&cp.short_oid),
            cp.time,
            terminal_text(&cp.summary)
        );
    }
    Ok(())
}
