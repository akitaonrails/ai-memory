//! `ai-memory repair-split` — join a checkout split in two by its git
//! remote's arrival (#1993).
//!
//! The server moves the empty remote-named project's repository identity onto
//! the project that holds the sessions (`POST /admin/adopt-project-identity`),
//! so identity-based capture and `ai-memory run` reach the sessions again.
//! This client then points its own checkout links and bindings at that
//! project.

use std::str::FromStr as _;

use ai_memory_core::ProjectId;
use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::cli::RepairSplitArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// What the server reported after joining the projects.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Adopted {
    /// The session project, under its final name.
    pub(crate) project: String,
    /// Its name before.
    pub(crate) previous: String,
    /// The emptied project's name afterwards.
    pub(crate) archived: String,
    /// The repository identity that moved.
    pub(crate) identity: String,
}

/// Run the `repair-split` subcommand.
///
/// # Errors
/// Returns an error when the ids are malformed, the server is unreachable, or
/// it refuses (for example, the "empty" project holds sessions or pages).
pub async fn run(config: &Config, args: RepairSplitArgs) -> Result<()> {
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    let workspace = super::resolve_workspace(config, args.workspace.as_deref())?;
    let endpoint = ServerEndpoint::for_checkout(config).await?;
    let adopted = adopt(
        config,
        &endpoint,
        &workspace,
        &args.project_id,
        &args.from_project_id,
        args.archive_name.as_deref(),
    )
    .await?;
    println!(
        "{}",
        crate::localization::format(
            &config.i18n,
            "cli-repair-split-done",
            &[
                ("workspace", workspace),
                ("project", super::show::terminal_text(&adopted.project)),
                ("identity", super::show::terminal_text(&adopted.identity)),
                ("archived", super::show::terminal_text(&adopted.archived)),
            ]
        )
    );
    Ok(())
}

/// Join the projects on the server, then re-key this client's checkout links
/// and bindings. The server change is what joins them; a failed local update
/// is reported and leaves the next launch to resolve by identity.
pub(crate) async fn adopt(
    config: &Config,
    endpoint: &ServerEndpoint,
    workspace: &str,
    project_id: &str,
    from_project_id: &str,
    archive_name: Option<&str>,
) -> Result<Adopted> {
    let into = ProjectId::from_str(project_id)
        .with_context(|| format!("--project-id {project_id} is not a project id"))?;
    let from = ProjectId::from_str(from_project_id)
        .with_context(|| format!("--from-project-id {from_project_id} is not a project id"))?;
    let body = serde_json::json!({
        "workspace": workspace,
        "project_id": into.to_string(),
        "from_project_id": from.to_string(),
        "archive_name": archive_name,
    });
    let summary: serde_json::Value =
        post_json(endpoint, "/admin/adopt-project-identity", &body).await?;
    let adopted: Adopted = serde_json::from_value(summary)
        .context("reading the server's adopt-project-identity summary")?;
    let mut local = Ok(());
    if adopted.previous != adopted.project {
        local = super::project_registry::rekey_scope(
            config,
            endpoint,
            workspace,
            &adopted.previous,
            workspace,
            &adopted.project,
        );
    }
    let local = local.and_then(|()| {
        super::project_registry::rebind_project(config, endpoint, from, into, &adopted.project)
    });
    if let Err(error) = local {
        eprintln!(
            "{}",
            crate::localization::diagnostic(
                &config.i18n,
                format!(
                    "ai-memory: the projects were joined, but this checkout's local link could not be updated ({error:#}); the next launch resolves it by the repository"
                )
            )
        );
    }
    Ok(adopted)
}
