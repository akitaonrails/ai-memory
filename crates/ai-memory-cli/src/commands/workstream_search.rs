//! Explicit retrieval for history older than a managed startup packet.

use ai_memory_core::{NativeSessionIdentity, Sanitizer, WorkstreamEvent};
use anyhow::Result;

use crate::cli::WorkstreamSearchArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Search or tail one portable managed-workstream ledger.
pub async fn run(config: &Config, args: WorkstreamSearchArgs) -> Result<()> {
    let i18n = &config.i18n;
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
        println!(
            "{}",
            i18n.text("cli-workstream-search-no-matching-managed-workstream-events")
        );
        return Ok(());
    }
    for event in events {
        let role = event.display_label();
        println!(
            "{}",
            i18n.format(
                "cli-workstream-search-event",
                &[
                    ("value0", format!("{}", event.sequence)),
                    ("value1", event.agent.as_str().to_string()),
                    ("value2", role.to_string())
                ]
            )
        );
        if let Some(source) = &event.source_record_id {
            println!(
                "{}",
                i18n.format(
                    "cli-workstream-search-source-record",
                    &[("value0", serde_json::to_string(source)?.to_string())]
                )
            );
        }
        if !event.metadata.is_null() {
            println!(
                "{}",
                i18n.format(
                    "cli-workstream-search-provenance",
                    &[(
                        "value0",
                        serde_json::to_string(&event.metadata)?.to_string()
                    )]
                )
            );
        }
        println!("{}\n", event.content);
    }
    Ok(())
}
