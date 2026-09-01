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

    if args.expire_all {
        return expire(&endpoint, &workspace, &project, &args).await;
    }

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
            super::humanize_age_secs(now_ms.saturating_sub(h.created_at_ms) / 1_000),
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

#[cfg(test)]
mod tests {
    use super::super::humanize_age_secs;

    /// Both read-only listings render ages through one helper now; these pin
    /// the rendering the handoff listing depends on.
    #[test]
    fn age_reports_the_coarsest_useful_unit() {
        assert_eq!(humanize_age_secs(0), "just now");
        assert_eq!(humanize_age_secs(5 * 60), "5 minutes ago");
        assert_eq!(humanize_age_secs(3_600), "1 hour ago");
        assert_eq!(humanize_age_secs(3 * 3_600), "3 hours ago");
        assert_eq!(humanize_age_secs(9 * 86_400), "9 days ago");
    }

    /// Server and client clocks can disagree; a future timestamp must not
    /// render as an enormous age.
    #[test]
    fn a_future_timestamp_clamps_to_just_now() {
        assert_eq!(humanize_age_secs(-5), "just now");
    }
}

/// Request body for `POST /admin/handoffs/expire`.
#[derive(serde::Serialize)]
struct ExpireRequest<'a> {
    workspace: &'a str,
    project: &'a str,
    confirm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    older_than_days: Option<u32>,
}

/// Clear the open handoff backlog for one scope.
async fn expire(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    args: &HandoffsArgs,
) -> Result<()> {
    if !args.confirm {
        let age = args
            .older_than_days
            .map(|d| format!(" --older-than-days {d}"))
            .unwrap_or_default();
        anyhow::bail!(
            "--expire-all marks every open handoff in {workspace}/{project} expired.\n\
             The rows and their summaries are kept; they just stop being consumable.\n\
             Unlike the automatic sweep this also clears manual handoffs and ones from \
             another directory.\n\n  \
             ai-memory handoffs --workspace {workspace} --project {project}{age} \
             --expire-all --confirm"
        );
    }

    let report: serde_json::Value = crate::http_client::post_json(
        endpoint,
        "/admin/handoffs/expire",
        &ExpireRequest {
            workspace,
            project,
            confirm: true,
            older_than_days: args.older_than_days,
        },
    )
    .await?;

    let expired = report["expired"].as_u64().unwrap_or(0);
    if expired == 0 {
        println!("No open handoffs to expire in {workspace}/{project}.");
    } else {
        println!(
            "Expired {expired} open handoff{} in {workspace}/{project}. \
             The rows are kept and remain visible in the audit log; they are \
             no longer offered to an agent.",
            if expired == 1 { "" } else { "s" }
        );
    }
    Ok(())
}
