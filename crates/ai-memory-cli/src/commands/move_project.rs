//! `ai-memory move-project` — thin HTTP client for cross-workspace project move.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::MoveProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/move-project`.
#[derive(Serialize)]
struct MoveProjectRequest {
    from_workspace: String,
    project: String,
    to_workspace: String,
    confirm: bool,
    force: bool,
    on_conflict: String,
}

/// Run the `move-project` subcommand.
///
/// Resolves the source project name (auto-derived from the git repo root
/// when `--project` is omitted), requires `--confirm` before sending the
/// request (a true-move re-stamp or a copy+purge merge, both irreversible),
/// then prints the report.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the server is unreachable,
/// or the server returns a non-2xx response.
pub async fn run(config: &Config, args: MoveProjectArgs) -> Result<()> {
    let (from_workspace, project) = super::resolve_scope(
        config,
        args.from_workspace.as_deref(),
        args.project.as_deref(),
    )?;

    if !args.confirm {
        bail!(
            "move-project moves {}/{} to workspace {}. If the destination has \
             no same-named project it is a lossless true-move (re-stamp in \
             place — sessions, observations and history preserved). If it \
             already has one, the pages are copied in and merged, then the \
             source is PURGED. Both are irreversible.\n\
             Re-run with --confirm to proceed:\n\n  \
             ai-memory move-project --from-workspace {} --project {} \
             --to-workspace {} --confirm",
            from_workspace,
            project,
            args.to_workspace,
            from_workspace,
            project,
            args.to_workspace,
        );
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/move-project",
        &MoveProjectRequest {
            from_workspace: from_workspace.clone(),
            project: project.clone(),
            to_workspace: args.to_workspace.clone(),
            confirm: true,
            force: args.force,
            on_conflict: args.on_conflict.clone(),
        },
    )
    .await?;

    let pages = report["pages_copied"].as_u64().unwrap_or(0);
    let purged = report["source_purged"].as_bool().unwrap_or(false);
    let moved_via = report["moved_via"].as_str().unwrap_or("");
    let skipped_count = report["pages_skipped"].as_array().map_or(0, |s| s.len());

    if moved_via == "true-move" {
        // Lossless: re-stamped in place, nothing copied or purged.
        println!(
            "Moved {}/{} → {}/{}: {pages} pages re-stamped (true move — \
             sessions, observations and history preserved).",
            from_workspace, project, args.to_workspace, project,
        );
    } else {
        // copy-purge (merge into an existing same-named project).
        let tail = if skipped_count > 0 {
            ", SOURCE LEFT INTACT (some pages unreadable — fix and re-run)"
        } else if purged {
            ", source purged"
        } else {
            ", SOURCE LEFT INTACT (partial copy)"
        };
        println!(
            "Moved {}/{} → {}/{}: {pages} pages copied (merged into existing \
             project){tail}.",
            from_workspace, project, args.to_workspace, project,
        );
        if skipped_count > 0 {
            println!(
                "Warning: {skipped_count} page(s) could not be read from the \
                 source and were skipped; the source was NOT purged. Fix and re-run.",
            );
        }
        if let Some(conflicts) = report["conflicts"].as_array().filter(|c| !c.is_empty()) {
            println!(
                "{} path conflict(s) — source page kept under a de-duplicated path \
                 (both versions preserved):",
                conflicts.len()
            );
            for c in conflicts {
                println!(
                    "  {} → {}",
                    c["path"].as_str().unwrap_or("?"),
                    c["moved_to"].as_str().unwrap_or("?"),
                );
            }
        }
    }
    Ok(())
}
