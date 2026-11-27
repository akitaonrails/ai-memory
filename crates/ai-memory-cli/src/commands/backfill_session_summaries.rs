//! Thin HTTP client for `ai-memory backfill-session-summaries`.

use ai_memory_mcp::admin::{SessionSummaryBackfillReport, SessionSummaryBackfillRequest};
use anyhow::Result;

use crate::cli::BackfillSessionSummariesArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request a read-only preview or apply one bounded batch.
///
/// # Errors
/// Invalid scope, transport failures and server refusals propagate.
pub async fn run(config: &Config, args: BackfillSessionSummariesArgs) -> Result<()> {
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let report: SessionSummaryBackfillReport = post_json(
        &endpoint,
        "/admin/sessions/backfill-summaries",
        &SessionSummaryBackfillRequest {
            ended_before_days: args.ended_before_days,
            limit: args.limit,
            after: args.after,
            workspace: args.workspace,
            project: args.project,
            confirm: args.confirm,
        },
    )
    .await?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
