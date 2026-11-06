//! Read-only listing of open cross-agent handoffs.
//!
//! `memory_handoff_cancel` takes an exact handoff id and nothing exposed one,
//! so an accumulated backlog was visible as a count in `status` and could not
//! be addressed (#513). Automatic expiry deliberately spares manual and
//! sibling-directory handoffs, so a months-old entry surviving here is the
//! policy working as designed rather than a bug — but the operator still needs
//! to see it to decide.

use std::fmt::Write as _;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use ai_memory_core::i18n::Localizer;

use super::show::terminal_text;
use crate::cli::HandoffsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

#[derive(Debug, Deserialize, Serialize)]
struct OpenHandoff {
    id: String,
    from_agent: String,
    to_agent: Option<String>,
    cwd: Option<String>,
    created_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct OpenHandoffsResponse {
    handoffs: Vec<OpenHandoff>,
}

/// List the open handoffs for a project, oldest first.
///
/// # Errors
/// Returns [`anyhow::Error`] when the scope cannot be resolved, the server is
/// unreachable, or it answers non-2xx.
pub async fn run(config: &Config, args: HandoffsArgs) -> Result<()> {
    let i18n = &config.i18n;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;

    if args.expire_all {
        return expire(&endpoint, i18n, &workspace, &project, &args).await;
    }

    let response: OpenHandoffsResponse = get_json(
        &endpoint,
        "/admin/handoffs",
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
            ("limit", &args.limit.to_string()),
        ],
    )
    .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&response.handoffs)?);
        return Ok(());
    }

    if response.handoffs.is_empty() {
        println!(
            "{}",
            i18n.format(
                "cli-handoffs-no-open-handoffs-for-workspace-project",
                &[
                    ("value0", workspace.to_string()),
                    ("value1", project.to_string())
                ]
            )
        );
        return Ok(());
    }

    let now_ms = jiff::Timestamp::now().as_millisecond();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        i18n.diagnostic(&format!(
            "Open handoffs for {workspace}/{project} (oldest first):"
        ))
    );
    for h in &response.handoffs {
        let target = h.to_agent.as_deref().unwrap_or("any agent");
        let _ = writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!(
                "  {}  from {} -> {}",
                i18n.age(now_ms.saturating_sub(h.created_at_ms) / 1_000),
                terminal_text(&h.from_agent),
                terminal_text(target)
            ))
        );
        let _ = writeln!(
            out,
            "{}",
            i18n.diagnostic(&format!("    id: {}", terminal_text(&h.id)))
        );
        if let Some(cwd) = h.cwd.as_deref() {
            let _ = writeln!(
                out,
                "{}",
                i18n.diagnostic(&format!("    cwd: {}", terminal_text(cwd)))
            );
        }
    }
    let _ = writeln!(
        out,
        "{}",
        i18n.diagnostic("\nCancel one with the MCP tool `memory_handoff_cancel`, passing its id.")
    );
    print!("{out}");
    Ok(())
}

/// Request body for `POST /admin/handoffs/expire`.
#[derive(serde::Serialize)]
struct ExpireRequest<'a> {
    workspace: &'a str,
    project: &'a str,
    confirm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    older_than_days: Option<u32>,
}

/// Clear the open handoff backlog for one scope.
async fn expire(
    endpoint: &ServerEndpoint,
    i18n: &Localizer,
    workspace: &str,
    project: &str,
    args: &HandoffsArgs,
) -> Result<()> {
    if !args.confirm {
        let age = args
            .older_than_days
            .map(|d| format!(" --older-than-days {d}"))
            .unwrap_or_default();
        // The copyable command is one value: it is never translated, and a
        // catalog template cannot split two adjacent values back apart.
        let command = format!(
            "ai-memory handoffs --workspace {workspace} --project {project}{age} \
             --expire-all --confirm"
        );
        anyhow::bail!(
            "--expire-all marks every open handoff in {workspace}/{project} expired.\n\
             The rows and their summaries are kept; they just stop being consumable.\n\
             Unlike the automatic sweep this also clears manual handoffs and ones from \
             another directory.\n\n  {command}"
        );
    }

    let report: serde_json::Value = crate::http_client::post_json(
        endpoint,
        "/admin/handoffs/expire",
        &ExpireRequest {
            workspace,
            project,
            confirm: true,
            older_than_days: args.older_than_days,
        },
    )
    .await?;

    let expired = report["expired"].as_u64().unwrap_or(0);
    println!("{}", expire_report(i18n, expired, workspace, project));
    Ok(())
}

/// The confirmation `--expire-all` prints. Each shape keeps its English
/// literal inside the `diagnostic` call so the catalog drift guard sees it;
/// the singular is its own entry because `diagnostic` matches rendered text.
fn expire_report(i18n: &Localizer, expired: u64, workspace: &str, project: &str) -> String {
    let (workspace, project) = (terminal_text(workspace), terminal_text(project));
    match expired {
        0 => i18n.diagnostic(&format!(
            "No open handoffs to expire in {workspace}/{project}."
        )),
        1 => i18n.diagnostic(&format!(
            "Expired {expired} open handoff in {workspace}/{project}. The rows are kept and \
             remain visible in the audit log; they are no longer offered to an agent."
        )),
        _ => i18n.diagnostic(&format!(
            "Expired {expired} open handoffs in {workspace}/{project}. The rows are kept and \
             remain visible in the audit log; they are no longer offered to an agent."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::humanize_age_secs;
    use super::{expire, expire_report};
    use crate::cli::HandoffsArgs;
    use crate::http_client::ServerEndpoint;
    use ai_memory_core::i18n::{Language, Localizer};

    /// Both read-only listings render ages through one helper now; these pin
    /// the rendering the handoff listing depends on.
    #[test]
    fn age_reports_the_coarsest_useful_unit() {
        assert_eq!(humanize_age_secs(0), "just now");
        assert_eq!(humanize_age_secs(5 * 60), "5 minutes ago");
        assert_eq!(humanize_age_secs(3_600), "1 hour ago");
        assert_eq!(humanize_age_secs(3 * 3_600), "3 hours ago");
        assert_eq!(humanize_age_secs(9 * 86_400), "9 days ago");
    }

    /// Server and client clocks can disagree; a future timestamp must not
    /// render as an enormous age.
    #[test]
    fn a_future_timestamp_clamps_to_just_now() {
        assert_eq!(humanize_age_secs(-5), "just now");
    }

    /// `--expire-all` confirms the sweep with the server's count: each shape,
    /// the singular included, reaches a Portuguese terminal translated and an
    /// English one unchanged, and the echoed scope prints inert.
    #[test]
    fn expire_report_is_localized_in_every_shape() {
        let portuguese = Localizer::new(Language::PortugueseBrazil);
        assert_eq!(
            expire_report(&portuguese, 0, "default", "x"),
            "Nenhum handoff aberto para expirar em default/x."
        );
        let one = expire_report(&portuguese, 1, "default", "x");
        assert!(
            one.starts_with("1 handoff aberto expirou em default/x."),
            "{one}"
        );
        assert!(one.contains("log de auditoria"), "{one}");
        let many = expire_report(&portuguese, 3, "default", "x");
        assert!(
            many.starts_with("3 handoffs abertos expiraram em default/x."),
            "{many}"
        );

        let english = Localizer::new(Language::English);
        assert_eq!(
            expire_report(&english, 1, "default", "x"),
            "Expired 1 open handoff in default/x. The rows are kept and remain visible in \
             the audit log; they are no longer offered to an agent."
        );
        assert_eq!(
            expire_report(&english, 0, "default", "x\u{1b}[2J"),
            "No open handoffs to expire in default/x?[2J."
        );
    }

    /// Without `--confirm` the refusal explains the sweep in the operator's
    /// language and keeps the copyable command verbatim. No request is sent.
    #[tokio::test]
    async fn expire_refusal_is_localized_and_keeps_the_command() {
        let args = HandoffsArgs {
            workspace: None,
            project: None,
            limit: 50,
            json: false,
            expire_all: true,
            older_than_days: Some(7),
            confirm: false,
        };
        let endpoint = ServerEndpoint::from_pair(Some("http://127.0.0.1:9".to_owned()), None);
        let portuguese = Localizer::new(Language::PortugueseBrazil);
        let error = expire(&endpoint, &portuguese, "default", "x", &args)
            .await
            .expect_err("--expire-all without --confirm refuses");
        let report = crate::localization::failure_report(&crate::localization::localize_failure(
            error,
            Language::PortugueseBrazil,
        ));
        assert!(
            report.contains("--expire-all marca todos os handoffs abertos em default/x"),
            "{report}"
        );
        assert!(
            report.contains(
                "ai-memory handoffs --workspace default --project x --older-than-days 7 \
                 --expire-all --confirm"
            ),
            "{report}"
        );
    }
}
