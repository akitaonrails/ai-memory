//! `ai-memory merge-project` — losslessly join two project identities.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::MergeProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct MergeProjectRequest {
    workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    into: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    into_project_id: Option<String>,
    confirm: bool,
}

/// Run the `merge-project` subcommand.
pub async fn run(config: &Config, args: MergeProjectArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.from.as_deref(), "--from")?;
    super::refuse_empty_explicit(args.from_project_id.as_deref(), "--from-project-id")?;
    super::refuse_empty_explicit(args.into.as_deref(), "--into")?;
    super::refuse_empty_explicit(args.into_project_id.as_deref(), "--into-project-id")?;
    if args.from.is_none() == args.from_project_id.is_none() {
        bail!(
            "{}",
            i18n.text("cli-merge-project-exactly-one-of-from-or-from-project-id-is-required")
        );
    }
    if args.into.is_none() == args.into_project_id.is_none() {
        bail!(
            "{}",
            i18n.text("cli-merge-project-exactly-one-of-into-or-into-project-id-is-required")
        );
    }
    let workspace = super::resolve_workspace(config, args.workspace.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/merge-project",
        &MergeProjectRequest {
            workspace,
            from: args.from,
            from_project_id: args.from_project_id,
            into: args.into,
            into_project_id: args.into_project_id,
            confirm: args.confirm,
        },
    )
    .await?;

    let verb = if report["dry_run"].as_bool().unwrap_or(true) {
        "Would merge"
    } else {
        "Merged"
    };
    println!(
        "{}", i18n.format("cli-merge-project-verb-page-versions-sessions-observations-handoffs-workstreams-page-conflicts", &[("value0", verb.to_string()), ("value1", report["from"].as_str().unwrap_or("source").to_string()), ("value2", report["into"].as_str().unwrap_or("target").to_string()), ("value3", format!("{}", report["page_versions"].as_u64().unwrap_or(0))), ("value4", format!("{}", report["sessions"].as_u64().unwrap_or(0))), ("value5", format!("{}", report["observations"].as_u64().unwrap_or(0))), ("value6", format!("{}", report["handoffs"].as_u64().unwrap_or(0))), ("value7", format!("{}", report["workstreams"].as_u64().unwrap_or(0))), ("value8", format!("{}", report["page_conflicts"].as_u64().unwrap_or(0)))]));
    if !args.confirm {
        println!(
            "{}",
            i18n.text("cli-merge-project-re-run-the-same-command-with-confirm-to-apply-this-merge")
        );
    }
    if let Some(warning) = report["manifest_warning"].as_str() {
        eprintln!("{}", i18n.format("cli-merge-project-warning-merge-committed-but-scope-manifest-refresh-failed-warning", &[("value0", warning.to_string())]));
    }
    Ok(())
}
