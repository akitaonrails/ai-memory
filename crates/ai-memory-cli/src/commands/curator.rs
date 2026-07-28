//! `ai-memory curator` — rule-based report-only maintenance review.

use ai_memory_consolidate::CuratorReport;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::CuratorArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct CuratorRequest {
    workspace: String,
    project: String,
    dry_run: bool,
    stage: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct StageResponse {
    run_id: String,
    proposal_ids: Vec<String>,
    sidecar_paths: Vec<String>,
    report: CuratorReport,
}

/// Run the `curator` subcommand.
///
/// # Errors
/// Returns an error if mutually-exclusive mode flags are set or the server
/// rejects the request.
pub async fn run(config: &Config, args: CuratorArgs) -> Result<()> {
    if args.dry_run && args.stage {
        bail!("choose either --dry-run or --stage, not both");
    }
    let dry_run = args.dry_run || !args.stage;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let request = CuratorRequest {
        workspace,
        project: project.clone(),
        dry_run,
        stage: args.stage,
    };

    if args.stage {
        let response: StageResponse = post_json(&endpoint, "/admin/curator", &request).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&response)?);
        } else {
            println!("Staged curator report run {}", response.run_id);
            for (id, path) in response
                .proposal_ids
                .iter()
                .zip(response.sidecar_paths.iter())
            {
                println!("  - {id}: {path}");
            }
        }
    } else {
        let report: CuratorReport = post_json(&endpoint, "/admin/curator", &request).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print_human_report(&report, &project);
            println!("\n--- machine-readable ---");
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

fn print_human_report(report: &CuratorReport, project: &str) {
    println!("\nCurator dry-run for {project}\n");
    println!("Summary: {}", report.summary);
    println!("Findings: {}", report.findings.len());
    for finding in report.findings.iter().take(10) {
        println!(
            "  - {} [{}]: {}",
            finding.kind, finding.severity, finding.message
        );
    }
    if report.findings.len() > 10 {
        println!("  ... {} more", report.findings.len() - 10);
    }
}
