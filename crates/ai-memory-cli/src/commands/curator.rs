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
    let i18n = &config.i18n;
    if args.dry_run && args.stage {
        bail!(
            "{}",
            i18n.text("cli-curator-choose-either-dry-run-or-stage-not-both")
        );
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
            print_human_report(i18n, &report, &project);
            println!("{}", i18n.text("cli-auto-improve-report-machine-readable"));
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

fn render_stage_human(response: &StageResponse) -> String {
    let mut lines = vec![format!("Staged curator report run {}", response.run_id)];
    lines.extend(
        response
            .proposal_ids
            .iter()
            .zip(response.sidecar_paths.iter())
            .map(|(id, path)| format!("  - {id}: {path}")),
    );
    lines.extend(super::skipped_proposal_lines(&response.skipped));
    lines.join("\n")
}

fn print_human_report(
    i18n: &ai_memory_core::i18n::Localizer,
    report: &CuratorReport,
    project: &str,
) {
    println!(
        "{}",
        i18n.format(
            "cli-curator-curator-dry-run-for-project",
            &[("value0", project.to_string())]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-auto-improve-report-summary",
            &[("value0", report.summary.to_string())]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-auto-improve-report-findings",
            &[("value0", format!("{}", report.findings.len()))]
        )
    );
    for finding in report.findings.iter().take(10) {
        println!(
            "  - {} [{}]: {}",
            finding.kind, finding.severity, finding.message
        );
    }
    if report.findings.len() > 10 {
        println!(
            "{}",
            i18n.format(
                "cli-auto-improve-report-more",
                &[("value0", format!("{}", report.findings.len() - 10))]
            )
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(skipped: Vec<SkippedProposal>) -> StageResponse {
        StageResponse {
            run_id: "run-1".into(),
            proposal_ids: Vec::new(),
            sidecar_paths: Vec::new(),
            skipped,
            report: CuratorReport {
                workspace: "default".into(),
                project: "project".into(),
                generated_at: "2026-01-01T00:00:00Z".into(),
                dry_run: false,
                summary: "summary".into(),
                params: ai_memory_consolidate::CuratorParams::default(),
                findings: Vec::new(),
            },
        }
    }

    #[test]
    fn staged_collision_reaches_human_and_json_output() {
        let response = response(vec![SkippedProposal {
            target_path: "_reports/curator.md".into(),
            reason: "a proposal is already pending review for this path".into(),
        }]);
        let human = render_stage_human(&response);
        assert!(human.contains("_reports/curator.md"), "{human}");
        assert!(human.contains("already pending review"), "{human}");
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["skipped"][0]["target_path"], "_reports/curator.md");
    }

    #[test]
    fn older_server_response_without_skipped_still_parses() {
        let mut json = serde_json::to_value(response(Vec::new())).unwrap();
        json.as_object_mut().unwrap().remove("skipped");
        let parsed: StageResponse = serde_json::from_value(json).unwrap();
        assert!(parsed.skipped.is_empty());
    }
}
