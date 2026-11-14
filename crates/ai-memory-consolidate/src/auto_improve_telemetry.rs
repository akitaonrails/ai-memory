//! Report-only telemetry for auto-improvement proposal outcomes.

use ai_memory_core::{ProjectId, WorkspaceId};
use ai_memory_store::{AutoImproveTelemetryAggregate, AutoImproveTelemetryCount, ReaderPool};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

const US_PER_DAY: i64 = 86_400_000_000;

/// Default telemetry lookback window.
pub const DEFAULT_AUTO_IMPROVE_TELEMETRY_SINCE_DAYS: u32 = 30;
/// Default number of top count rows rendered per section.
pub const DEFAULT_AUTO_IMPROVE_TELEMETRY_TOP_LIMIT: usize = 10;

/// Parameters for an auto-improve telemetry report.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTelemetryParams {
    /// Lookback window in days.
    pub since_days: u32,
    /// Maximum rows in each top-N count table.
    pub top_limit: usize,
}

impl Default for AutoImproveTelemetryParams {
    fn default() -> Self {
        Self {
            since_days: DEFAULT_AUTO_IMPROVE_TELEMETRY_SINCE_DAYS,
            top_limit: DEFAULT_AUTO_IMPROVE_TELEMETRY_TOP_LIMIT,
        }
    }
}

/// Terminal-rate denominator and rates for learning proposals.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTerminalRates {
    /// Terminal learning proposals only: approved + rejected + conflict + failed.
    pub denominator: usize,
    /// Approved / denominator.
    pub approved_rate: f64,
    /// Rejected / denominator.
    pub rejected_rate: f64,
    /// Conflict / denominator.
    pub conflict_rate: f64,
    /// Failed / denominator.
    pub failed_rate: f64,
}

/// One bounded operational signal in the telemetry report.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTelemetryFinding {
    /// Machine-readable finding kind.
    pub kind: String,
    /// Human severity for CLI/admin rendering.
    pub severity: String,
    /// Human-readable finding text.
    pub message: String,
    /// Optional structured details containing counts only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Structured report-only auto-improve telemetry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTelemetryReport {
    /// Workspace reviewed.
    pub workspace: String,
    /// Project reviewed.
    pub project: String,
    /// ISO timestamp of report generation.
    pub generated_at: String,
    /// Lower bound timestamp for queried rows, in Unix microseconds.
    pub since_created_at: i64,
    /// Short summary.
    pub summary: String,
    /// Parameters used for this report.
    pub params: AutoImproveTelemetryParams,
    /// Store-provided aggregate counts.
    pub aggregate: AutoImproveTelemetryAggregate,
    /// Terminal learning proposal rates. Pending proposals are excluded.
    pub terminal_rates: AutoImproveTerminalRates,
    /// Bounded operational findings.
    pub findings: Vec<AutoImproveTelemetryFinding>,
    /// Known blind spots for this schema-backed report.
    pub blind_spots: Vec<String>,
}

/// Build a report-only auto-improve telemetry report from persisted store rows.
///
/// # Errors
/// Propagates store read failures.
pub async fn run_auto_improve_telemetry_report(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    workspace_name: &str,
    project_name: &str,
    params: AutoImproveTelemetryParams,
) -> ai_memory_store::StoreResult<AutoImproveTelemetryReport> {
    let now = Timestamp::now();
    let window_us = i64::from(params.since_days).saturating_mul(US_PER_DAY);
    let since_created_at = now.as_microsecond().saturating_sub(window_us);
    let aggregate = reader
        .auto_improve_telemetry_aggregate(
            workspace_id,
            project_id,
            since_created_at,
            params.top_limit,
        )
        .await?;
    Ok(build_auto_improve_telemetry_report(
        workspace_name,
        project_name,
        now.to_string(),
        since_created_at,
        params,
        aggregate,
    ))
}

/// Build report semantics from already-collected aggregate counts.
#[must_use]
pub fn build_auto_improve_telemetry_report(
    workspace_name: &str,
    project_name: &str,
    generated_at: String,
    since_created_at: i64,
    params: AutoImproveTelemetryParams,
    aggregate: AutoImproveTelemetryAggregate,
) -> AutoImproveTelemetryReport {
    let terminal_rates = terminal_rates(&aggregate.proposals_by_status);
    let pending = count_for(&aggregate.proposals_by_status, "pending");
    let maintenance_count: usize = aggregate
        .maintenance_proposals_by_kind
        .iter()
        .map(|row| row.count)
        .sum();
    let eval_gate_count = aggregate.eval_gate_rejections;
    let mut findings = Vec::new();
    if terminal_rates.denominator == 0 {
        findings.push(AutoImproveTelemetryFinding {
            kind: "no_terminal_learning_proposals".into(),
            severity: "info".into(),
            message: "No terminal learning proposals in the selected window.".into(),
            detail: None,
        });
    }
    if pending > 0 {
        findings.push(AutoImproveTelemetryFinding {
            kind: "pending_learning_proposals_excluded".into(),
            severity: "info".into(),
            message: format!(
                "{pending} pending learning proposal(s) are counted but excluded from terminal rates."
            ),
            detail: Some(serde_json::json!({ "pending": pending })),
        });
    }
    if maintenance_count > 0 {
        findings.push(AutoImproveTelemetryFinding {
            kind: "maintenance_proposals_excluded".into(),
            severity: "info".into(),
            message: format!(
                "{maintenance_count} maintenance/report proposal(s) are excluded from learning metrics."
            ),
            detail: Some(serde_json::json!({ "maintenance_proposals": maintenance_count })),
        });
    }
    if eval_gate_count > 0 {
        findings.push(AutoImproveTelemetryFinding {
            kind: "eval_gate_rejections_seen".into(),
            severity: "info".into(),
            message: format!(
                "{eval_gate_count} eval-gate rejection(s) were recorded. This report does not infer eval success rates or score deltas."
            ),
            detail: Some(serde_json::json!({ "eval_gate_rejections": eval_gate_count })),
        });
    }
    for row in &aggregate.repeated_rejection_fingerprints {
        findings.push(AutoImproveTelemetryFinding {
            kind: "repeated_rejection_fingerprint".into(),
            severity: "warning".into(),
            message: format!(
                "Rejection fingerprint {} appeared {} times.",
                truncate(&row.key, 64),
                row.count
            ),
            detail: Some(serde_json::json!({
                "fingerprint": truncate(&row.key, 64),
                "count": row.count,
            })),
        });
    }

    let summary = if terminal_rates.denominator == 0 {
        format!(
            "{} run(s), no terminal learning proposals in the last {} day(s).",
            aggregate.run_count, params.since_days
        )
    } else {
        format!(
            "{} run(s), {} terminal learning proposal(s), {:.1}% approved in the last {} day(s).",
            aggregate.run_count,
            terminal_rates.denominator,
            terminal_rates.approved_rate * 100.0,
            params.since_days
        )
    };

    AutoImproveTelemetryReport {
        workspace: workspace_name.to_string(),
        project: project_name.to_string(),
        generated_at,
        since_created_at,
        summary,
        params,
        aggregate,
        terminal_rates,
        findings,
        blind_spots: vec![
            "Provider/LLM errors before a run is staged may not appear in proposal or rejection counts."
                .into(),
            "Eval-gate rejection reasons are counted, but eval success rates and score deltas are not persisted in this schema."
                .into(),
            "Terminal rates use learning proposals only; pending and maintenance/report proposals are excluded from the denominator."
                .into(),
        ],
    }
}

/// Render the telemetry report as a normal wiki markdown page.
#[must_use]
pub fn render_auto_improve_telemetry_report_markdown(
    report: &AutoImproveTelemetryReport,
) -> String {
    let mut out = String::new();
    out.push_str("# Auto-Improve Telemetry Report\n\n");
    out.push_str(
        "> Report-only: approving a staged copy of this report would store this page only. ",
    );
    out.push_str("It does not edit rules, procedures, notes, or `_meta` pages.\n\n");
    out.push_str(&format!("- Workspace: `{}`\n", report.workspace));
    out.push_str(&format!("- Project: `{}`\n", report.project));
    out.push_str(&format!("- Generated: `{}`\n", report.generated_at));
    out.push_str(&format!(
        "- Window: last {} day(s) (`since_created_at={}`)\n",
        report.params.since_days, report.since_created_at
    ));
    out.push_str(&format!("- Summary: {}\n\n", report.summary));

    out.push_str("## Terminal Learning Proposal Rates\n\n");
    out.push_str(
        "Denominator: terminal learning proposals only (`approved`, `rejected`, `conflict`, `failed`). Pending proposals and maintenance/report proposals are excluded.\n\n",
    );
    out.push_str(&format!(
        "- Denominator: {}\n- Approved: {:.1}%\n- Rejected: {:.1}%\n- Conflict: {:.1}%\n- Failed: {:.1}%\n\n",
        report.terminal_rates.denominator,
        report.terminal_rates.approved_rate * 100.0,
        report.terminal_rates.rejected_rate * 100.0,
        report.terminal_rates.conflict_rate * 100.0,
        report.terminal_rates.failed_rate * 100.0,
    ));

    out.push_str("## Counts\n\n");
    render_counts(
        &mut out,
        "Learning proposals by status",
        &report.aggregate.proposals_by_status,
    );
    render_counts(
        &mut out,
        "Learning proposals by operation",
        &report.aggregate.proposals_by_operation,
    );
    render_counts(
        &mut out,
        "Learning proposals by edit mode",
        &report.aggregate.proposals_by_edit_mode,
    );
    render_counts(
        &mut out,
        "Learning proposals by kind",
        &report.aggregate.proposals_by_kind,
    );
    render_counts(
        &mut out,
        "Maintenance/report proposals by kind",
        &report.aggregate.maintenance_proposals_by_kind,
    );
    render_counts(
        &mut out,
        "Top learning targets",
        &report.aggregate.top_targets,
    );
    render_counts(
        &mut out,
        "Rejections by reason",
        &report.aggregate.rejections_by_reason,
    );
    render_counts(
        &mut out,
        "Repeated rejection fingerprints",
        &report.aggregate.repeated_rejection_fingerprints,
    );
    render_counts(
        &mut out,
        "Rejected targets",
        &report.aggregate.rejected_targets,
    );

    out.push_str("## Findings\n\n");
    if report.findings.is_empty() {
        out.push_str("No telemetry findings.\n\n");
    } else {
        for finding in &report.findings {
            out.push_str(&format!(
                "- **{}** ({}) — {}\n",
                finding.kind, finding.severity, finding.message
            ));
        }
        out.push('\n');
    }

    out.push_str("## Known Blind Spots\n\n");
    for blind_spot in &report.blind_spots {
        out.push_str(&format!("- {blind_spot}\n"));
    }
    out
}

fn terminal_rates(statuses: &[AutoImproveTelemetryCount]) -> AutoImproveTerminalRates {
    let approved = count_for(statuses, "approved");
    let rejected = count_for(statuses, "rejected");
    let conflict = count_for(statuses, "conflict");
    let failed = count_for(statuses, "failed");
    let denominator = approved + rejected + conflict + failed;
    AutoImproveTerminalRates {
        denominator,
        approved_rate: rate(approved, denominator),
        rejected_rate: rate(rejected, denominator),
        conflict_rate: rate(conflict, denominator),
        failed_rate: rate(failed, denominator),
    }
}

fn count_for(rows: &[AutoImproveTelemetryCount], key: &str) -> usize {
    rows.iter()
        .find(|row| row.key == key)
        .map(|row| row.count)
        .unwrap_or(0)
}

fn rate(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn render_counts(out: &mut String, title: &str, rows: &[AutoImproveTelemetryCount]) {
    out.push_str(&format!("### {title}\n\n"));
    if rows.is_empty() {
        out.push_str("- None\n\n");
        return;
    }
    for row in rows {
        out.push_str(&format!("- `{}`: {}\n", truncate(&row.key, 96), row.count));
    }
    out.push('\n');
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(key: &str, n: usize) -> AutoImproveTelemetryCount {
        AutoImproveTelemetryCount {
            key: key.to_string(),
            count: n,
        }
    }

    #[test]
    fn report_uses_terminal_learning_denominator_and_explains_exclusions() {
        let aggregate = AutoImproveTelemetryAggregate {
            run_count: 4,
            runs_with_learning_proposals: 2,
            proposals_by_status: vec![
                count("approved", 3),
                count("pending", 2),
                count("rejected", 1),
                count("conflict", 1),
            ],
            proposals_by_operation: vec![count("create", 5), count("update", 2)],
            proposals_by_edit_mode: vec![count("full_page", 6), count("patch", 1)],
            proposals_by_kind: vec![count("note", 6), count("procedure", 1)],
            maintenance_proposals_by_kind: vec![count("auto_improve_report", 1)],
            top_targets: vec![count("notes/a.md", 2)],
            rejections_by_reason: vec![count("eval_gate_failed", 2)],
            eval_gate_rejections: 2,
            repeated_rejection_fingerprints: vec![count("abc123", 2)],
            rejected_targets: vec![count("notes/a.md", 2)],
        };
        let report = build_auto_improve_telemetry_report(
            "default",
            "app",
            "2026-06-22T00:00:00Z".into(),
            42,
            AutoImproveTelemetryParams::default(),
            aggregate,
        );

        assert_eq!(report.terminal_rates.denominator, 5);
        assert!((report.terminal_rates.approved_rate - 0.6).abs() < f64::EPSILON);
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == "pending_learning_proposals_excluded")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == "maintenance_proposals_excluded")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == "eval_gate_rejections_seen")
        );

        let markdown = render_auto_improve_telemetry_report_markdown(&report);
        assert!(markdown.contains("# Auto-Improve Telemetry Report"));
        assert!(markdown.contains("Denominator: terminal learning proposals only"));
        assert!(markdown.contains("Eval-gate rejection reasons are counted"));
    }

    #[test]
    fn eval_gate_finding_survives_a_truncated_reason_table() {
        let aggregate = AutoImproveTelemetryAggregate {
            run_count: 1,
            runs_with_learning_proposals: 1,
            proposals_by_status: vec![count("rejected", 3)],
            proposals_by_operation: Vec::new(),
            proposals_by_edit_mode: Vec::new(),
            proposals_by_kind: Vec::new(),
            maintenance_proposals_by_kind: Vec::new(),
            top_targets: Vec::new(),
            rejections_by_reason: vec![count("human rejected", 3)],
            eval_gate_rejections: 2,
            repeated_rejection_fingerprints: Vec::new(),
            rejected_targets: Vec::new(),
        };
        let report = build_auto_improve_telemetry_report(
            "default",
            "app",
            "2026-06-22T00:00:00Z".into(),
            42,
            AutoImproveTelemetryParams {
                since_days: 30,
                top_limit: 1,
            },
            aggregate,
        );

        let finding = report
            .findings
            .iter()
            .find(|finding| finding.kind == "eval_gate_rejections_seen")
            .expect("eval-gate finding must not vanish with --top-limit 1");
        assert_eq!(
            finding.detail,
            Some(serde_json::json!({ "eval_gate_rejections": 2 }))
        );
    }
}
