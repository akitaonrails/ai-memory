//! Explicit retrieval for history older than a managed startup packet.

use ai_memory_core::WorkstreamEvent;
use anyhow::Result;

use crate::cli::WorkstreamSearchArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Search or tail one portable managed-workstream ledger.
pub async fn run(config: &Config, args: WorkstreamSearchArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let path = format!("/workstream/{}/events", args.workstream_id);
    let limit = args.limit.to_string();
    let events: Vec<WorkstreamEvent> = get_json(
        &endpoint,
        &path,
        &[("q", args.query.as_str()), ("limit", limit.as_str())],
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(());
    }
    if events.is_empty() {
        println!("No matching managed-workstream events.");
        return Ok(());
    }
    for event in events {
        let role = event.role.as_deref().unwrap_or(event.kind.as_str());
        println!(
            "## Event {} | {} | {}\n{}\n",
            event.sequence,
            event.agent.as_str(),
            role,
            event.content
        );
    }
    Ok(())
}
