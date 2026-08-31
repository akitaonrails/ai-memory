//! `ai-memory purge-project` — thin HTTP client for project purge.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::PurgeProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/purge-project`.
#[derive(Serialize)]
struct PurgeProjectRequest {
    workspace: String,
    project: String,
    confirm: bool,
    /// Purge even when a managed workstream still holds a live run lease.
    force: bool,
    /// Rebuild the FTS indexes and VACUUM after the delete commits.
    compact: bool,
}

/// Run the `purge-project` subcommand.
///
/// Resolves the project name (auto-derived from the git repo root when
/// `--project` is omitted), requires `--confirm` before sending the
/// destructive request, then prints the JSON summary.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the server is unreachable,
/// or the server returns a non-2xx response.
pub async fn run(config: &Config, args: PurgeProjectArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    if !args.confirm {
        bail!(
            "purge-project is destructive and irreversible.\n\
             Re-run with --confirm to proceed:\n\n  \
             ai-memory purge-project --workspace {} --project {} --confirm",
            workspace,
            project,
        );
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/purge-project",
        &PurgeProjectRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            confirm: true,
            force: args.force,
            compact: args.compact,
        },
    )
    .await?;

    // Human-friendly one-liner followed by the raw JSON for scripting.
    let fallback_label = format!("{}/{}", workspace, project);
    let label = report["label"].as_str().unwrap_or(&fallback_label);
    let pages = report["pages_deleted"].as_u64().unwrap_or(0);
    let sessions = report["sessions_deleted"].as_u64().unwrap_or(0);
    let observations = report["observations_deleted"].as_u64().unwrap_or(0);
    let handoffs = report["handoffs_deleted"].as_u64().unwrap_or(0);
    let embeddings = report["embeddings_deleted"].as_u64().unwrap_or(0);
    // Workstreams cascade out of the project row, so a scope that looks empty
    // by every other counter can still be carrying a managed workstream and
    // its portable event ledger. Always name them.
    let workstreams = report["workstreams_deleted"].as_u64().unwrap_or(0);
    let managed_runs = report["managed_runs_deleted"].as_u64().unwrap_or(0);
    println!(
        "Purged {label}: {pages} pages, {sessions} sessions, \
         {observations} observations, {handoffs} handoffs, {embeddings} embeddings, \
         {workstreams} workstreams, {managed_runs} managed runs."
    );
    if let Some(ids) = report["workstream_ids"].as_array()
        && !ids.is_empty()
    {
        println!(
            "The following workstream segment directories are now orphaned under \
             <data_dir>/raw/workstreams/ and can be removed:"
        );
        for id in ids.iter().filter_map(serde_json::Value::as_str) {
            println!("  - {id}");
        }
    }
    if let Some(failed) = report["files_failed"].as_array()
        && !failed.is_empty()
    {
        println!(
            "Warning: {} wiki file(s) could not be removed from disk (DB rows are gone).",
            failed.len()
        );
    }
    if report["compacted"].as_bool().unwrap_or(false) {
        println!("Database compacted: freed bytes reclaimed.");
    } else {
        println!(
            "Note: this was a logical delete. The project is unreachable through \
             the API and search, but its bytes remain in the database file until \
             it is rewritten. Re-run with --compact to reclaim them (slow), and \
             see docs/lifecycle-ops.md for what that does and does not guarantee."
        );
    }
    Ok(())
}
