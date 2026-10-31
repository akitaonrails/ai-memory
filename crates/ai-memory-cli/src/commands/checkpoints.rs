//! `ai-memory checkpoints` — list recent wiki git checkpoints.

use anyhow::Result;
use serde::{Deserialize, Serialize};

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
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
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
        println!("{}  {}  {}", cp.short_oid, cp.time, cp.summary);
    }
    Ok(())
}
