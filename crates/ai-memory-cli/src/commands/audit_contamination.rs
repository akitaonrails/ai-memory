//! `ai-memory audit-contamination` — structural cross-project contamination audit.
//!
//! Thin HTTP client. Calls `GET /admin/audit-contamination` on the configured
//! server and renders the report as human text or JSON. Read-only — the server
//! only reports, never mutates; remediation is a separate operator step.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::AuditContaminationArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Server-shaped response. Mirrors `ai_memory_store::ContaminationReport`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Report {
    #[serde(default)]
    summary: Summary,
    #[serde(default)]
    findings: Vec<Finding>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Summary {
    #[serde(default)]
    sessions_misbucketed: usize,
    #[serde(default)]
    project_name_collisions: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct Finding {
    check: String,
    confidence: String,
    entity_kind: String,
    entity_id: String,
    landed_workspace: String,
    landed_project: String,
    #[serde(default)]
    expected_project: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    peer_projects: Option<String>,
    #[serde(default)]
    pages: Option<u64>,
    #[serde(default)]
    sessions: Option<u64>,
    #[serde(default)]
    collision_kind: Option<String>,
    #[serde(default)]
    last_activity_us: Option<i64>,
}

/// Run the `audit-contamination` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable, returns non-2xx, or the
/// response can't be parsed.
pub async fn run(config: &Config, args: AuditContaminationArgs) -> Result<()> {
    let i18n = &config.i18n;
    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let query = scope_query(&args)?;
    let report: Report = get_json(&ep, "/admin/audit-contamination", &query).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if report.findings.is_empty() {
        println!(
            "{}", i18n.text("cli-audit-contamination-no-structural-contamination-found-no-session-mis-bucketed-by-cwd-no-project"));
        return Ok(());
    }
    println!(
        "{}", i18n.format("cli-audit-contamination-contamination-audit-session-s-mis-bucketed-project-s-with-a-colliding-name", &[("value0", format!("{}", report.summary.sessions_misbucketed)), ("value1", format!("{}", report.summary.project_name_collisions))]));
    for f in &report.findings {
        let expected = f.expected_project.as_deref().unwrap_or("?");
        match f.check.as_str() {
            "session_wrong_bucket" => println!(
                "{}", i18n.format("cli-audit-contamination-session-landed-in-but-its-cwd-resolves-to-project", &[("value0", f.confidence.to_string()), ("value1", f.entity_id.to_string()), ("value2", f.landed_workspace.to_string()), ("value3", f.landed_project.to_string()), ("value4", f.cwd.as_deref().unwrap_or("?").to_string()), ("value5", expected.to_string())])),
            "project_name_collision" => println!(
                "{}", i18n.format("cli-audit-contamination-project-page-s-session-s-last-activity-collides-by-with-add-a-ai-memory-toml", &[("value0", f.confidence.to_string()), ("value1", f.entity_id.to_string()), ("value2", f.landed_workspace.to_string()), ("value3", f.landed_project.to_string()), ("value4", format!("{}", f.pages.unwrap_or(0))), ("value5", format!("{}", f.sessions.unwrap_or(0))), ("value6", f.last_activity_us
                    .map_or_else(|| "unknown".into(), |value| value.to_string()).to_string()), ("value7", f.collision_kind.as_deref().unwrap_or("spelling").to_string()), ("value8", f.peer_projects.as_deref().unwrap_or("?").to_string())])),
            other => println!(
                "{}", i18n.format("cli-audit-contamination-in", &[("value0", f.confidence.to_string()), ("value1", other.to_string()), ("value2", f.entity_id.to_string()), ("value3", f.landed_workspace.to_string()), ("value4", f.landed_project.to_string())])),
        }
    }
    Ok(())
}

fn scope_query(args: &AuditContaminationArgs) -> Result<Vec<(&str, &str)>> {
    match (args.workspace.as_deref(), args.project.as_deref()) {
        (Some(ws), Some(proj)) => Ok(vec![("workspace", ws), ("project", proj)]),
        (None, None) => Ok(Vec::new()),
        _ => bail!("--workspace and --project must be provided together"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(workspace: Option<&str>, project: Option<&str>) -> AuditContaminationArgs {
        AuditContaminationArgs {
            workspace: workspace.map(str::to_string),
            project: project.map(str::to_string),
            json: false,
        }
    }

    #[test]
    fn scope_query_rejects_partial_scope() {
        assert!(scope_query(&args(Some("default"), None)).is_err());
        assert!(scope_query(&args(None, Some("ai-memory"))).is_err());
    }

    #[test]
    fn scope_query_accepts_global_or_complete_scope() {
        assert!(scope_query(&args(None, None)).unwrap().is_empty());
        assert_eq!(
            scope_query(&args(Some("default"), Some("ai-memory"))).unwrap(),
            vec![("workspace", "default"), ("project", "ai-memory")]
        );
    }
}
