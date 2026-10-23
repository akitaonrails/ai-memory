//! Explicit retrieval for history older than a managed startup packet.

use ai_memory_core::{NativeSessionIdentity, Sanitizer, WorkstreamEvent, WorkstreamEventKind};
use anyhow::Result;

use crate::cli::WorkstreamSearchArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Search or tail one portable managed-workstream ledger.
pub async fn run(config: &Config, args: WorkstreamSearchArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let path = format!("/workstream/{}/events", args.workstream_id);
    let limit = args.limit.to_string();
    let mut events: Vec<WorkstreamEvent> = get_json(
        &endpoint,
        &path,
        &[("q", args.query.as_str()), ("limit", limit.as_str())],
    )
    .await?;
    let sanitizer = Sanitizer::new(&config.sanitize)?;
    for event in &mut events {
        event.native_session_id =
            NativeSessionIdentity::project(&event.native_session_id, &sanitizer);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(());
    }
    if events.is_empty() {
        println!("No matching managed-workstream events.");
        return Ok(());
    }
    for event in events {
        // Adapters give tool events a role, so the kind decides their label;
        // otherwise a past tool call reads as a pending assistant action.
        let role = match event.kind {
            WorkstreamEventKind::ToolCall => "historical tool call (completed evidence)",
            WorkstreamEventKind::ToolResult => "historical tool result (completed evidence)",
            _ => event.role.as_deref().unwrap_or(event.kind.as_str()),
        };
        println!(
            "## Event {} | {} | {}",
            event.sequence,
            event.agent.as_str(),
            role,
        );
        if let Some(source) = &event.source_record_id {
            println!("Source record: {}", serde_json::to_string(source)?);
        }
        if !event.metadata.is_null() {
            println!("Provenance: {}", serde_json::to_string(&event.metadata)?);
        }
        println!("{}\n", event.content);
    }
    Ok(())
}
