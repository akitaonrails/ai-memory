//! Checkout-local rename for managed workstreams.

use ai_memory_core::{RenameManagedWorkstreamRequest, RenamedManagedWorkstream};
use ai_memory_workstream::inspect_repository;
use anyhow::{Context as _, Result, bail};

use crate::cli::RenameWorkstreamArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Retitle one workstream selectable from this checkout.
pub async fn run(config: &Config, args: RenameWorkstreamArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    super::refuse_empty_explicit(args.from.as_deref(), "--from")?;
    if args.to.trim().is_empty() {
        bail!(
            "{}",
            i18n.text("cli-rename-workstream-to-was-passed-empty-specify-the-new-workstream-name")
        );
    }

    // clap's `conflicts_with` rejects passing both, but not passing neither:
    // the pair is optional on either side, so an empty invocation reaches here
    // with nothing to address.
    if args.from.is_none() && args.workstream_id.is_none() {
        bail!("{}", i18n.text("cli-rename-workstream-pass-from-name-or-workstream-id-id-to-choose-the-workstream-to-rename"));
    }
    let cwd = std::env::current_dir()
        .context(i18n.text("cli-rename-workstream-getting-managed-workstream-checkout"))?;
    let repository = inspect_repository(&cwd)?;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let renamed: RenamedManagedWorkstream = post_json(
        &endpoint,
        "/workstream/rename",
        &RenameManagedWorkstreamRequest {
            workspace,
            project,
            repo_fingerprint: repository.repo_fingerprint,
            worktree_fingerprint: repository.worktree_fingerprint,
            from: args.from,
            workstream_id: args.workstream_id,
            to: args.to,
        },
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&renamed)?);
        return Ok(());
    }
    print!("{}", render_human(&config.i18n, &renamed));
    Ok(())
}

fn render_human(
    i18n: &ai_memory_core::i18n::Localizer,
    renamed: &RenamedManagedWorkstream,
) -> String {
    if renamed.from == renamed.to {
        // A repeated rename is not a failure, but reporting it as a change
        // would be a lie: nothing was written.
        return i18n.format(
            "cli-rename-workstream-workstream-already-has-that-name",
            &[("value0", renamed.to.to_string())],
        );
    }
    i18n.format(
        "cli-rename-workstream-renamed-workstream-to-id",
        &[
            ("value0", renamed.from.to_string()),
            ("value1", renamed.to.to_string()),
            ("value2", format!("{}", renamed.workstream_id)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::WorkstreamId;

    fn renamed(from: &str, to: &str) -> RenamedManagedWorkstream {
        RenamedManagedWorkstream {
            workstream_id: WorkstreamId::new(),
            from: from.to_owned(),
            to: to.to_owned(),
        }
    }

    #[test]
    fn human_output_reports_both_names_and_the_stable_id() {
        let renamed = renamed("typo-nmae", "refactor-db");
        let rendered = render_human(&ai_memory_core::i18n::Localizer::default(), &renamed);
        assert!(rendered.contains("'typo-nmae' to 'refactor-db'"));
        // The id is indented far enough that a continuation line cannot be
        // mistaken for the start of another record, matching `workstreams`.
        assert!(rendered.contains("\n    id: "));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn renaming_to_the_same_name_does_not_claim_a_change() {
        let rendered = render_human(
            &ai_memory_core::i18n::Localizer::default(),
            &renamed("stable", "stable"),
        );
        assert!(rendered.contains("already has that name"));
        assert!(!rendered.contains("Renamed"));
    }

    fn test_args() -> RenameWorkstreamArgs {
        RenameWorkstreamArgs {
            workspace: None,
            project: None,
            from: Some("old".into()),
            workstream_id: None,
            to: "new".into(),
            json: false,
        }
    }

    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = test_args();
        a.project = Some("   ".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_workspace_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = test_args();
        a.workspace = Some("".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--workspace was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_from_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = test_args();
        a.from = Some("   ".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--from was passed empty"),
            "unexpected error: {error}"
        );
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
}
