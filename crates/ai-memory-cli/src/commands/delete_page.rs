//! `ai-memory delete-page` — delete a wiki page via the server.
//!
//! Sends a `POST /admin/delete-page` request to the running server.
//! The server resolves `(workspace, project)` via the same path the
//! read tools use (`resolve_ws_proj`) so a delete targeting a project
//! that exists in multiple workspaces can never silently land in the
//! wrong slot — closes the structural gap that `memory_delete_page`
//! (MCP) had until this milestone.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::DeletePageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct DeletePageBody {
    workspace: String,
    project: String,
    path: String,
}

#[derive(Deserialize)]
struct DeletePageResponseBody {
    path: String,
    deleted: bool,
}

/// Run the `delete-page` subcommand.
///
/// # Errors
/// Returns an error if the POST to `/admin/delete-page` fails (network
/// failure, scope resolution failure, admission webhook rejecting the
/// delete, or filesystem error).
pub async fn run(config: &Config, args: DeletePageArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    if args.path.trim().is_empty() {
        bail!(
            "{}",
            i18n.text("cli-delete-page-path-was-passed-empty-specify-the-wiki-page-path-to-delete")
        );
    }

    // Resolve the project the same way write-page/read-page do: explicit
    // flag wins, otherwise derive the current project from host cwd / repo
    // root. Keeps delete + read-back pairs targeting the same project.
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let resp: DeletePageResponseBody = post_json(
        &endpoint,
        "/admin/delete-page",
        &DeletePageBody {
            workspace: workspace.clone(),
            project: project.clone(),
            path: args.path.clone(),
        },
    )
    .await
    .context(i18n.text("cli-delete-page-deleting-page-via-server"))?;

    let status = if resp.deleted { "✓ deleted" } else { "no-op" };
    println!(
        "{}",
        i18n.format(
            "cli-delete-page-under",
            &[
                ("value0", status.to_string()),
                ("value1", resp.path.to_string()),
                ("value2", workspace.to_string()),
                ("value3", project.to_string())
            ]
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(path: &str, project: Option<&str>) -> DeletePageArgs {
        DeletePageArgs {
            path: path.into(),
            workspace: None,
            project: project.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_any_request() {
        let config = Config::default();
        let a = args("notes/example.md", Some("   "));
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_workspace_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = args("notes/example.md", Some("app"));
        a.workspace = Some("".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_path_is_refused_before_any_request() {
        let config = Config::default();
        let a = args("   ", Some("app"));
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--path was passed empty"),
            "unexpected error: {error}"
        );
    }
}
