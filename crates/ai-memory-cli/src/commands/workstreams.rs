//! Checkout-local discovery for managed workstreams.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::Path;

use ai_memory_core::{ListManagedWorkstreamsRequest, ManagedWorkstreamSummary};
use ai_memory_workstream::inspect_repository_fingerprints;
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
    let fingerprints = inspect_repository_fingerprints(&cwd)?;
    let project_id = if args.project_id.is_some() || args.project.is_some() {
        args.project_id
    } else {
        super::project_registry::project_binding(config, &endpoint, &workspace, &project, &cwd)?
            .map(|scope| scope.project_id)
    };
    let summaries = match list_for_checkout(
        &endpoint,
        &workspace,
        &project,
        &cwd,
        usize::from(args.limit),
        project_id,
    )
    .await
    {
        Err(error) if super::managed_project::is_ambiguous(&error) && project_id.is_none() => {
            let scope = super::managed_project::choose(
                &config.i18n,
                &endpoint,
                &workspace,
                &project,
                !args.json && std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await?;
            let summaries = list_for_checkout(
                &endpoint,
                &workspace,
                &project,
                &cwd,
                usize::from(args.limit),
                Some(scope.project_id),
            )
            .await?;
            super::project_registry::record_project_binding(
                config,
                &endpoint,
                &project,
                &cwd,
                &scope,
                &fingerprints,
            )?;
            summaries
        }
        result => result?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
        return Ok(());
    }
    print!(
        "{}",
        render_human(&config.i18n, &summaries, &workspace, &project)
    );
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
    project_id: Option<ai_memory_core::ProjectId>,
) -> Result<Vec<ManagedWorkstreamSummary>> {
    let repository = inspect_repository_fingerprints(checkout)
        .context("inspecting managed workstream checkout")?;
    post_json(
        endpoint,
        "/workstream/recent",
        &ListManagedWorkstreamsRequest {
            project_id,
            workspace: workspace.to_owned(),
            project: if project_id.is_some() {
                String::new()
            } else {
                project.to_owned()
            },
            repo_fingerprint: repository.repo_fingerprint,
            worktree_fingerprint: repository.worktree_fingerprint,
            limit,
            offset: 0,
        },
    )
    .await
}

/// Fetch every checkout-local page without changing the server's per-page cap.
pub(super) async fn list_all_for_checkout(
    config: &Config,
    endpoint: &ServerEndpoint,
    workspace: &str,
    project: &str,
    checkout: &Path,
) -> Result<Vec<ManagedWorkstreamSummary>> {
    let i18n = &config.i18n;
    let repository = inspect_repository_fingerprints(checkout)
        .context(i18n.text("cli-workstreams-inspecting-managed-workstream-checkout"))?;
    let project_id =
        super::project_registry::project_binding(config, endpoint, workspace, project, checkout)?
            .map(|scope| scope.project_id);
    let mut request = ListManagedWorkstreamsRequest {
        project_id,
        workspace: workspace.to_owned(),
        project: if project_id.is_some() {
            String::new()
        } else {
            project.to_owned()
        },
        repo_fingerprint: repository.repo_fingerprint.clone(),
        worktree_fingerprint: repository.worktree_fingerprint.clone(),
        limit: 100,
        offset: 0,
    };
    let mut summaries = Vec::new();
    let mut seen = HashSet::new();
    loop {
        let page: Vec<ManagedWorkstreamSummary> =
            match post_json(endpoint, "/workstream/recent", &request).await {
                Err(error)
                    if request.project_id.is_none()
                        && super::managed_project::is_ambiguous(&error) =>
                {
                    let scope = super::managed_project::choose(
                        &config.i18n,
                        endpoint,
                        workspace,
                        project,
                        std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
                        &tokio_util::sync::CancellationToken::new(),
                    )
                    .await?;
                    request.project_id = Some(scope.project_id);
                    request.project.clear();
                    let page = post_json(endpoint, "/workstream/recent", &request).await?;
                    super::project_registry::record_project_binding(
                        config,
                        endpoint,
                        project,
                        checkout,
                        &scope,
                        &repository,
                    )?;
                    page
                }
                result => result?,
            };
        let count = page.len();
        let before = summaries.len();
        summaries.extend(
            page.into_iter()
                .filter(|row| seen.insert(row.workstream_id)),
        );
        if count < request.limit {
            break;
        }
        // Older servers ignore offset and repeat their first page. Stop
        // instead of looping, and say the list is partial rather than fail:
        // the CLI and server may be upgraded on different machines.
        if summaries.len() == before {
            eprintln!(
                "{}", i18n.format("cli-workstreams-warning-the-ai-memory-server-ignored-workstream-pagination-showing-its-first", &[("value0", format!("{}", summaries.len()))]));
            break;
        }
        // Pages are ordered by recency, so a workstream that becomes active
        // between two requests can move ahead of the offset and be skipped
        // until the next listing; `seen` keeps the reverse move from
        // duplicating a row. A picker tolerates that.
        request.offset = request
            .offset
            .checked_add(count)
            .context(i18n.text("cli-workstreams-workstream-pagination-offset-overflow"))?;
    }
    Ok(summaries)
}

fn render_human(
    i18n: &ai_memory_core::i18n::Localizer,
    summaries: &[ManagedWorkstreamSummary],
    workspace: &str,
    project: &str,
) -> String {
    let mut output = i18n.format(
        "cli-workstreams-managed-workstreams-for-workspace-project",
        &[
            ("value0", workspace.to_string()),
            ("value1", project.to_string()),
        ],
    );
    if summaries.is_empty() {
        output.push_str(&i18n.text("cli-workstreams-no-managed-workstreams-for-this-checkout"));
        return output;
    }
    for summary in summaries {
        let marker = if summary.current { '*' } else { ' ' };
        let harnesses = if summary.linked_harnesses.is_empty() {
            i18n.text("cli-workstreams-no-linked-harnesses")
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
            humanize_age(i18n, &summary.last_active_at),
            harnesses,
        );
        let _ = writeln!(
            output,
            "{}",
            i18n.format(
                "cli-message-id",
                &[("value0", format!("{}", summary.workstream_id))]
            )
        );
    }
    output
}

fn humanize_age(i18n: &ai_memory_core::i18n::Localizer, raw: &str) -> String {
    let Ok(then) = raw.parse::<jiff::Timestamp>() else {
        return "unknown activity".to_owned();
    };
    i18n.age((jiff::Timestamp::now() - then).get_seconds())
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

        let rendered = render_human(
            &ai_memory_core::i18n::Localizer::default(),
            &rows,
            "default",
            "game",
        );
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
            render_human(
                &ai_memory_core::i18n::Localizer::default(),
                &[],
                "default",
                "game"
            )
            .contains("No managed workstreams for this checkout.")
        );
    }
}
