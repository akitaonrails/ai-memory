//! `ai-memory purge-session` — thin HTTP client for single-session purge.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::PurgeSessionArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/purge-session`.
#[derive(Serialize)]
struct PurgeSessionRequest {
    workspace: String,
    project: String,
    session_id: String,
    confirm: bool,
    /// Rebuild the FTS indexes and VACUUM after the delete commits.
    compact: bool,
}

/// Run the `purge-session` subcommand.
///
/// Requires the full session UUID and `--confirm`. The scope is resolved the
/// same way as every other project-scoped command, and the server refuses a
/// session that does not belong to it, so a UUID alone is never authority
/// over another workspace or project.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the session id is not a
/// UUID, the server is unreachable, or the server returns a non-2xx
/// response (including `404` for a session outside the named scope).
pub async fn run(config: &Config, args: PurgeSessionArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    // Validate locally so an obvious typo fails before anything destructive
    // is sent. The server validates again — this is convenience, not the
    // security boundary.
    let session_id = args.session_id.trim();
    if uuid::Uuid::parse_str(session_id).is_err() {
        bail!(
            "`--session-id {session_id}` is not a UUID. Pass the full \
             `sessions.id` value; a prefix or a workstream id will not match."
        );
    }

    if !args.confirm {
        bail!(
            "purge-session is destructive and irreversible.\n\
             Re-run with --confirm to proceed:\n\n  \
             ai-memory purge-session --workspace {workspace} --project {project} \
             --session-id {session_id} --confirm",
        );
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/purge-session",
        &PurgeSessionRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            session_id: session_id.to_owned(),
            confirm: true,
            compact: args.compact,
        },
    )
    .await?;

    let n = |key: &str| report[key].as_u64().unwrap_or(0);
    // The session id is deliberately not echoed. The caller passed it, so
    // repeating it adds nothing they do not have, and this command exists to
    // make a session stop existing — writing its id into terminal scrollback
    // and shell history leaves a pointer to the thing just erased. The scope
    // and the counts are what confirm the operation did what was asked.
    println!(
        "Purged session from {workspace}/{project}: \
         {} observations, {} handoffs, {} pages, {} auto-improve runs.",
        n("observations_deleted"),
        n("handoffs_deleted"),
        n("pages_deleted"),
        n("auto_improve_runs_deleted"),
    );
    if let Some(paths) = report["removed_paths"].as_array()
        && !paths.is_empty()
    {
        println!("Wiki pages removed:");
        for path in paths.iter().filter_map(serde_json::Value::as_str) {
            println!("  - {path}");
        }
    }
    if report["compacted"].as_bool().unwrap_or(false) {
        println!("Database compacted: freed bytes reclaimed.");
    } else {
        println!(
            "Note: this was a logical delete. The session is unreachable through \
             the API and search, but its bytes remain in the database file until \
             it is rewritten. Re-run with --compact to reclaim them (slow), and \
             see `docs/` for what that does and does not guarantee."
        );
    }
    Ok(())
}
