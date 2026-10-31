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

/// Turn a live-mode `/admin/embed` report into an exit status.
///
/// The server answers 200 even when pages failed so the operator can
/// read the counts out of the body; the exit code must carry that
/// signal too, or a run in which every page failed still reads as
/// success to a script. Absent fields count as zero, so a partial or
/// older report with no `failed` key cannot fail the command.
fn check_report(report: &serde_json::Value) -> Result<()> {
    let embedded = report["embedded"].as_u64().unwrap_or(0);
    let failed = report["failed"].as_u64().unwrap_or(0);
    let skipped = report["skipped"].as_u64().unwrap_or(0);
    if failed == 0 {
        return Ok(());
    }
    anyhow::bail!(
        "{failed} page(s) failed to embed ({embedded} embedded, {skipped} skipped); run `ai-memory embed` again to retry them"
    )
}

/// Run the `embed` subcommand.
///
/// Sends the request to the server over HTTP and prints the JSON
/// response. In dry-run mode the server counts pages that would be
/// embedded without calling the embedder or writing anything.
///
/// # Errors
/// Returns an error if the server is unreachable or returns a non-2xx
/// response, or (live mode only) if the report shows pages that failed
/// to embed — the full report is still printed to stdout first.
pub async fn run(config: &Config, args: EmbedArgs) -> Result<()> {
    let i18n = &config.i18n;
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
            super::resolve_workspace(config, args.workspace.as_deref())?,
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
        println!(
            "{}",
            i18n.format(
                "cli-embed-dry-run-would-embed-would-page-s-skipped-already-up-to-date",
                &[
                    ("value0", format!("{}", would)),
                    ("value1", format!("{}", skipped))
                ]
            )
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
        // Live mode: the report is on stdout, the exit code must agree.
        check_report(&report)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};
    use crate::config::{Config, RuntimeEnv};
    use clap::Parser;
    use serde_json::json;

    #[test]
    fn check_report_accepts_a_clean_live_report() {
        let report = json!({ "embedded": 7, "failed": 0, "skipped": 2 });
        super::check_report(&report).expect("failed == 0 must succeed");
    }

    #[test]
    fn check_report_fails_with_the_counts_when_pages_failed() {
        let report = json!({ "embedded": 3, "failed": 2, "skipped": 1 });
        let err = super::check_report(&report).expect_err("failed > 0 must fail");
        let msg = err.to_string();
        assert!(msg.contains("2 page(s) failed to embed"), "message: {msg}");
        assert!(msg.contains("3 embedded"), "message: {msg}");
        assert!(msg.contains("1 skipped"), "message: {msg}");
        assert!(
            msg.contains("run `ai-memory embed` again"),
            "message: {msg}"
        );
    }

    #[test]
    fn check_report_treats_absent_fields_as_zero() {
        let empty = json!({});
        super::check_report(&empty).expect("absent failed must count as 0");
        let embedded_only = json!({ "embedded": 4, "skipped": 1 });
        super::check_report(&embedded_only).expect("a report without a failed key must succeed");
    }

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
    fn force_with_explicit_workspace_bypasses_invalid_home_routes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(home.join(".ai-memory.toml"), "route_project=\"orphan\"\n").unwrap();
        let config = Config {
            runtime_env: RuntimeEnv::with_host_cwd_and_home_for_tests(
                repo.to_string_lossy(),
                home.to_string_lossy(),
            ),
            ..Config::default()
        };
        assert_eq!(
            super::super::resolve_workspace(&config, Some("explicit")).unwrap(),
            "explicit"
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
