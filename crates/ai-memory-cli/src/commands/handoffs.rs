//! Read-only listing of open cross-agent handoffs.
//!
//! `memory_handoff_cancel` takes an exact handoff id and nothing exposed one,
//! so an accumulated backlog was visible as a count in `status` and could not
//! be addressed (#513). Automatic expiry deliberately spares manual and
//! sibling-directory handoffs, so a months-old entry surviving here is the
//! policy working as designed rather than a bug — but the operator still needs
//! to see it to decide.

use std::fmt::Write as _;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::HandoffsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

#[derive(Debug, Deserialize, Serialize)]
struct OpenHandoff {
    id: String,
    from_agent: String,
    to_agent: Option<String>,
    cwd: Option<String>,
    created_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct OpenHandoffsResponse {
    handoffs: Vec<OpenHandoff>,
}

/// List the open handoffs for a project, oldest first.
///
/// # Errors
/// Returns [`anyhow::Error`] when the scope cannot be resolved, the server is
/// unreachable, or it answers non-2xx.
pub async fn run(config: &Config, args: HandoffsArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let response: OpenHandoffsResponse = get_json(
        &endpoint,
        "/admin/handoffs",
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
            ("limit", &args.limit.to_string()),
        ],
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response.handoffs)?);
        return Ok(());
    }

    if response.handoffs.is_empty() {
        println!("No open handoffs for {workspace}/{project}.");
        return Ok(());
    }

    let now_ms = jiff::Timestamp::now().as_millisecond();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Open handoffs for {workspace}/{project} (oldest first):"
    );
    for h in &response.handoffs {
        let target = h.to_agent.as_deref().unwrap_or("any agent");
        let _ = writeln!(
            out,
            "  {}  from {} -> {}",
            age(now_ms.saturating_sub(h.created_at_ms)),
            h.from_agent,
            target
        );
        let _ = writeln!(out, "    id: {}", h.id);
        if let Some(cwd) = h.cwd.as_deref() {
            let _ = writeln!(out, "    cwd: {cwd}");
        }
    }
    let _ = writeln!(
        out,
        "\nCancel one with the MCP tool `memory_handoff_cancel`, passing its id."
    );
    print!("{out}");
    Ok(())
}

/// Coarse age, because the operator's question is "is this stale", not "when
/// exactly".
fn age(ms: i64) -> String {
    let secs = ms.max(0) / 1_000;
    let days = secs / 86_400;
    if days > 0 {
        return format!("{days}d ago");
    }
    let hours = secs / 3_600;
    if hours > 0 {
        return format!("{hours}h ago");
    }
    format!("{}m ago", secs / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_reports_the_coarsest_useful_unit() {
        assert_eq!(age(0), "0m ago");
        assert_eq!(age(5 * 60 * 1_000), "5m ago");
        assert_eq!(age(3 * 3_600 * 1_000), "3h ago");
        assert_eq!(age(9 * 86_400 * 1_000), "9d ago");
    }

    /// A clock skew between server and client must not render as a huge
    /// positive age from a negative difference.
    #[test]
    fn a_future_timestamp_clamps_to_zero() {
        assert_eq!(age(-5_000), "0m ago");
    }
}
