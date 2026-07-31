//! `ai-memory curator` — rule-based report-only maintenance review.

use ai_memory_consolidate::CuratorReport;
use ai_memory_store::SkippedProposal;
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
    /// The CLI is the only in-tree caller of `/admin/curator`, so a key it does
    /// not declare reaches nobody at all. A curator run stages a single report
    /// page: when that one collides, `proposal_ids` is empty and without this
    /// the command prints a bare, unexplained list. `default` keeps an older
    /// server (which sends no such key) parsing.
    #[serde(default)]
    skipped: Vec<SkippedProposal>,
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
            println!("{}", render_stage_human(&response));
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

fn render_stage_human(response: &StageResponse) -> String {
    let mut lines = vec![format!("Staged curator report run {}", response.run_id)];
    for (id, path) in response
        .proposal_ids
        .iter()
        .zip(response.sidecar_paths.iter())
    {
        lines.push(format!("  - {id}: {path}"));
    }
    lines.extend(super::skipped_proposal_lines(&response.skipped));
    lines.join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(skipped: serde_json::Value) -> serde_json::Value {
        let report = CuratorReport {
            workspace: "default".into(),
            project: "proj".into(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            dry_run: false,
            summary: "nothing alarming".into(),
            params: ai_memory_consolidate::CuratorParams::default(),
            findings: Vec::new(),
        };
        serde_json::json!({
            "run_id": "run-1",
            "proposal_ids": [],
            "sidecar_paths": [],
            "skipped": skipped,
            "report": serde_json::to_value(&report).expect("report serialises"),
        })
    }

    fn colliding() -> serde_json::Value {
        serde_json::json!([{
            "target_path": "_pending/curator/report.md",
            "reason": "a proposal is already pending review for this path",
        }])
    }

    #[test]
    fn skipped_survives_the_response_round_trip_into_json_output() {
        let response: StageResponse =
            serde_json::from_value(wire(colliding())).expect("server body parses");

        // `--json` prints this struct, so the assertion has to be on what the
        // struct serialises back to, not on what arrived.
        let printed = serde_json::to_value(&response).expect("re-serialise");
        assert_eq!(
            printed["skipped"][0]["target_path"], "_pending/curator/report.md",
            "--json dropped the skipped proposals: {printed}"
        );
    }

    #[test]
    fn a_collision_explains_the_empty_proposal_list() {
        let response: StageResponse =
            serde_json::from_value(wire(colliding())).expect("server body parses");
        let human = render_stage_human(&response);
        assert!(
            response.proposal_ids.is_empty(),
            "fixture must be the single-proposal collision case"
        );
        assert!(human.contains("_pending/curator/report.md"), "{human}");
        assert!(human.contains("already pending review"), "{human}");
    }

    #[test]
    fn a_clean_run_prints_no_skipped_section() {
        let response: StageResponse =
            serde_json::from_value(wire(serde_json::json!([]))).expect("server body parses");
        assert!(!render_stage_human(&response).contains("Skipped"));
    }

    #[test]
    fn a_server_without_the_field_still_parses() {
        let mut body = wire(serde_json::json!([]));
        body.as_object_mut().unwrap().remove("skipped");
        let response: StageResponse = serde_json::from_value(body).expect("older server body");
        assert!(response.skipped.is_empty());
    }
}
