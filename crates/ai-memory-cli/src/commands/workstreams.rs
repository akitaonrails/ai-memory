//! Checkout-local discovery for managed workstreams.

use std::fmt::Write as _;
use std::path::Path;

use ai_memory_core::{ListManagedWorkstreamsRequest, ManagedWorkstreamSummary};
use ai_memory_workstream::inspect_repository;
use anyhow::{Context as _, Result};

use crate::cli::WorkstreamsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// List workstreams that `run --workstream` can select in this checkout.
pub async fn run(config: &Config, args: WorkstreamsArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("getting managed workstream checkout")?;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let summaries = list_for_checkout(
        &endpoint,
        &workspace,
        &project,
        &cwd,
        usize::from(args.limit),
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
        return Ok(());
    }
    print!("{}", render_human(&summaries, &workspace, &project));
    Ok(())
}

/// List recent managed workstreams for one local checkout.
///
/// The server receives only its stable repository/worktree fingerprints; the
/// checkout path stays client-local so a remote server can never choose or
/// disclose a path on this host.
pub(super) async fn list_for_checkout(
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    checkout: &Path,
    limit: usize,
) -> Result<Vec<ManagedWorkstreamSummary>> {
    let repository =
        inspect_repository(checkout).context("inspecting managed workstream checkout")?;
    post_json(
        endpoint,
        "/workstream/recent",
        &ListManagedWorkstreamsRequest {
            workspace: workspace.to_owned(),
            project: project.to_owned(),
            repo_fingerprint: repository.repo_fingerprint,
            worktree_fingerprint: repository.worktree_fingerprint,
            limit,
        },
    )
    .await
}

fn render_human(summaries: &[ManagedWorkstreamSummary], workspace: &str, project: &str) -> String {
    let mut output = format!("Managed workstreams for {workspace}/{project}:\n");
    if summaries.is_empty() {
        output.push_str("No managed workstreams for this checkout.\n");
        return output;
    }
    for summary in summaries {
        let marker = if summary.current { '*' } else { ' ' };
        let harnesses = if summary.linked_harnesses.is_empty() {
            "no linked harnesses".to_owned()
        } else {
            summary
                .linked_harnesses
                .iter()
                .map(|agent| agent.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(
            output,
            "{marker} {}  {}  [{}]",
            summary.name,
            humanize_age(&summary.last_active_at),
            harnesses,
        );
        let _ = writeln!(output, "    id: {}", summary.workstream_id);
    }
    output
}

fn humanize_age(raw: &str) -> String {
    let Ok(then) = raw.parse::<jiff::Timestamp>() else {
        return "unknown activity".to_owned();
    };
    super::humanize_age_secs((jiff::Timestamp::now() - then).get_seconds())
}

#[cfg(test)]
mod tests {
    use ai_memory_core::{AgentKind, WorkstreamId};

    use super::*;

    #[test]
    fn human_output_marks_current_and_lists_harnesses() {
        let rows = vec![ManagedWorkstreamSummary {
            workstream_id: WorkstreamId::new(),
            name: "decide-after-death".into(),
            created_at: jiff::Timestamp::now().to_string(),
            last_active_at: jiff::Timestamp::now().to_string(),
            current: true,
            linked_harnesses: vec![AgentKind::OpenCode, AgentKind::Codex],
        }];

        let rendered = render_human(&rows, "default", "game");
        assert!(rendered.contains("Managed workstreams for default/game:"));
        assert!(rendered.contains("* decide-after-death"));
        // The id line is indented past the marker column so it cannot be
        // misread as a second, unmarked workstream.
        assert!(rendered.contains("\n    id: "));
        assert!(rendered.contains("[open-code, codex]"));
        assert!(rendered.contains(&rows[0].workstream_id.to_string()));
    }

    #[test]
    fn human_output_explains_an_empty_checkout() {
        assert!(
            render_human(&[], "default", "game")
                .contains("No managed workstreams for this checkout.")
        );
    }
}
