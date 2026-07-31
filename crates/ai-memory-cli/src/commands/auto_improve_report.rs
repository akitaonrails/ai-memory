//! `ai-memory auto-improve-report` — read-only telemetry for auto-improvement outcomes.

use ai_memory_consolidate::AutoImproveTelemetryReport;
use ai_memory_store::SkippedProposal;
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
    /// The CLI is the only in-tree caller of `/admin/auto-improve/report`, so a
    /// key it does not declare reaches nobody at all. This run stages a single
    /// telemetry page: when that one collides, `Proposals:` below prints an
    /// empty line with nothing saying why. `default` keeps an older server
    /// (which sends no such key) parsing.
    #[serde(default)]
    skipped: Vec<SkippedProposal>,
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
            println!("{}", render_stage_human(&response, &project));
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

fn render_stage_human(response: &AutoImproveReportStageResponse, project: &str) -> String {
    let mut lines = vec![
        format!("\nStaged auto-improve telemetry report for {project}\n"),
        format!("Run: {}", response.run_id),
        format!("Proposals: {}", response.proposal_ids.join(", ")),
        format!("Sidecars: {}", response.sidecar_paths.join(", ")),
        format!("Summary: {}", response.report.summary),
    ];
    lines.extend(super::skipped_proposal_lines(&response.skipped));
    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(skipped: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "run_id": "run-1",
            "proposal_ids": [],
            "sidecar_paths": [],
            "skipped": skipped,
            "report": {
                "workspace": "default",
                "project": "proj",
                "generated_at": "2026-01-01T00:00:00Z",
                "since_created_at": 0,
                "summary": "no learning proposals in window",
                "params": { "since_days": 30, "top_limit": 10 },
                "aggregate": {
                    "run_count": 0,
                    "runs_with_learning_proposals": 0,
                    "proposals_by_status": [],
                    "proposals_by_operation": [],
                    "proposals_by_edit_mode": [],
                    "proposals_by_kind": [],
                    "maintenance_proposals_by_kind": [],
                    "top_targets": [],
                    "rejections_by_reason": [],
                    "repeated_rejection_fingerprints": [],
                    "rejected_targets": [],
                },
                "terminal_rates": {
                    "denominator": 0,
                    "approved_rate": 0.0,
                    "rejected_rate": 0.0,
                    "conflict_rate": 0.0,
                    "failed_rate": 0.0,
                },
                "findings": [],
                "blind_spots": [],
            },
        })
    }

    fn colliding() -> serde_json::Value {
        serde_json::json!([{
            "target_path": "_pending/auto-improve/telemetry.md",
            "reason": "a proposal is already pending review for this path",
        }])
    }

    #[test]
    fn skipped_survives_the_response_round_trip_into_json_output() {
        let response: AutoImproveReportStageResponse =
            serde_json::from_value(wire(colliding())).expect("server body parses");

        // `--json` prints this struct, so the assertion has to be on what the
        // struct serialises back to, not on what arrived.
        let printed = serde_json::to_value(&response).expect("re-serialise");
        assert_eq!(
            printed["skipped"][0]["target_path"], "_pending/auto-improve/telemetry.md",
            "--json dropped the skipped proposals: {printed}"
        );
    }

    #[test]
    fn a_collision_explains_the_empty_proposals_line() {
        let response: AutoImproveReportStageResponse =
            serde_json::from_value(wire(colliding())).expect("server body parses");
        assert!(
            response.proposal_ids.is_empty(),
            "fixture must be the single-proposal collision case"
        );
        let human = render_stage_human(&response, "proj");
        assert!(
            human.contains("_pending/auto-improve/telemetry.md"),
            "{human}"
        );
        assert!(human.contains("already pending review"), "{human}");
    }

    #[test]
    fn a_clean_run_prints_no_skipped_section() {
        let response: AutoImproveReportStageResponse =
            serde_json::from_value(wire(serde_json::json!([]))).expect("server body parses");
        assert!(!render_stage_human(&response, "proj").contains("Skipped"));
    }

    #[test]
    fn a_server_without_the_field_still_parses() {
        let mut body = wire(serde_json::json!([]));
        body.as_object_mut().unwrap().remove("skipped");
        let response: AutoImproveReportStageResponse =
            serde_json::from_value(body).expect("older server body");
        assert!(response.skipped.is_empty());
    }
}
