//! `ai-memory resume` — pick a managed workstream in the current checkout,
//! or with `--all` across every locally linked checkout.
//!
//! The server deliberately cannot list client filesystem paths. This command
//! joins the current checkout (and, with `--all`, the checkout-local registry)
//! to each checkout's privacy-preserving workstream listing, revalidates the
//! selected path before use, and delegates the launch to `run --workstream`.

use std::collections::HashSet;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

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

#[derive(Debug)]
struct Candidate {
    link: ProjectLink,
    target: PathBuf,
    summary: ManagedWorkstreamSummary,
}

/// Interactively select a workstream in the current checkout, or in every
/// locally linked checkout with `--all`.
pub async fn run(config: &Config, args: ResumeArgs) -> Result<i32> {
    let i18n = &config.i18n;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "{}", i18n.text("cli-resume-ai-memory-resume-needs-a-terminal-use-ai-memory-workstreams-to-list-a-checko"));
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let cwd =
        std::env::current_dir().context(i18n.text("cli-resume-reading-the-current-checkout"))?;
    let candidates = if args.all {
        candidates_across_checkouts(config, &endpoint, &args, &cwd).await?
    } else {
        candidates_for_checkout(config, &endpoint, &args, &cwd).await?
    };

    let harnesses = available_harnesses(config);
    let mut harness_indices = vec![0usize; candidates.len()];
    let mut choices = candidates
        .iter()
        .map(|candidate| choice_for(candidate, None))
        .collect::<Vec<_>>();
    let Some(index) = select_with_horizontal(
        i18n,
        &i18n.text("cli-picker-resume-title"),
        &mut choices,
        &i18n.text("cli-picker-horizontal-hint"),
        args.search.as_deref(),
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
    let target = continue_session::resolve_target(config, &selected.link)?;
    let project_id = project_registry::project_binding(
        config,
        &endpoint,
        &selected.link.workspace,
        &selected.link.project,
        &target,
    )?
    .map(|scope| scope.project_id);
    let harness = selected_harness(harness_indices[index], &harnesses);
    eprintln!(
        "{}",
        i18n.format(
            "cli-resume-resuming-workstream-with-harness-in-at",
            &[
                ("value0", terminal_text(&selected.summary.name).to_string()),
                (
                    "value1",
                    harness
                        .map(harness_name)
                        .unwrap_or_else(|| "auto".to_owned())
                        .to_string()
                ),
                ("value2", scope_label(&selected.link).to_string()),
                (
                    "value3",
                    terminal_text(&selected.target.to_string_lossy()).to_string()
                )
            ]
        )
    );
    crate::commands::run::run_from(
        config,
        RunArgs {
            workspace: Some(selected.link.workspace.clone()),
            project: project_id.is_none().then(|| selected.link.project.clone()),
            project_id,
            workstream: Some(selected.summary.name.clone()),
            new_workstream: None,
            executable: None,
            yolo: args.yolo,
            true_yolo: args.true_yolo,
            jail: None,
            no_jail: false,
            fresh: args.fresh,
            force_unlock: false,
            no_autowire: false,
            no_exit_prompt: false,
            require_server: false,
            profile: None,
            env: Vec::new(),
            env_file: None,
            harness,
            claude_wrapper: None,
            native_args: Vec::new(),
        },
        &target,
    )
    .await
}

async fn candidates_for_checkout(
    config: &Config,
    endpoint: &ServerEndpoint,
    args: &ResumeArgs,
    cwd: &Path,
) -> Result<Vec<Candidate>> {
    let i18n = &config.i18n;
    let link = current_checkout_link(config, endpoint, cwd)?;
    if args
        .workspace
        .as_deref()
        .is_some_and(|workspace| workspace != link.workspace)
    {
        bail!(
            "{}",
            i18n.format(
                "cli-resume-current-checkout-belongs-to-not-workspace",
                &[
                    ("value0", scope_label(&link).to_string()),
                    (
                        "value1",
                        terminal_text(args.workspace.as_deref().unwrap_or_default()).to_string()
                    )
                ]
            )
        );
    }
    let summaries = workstreams::list_all_for_checkout(
        config,
        endpoint,
        &link.workspace,
        &link.project,
        &link.path,
    )
    .await?;
    if summaries.is_empty() {
        bail!(
            "{}", i18n.format("cli-resume-no-managed-workstreams-for-the-current-checkout-launch-one-here-with-ai-memo", &[("value0", scope_label(&link).to_string())]));
    }
    let mut candidates = summaries
        .into_iter()
        .map(|summary| Candidate {
            target: link.path.clone(),
            link: link.clone(),
            summary,
        })
        .collect::<Vec<_>>();
    order_candidates(&mut candidates);
    apply_limit(&mut candidates, args, "the current checkout")?;
    Ok(candidates)
}

/// `--all`: every checkout linked for this server, the current one first.
/// Each checkout is listed through its own fingerprint-scoped query, and one
/// that cannot be resolved or queried is skipped with a note.
async fn candidates_across_checkouts(
    config: &Config,
    endpoint: &ServerEndpoint,
    args: &ResumeArgs,
    cwd: &Path,
) -> Result<Vec<Candidate>> {
    let i18n = &config.i18n;
    let mut links = project_registry::links_for_server(config, endpoint)?;
    match current_checkout_link(config, endpoint, cwd) {
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
            "{}",
            i18n.format(
                "cli-resume-skipping-current-checkout",
                &[("value0", terminal_text(&format!("{error:#}")).to_string())]
            )
        ),
    }
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
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
                    "{}",
                    i18n.format(
                        "cli-continue-session-skipping-label",
                        &[
                            ("value0", scope_label(&link).to_string()),
                            ("value1", terminal_text(&error.to_string()).to_string())
                        ]
                    )
                );
                skipped += 1;
                continue;
            }
        };
        let summaries = match workstreams::list_all_for_checkout(
            config,
            endpoint,
            &link.workspace,
            &link.project,
            &target,
        )
        .await
        {
            Ok(summaries) => summaries,
            Err(error) => {
                eprintln!(
                    "{}",
                    i18n.format(
                        "cli-resume-skipping-could-not-list-managed-workstreams",
                        &[
                            ("value0", scope_label(&link).to_string()),
                            ("value1", terminal_text(&format!("{error:#}")).to_string())
                        ]
                    )
                );
                skipped += 1;
                continue;
            }
        };
        for summary in summaries {
            if seen.insert(summary.workstream_id) {
                candidates.push(Candidate {
                    link: link.clone(),
                    target: target.clone(),
                    summary,
                });
            }
        }
    }
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
            "{}", i18n.format("cli-resume-no-managed-workstreams-are-available-detail-launch-one-with-ai-memory-run-ha", &[("value0", detail.to_string())]));
    }
    order_candidates(&mut candidates);
    apply_limit(&mut candidates, args, "any local checkout")?;
    Ok(candidates)
}

/// A requested limit applies after search, so an older matching row is never
/// hidden behind newer non-matching workstreams.
fn apply_limit(candidates: &mut Vec<Candidate>, args: &ResumeArgs, scope: &str) -> Result<()> {
    let Some(limit) = args.limit else {
        return Ok(());
    };
    let search = args.search.as_deref().unwrap_or_default().to_lowercase();
    candidates.retain(|candidate| candidate.summary.name.to_lowercase().contains(&search));
    candidates.truncate(limit as usize);
    if candidates.is_empty() {
        bail!(
            "no workstreams match '{}' in {scope}",
            terminal_text(&search)
        );
    }
    Ok(())
}

fn current_checkout_link(
    config: &Config,
    endpoint: &ServerEndpoint,
    cwd: &Path,
) -> Result<ProjectLink> {
    let path = cwd
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
    use crate::commands::project_registry;
    use ai_memory_core::{AgentKind, ListManagedWorkstreamsRequest, WorkstreamId};
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn args() -> ResumeArgs {
        ResumeArgs {
            workspace: None,
            all: false,
            limit: None,
            search: None,
            yolo: false,
            true_yolo: false,
            fresh: false,
        }
    }

    fn checkout(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args([
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/shared.git"
                ])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            path.join(".ai-memory.toml"),
            "workspace = \"work\"\nproject = \"app\"\n",
        )
        .unwrap();
        path.canonicalize().unwrap()
    }

    async fn listing_server(
        local: &Path,
        rows: Vec<ManagedWorkstreamSummary>,
        paginate: bool,
    ) -> (
        ServerEndpoint,
        Arc<Mutex<Vec<ListManagedWorkstreamsRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let repository = ai_memory_workstream::inspect_repository(local).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let foreign = candidate("foreign-workstream", false, "2026-09-01T00:00:00Z").summary;
        let app = Router::new().route(
            "/workstream/recent",
            post(move |Json(request): Json<ListManagedWorkstreamsRequest>| {
                let recorded = recorded.clone();
                let rows = rows.clone();
                let repository = repository.clone();
                let foreign = foreign.clone();
                async move {
                    recorded.lock().unwrap().push(request.clone());
                    let rows = if request.workspace == "work"
                        && request.project == "app"
                        && request.repo_fingerprint == repository.repo_fingerprint
                        && request.worktree_fingerprint == repository.worktree_fingerprint
                    {
                        rows
                    } else {
                        vec![foreign]
                    };
                    Json(
                        rows.into_iter()
                            .skip(if paginate { request.offset } else { 0 })
                            .take(request.limit)
                            .collect::<Vec<_>>(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = ServerEndpoint::from_pair(
            Some(format!("http://{}", listener.local_addr().unwrap())),
            None,
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (endpoint, requests, server)
    }

    #[tokio::test]
    async fn resume_lists_all_current_checkout_rows_not_registered_foreign_checkouts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let local = checkout(&tmp.path().join("local"));
        let foreign = checkout(&tmp.path().join("foreign"));
        let config = Config {
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        let rows = (0..125)
            .map(|index| {
                candidate(
                    &format!("local-{index:03}"),
                    index == 0,
                    "2026-09-01T00:00:00Z",
                )
                .summary
            })
            .collect();
        let (endpoint, requests, server) = listing_server(&local, rows, true).await;
        project_registry::record_prepared_checkout(&config, &endpoint, "work", "app", &foreign)
            .unwrap();
        let candidates = candidates_for_checkout(&config, &endpoint, &args(), &local)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 125);
        assert!(
            candidates
                .iter()
                .all(|row| row.summary.name.starts_with("local-") && row.target == local)
        );
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.offset)
                .collect::<Vec<_>>(),
            [0, 100]
        );
        assert!(candidates[0].summary.current);
        // Resume must not depend on even a readable project registry.
        std::fs::write(config.data_dir.join("client-projects.json"), "not json").unwrap();
        let mut searched = args();
        searched.search = Some("LOCAL-124".into());
        searched.limit = Some(1);
        let matching = candidates_for_checkout(&config, &endpoint, &searched, &local)
            .await
            .unwrap();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].summary.name, "local-124");
        searched.workspace = Some("elsewhere".into());
        let before = requests.lock().unwrap().len();
        assert!(
            candidates_for_checkout(&config, &endpoint, &searched, &local)
                .await
                .unwrap_err()
                .to_string()
                .contains("current checkout belongs to work/app")
        );
        assert_eq!(requests.lock().unwrap().len(), before);
        server.abort();
    }

    #[tokio::test]
    async fn empty_current_checkout_does_not_fall_back_to_the_registry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let local = checkout(&tmp.path().join("local"));
        let foreign = checkout(&tmp.path().join("foreign"));
        let config = Config {
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        let (endpoint, requests, server) = listing_server(&local, Vec::new(), true).await;
        project_registry::record_prepared_checkout(&config, &endpoint, "work", "app", &foreign)
            .unwrap();
        let error = candidates_for_checkout(&config, &endpoint, &args(), &local)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no managed workstreams for the current checkout")
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn a_server_ignoring_pagination_still_shows_its_first_page() {
        let tmp = tempfile::TempDir::new().unwrap();
        let local = checkout(&tmp.path().join("local"));
        let rows = (0..150)
            .map(|index| candidate(&format!("local-{index}"), false, "invalid").summary)
            .collect();
        let (endpoint, requests, server) = listing_server(&local, rows, false).await;
        let candidates = candidates_for_checkout(&Config::default(), &endpoint, &args(), &local)
            .await
            .unwrap();
        // The repeated first page ends the walk instead of looping or failing.
        assert_eq!(candidates.len(), 100);
        assert_eq!(requests.lock().unwrap().len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn resume_all_walks_every_linked_checkout_and_honours_the_workspace_filter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let local = checkout(&tmp.path().join("local"));
        let foreign = checkout(&tmp.path().join("foreign"));
        let config = Config {
            data_dir: tmp.path().join("data"),
            ..Config::default()
        };
        let rows = vec![candidate("local-only", true, "2026-09-02T00:00:00Z").summary];
        let (endpoint, requests, server) = listing_server(&local, rows, true).await;
        project_registry::record_prepared_checkout(&config, &endpoint, "work", "app", &foreign)
            .unwrap();
        // The registry keeps one checkout per project, so the stale link
        // belongs to a sibling project.
        let gone = checkout(&tmp.path().join("gone"));
        project_registry::record_prepared_checkout(&config, &endpoint, "work", "old", &gone)
            .unwrap();
        std::fs::remove_dir_all(&gone).unwrap();

        let mut all = args();
        all.all = true;
        let candidates = candidates_across_checkouts(&config, &endpoint, &all, &local)
            .await
            .unwrap();
        let rows = candidates
            .iter()
            .map(|row| (row.summary.name.as_str(), row.target.clone()))
            .collect::<Vec<_>>();
        // The current checkout's row leads; the other linked checkout's row
        // follows with its own target, and the deleted checkout is skipped.
        assert_eq!(
            rows,
            [
                ("local-only", local.clone()),
                ("foreign-workstream", foreign.clone())
            ]
        );
        assert_eq!(requests.lock().unwrap().len(), 2);

        all.workspace = Some("elsewhere".into());
        let error = candidates_across_checkouts(&config, &endpoint, &all, &local)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no managed workstreams are available")
        );
        assert_eq!(requests.lock().unwrap().len(), 2);

        // Without `--all` the same registry contributes nothing.
        let local_only = candidates_for_checkout(&config, &endpoint, &args(), &local)
            .await
            .unwrap();
        assert_eq!(local_only.len(), 1);
        server.abort();
    }

    #[test]
    fn current_checkout_resolves_subdirectories_and_symlinks_without_registry_links() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = checkout(&tmp.path().join("local"));
        let nested = root.join("src");
        std::fs::create_dir(&nested).unwrap();
        let endpoint = ServerEndpoint::from_pair(None, None);
        let config = Config::default();
        for cwd in [&root, &nested] {
            let link = current_checkout_link(&config, &endpoint, cwd).unwrap();
            assert_eq!((&*link.workspace, &*link.project), ("work", "app"));
            let identity = ai_memory_workstream::inspect_repository(&link.path).unwrap();
            let expected = ai_memory_workstream::inspect_repository(&root).unwrap();
            assert_eq!(identity.repo_fingerprint, expected.repo_fingerprint);
            assert_eq!(identity.worktree_fingerprint, expected.worktree_fingerprint);
        }
        #[cfg(unix)]
        {
            let alias = tmp.path().join("alias");
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert_eq!(
                current_checkout_link(&config, &endpoint, &alias)
                    .unwrap()
                    .path,
                root
            );
        }
    }

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
