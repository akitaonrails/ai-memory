//! Human report formatting; collection and diagnostic verdicts stay in the command.

use super::*;

pub(super) fn render(report: &DoctorReport, terminal: &crate::terminal::Terminal) {
    let i18n = &terminal.i18n;
    terminal.header(&i18n.text("cli-presentation-doctor-capture-coverage"));
    terminal.section(&i18n.text("cli-presentation-connection"));
    terminal.line(format_args!(
        "{}",
        i18n.format(
            "cli-presentation-capture-coverage-for-server",
            &[
                ("value0", report.workspace.to_string()),
                ("value1", report.project.to_string()),
                ("value2", report.server.to_string())
            ]
        )
    ));
    if let Some(identity) = &report.identity {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-identity-server",
                &[
                    (
                        "value0",
                        identity
                            .operator
                            .as_deref()
                            .unwrap_or("anonymous")
                            .to_string()
                    ),
                    ("value1", identity.level.to_string()),
                    ("value2", identity.version.to_string())
                ]
            )
        ));
    } else {
        terminal.line(format_args!(
            "{}",
            i18n.text("cli-presentation-identity-unavailable")
        ));
    }
    if let Some(posture) = report.auth_posture {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-auth",
                &[("value0", posture.localized_line(i18n).to_string())]
            )
        ));
    }
    if report.capture_owner_active {
        terminal.line(format_args!(
            "{}", i18n.text("cli-presentation-ai-memory-capture-owner-is-active-native-capture-is-suppressed-in-this-proce")));
    }
    if let Some(coordinate) = &report.project_coordinate {
        terminal.text(&render_project_coordinate(i18n, coordinate));
    }
    if let Some(problem) = &report.capture_coverage_problem {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-capture-coverage-unavailable",
                &[("value0", problem.reason.to_string())]
            )
        ));
    }
    terminal.section(&i18n.text("cli-presentation-capture-health"));
    terminal.text(&render_capture_health(i18n, report));
    terminal.text(&render_plugins(i18n, &report.plugins));
    terminal.text(&render_muse(i18n, report.muse.as_ref()));
    terminal.text(&hermes::render(&report.hermes));
    terminal.text(&super::super::agentmemory_residue::render(
        &report.agentmemory_residue,
    ));
    if report.since_days == 0 {
        terminal.line(format_args!(
            "{}",
            i18n.text("cli-presentation-recent-window-all-on-disk-sessions")
        ));
    } else {
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-recent-window-last-days",
                &[("value0", format!("{}", report.since_days))]
            )
        ));
    }

    if let Some(problem) = &report.windows_logon {
        terminal.line(format_args!("{}", problem.render_human()));
    }

    if let Some(problem) = &report.marker_capture_problem {
        terminal.line(format_args!(
            "{}", i18n.format("cli-presentation-the-settings-marker-has-an-invalid-capture-section-it-fails-closed-file-and", &[("value0", problem.classification.to_string())])));
    }

    if !report.native_memory.is_empty() {
        terminal.line(format_args!(
            "{}", i18n.text("cli-presentation-note-a-shell-powershell-hook-install-does-not-enforce-capture-policy-v1-excl")));
    }

    if report.rows.is_empty() {
        if report.capture_coverage_problem.is_some() {
            terminal.line(format_args!(
                "{}", i18n.text("cli-presentation-no-local-harness-sessions-found-for-this-project-server-capture-counts-are-u")));
        } else {
            terminal.line(format_args!(
                "{}", i18n.text("cli-presentation-no-local-harness-sessions-found-for-this-project-and-nothing-captured-yet")));
        }
        return;
    }

    terminal.section(&i18n.text("cli-presentation-agent-coverage"));
    for row in &report.rows {
        let mark = match row.uncaptured {
            Some(true) => "⚠",
            Some(false) => "✓",
            None => "?",
        };
        let captured = row.captured.map_or_else(
            || i18n.text("cli-presentation-unavailable"),
            |count| count.to_string(),
        );
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-agent-row",
                &[
                    ("value0", mark.to_string()),
                    ("value1", format!("{:<14}", row.agent)),
                    ("value2", format!("{:>4}", row.local_total)),
                    ("value3", format!("{}", row.local_recent)),
                    ("value4", format!("{:>11}", captured))
                ]
            )
        ));
        if row.uncaptured == Some(true) {
            if report.capture_owner_active {
                terminal.line(format_args!(
                    "{}", i18n.text("cli-presentation-no-captured-session-check-the-external-producer-and-repository-policy")));
            } else {
                terminal.line(format_args!(
                    "{}", i18n.format("cli-presentation-ran-here-but-nothing-was-captured-install-its-hook-ai-memory-install-hooks-a", &[("value0", row.agent.to_string())])));
            }
        }
        if let Some(count) = row.mixed_capture_sessions.filter(|count| *count > 0) {
            terminal.line(format_args!(
                "{}", i18n.format("cli-presentation-count-session-s-contain-events-from-multiple-capture-sources-backfill-can-al", &[("value0", format!("{}", count))])));
        }
        if let Some(memory) = report.native_memory.iter().find(|m| m.agent == row.agent) {
            terminal.line(format_args!(
                "{}",
                i18n.message(
                    "cli-presentation-native-memory-file",
                    &[
                        ("value0", (memory.path.to_string()).into()),
                        ("value1", (format!("{}", memory.file_count)).into()),
                        ("value2", (memory.file_count).into())
                    ]
                )
            ));
            terminal.line(format_args!(
                "{}",
                i18n.format(
                    "cli-presentation-capture",
                    &[("value0", memory.status.to_string())]
                )
            ));
        }
    }

    if report.capture_coverage_problem.is_some() {
        terminal.line(format_args!(
            "{}", i18n.text("cli-presentation-capture-verdict-unavailable-server-capture-counts-could-not-be-resolved")));
    } else if report.uncaptured.is_empty() {
        terminal.line(format_args!(
            "{}", i18n.text("cli-presentation-every-harness-that-ran-in-this-project-recently-is-captured-on-the-server")));
    } else {
        let remedy = if report.capture_owner_active {
            "Check the external producer and repository policy, then re-run `ai-memory doctor`."
        } else {
            "Install the hook(s) above, then re-run `ai-memory doctor` to confirm."
        };
        terminal.line(format_args!(
            "{}",
            i18n.format(
                "cli-presentation-harness-es-ran-here-recently-with-no-captured-sessions-remedy",
                &[
                    ("value0", format!("{}", report.uncaptured.len())),
                    ("value1", report.uncaptured.join(", ").to_string()),
                    ("value2", remedy.to_string())
                ]
            )
        ));
    }
}
