//! `ai-memory restore-page` — restore one wiki page from retained history.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::RestorePageArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct RestorePageRequest {
    workspace: String,
    project: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_id: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct RestorePageResponse {
    page_id: String,
    path: String,
    restored_from: String,
    #[serde(default)]
    pre_checkpoint: Option<String>,
    #[serde(default)]
    checkpoint: Option<String>,
}

/// Run the `restore-page` subcommand.
///
/// # Errors
/// Returns an error if an explicit scope flag, `--path`, `--from`, or
/// `--page-id` is empty; the server is unreachable; the requested checkpoint
/// or page version cannot be restored; or the restored page cannot be
/// reindexed.
pub async fn run(config: &Config, args: RestorePageArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    if args.path.trim().is_empty() {
        bail!(
            "{}",
            i18n.text(
                "cli-restore-page-path-was-passed-empty-specify-the-wiki-page-path-to-restore"
            )
        );
    }
    super::refuse_empty_explicit(args.from.as_deref(), "--from")?;
    super::refuse_empty_explicit(args.page_id.as_deref(), "--page-id")?;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let resp: RestorePageResponse = post_json(
        &endpoint,
        "/admin/restore-page",
        &RestorePageRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            path: args.path.clone(),
            rev: args.from.clone(),
            page_id: args.page_id.clone(),
        },
    )
    .await
    .context(i18n.text("cli-restore-page-restoring-page-via-server"))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp)?);
        return Ok(());
    }

    println!(
        "{}",
        i18n.format(
            "cli-restore-page-restored-under-from-page-id",
            &[
                ("value0", resp.path.to_string()),
                ("value1", workspace.to_string()),
                ("value2", project.to_string()),
                ("value3", resp.restored_from.to_string()),
                (
                    "value4",
                    super::write_page::short_page_id(&resp.page_id).to_string()
                )
            ]
        )
    );
    if let Some(pre) = resp.pre_checkpoint {
        println!(
            "{}",
            i18n.format(
                "cli-restore-page-pre-restore-checkpoint-pre",
                &[("value0", pre.to_string())]
            )
        );
    }
    if let Some(cp) = resp.checkpoint {
        println!(
            "{}",
            i18n.format(
                "cli-restore-page-restore-checkpoint-cp",
                &[("value0", cp.to_string())]
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(path: &str, from: Option<&str>, page_id: Option<&str>) -> RestorePageArgs {
        RestorePageArgs {
            path: path.into(),
            from: from.map(str::to_owned),
            page_id: page_id.map(str::to_owned),
            workspace: None,
            project: None,
            json: false,
        }
    }

    #[tokio::test]
    async fn an_empty_path_is_refused_before_scope_or_network_work() {
        for empty in ["", " \t "] {
            let error = run(&Config::default(), args(empty, Some("HEAD~1"), None))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("--path was passed empty"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_revision_is_refused_before_scope_or_network_work() {
        for empty in ["", " \t "] {
            let error = run(
                &Config::default(),
                args("notes/example.md", Some(empty), None),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("--from was passed empty"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_page_id_is_refused_before_scope_or_network_work() {
        for empty in ["", " \t "] {
            let error = run(
                &Config::default(),
                args("notes/example.md", None, Some(empty)),
            )
            .await
            .unwrap_err();
            assert!(
                error.to_string().contains("--page-id was passed empty"),
                "unexpected error: {error}"
            );
        }
    }
}
