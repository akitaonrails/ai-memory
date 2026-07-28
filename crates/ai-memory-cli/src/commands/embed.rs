//! `ai-memory embed` — thin HTTP client for the M9 embedding backfill.

use anyhow::Result;
use serde::Serialize;

use crate::cli::EmbedArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/embed`.
#[derive(Serialize)]
struct EmbedRequest {
    workspace: String,
    project: String,
    reembed: bool,
    dry_run: bool,
    all_projects: bool,
}

/// Run the `embed` subcommand.
///
/// Sends the request to the server over HTTP and prints the JSON
/// response. In dry-run mode the server counts pages that would be
/// embedded without calling the embedder or writing anything.
///
/// # Errors
/// Returns an error if the server is unreachable or returns a non-2xx
/// response.
pub async fn run(config: &Config, args: EmbedArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    // Model migrations must reach stale rows in every project namespace.
    // Without an explicit `--project`, `--force` / `--reembed` fans out
    // across the whole workspace instead of the CWD-derived project only.
    let all_projects = args.force && args.project.is_none();
    // The fan-out still targets one workspace, and the marker decides which —
    // but only the workspace half is resolved there. Resolving the pair would
    // make `--force` fail on a cwd whose project name cannot be derived (the
    // stale-docker-wrapper bail, or a non-repo dir) for a name it then throws
    // away, and would announce a project the request never carries.
    let (workspace, project) = if all_projects {
        (
            super::resolve_workspace(config, args.workspace.as_deref()),
            String::new(),
        )
    } else {
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?
    };
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/embed",
        &EmbedRequest {
            workspace,
            project,
            // The CLI flag was historically named `force`; the server
            // field is `reembed` — map them here.
            reembed: args.force,
            dry_run: args.dry_run,
            all_projects,
        },
    )
    .await?;

    if args.dry_run {
        let would = report["would_embed"].as_u64().unwrap_or(0);
        let skipped = report["skipped"].as_u64().unwrap_or(0);
        println!("dry-run: would embed {would} page(s), {skipped} already up-to-date");
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};
    use clap::Parser;

    #[test]
    fn force_without_project_enables_all_projects() {
        let cli = Cli::try_parse_from(["ai-memory", "embed", "--force"]).unwrap();
        let Command::Embed(args) = cli.command else {
            panic!("expected embed command");
        };
        assert!(args.force);
        assert!(args.project.is_none());
        assert!(
            args.force && args.project.is_none(),
            "--force without --project must fan out to all projects"
        );
    }

    #[test]
    fn force_with_explicit_project_stays_scoped() {
        let cli = Cli::try_parse_from(["ai-memory", "embed", "--force", "--project", "consisanet"])
            .unwrap();
        let Command::Embed(args) = cli.command else {
            panic!("expected embed command");
        };
        let all_projects = args.force && args.project.is_none();
        assert!(
            !all_projects,
            "--force with --project must stay scoped to that project"
        );
    }
}
