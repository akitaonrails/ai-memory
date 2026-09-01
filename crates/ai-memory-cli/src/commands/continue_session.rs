//! `ai-memory continue` — resume the most recent managed checkout globally.
//!
//! `run`'s bare mode already continues the current checkout's newest usable
//! session, but its workstream lookup is keyed by `(workspace, project,
//! repo fingerprint, worktree fingerprint)`, so the caller must already be
//! inside the project. This command supplies the missing step: it picks the
//! checkout from the client-local registry, then hands the same bare-mode
//! launch the directory to run in.
//!
//! Ordering comes from `ProjectLink::linked_at`, which every successful
//! managed prepare refreshes. No server round-trip decides the checkout: the
//! server never exposes host filesystem paths, and a stale or retargeted
//! link must be rejected on this host anyway.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::cli::{ContinueArgs, RunArgs};
use crate::commands::project_registry::{self, ProjectLink};
use crate::commands::show::{terminal_text, validate_registered_path};
use crate::config::Config;
use crate::http_client::ServerEndpoint;

/// Resolve the newest still-valid checkout and continue it.
pub async fn run(config: &Config, args: ContinueArgs) -> Result<i32> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let links = project_registry::links_for_server(config, &endpoint)?;
    let candidates = filter_workspace(links, args.workspace.as_deref());
    if candidates.is_empty() {
        bail!(
            "no managed checkout is linked for this server yet; \
             launch one with `ai-memory show` or `ai-memory run <harness>` first"
        );
    }

    let (candidates, invalid_timestamps) = newest_first(candidates);
    let mut rejected = invalid_timestamps.len();
    for link in invalid_timestamps {
        eprintln!(
            "skipping {}: client registry has an invalid linked_at timestamp",
            scope_label(&link)
        );
    }
    for link in candidates {
        let target = match resolve_target(config, &link) {
            Ok(target) => target,
            Err(reason) => {
                // Never fall through to an older checkout silently: the user
                // asked for "where I was", and landing somewhere else without
                // saying so would attach this session to the wrong project.
                // The reason quotes the registry's own path, so it is
                // stripped like every other stored string before it reaches
                // the terminal.
                let label = scope_label(&link);
                eprintln!("skipping {label}: {}", terminal_text(&reason.to_string()));
                rejected += 1;
                continue;
            }
        };
        eprintln!(
            "continuing {} in {}",
            scope_label(&link),
            terminal_text(&target.to_string_lossy())
        );
        return crate::commands::run::run_from(
            config,
            RunArgs {
                workspace: Some(link.workspace),
                project: Some(link.project),
                workstream: None,
                new_workstream: None,
                executable: None,
                yolo: args.yolo,
                fresh: args.fresh,
                // Bare mode: `run` resolves the harness that owns the newest
                // usable session for this workstream.
                harness: None,
                native_args: Vec::new(),
            },
            &target,
        )
        .await;
    }

    bail!(
        "no linked checkout is still usable ({} skipped); \
         pick one explicitly with `ai-memory show`",
        rejected
    )
}

fn filter_workspace(links: Vec<ProjectLink>, workspace: Option<&str>) -> Vec<ProjectLink> {
    match workspace {
        Some(name) => links.into_iter().filter(|l| l.workspace == name).collect(),
        None => links,
    }
}

/// Order links newest-first.
///
/// Corrupt timestamps are returned separately and are never launch candidates.
/// One bad row must not strand valid checkouts. Ties break on `(workspace,
/// project)` so the choice is deterministic.
fn newest_first(links: Vec<ProjectLink>) -> (Vec<ProjectLink>, Vec<ProjectLink>) {
    let mut valid = Vec::with_capacity(links.len());
    let mut invalid = Vec::new();
    for link in links {
        match linked_at(&link) {
            Some(timestamp) => valid.push((timestamp, link)),
            None => invalid.push(link),
        }
    }
    valid.sort_by(|(a_timestamp, a_link), (b_timestamp, b_link)| {
        b_timestamp
            .cmp(a_timestamp)
            .then_with(|| a_link.workspace.cmp(&b_link.workspace))
            .then_with(|| a_link.project.cmp(&b_link.project))
    });
    (valid.into_iter().map(|(_, link)| link).collect(), invalid)
}

fn linked_at(link: &ProjectLink) -> Option<jiff::Timestamp> {
    link.linked_at.parse().ok()
}

/// Revalidate a stored checkout before launching anything inside it.
///
/// Both checks matter. The path check rejects a directory that moved or was
/// replaced by a symlink; the scope check rejects a directory that still
/// exists but now resolves to a different project, which would otherwise
/// file this session's memory under the wrong scope.
pub(super) fn resolve_target(config: &Config, link: &ProjectLink) -> Result<PathBuf> {
    let path = validate_registered_path(&link.path)?;
    let (workspace, project) = super::resolve_scope_for_path(config, &path)?;
    if workspace != link.workspace || project != link.project {
        bail!(
            "checkout now resolves to {}/{}",
            terminal_text(&workspace),
            terminal_text(&project)
        );
    }
    Ok(path)
}

fn scope_label(link: &ProjectLink) -> String {
    terminal_text(&format!("{}/{}", link.workspace, link.project))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(workspace: &str, project: &str, linked_at: &str) -> ProjectLink {
        ProjectLink {
            server: "http://127.0.0.1:49374".to_owned(),
            workspace: workspace.to_owned(),
            project: project.to_owned(),
            path: PathBuf::from("/checkout").join(project),
            linked_at: linked_at.to_owned(),
        }
    }

    fn projects(links: &[ProjectLink]) -> Vec<&str> {
        links.iter().map(|l| l.project.as_str()).collect()
    }

    #[test]
    fn newest_linked_checkout_wins() {
        let (ordered, invalid) = newest_first(vec![
            link("default", "older", "2026-07-01T10:00:00Z"),
            link("default", "newest", "2026-08-01T09:00:00Z"),
            link("default", "middle", "2026-07-20T23:59:59Z"),
        ]);
        assert!(invalid.is_empty());
        assert_eq!(projects(&ordered), ["newest", "middle", "older"]);
    }

    /// Timestamps are compared as instants, not strings: an offset spelling
    /// that sorts earlier lexically can still be the newer instant.
    #[test]
    fn ordering_compares_instants_not_raw_strings() {
        let (ordered, invalid) = newest_first(vec![
            link("default", "utc", "2026-08-01T09:00:00Z"),
            link("default", "offset", "2026-08-01T11:30:00+02:00"),
        ]);
        assert!(invalid.is_empty());
        assert_eq!(projects(&ordered), ["offset", "utc"]);
    }

    /// One corrupt row must not strand every other checkout.
    #[test]
    fn unparsable_timestamps_are_rejected_without_stranding_valid_links() {
        let (ordered, invalid) = newest_first(vec![
            link("default", "corrupt", "not-a-timestamp"),
            link("default", "valid", "2026-01-01T00:00:00Z"),
        ]);
        assert_eq!(projects(&ordered), ["valid"]);
        assert_eq!(projects(&invalid), ["corrupt"]);
    }

    #[test]
    fn equal_timestamps_break_ties_deterministically() {
        let same = "2026-08-01T09:00:00Z";
        let (first, first_invalid) = newest_first(vec![
            link("b-workspace", "zeta", same),
            link("a-workspace", "alpha", same),
        ]);
        let (second, second_invalid) = newest_first(vec![
            link("a-workspace", "alpha", same),
            link("b-workspace", "zeta", same),
        ]);
        assert!(first_invalid.is_empty());
        assert!(second_invalid.is_empty());
        assert_eq!(projects(&first), ["alpha", "zeta"]);
        assert_eq!(projects(&first), projects(&second));
    }

    #[test]
    fn empty_registry_orders_to_nothing() {
        let (ordered, invalid) = newest_first(Vec::new());
        assert!(ordered.is_empty());
        assert!(invalid.is_empty());
    }

    fn config_at(path: &std::path::Path) -> Config {
        Config {
            data_dir: path.to_path_buf(),
            ..Config::default()
        }
    }

    fn make_project(root: &std::path::Path, name: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("Cargo.toml"), b"").unwrap();
        path.canonicalize().unwrap()
    }

    fn link_to(path: PathBuf, workspace: &str, project: &str) -> ProjectLink {
        ProjectLink {
            server: "http://127.0.0.1:49374".to_owned(),
            workspace: workspace.to_owned(),
            project: project.to_owned(),
            path,
            linked_at: "2026-08-01T09:00:00Z".to_owned(),
        }
    }

    #[test]
    fn a_checkout_whose_scope_still_matches_resolves() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = make_project(tmp.path(), "app");
        let link = link_to(project.clone(), "default", "app");

        assert_eq!(
            resolve_target(&config_at(tmp.path()), &link).unwrap(),
            project
        );
    }

    /// A checkout that was deleted (or renamed) must not resume: the launch
    /// would otherwise start in whatever now occupies the path.
    #[test]
    fn a_missing_checkout_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = make_project(tmp.path(), "app");
        let link = link_to(project.clone(), "default", "app");
        std::fs::remove_dir_all(&project).unwrap();

        assert!(resolve_target(&config_at(tmp.path()), &link).is_err());
    }

    /// The link's recorded scope is authoritative. If the directory now
    /// resolves elsewhere, resuming would file this session's memory under
    /// the wrong project.
    #[test]
    fn a_checkout_that_changed_scope_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = make_project(tmp.path(), "app");
        let link = link_to(project, "default", "was-named-differently");

        let error = resolve_target(&config_at(tmp.path()), &link).unwrap_err();
        assert!(
            error.to_string().contains("now resolves to"),
            "unexpected error: {error}"
        );
    }

    /// Registry strings reach the terminal on the skip path, so an escape
    /// sequence stored in a checkout path must not be replayed.
    #[test]
    fn skip_reasons_are_stripped_before_reaching_the_terminal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let link = link_to(
            PathBuf::from("/missing\u{1b}[31m/checkout"),
            "default",
            "app",
        );

        let reason = resolve_target(&config_at(tmp.path()), &link).unwrap_err();
        let rendered = terminal_text(&reason.to_string());
        assert!(!rendered.contains('\u{1b}'), "escape survived: {rendered}");
    }

    /// A recorded directory replaced by a symlink pointing somewhere else
    /// must not be followed.
    #[cfg(unix)]
    #[test]
    fn a_checkout_replaced_by_a_symlink_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = make_project(tmp.path(), "app");
        let elsewhere = make_project(tmp.path(), "elsewhere");
        let link = link_to(real.clone(), "default", "app");

        std::fs::remove_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &real).unwrap();

        assert!(resolve_target(&config_at(tmp.path()), &link).is_err());
    }

    #[test]
    fn workspace_filter_keeps_only_the_requested_scope() {
        let links = vec![
            link("work", "api", "2026-08-01T09:00:00Z"),
            link("personal", "blog", "2026-08-02T09:00:00Z"),
        ];
        let filtered = filter_workspace(links.clone(), Some("work"));
        assert_eq!(projects(&filtered), ["api"]);
        assert_eq!(filter_workspace(links.clone(), Some("absent")).len(), 0);
        assert_eq!(filter_workspace(links, None).len(), 2);
    }
}
