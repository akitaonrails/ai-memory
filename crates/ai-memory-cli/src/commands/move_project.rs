//! `ai-memory move-project` — thin HTTP client for cross-workspace project move.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::MoveProjectArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/move-project`.
#[derive(Serialize)]
struct MoveProjectRequest {
    from_workspace: String,
    /// Exactly one of `project` / `project_id` is `Some`. `--project-id`
    /// bypasses the server's name/legacy/canonical lookup, which is the point:
    /// a legacy row whose name collides with another project's `legacy_name`
    /// is otherwise unreachable by name. (#1208)
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    to_workspace: String,
    confirm: bool,
    force: bool,
    on_conflict: String,
    /// Acknowledge the copy+purge merge (#1713).
    allow_lossy_merge: bool,
}

/// The exact command that performs the move the refusal just described.
///
/// Rebuilt from `args` rather than hand-written, the same way
/// `move_session::apply_command` does, so the printed command cannot drift
/// from the request the command actually sends. Every flag the operator
/// passed has to reappear here: the request built below carries
/// `args.force` and `args.on_conflict`, so a printed command that dropped
/// them would not reproduce the operation just described. A dropped
/// `--on-conflict overwrite` is the worst of the two — `on_conflict`
/// defaults to `block`, so the pasted command REFUSES with a 409 listing the
/// conflicting paths instead of merging them, and a dropped `--force` leaves
/// the active-project guard in place.
fn confirm_command(from_workspace: &str, target_flag: &str, args: &MoveProjectArgs) -> String {
    let from_workspace = super::render_shared::native_shell_quote(from_workspace);
    let to_workspace = super::render_shared::native_shell_quote(&args.to_workspace);
    let mut cmd = format!(
        "ai-memory move-project --from-workspace {from_workspace} {target_flag} --to-workspace {to_workspace}"
    );
    if args.force {
        cmd.push_str(" --force");
    }
    if args.allow_lossy_merge {
        cmd.push_str(" --allow-lossy-merge");
    }
    if args.on_conflict != "block" {
        cmd.push_str(&format!(
            " --on-conflict {}",
            super::render_shared::native_shell_quote(&args.on_conflict)
        ));
    }
    cmd.push_str(" --confirm");
    cmd
}

/// Run the `move-project` subcommand.
///
/// Resolves the source project name (auto-derived from the git repo root
/// when `--project` is omitted) -- or, when `--project-id` is given, skips
/// name resolution and moves that id directly, the only way to reach a legacy
/// row whose name is shadowed by another project's `legacy_name` -- requires
/// `--confirm` before sending the request (a true-move re-stamp or a
/// copy+purge merge, both irreversible), then prints the report.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the server is unreachable,
/// or the server returns a non-2xx response.
pub async fn run(config: &Config, args: MoveProjectArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.from_workspace.as_deref(), "--from-workspace")?;
    super::refuse_empty_explicit(Some(args.to_workspace.as_str()), "--to-workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    super::refuse_empty_explicit(args.project_id.as_deref(), "--project-id")?;
    let (from_workspace, project, project_id) = match &args.project_id {
        Some(id) => (
            super::resolve_workspace(config, args.from_workspace.as_deref())?,
            None,
            Some(id.clone()),
        ),
        None => {
            let (ws, project) = super::resolve_scope(
                config,
                args.from_workspace.as_deref(),
                args.project.as_deref(),
            )?;
            (ws, Some(project), None)
        }
    };
    let (target_flag, requested) = match (&project, &project_id) {
        (Some(project), _) => (
            format!(
                "--project {}",
                super::render_shared::native_shell_quote(project)
            ),
            project.clone(),
        ),
        (None, Some(id)) => (
            format!(
                "--project-id {}",
                super::render_shared::native_shell_quote(id)
            ),
            id.clone(),
        ),
        (None, None) => bail!(
            "{}",
            i18n.text("cli-move-project-one-of-project-or-project-id-is-required")
        ),
    };

    if !args.confirm {
        bail!(
            "{}", i18n.format("cli-move-project-move-project-moves-to-workspace-if-the-destination-has-no-same-named-project", &[("value0", from_workspace.to_string()), ("value1", target_flag.to_string()), ("value2", args.to_workspace.to_string()), ("value3", confirm_command(&from_workspace, &target_flag, &args).to_string())]));
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/move-project",
        &MoveProjectRequest {
            from_workspace: from_workspace.clone(),
            project: project.clone(),
            project_id: project_id.clone(),
            to_workspace: args.to_workspace.clone(),
            confirm: true,
            force: args.force,
            on_conflict: args.on_conflict.clone(),
            allow_lossy_merge: args.allow_lossy_merge,
        },
    )
    .await?;

    let pages = report["pages_copied"].as_u64().unwrap_or(0);
    let target_label = moved_project_name(&report, requested);
    let purged = report["source_purged"].as_bool().unwrap_or(false);
    let moved_via = report["moved_via"].as_str().unwrap_or("");
    let skipped_count = report["pages_skipped"].as_array().map_or(0, |s| s.len());
    if let Some(warning) = report["manifest_warning"].as_str() {
        eprintln!(
            "{}",
            i18n.format(
                "cli-move-project-ai-memory-warning-warning",
                &[("value0", warning.to_string())]
            )
        );
    }

    if (moved_via == "true-move" || purged)
        && let Err(error) = super::project_registry::rekey_scope(
            config,
            &endpoint,
            &from_workspace,
            &target_label,
            &args.to_workspace,
            &target_label,
        )
    {
        eprintln!(
            "{}", i18n.format("cli-move-project-ai-memory-project-moved-but-the-client-local-checkout-link-could-not-be-upda", &[("value0", format!("{:#}", error))]));
    }

    if moved_via == "true-move" {
        // Lossless: re-stamped in place, nothing copied or purged.
        println!(
            "{}", i18n.format("cli-move-project-moved-pages-pages-re-stamped-true-move-sessions-observations-and-history-pre", &[("value0", from_workspace.to_string()), ("value1", target_label.to_string()), ("value2", args.to_workspace.to_string()), ("value3", target_label.to_string()), ("value4", format!("{}", pages))]));
    } else {
        // copy-purge (merge into an existing same-named project).
        let tail = if skipped_count > 0 {
            ", SOURCE LEFT INTACT (some pages unreadable — fix and re-run)"
        } else if purged {
            ", source purged"
        } else {
            ", SOURCE LEFT INTACT (partial copy)"
        };
        println!(
            "{}",
            i18n.format(
                "cli-move-project-moved-pages-pages-copied-merged-into-existing-project-tail",
                &[
                    ("value0", from_workspace.to_string()),
                    ("value1", target_label.to_string()),
                    ("value2", args.to_workspace.to_string()),
                    ("value3", target_label.to_string()),
                    ("value4", format!("{}", pages)),
                    ("value5", tail.to_string())
                ]
            )
        );
        if skipped_count > 0 {
            println!(
                "{}", i18n.format("cli-move-project-warning-skipped-count-page-s-could-not-be-read-from-the-source-and-were-skip", &[("value0", format!("{}", skipped_count))]));
        }
        if let Some(conflicts) = report["conflicts"].as_array().filter(|c| !c.is_empty()) {
            println!(
                "{}", i18n.format("cli-move-project-path-conflict-s-source-page-kept-under-a-de-duplicated-path-both-versions-pr", &[("value0", format!("{}", conflicts.len()))]));
            for c in conflicts {
                println!(
                    "  {} → {}",
                    c["path"].as_str().unwrap_or("?"),
                    c["moved_to"].as_str().unwrap_or("?"),
                );
            }
        }
    }
    Ok(())
}

/// The moved project's name as the server resolved it. The report's `from` is
/// `<workspace>/<project>`, and it is the only way to learn the name behind
/// `--project-id`; the client-local checkout link is keyed by that name.
fn moved_project_name(report: &serde_json::Value, requested: String) -> String {
    report["from"]
        .as_str()
        .and_then(|label| label.rsplit_once('/'))
        .map_or(requested, |(_, name)| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{confirm_command, moved_project_name};
    use crate::cli::MoveProjectArgs;

    fn args() -> MoveProjectArgs {
        MoveProjectArgs {
            from_workspace: Some("default".into()),
            project: Some("api".into()),
            project_id: None,
            to_workspace: "team".into(),
            confirm: false,
            force: false,
            on_conflict: "block".into(),
            allow_lossy_merge: false,
        }
    }

    /// The command the refusal hands the operator must round-trip every flag
    /// they passed, because the request this command actually builds carries
    /// `force` and `on_conflict` verbatim. A dropped `--on-conflict
    /// overwrite` falls back to the `block` default, so the pasted command
    /// comes back as a 409 listing the conflicting paths instead of merging
    /// them — the opposite of what the operator asked for.
    #[test]
    fn confirm_command_repeats_force_and_on_conflict() {
        let mut a = args();
        a.force = true;
        a.on_conflict = "overwrite".into();
        assert_eq!(
            confirm_command("default", "--project api", &a),
            "ai-memory move-project --from-workspace default --project api \
             --to-workspace team --force --on-conflict overwrite --confirm"
        );
    }

    /// And the mirror: a plain run must not start emitting flags nobody
    /// typed. `--on-conflict` has a clap default of `block`, so echoing it
    /// unconditionally would put a merge policy into a command the operator
    /// never chose one for.
    #[test]
    fn confirm_command_omits_flags_the_operator_did_not_pass() {
        let mut a = args();
        a.project = None;
        a.project_id = Some("0192b6a1-4c2e-7d3f-8a5b-1234567890ab".into());
        assert_eq!(
            confirm_command(
                "default",
                "--project-id 0192b6a1-4c2e-7d3f-8a5b-1234567890ab",
                &a
            ),
            "ai-memory move-project --from-workspace default \
             --project-id 0192b6a1-4c2e-7d3f-8a5b-1234567890ab \
             --to-workspace team --confirm"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn confirm_command_quotes_free_form_scope_names() {
        let mut a = args();
        a.to_workspace = "new team".into();
        assert_eq!(
            confirm_command("old team", "--project 'api; echo nope'", &a),
            "ai-memory move-project --from-workspace 'old team' --project 'api; echo nope' --to-workspace 'new team' --confirm"
        );
    }

    #[test]
    fn a_move_by_id_rekeys_the_link_under_the_resolved_name() {
        let report = serde_json::json!({ "from": "default/api", "to": "team/api" });
        let id = "0193e7a1-0000-7000-8000-000000000001".to_owned();
        assert_eq!(moved_project_name(&report, id.clone()), "api");
        // A report without the label keeps what the operator asked for.
        assert_eq!(moved_project_name(&serde_json::json!({}), id.clone()), id);
    }
}
