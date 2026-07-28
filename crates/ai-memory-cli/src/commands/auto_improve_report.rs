//! `ai-memory auto-improve-report` — read-only telemetry for auto-improvement outcomes.

use ai_memory_consolidate::AutoImproveTelemetryReport;
use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::AutoImproveReportArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

#[derive(Serialize)]
struct AutoImproveReportRequest {
    workspace: String,
    project: String,
    since_days: u32,
    limit: usize,
    stage: bool,
}

#[derive(Deserialize, Serialize)]
struct AutoImproveReportStageResponse {
    run_id: String,
    proposal_ids: Vec<String>,
    sidecar_paths: Vec<String>,
    report: AutoImproveTelemetryReport,
}

/// Run the `auto-improve-report` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or rejects the read-only report request.
pub async fn run(config: &Config, args: AutoImproveReportArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let request = AutoImproveReportRequest {
        workspace,
        project: project.clone(),
        since_days: args.days,
        limit: args.limit,
        stage: args.stage,
    };

    if args.stage {
        let response: AutoImproveReportStageResponse =
            post_json(&endpoint, "/admin/auto-improve/report", &request).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&response)?);
        } else {
            println!("\nStaged auto-improve telemetry report for {project}\n");
            println!("Run: {}", response.run_id);
            println!("Proposals: {}", response.proposal_ids.join(", "));
            println!("Sidecars: {}", response.sidecar_paths.join(", "));
            println!("Summary: {}", response.report.summary);
            println!("\n--- machine-readable ---");
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        return Ok(());
    }

    let report: AutoImproveTelemetryReport =
        post_json(&endpoint, "/admin/auto-improve/report", &request).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human_report(&report, &project);
        println!("\n--- machine-readable ---");
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

fn print_human_report(report: &AutoImproveTelemetryReport, project: &str) {
    println!("\nAuto-improve telemetry for {project}\n");
    println!("Summary: {}", report.summary);
    println!(
        "Terminal denominator: {}",
        report.terminal_rates.denominator
    );
    println!(
        "Rates: approved {:.1}%, rejected {:.1}%, conflict {:.1}%, failed {:.1}%",
        report.terminal_rates.approved_rate * 100.0,
        report.terminal_rates.rejected_rate * 100.0,
        report.terminal_rates.conflict_rate * 100.0,
        report.terminal_rates.failed_rate * 100.0,
    );
    println!(
        "Runs: {} ({} with learning proposals)",
        report.aggregate.run_count, report.aggregate.runs_with_learning_proposals
    );

    if !report.aggregate.top_targets.is_empty() {
        println!("Top targets:");
        for row in report.aggregate.top_targets.iter().take(10) {
            println!("  - {}: {}", row.key, row.count);
        }
    }

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
