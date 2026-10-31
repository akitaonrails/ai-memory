//! `ai-memory rename-project` — thin HTTP client for project rename.

use anyhow::{Context, Result};

use crate::cli::RenameProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Run the `rename-project` subcommand.
///
/// Resolves the current project name (auto-derived from the git repo root
/// when `--from` is omitted) -- or, when `--project-id` is given, skips name
/// resolution and renames that id directly, which is the only way to reach a
/// legacy row whose name is shadowed by another project's `legacy_name` --
/// sends the rename request to the server, then prints a human-readable
/// confirmation line.
///
/// # Errors
/// Returns an error when the server is unreachable or returns a non-2xx
/// response (e.g. 404 workspace/project not found, 422 name taken or invalid).
pub async fn run(config: &Config, args: RenameProjectArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.from.as_deref(), "--from")?;
    super::refuse_empty_explicit(args.project_id.as_deref(), "--project-id")?;
    super::refuse_empty_explicit(Some(args.to.as_str()), "--to")?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    // Rename happens inside one workspace, so the marker resolves it the same
    // way it does for every other command. `--to` stays literal: it names the
    // new project, it does not select a scope.
    let (workspace, from, project_id) = match args.project_id {
        Some(id) => (
            super::resolve_workspace(config, args.workspace.as_deref())?,
            None,
            Some(id),
        ),
        None => {
            let (ws, from) =
                super::resolve_scope(config, args.workspace.as_deref(), args.from.as_deref())?;
            (ws, Some(from), None)
        }
    };
    let requested = from
        .clone()
        .or_else(|| project_id.clone())
        .context(i18n.text("cli-rename-project-one-of-from-or-project-id-is-required"))?;
    let body = serde_json::json!({
        "workspace": workspace,
        "from": from,
        "project_id": project_id,
        "to": args.to,
    });
    let summary: serde_json::Value = post_json(&endpoint, "/admin/rename-project", &body).await?;
    let pages = summary["pages"].as_u64().unwrap_or(0);
    let old_name = renamed_from(&summary, requested);
    if let Some(warning) = summary["manifest_warning"].as_str() {
        eprintln!(
            "{}",
            i18n.format(
                "cli-move-project-ai-memory-warning-warning",
                &[("value0", warning.to_string())]
            )
        );
    }
    if let Err(error) = super::project_registry::rekey_scope(
        config, &endpoint, &workspace, &old_name, &workspace, &args.to,
    ) {
        eprintln!(
            "{}", i18n.format("cli-rename-project-ai-memory-project-renamed-but-the-client-local-checkout-link-could-not-be-up", &[("value0", format!("{:#}", error))]));
    }
    println!(
        "{}",
        i18n.format(
            "cli-rename-project-renamed-pages-now-under-the-new-name",
            &[
                ("value0", workspace.to_string()),
                ("value1", old_name.to_string()),
                ("value2", workspace.to_string()),
                ("value3", args.to.to_string()),
                ("value4", format!("{}", pages))
            ]
        )
    );
    Ok(())
}

/// The renamed project's old name as the server resolved it, the only way to
/// learn the name behind `--project-id`; the client-local checkout link is
/// keyed by that name.
fn renamed_from(summary: &serde_json::Value, requested: String) -> String {
    summary["from"].as_str().map_or(requested, str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rename_by_id_rekeys_the_link_under_the_resolved_name() {
        let id = "0193e7a1-0000-7000-8000-000000000001".to_owned();
        let summary = serde_json::json!({ "from": "api", "to": "api-v2" });
        assert_eq!(renamed_from(&summary, id.clone()), "api");
        assert_eq!(renamed_from(&serde_json::json!({}), id.clone()), id);
    }

    fn test_args() -> RenameProjectArgs {
        RenameProjectArgs {
            workspace: None,
            from: Some("old".into()),
            project_id: None,
            to: "new".into(),
        }
    }

    #[tokio::test]
    async fn an_empty_to_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = test_args();
        a.to = "   ".into();
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--to was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_from_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = test_args();
        a.from = Some("".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--from was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_workspace_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = test_args();
        a.workspace = Some("  ".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
    }
}
