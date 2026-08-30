//! `ai-memory resume` — pick a managed workstream across local checkouts.
//!
//! The server deliberately cannot list client filesystem paths. This command
//! joins its checkout-local registry to each checkout's privacy-preserving
//! workstream listing, revalidates every path before use, and delegates the
//! selected launch to `run --workstream`.

use std::io::IsTerminal as _;
use std::path::PathBuf;

use ai_memory_core::ManagedWorkstreamSummary;
use anyhow::{Context as _, Result, bail};

use crate::cli::{ResumeArgs, RunArgs, RunHarnessChoice};
use crate::commands::project_registry::{self, ProjectLink};
use crate::commands::show::{
    Choice, HorizontalDirection, available_harnesses, harness_name, select_with_horizontal,
    terminal_text,
};
use crate::commands::{continue_session, workstreams};
use crate::config::Config;
use crate::http_client::ServerEndpoint;

struct Candidate {
    link: ProjectLink,
    target: PathBuf,
    summary: ManagedWorkstreamSummary,
}

/// Interactively select a workstream from every valid locally linked checkout.
pub async fn run(config: &Config, args: ResumeArgs) -> Result<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "`ai-memory resume` needs a terminal; use `ai-memory workstreams` to list a checkout in scripts"
        );
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let mut links = project_registry::links_for_server(config, &endpoint)?;
    match current_checkout_link(config, &endpoint) {
        Ok(current) => {
            if !links.iter().any(|link| {
                link.workspace == current.workspace
                    && link.project == current.project
                    && link.path == current.path
            }) {
                links.insert(0, current);
            }
        }
        Err(error) => eprintln!(
            "skipping current checkout: {}",
            terminal_text(&format!("{error:#}"))
        ),
    }
    let mut candidates = Vec::new();
    let mut skipped = 0usize;

    for link in links.into_iter().filter(|link| {
        args.workspace
            .as_deref()
            .is_none_or(|workspace| link.workspace == workspace)
    }) {
        let target = match continue_session::resolve_target(config, &link) {
            Ok(target) => target,
            Err(error) => {
                eprintln!(
                    "skipping {}: {}",
                    scope_label(&link),
                    terminal_text(&error.to_string())
                );
                skipped += 1;
                continue;
            }
        };
        let summaries = match workstreams::list_for_checkout(
            &endpoint,
            &link.workspace,
            &link.project,
            &target,
            usize::from(args.limit),
        )
        .await
        {
            Ok(summaries) => summaries,
            Err(error) => {
                eprintln!(
                    "skipping {}: could not list managed workstreams ({})",
                    scope_label(&link),
                    terminal_text(&format!("{error:#}"))
                );
                skipped += 1;
                continue;
            }
        };
        for summary in summaries {
            if candidates.iter().any(|candidate: &Candidate| {
                candidate.summary.workstream_id == summary.workstream_id
            }) {
                continue;
            }
            candidates.push(Candidate {
                link: link.clone(),
                target: target.clone(),
                summary,
            });
        }
    }

    order_candidates(&mut candidates);
    candidates.truncate(usize::from(args.limit));
    if candidates.is_empty() {
        let detail = if skipped == 0 {
            "no local managed checkout has a saved workstream yet".to_owned()
        } else {
            format!(
                "{skipped} local checkout{} could not be queried",
                if skipped == 1 { "" } else { "s" }
            )
        };
        bail!(
            "no managed workstreams are available ({detail}); launch one with `ai-memory run <harness>` first"
        );
    }

    let harnesses = available_harnesses();
    let mut harness_indices = vec![0usize; candidates.len()];
    let mut choices = candidates
        .iter()
        .map(|candidate| choice_for(candidate, None))
        .collect::<Vec<_>>();
    let Some(index) = select_with_horizontal(
        "Resume workstream",
        &mut choices,
        "left/right harness",
        &mut |index, direction, choice| {
            let harness =
                cycle_selected_harness(&mut harness_indices, index, &harnesses, direction);
            *choice = choice_for(&candidates[index], harness);
        },
    )?
    else {
        return Ok(0);
    };
    let selected = &candidates[index];
    let harness = selected_harness(harness_indices[index], &harnesses);
    eprintln!(
        "resuming workstream '{}' with harness '{}' in {} at {}",
        terminal_text(&selected.summary.name),
        harness
            .map(harness_name)
            .unwrap_or_else(|| "auto".to_owned()),
        scope_label(&selected.link),
        terminal_text(&selected.target.to_string_lossy())
    );
    crate::commands::run::run_from(
        config,
        RunArgs {
            workspace: Some(selected.link.workspace.clone()),
            project: Some(selected.link.project.clone()),
            workstream: Some(selected.summary.name.clone()),
            new_workstream: None,
            executable: None,
            yolo: args.yolo,
            fresh: args.fresh,
            harness,
            native_args: Vec::new(),
        },
        &selected.target,
    )
    .await
}

fn current_checkout_link(config: &Config, endpoint: &ServerEndpoint) -> Result<ProjectLink> {
    let path = std::env::current_dir()
        .context("reading the current checkout")?
        .canonicalize()
        .context("canonicalizing the current checkout")?;
    let (workspace, project) = super::resolve_scope_for_path(config, &path)?;
    Ok(ProjectLink {
        server: endpoint.identity(),
        workspace,
        project,
        path,
        linked_at: jiff::Timestamp::now().to_string(),
    })
}

fn choice_for(candidate: &Candidate, harness: Option<RunHarnessChoice>) -> Choice {
    let marker = if candidate.summary.current { "* " } else { "" };
    let harnesses = if candidate.summary.linked_harnesses.is_empty() {
        "no linked harnesses".to_owned()
    } else {
        candidate
            .summary
            .linked_harnesses
            .iter()
            .map(|agent| agent.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    Choice {
        label: format!("{marker}{}", terminal_text(&candidate.summary.name)),
        detail: format!(
            "{} | {} | harness <{}> | linked [{}]",
            scope_label(&candidate.link),
            humanize_age(&candidate.summary.last_active_at),
            harness
                .map(harness_name)
                .unwrap_or_else(|| "auto".to_owned()),
            terminal_text(&harnesses)
        ),
    }
}

fn cycle_harness_index(
    current: usize,
    harness_count: usize,
    direction: HorizontalDirection,
) -> usize {
    let choice_count = harness_count.saturating_add(1);
    match direction {
        HorizontalDirection::Left => current.checked_sub(1).unwrap_or(choice_count - 1),
        HorizontalDirection::Right => (current + 1) % choice_count,
    }
}

fn selected_harness(index: usize, harnesses: &[RunHarnessChoice]) -> Option<RunHarnessChoice> {
    index
        .checked_sub(1)
        .and_then(|index| harnesses.get(index))
        .copied()
}

fn cycle_selected_harness(
    indices: &mut [usize],
    row: usize,
    harnesses: &[RunHarnessChoice],
    direction: HorizontalDirection,
) -> Option<RunHarnessChoice> {
    indices[row] = cycle_harness_index(indices[row], harnesses.len(), direction);
    selected_harness(indices[row], harnesses)
}

fn order_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(|left, right| {
        right
            .summary
            .current
            .cmp(&left.summary.current)
            .then_with(|| {
                timestamp(&right.summary.last_active_at)
                    .cmp(&timestamp(&left.summary.last_active_at))
            })
            .then_with(|| left.link.workspace.cmp(&right.link.workspace))
            .then_with(|| left.link.project.cmp(&right.link.project))
            .then_with(|| left.summary.name.cmp(&right.summary.name))
    });
}

fn timestamp(raw: &str) -> Option<jiff::Timestamp> {
    raw.parse().ok()
}

fn humanize_age(raw: &str) -> String {
    let Some(then) = timestamp(raw) else {
        return "unknown activity".to_owned();
    };
    super::humanize_age_secs((jiff::Timestamp::now() - then).get_seconds())
}

fn scope_label(link: &ProjectLink) -> String {
    terminal_text(&format!("{}/{}", link.workspace, link.project))
}

#[cfg(test)]
mod tests {
    use ai_memory_core::{AgentKind, WorkstreamId};

    use super::*;

    fn candidate(name: &str, current: bool, last_active_at: &str) -> Candidate {
        Candidate {
            link: ProjectLink {
                server: "http://127.0.0.1:49374".to_owned(),
                workspace: "default".to_owned(),
                project: "app".to_owned(),
                path: PathBuf::from("/checkout/app"),
                linked_at: "2026-08-30T00:00:00Z".to_owned(),
            },
            target: PathBuf::from("/checkout/app"),
            summary: ManagedWorkstreamSummary {
                workstream_id: WorkstreamId::new(),
                name: name.to_owned(),
                created_at: "2026-08-01T00:00:00Z".to_owned(),
                last_active_at: last_active_at.to_owned(),
                current,
                linked_harnesses: vec![AgentKind::Codex],
            },
        }
    }

    #[test]
    fn current_workstream_leads_then_newest_activity_wins() {
        let mut candidates = vec![
            candidate("older", false, "2026-08-01T00:00:00Z"),
            candidate("current", true, "2026-07-01T00:00:00Z"),
            candidate("newer", false, "2026-08-02T00:00:00Z"),
        ];

        order_candidates(&mut candidates);

        let names = candidates
            .iter()
            .map(|candidate| candidate.summary.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["current", "newer", "older"]);
    }

    #[test]
    fn malformed_activity_sorts_after_valid_activity() {
        let mut candidates = vec![
            candidate("malformed", false, "not-a-timestamp"),
            candidate("valid", false, "2026-08-01T00:00:00Z"),
        ];

        order_candidates(&mut candidates);

        assert_eq!(candidates[0].summary.name, "valid");
    }

    #[test]
    fn choice_sanitizes_server_control_characters() {
        let choice = choice_for(&candidate("unsafe\u{1b}[31m", false, "invalid"), None);

        assert!(!choice.label.contains('\u{1b}'));
        assert!(choice.detail.contains("unknown activity"));
    }

    #[test]
    fn harness_cycle_wraps_through_auto_and_available_harnesses() {
        assert_eq!(cycle_harness_index(0, 2, HorizontalDirection::Right), 1);
        assert_eq!(cycle_harness_index(1, 2, HorizontalDirection::Right), 2);
        assert_eq!(cycle_harness_index(2, 2, HorizontalDirection::Right), 0);
        assert_eq!(cycle_harness_index(0, 2, HorizontalDirection::Left), 2);
        assert_eq!(cycle_harness_index(0, 0, HorizontalDirection::Right), 0);
    }

    #[test]
    fn harness_index_zero_preserves_automatic_selection() {
        let harnesses = [RunHarnessChoice::Claude, RunHarnessChoice::Codex];

        assert!(selected_harness(0, &harnesses).is_none());
        assert!(matches!(
            selected_harness(2, &harnesses),
            Some(RunHarnessChoice::Codex)
        ));
    }

    #[test]
    fn choice_displays_the_active_and_linked_harnesses() {
        let choice = choice_for(
            &candidate("feature", false, "invalid"),
            Some(RunHarnessChoice::Claude),
        );

        assert!(choice.detail.contains("harness <claude>"));
        assert!(choice.detail.contains("linked [codex]"));
    }

    #[test]
    fn each_workstream_remembers_its_own_harness() {
        let harnesses = [RunHarnessChoice::Claude, RunHarnessChoice::Codex];
        let mut indices = [0, 0];

        let first = cycle_selected_harness(&mut indices, 0, &harnesses, HorizontalDirection::Right);
        let second = cycle_selected_harness(&mut indices, 1, &harnesses, HorizontalDirection::Left);

        assert!(matches!(first, Some(RunHarnessChoice::Claude)));
        assert!(matches!(second, Some(RunHarnessChoice::Codex)));
        assert_eq!(indices, [1, 2]);
    }
}
