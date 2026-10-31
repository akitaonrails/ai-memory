//! `ai-memory move-session` — thin HTTP client for moving one session (or
//! every session of a project) to another project.

use anyhow::Result;
use serde::Serialize;

use crate::cli::MoveSessionArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/move-session`. Exactly one of `session_id`
/// / `from_project` is set.
#[derive(Serialize)]
struct MoveSessionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    project: String,
    pages: String,
    confirm: bool,
    force: bool,
    create: bool,
}

/// Run the `move-session` subcommand.
///
/// Without `--confirm` the server runs the move inside a rolled-back
/// transaction and this prints what would change plus the exact command to
/// apply it. With `--confirm` it prints the report.
///
/// A session already rooted in the destination is re-homed: the report says
/// `session row: already in destination` and the counts are the stray rows
/// gathered into it (zero when there were none).
///
/// # Errors
/// Returns an error when the server is unreachable or answers non-2xx (404
/// unknown session/target, 409 guard or page collision, 422 batch source
/// equal to the destination).
pub async fn run(config: &Config, args: MoveSessionArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.from_workspace.as_deref(), "--from-workspace")?;
    super::refuse_empty_explicit(args.from_project.as_deref(), "--from-project")?;
    super::refuse_empty_explicit(args.to_workspace.as_deref(), "--to-workspace")?;
    super::refuse_empty_explicit(Some(args.to.as_str()), "--to")?;

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;

    // The batch source is a scope like any other command's: marker, else
    // literal. The destination stays literal.
    let (from_workspace, from_project) = match args.from_project.as_deref() {
        Some(project) => {
            let (ws, proj) =
                super::resolve_scope(config, args.from_workspace.as_deref(), Some(project))?;
            (Some(ws), Some(proj))
        }
        None => (None, None),
    };
    let request = MoveSessionRequest {
        session_id: args.session_id.map(|id| id.to_string()),
        from_workspace,
        from_project,
        workspace: args.to_workspace.clone(),
        project: args.to.clone(),
        pages: args.pages.clone(),
        confirm: args.confirm,
        force: args.force,
        create: args.create,
    };
    let report: serde_json::Value = post_json(&endpoint, "/admin/move-session", &request).await?;
    if let Some(warning) = report["manifest_warning"].as_str() {
        eprintln!(
            "{}",
            i18n.format(
                "cli-move-project-ai-memory-warning-warning",
                &[("value0", warning.to_string())]
            )
        );
    }

    if request.session_id.is_some() {
        print_session_report(i18n, &report, args.confirm);
    } else {
        print_batch_report(i18n, &report, args.confirm);
    }
    if !args.confirm {
        println!(
            "{}",
            i18n.format(
                "cli-move-session-dry-run-nothing-was-written-re-run-with-confirm-to-apply",
                &[("value0", apply_command(&args).to_string())]
            )
        );
    }
    Ok(())
}

fn scope(label: &serde_json::Value) -> String {
    format!(
        "{}/{}",
        label["workspace"].as_str().unwrap_or("?"),
        label["project"].as_str().unwrap_or("?")
    )
}

/// One line saying whether the `sessions` row itself changes scope; a
/// re-home leaves it and only gathers stray rows.
fn session_row_line(report: &serde_json::Value, applied: bool) -> &'static str {
    match (report["session_moved"].as_bool().unwrap_or(false), applied) {
        (true, true) => "moved",
        (true, false) => "would move",
        (false, _) => "already in destination (re-home: only stray rows gathered)",
    }
}

fn print_session_report(
    i18n: &ai_memory_core::i18n::Localizer,
    report: &serde_json::Value,
    applied: bool,
) {
    let verb = if applied { "Moved" } else { "Would move" };
    println!(
        "{}",
        i18n.format(
            "cli-move-session-verb-session-from-to",
            &[
                ("value0", verb.to_string()),
                (
                    "value1",
                    report["session_id"].as_str().unwrap_or("?").to_string()
                ),
                ("value2", scope(&report["from"]).to_string()),
                ("value3", scope(&report["to"]).to_string())
            ]
        )
    );
    print_would_create(i18n, report);
    println!(
        "{}",
        i18n.format(
            "cli-move-session-session-row",
            &[("value0", session_row_line(report, applied).to_string())]
        )
    );
    print_counts(
        i18n,
        &report["summary"],
        report["page"].as_str().unwrap_or("none"),
    );
    print_source_scopes(i18n, report, &scope(&report["to"]));
    if let Some(warning) = report["cwd_warning"].as_str() {
        println!(
            "{}",
            i18n.format(
                "cli-move-session-warning-warning",
                &[("value0", warning.to_string())]
            )
        );
    }
    print_checkpoints(i18n, report);
}

/// Dry run with `--create` against a destination that does not exist yet.
fn print_would_create(i18n: &ai_memory_core::i18n::Localizer, report: &serde_json::Value) {
    if report["would_create_project"].as_bool().unwrap_or(false) {
        println!(
            "{}",
            i18n.format(
                "cli-move-session-would-create-project-absent-created-on-confirm",
                &[("value0", scope(&report["to"]).to_string())]
            )
        );
    }
}

/// The wiki checkpoints a confirmed run took: `pre_checkpoint` only when the
/// tree had uncommitted changes before the move, `checkpoint` when the move
/// changed the tree.
fn print_checkpoints(i18n: &ai_memory_core::i18n::Localizer, report: &serde_json::Value) {
    if let Some(oid) = report["pre_checkpoint"].as_str() {
        println!(
            "{}",
            i18n.format(
                "cli-move-session-pre-move-checkpoint-oid",
                &[("value0", oid.to_string())]
            )
        );
    }
    if let Some(oid) = report["checkpoint"].as_str() {
        println!(
            "{}",
            i18n.format(
                "cli-move-session-checkpoint-oid",
                &[("value0", oid.to_string())]
            )
        );
    }
}

fn print_batch_report(
    i18n: &ai_memory_core::i18n::Localizer,
    report: &serde_json::Value,
    applied: bool,
) {
    let verb = if applied { "Moved" } else { "Would move" };
    let total = report["total"].as_u64().unwrap_or(0);
    let moved = report["moved"].as_u64().unwrap_or(0);
    println!(
        "{}",
        i18n.format(
            "cli-move-session-verb-moved-of-total-session-s-from-to",
            &[
                ("value0", verb.to_string()),
                ("value1", format!("{}", moved)),
                ("value2", format!("{}", total)),
                ("value3", scope(&report["from"]).to_string()),
                ("value4", scope(&report["to"]).to_string())
            ]
        )
    );
    print_would_create(i18n, report);
    if let Some(sessions) = report["sessions"].as_array() {
        println!(
            "{}",
            i18n.format(
                "cli-move-session-table-header",
                &[
                    ("value0", format!("{:<38}", "session_id")),
                    ("value1", format!("{:<8}", "row")),
                    ("value2", format!("{:>6}", "obs")),
                    ("value3", format!("{:>8}", "handoffs")),
                    ("value4", format!("{:>5}", "jobs")),
                    ("value5", format!("{:<22}", "page"))
                ]
            )
        );
        println!("{}", "-".repeat(109));
        for s in sessions {
            println!(
                "{:<38} {:<8} {:>6} {:>8} {:>5} {:<22} {}",
                s["session_id"].as_str().unwrap_or("?"),
                if s["session_moved"].as_bool().unwrap_or(false) {
                    "moved"
                } else {
                    "re-home"
                },
                s["summary"]["observations"].as_u64().unwrap_or(0),
                s["summary"]["handoffs"].as_u64().unwrap_or(0),
                s["summary"]["consolidation_jobs"].as_u64().unwrap_or(0),
                s["page"].as_str().unwrap_or("none"),
                s["cwd"].as_str().unwrap_or(""),
            );
        }
        let rehomed = sessions
            .iter()
            .filter(|s| !s["session_moved"].as_bool().unwrap_or(false))
            .count();
        if rehomed > 0 {
            println!(
                "{}", i18n.format("cli-move-session-note-rehomed-session-s-marked-re-home-already-had-their-session-row-in-the-d", &[("value0", format!("{}", rehomed))]));
        }
        let warned = sessions
            .iter()
            .filter(|s| s["cwd_warning"].is_string())
            .count();
        if warned > 0 {
            println!(
                "{}", i18n.format("cli-move-session-warning-warned-session-s-have-a-cwd-whose-basename-is-not-the-destination-pr", &[("value0", format!("{}", warned))]));
        }
    }
    print_checkpoints(i18n, report);
}

/// Name every scope the move will drain, when it is more than the one the
/// caller asked about.
///
/// A move gathers dependent rows by session id alone, so it can empty a
/// project nobody named — and moving back afterwards cannot restore the
/// original split. The operator sees that here, in the dry run, rather than
/// inferring it from a count that came out larger than expected.
fn print_source_scopes(
    i18n: &ai_memory_core::i18n::Localizer,
    report: &serde_json::Value,
    destination: &str,
) {
    let Some(scopes) = report["source_scopes"].as_array() else {
        return;
    };
    let others: Vec<_> = scopes
        .iter()
        .filter(|s| s["observations"].as_u64().unwrap_or(0) > 0)
        .collect();
    if others.len() < 2 {
        return;
    }
    println!(
        "{}",
        i18n.format(
            "cli-move-session-gathering-observations-out-of-scopes-into-destination",
            &[
                ("value0", format!("{}", others.len())),
                ("value1", destination.to_string())
            ]
        )
    );
    for s in &others {
        println!(
            "{}",
            i18n.format(
                "cli-move-session-observation-s",
                &[
                    ("value0", s["workspace"].as_str().unwrap_or("?").to_string()),
                    ("value1", s["project"].as_str().unwrap_or("?").to_string()),
                    (
                        "value2",
                        format!("{}", s["observations"].as_u64().unwrap_or(0))
                    )
                ]
            )
        );
    }
    println!(
        "{}", i18n.text("cli-move-session-note-this-empties-every-scope-listed-above-not-only-the-one-named-as-the-sou"));
}

fn print_counts(i18n: &ai_memory_core::i18n::Localizer, summary: &serde_json::Value, page: &str) {
    let n = |key: &str| summary[key].as_u64().unwrap_or(0);
    println!(
        "{}",
        i18n.format(
            "cli-move-session-observations",
            &[("value0", format!("{}", n("observations")))]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-move-session-handoffs",
            &[("value0", format!("{}", n("handoffs")))]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-move-session-consolidation-jobs",
            &[("value0", format!("{}", n("consolidation_jobs")))]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-move-session-auto-improve-runs",
            &[("value0", format!("{}", n("auto_improve_runs")))]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-move-session-auto-improve-claims",
            &[("value0", format!("{}", n("auto_improve_claims")))]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-move-session-page-page-version-s-moved-retired",
            &[
                ("value0", page.to_string()),
                ("value1", format!("{}", n("page_versions_moved"))),
                ("value2", format!("{}", n("pages_regenerated")))
            ]
        )
    );
}

/// The exact command that applies what the dry run showed.
fn apply_command(args: &MoveSessionArgs) -> String {
    let mut cmd = String::from("ai-memory move-session");
    if let Some(id) = args.session_id {
        cmd.push(' ');
        cmd.push_str(&id.to_string());
    }
    if let Some(project) = &args.from_project {
        cmd.push_str(&format!(
            " --from-project {}",
            super::render_shared::native_shell_quote(project)
        ));
    }
    if let Some(ws) = &args.from_workspace {
        cmd.push_str(&format!(
            " --from-workspace {}",
            super::render_shared::native_shell_quote(ws)
        ));
    }
    cmd.push_str(&format!(
        " --to {}",
        super::render_shared::native_shell_quote(&args.to)
    ));
    if let Some(ws) = &args.to_workspace {
        cmd.push_str(&format!(
            " --to-workspace {}",
            super::render_shared::native_shell_quote(ws)
        ));
    }
    if args.pages != "move" {
        cmd.push_str(&format!(" --pages {}", args.pages));
    }
    if args.force {
        cmd.push_str(" --force");
    }
    if args.create {
        cmd.push_str(" --create");
    }
    cmd.push_str(" --confirm");
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(session_id: Option<&str>, from_project: Option<&str>) -> MoveSessionArgs {
        MoveSessionArgs {
            session_id: session_id.map(|s| s.parse().unwrap()),
            from_project: from_project.map(str::to_string),
            from_workspace: None,
            to: "target".into(),
            to_workspace: None,
            pages: "move".into(),
            confirm: false,
            force: false,
            create: false,
        }
    }

    #[test]
    fn apply_command_repeats_the_single_form_with_confirm() {
        let id = "0192b6a1-4c2e-7d3f-8a5b-1234567890ab";
        let mut a = args(Some(id), None);
        a.pages = "regenerate".into();
        a.to_workspace = Some("other".into());
        assert_eq!(
            apply_command(&a),
            format!(
                "ai-memory move-session {id} --to target --to-workspace other \
                 --pages regenerate --confirm"
            )
        );
    }

    #[test]
    fn apply_command_repeats_the_batch_form_and_flags() {
        let mut a = args(None, Some("ghost"));
        a.from_workspace = Some("default".into());
        a.force = true;
        a.create = true;
        assert_eq!(
            apply_command(&a),
            "ai-memory move-session --from-project ghost --from-workspace default --to target \
             --force --create --confirm"
        );
    }

    #[tokio::test]
    async fn an_empty_from_project_flag_is_refused_before_any_request() {
        let config = Config::default();
        let a = args(None, Some("   "));
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--from-project was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_from_workspace_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = args(None, Some("src"));
        a.from_workspace = Some("".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--from-workspace was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_to_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = args(None, Some("src"));
        a.to = "   ".into();
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error.to_string().contains("--to was passed empty"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn an_empty_to_workspace_flag_is_refused_before_any_request() {
        let config = Config::default();
        let mut a = args(None, Some("src"));
        a.to_workspace = Some("   ".into());
        let error = run(&config, a).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--to-workspace was passed empty"),
            "unexpected error: {error}"
        );
    }
}
