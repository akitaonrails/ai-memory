//! Subcommand implementations.

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;

pub mod apply_shared;
pub mod audit_contamination;
pub mod auth;
pub mod auto_improve;
pub mod auto_improve_report;
pub mod backup;
pub mod bootstrap;
pub mod checkpoints;
pub mod commit;
pub mod compact;
pub mod completions;
pub mod continue_session;
pub mod curator;
pub mod data_purge;
pub mod delete_page;
pub mod embed;
pub mod finalize_session;
pub mod forget_sweep;
pub mod generate_auth_token;
pub mod handoffs;
pub mod hook;
pub mod hook_capture;
pub mod hook_drain_process;
pub mod hook_spool;
pub mod init;
pub mod install_hooks;
pub mod install_instructions;
pub mod install_mcp;
pub mod install_skills;
pub mod lint;
pub mod llm_test;
pub mod mcp_bridge;
pub mod move_project;
pub mod move_session;
pub mod openclaw_plugin;
pub mod path_util;
pub mod pending_writes;
pub mod project_registry;
pub mod purge_project;
pub mod purge_session;
pub mod read_page;
pub mod reindex;
pub mod rename_project;
pub mod rename_workstream;
pub mod render_shared;
pub mod reorg;
pub mod reset;
pub mod restore;
pub mod restore_page;
pub mod run;
pub mod search;
pub mod serve;
pub mod setup_agent;
pub mod show;
pub mod status;
pub mod uninstall;
pub mod user;
pub mod workstream_search;
pub mod workstreams;
pub mod write_page;

/// Resolve the effective `(workspace, project)` pair for a client command.
///
/// Each field is resolved independently, in this order:
///
/// 1. The explicit flag (`--workspace` / `--project`) when non-empty.
/// 2. The nearest `.ai-memory.toml` marker: `workspace`, and `project` —
///    or the hook-compatible derived project when the marker leaves it
///    unpinned (`basename(cwd)` by default, main repo root for `repo-root`).
/// 3. Today's fallbacks: [`crate::config::DEFAULT_WORKSPACE`] and
///    [`resolve_project_name`]'s cwd chain.
///
/// Rung 2 is why this function exists. Before it, only the lifecycle hooks
/// read the marker, so a checkout declaring `workspace = "acme"` still had
/// every CLI command resolve into `default` — the same repository ended up
/// split across two scopes, with `run`'s managed workstream on one side and
/// the hook-captured sessions on the other.
///
/// Resolution is announced on stderr whenever the marker decides a field, so
/// a scope that differs from the flags the user typed is never silent.
/// Render an elapsed time the way the read-only listings say it.
///
/// `ai-memory workstreams` and `ai-memory handoffs` both answer "how long
/// ago", and each shipped its own renderer a day apart — one saying
/// `3 hours ago`, the other `3h ago` — so the same quantity read differently
/// depending on which command you ran.
///
/// Deliberately does **not** cover `status`'s spool line. That renders a
/// duration (`oldest: 2h 10m`), not an "ago", and is a stable surface people
/// read; folding it in here would change output that no fix requires.
///
/// Clamps negatives to "just now": server and client clocks can disagree, and
/// a skewed timestamp must not render as an enormous age.
pub(crate) fn humanize_age_secs(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return "just now".to_owned();
    }
    let (value, unit) = match seconds {
        v if v < 3_600 => (v / 60, "minute"),
        v if v < 86_400 => (v / 3_600, "hour"),
        v if v < 2_592_000 => (v / 86_400, "day"),
        v => (v / 2_592_000, "month"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

/// `AI_MEMORY_IGNORE_MARKER=1` skips rung 2 entirely.
pub(crate) fn resolve_scope(
    config: &Config,
    explicit_ws: Option<&str>,
    explicit_proj: Option<&str>,
) -> Result<(String, String)> {
    let explicit_ws = explicit_ws.filter(|s| !s.is_empty());
    let explicit_proj = explicit_proj.filter(|s| !s.is_empty());
    // Both halves pinned on the command line: the marker cannot change the
    // answer, so skip the walk and the file reads entirely.
    if let (Some(workspace), Some(project)) = (explicit_ws, explicit_proj) {
        return Ok((workspace.to_string(), project.to_string()));
    }
    let marker = marker_scope(config);

    let mut decided: Vec<&str> = Vec::with_capacity(2);
    let workspace = match explicit_ws {
        Some(explicit) => explicit.to_string(),
        None => match marker
            .as_ref()
            .and_then(|(scope, _, _)| scope.workspace.clone())
        {
            Some(declared) => {
                decided.push("workspace");
                declared
            }
            None => crate::config::DEFAULT_WORKSPACE.to_string(),
        },
    };

    let project = match explicit_proj {
        Some(explicit) => explicit.to_string(),
        None => {
            // A marker's explicit `project` pins the name for its whole tree.
            // Otherwise derive exactly as the hook does: basename(cwd) by
            // default, or the main repository root for `repo-root`.
            let declared = marker
                .as_ref()
                .and_then(|(scope, identity_cwd, lookup_cwd)| {
                    scope.project.clone().or_else(|| {
                        if scope.is_repo_root() {
                            crate::marker::repo_root_project(lookup_cwd)
                        } else {
                            ai_memory_consolidate::derive_project_name(
                                std::path::Path::new(identity_cwd),
                                ai_memory_consolidate::ProjectNameStrategy::Basename,
                            )
                            .map(|(name, _)| name)
                        }
                    })
                });
            match declared {
                Some(name) => {
                    decided.push("project");
                    name
                }
                None => resolve_project_name(config, None)?,
            }
        }
    };

    if let Some((scope, _, _)) = marker.as_ref().filter(|_| !decided.is_empty()) {
        // Name only the halves the marker actually decided: an explicit flag
        // that was honoured must not read as if the file overrode it.
        eprintln!(
            "ai-memory: scope {workspace}/{project} ({} from {})",
            decided.join(" + "),
            scope.path.display()
        );
    }
    Ok((workspace, project))
}

/// Resolve `(workspace, project)` for an explicit local directory without
/// changing the process working directory or consulting wrapper cwd overrides.
///
/// The policy matches [`resolve_scope`]: a marker may pin either half, an
/// unpinned project under a marker follows that marker's strategy, and a tree
/// without a scope marker falls back to the main repository root.
pub(crate) fn resolve_scope_for_path(
    config: &Config,
    cwd: &std::path::Path,
) -> Result<(String, String)> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("canonicalizing project candidate {}", cwd.display()))?;
    let identity = cwd.to_string_lossy().into_owned();
    let marker = crate::marker::read_scope(&identity, &config.runtime_env);
    let workspace = marker
        .as_ref()
        .and_then(|scope| scope.workspace.clone())
        .unwrap_or_else(|| crate::config::DEFAULT_WORKSPACE.to_string());
    let project = match marker.as_ref() {
        Some(scope) => scope
            .project
            .clone()
            .or_else(|| {
                if scope.is_repo_root() {
                    crate::marker::repo_root_project(&identity)
                } else {
                    ai_memory_consolidate::derive_project_name(
                        &cwd,
                        ai_memory_consolidate::ProjectNameStrategy::Basename,
                    )
                    .map(|(name, _)| name)
                }
            })
            .ok_or_else(|| anyhow!("could not derive project name from {}", cwd.display()))?,
        None => ai_memory_consolidate::derive_project_name(
            &cwd,
            ai_memory_consolidate::ProjectNameStrategy::MainRepoRoot,
        )
        .map(|(name, _)| name)
        .ok_or_else(|| anyhow!("could not derive project name from {}", cwd.display()))?,
    };
    Ok((workspace, project))
}

/// Resolve only the workspace half, for the one command whose project half is
/// deliberately absent (`embed --force` fans out across every project in the
/// workspace). Resolving the pair there would make the command fail on the
/// cwd-derived project name it is about to throw away.
pub(crate) fn resolve_workspace(config: &Config, explicit_ws: Option<&str>) -> String {
    if let Some(explicit) = explicit_ws.filter(|s| !s.is_empty()) {
        return explicit.to_string();
    }
    match marker_scope(config) {
        Some((scope, _, _)) => match scope.workspace {
            Some(declared) => {
                eprintln!(
                    "ai-memory: workspace {declared} (workspace from {})",
                    scope.path.display()
                );
                declared
            }
            None => crate::config::DEFAULT_WORKSPACE.to_string(),
        },
        None => crate::config::DEFAULT_WORKSPACE.to_string(),
    }
}

/// The nearest scope-declaring marker plus the cwd the walk started from.
fn marker_scope(config: &Config) -> Option<(crate::marker::MarkerScope, String, String)> {
    let identity_cwd = scope_cwd(config)?;
    // The Docker wrapper preserves the host cwd for identity but binds the
    // checkout at /work. Check the host path when its bounded root is mounted;
    // otherwise use the physical cwd so a marker in the /work bind is still
    // visible. Project derivation keeps using the host identity either way.
    let lookup_cwd = if let Some(scope_cwd) = config.runtime_env.scope_cwd() {
        scope_cwd.to_string()
    } else if std::path::Path::new(&identity_cwd).exists() {
        identity_cwd.clone()
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.to_string_lossy().into_owned())?
    };
    let scope = crate::marker::read_scope(&lookup_cwd, &config.runtime_env)?;
    Some((scope, identity_cwd, lookup_cwd))
}

/// The directory marker discovery walks up from.
///
/// Prefers `AI_MEMORY_HOST_CWD` for the same reason [`resolve_project_name`]
/// does: inside the docker wrapper the container's own `current_dir()` is the
/// `/work` bind mount, which would find the wrong marker (or none).
fn scope_cwd(config: &Config) -> Option<String> {
    if let Some(host_cwd) = config.runtime_env.host_cwd() {
        return Some(host_cwd.to_string());
    }
    std::env::current_dir()
        .ok()
        .map(|cwd| cwd.to_string_lossy().into_owned())
}

/// Resolve the effective project name for a client command.
///
/// Prefer [`resolve_scope`] for new call sites: this resolves only half the
/// scope and never consults the marker, so a command that uses it alone still
/// pairs its project with whatever workspace the caller passed.
///
/// Precedence:
/// 1. `explicit` (the user's `--project` flag) when non-empty.
/// 2. `AI_MEMORY_HOST_CWD` env var. The docker wrapper sets this
///    to the host's `$PWD` because inside the container the workdir
///    is always `/work` (a bind mount), so the container's own
///    `current_dir()` returns "work" for every invocation. Without
///    this env var, every dockerised bootstrap would land in project
///    `default/work` regardless of which host dir it was actually
///    run from. Honoured here as a basename, same heuristic as the
///    other fallbacks.
/// 3. Basename of the git repo root walked up from CWD (handles
///    running from any subdir of the project).
/// 4. Basename of the bare CWD (covers non-git directories).
///
/// Mirrors the heuristic the hook router uses in
/// `ai-memory-hooks::router::resolve_project_ids`, so commands
/// auto-target the same project the user's interactive sessions
/// have been writing into. Dot-prefixed dirs are preserved
/// verbatim (`~/.config` → project `.config`).
pub(crate) fn resolve_project_name(config: &Config, explicit: Option<&str>) -> Result<String> {
    if let Some(p) = explicit.filter(|s| !s.is_empty()) {
        return Ok(p.to_string());
    }
    if let Some(host_cwd) = config.runtime_env.host_cwd()
        && let Some(name) = std::path::Path::new(host_cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
    {
        return Ok(name.to_string());
    }

    // Safety net: when running inside the docker wrapper, the
    // container's workdir is bind-mounted at `/work` (a fresh path
    // chosen specifically because the host's `$PWD` would conflict
    // with the $HOME bind mount). If we fall through to here while
    // `current_dir()` is `/work`, the wrapper is STALE: it didn't
    // pass `-e AI_MEMORY_HOST_CWD=$PWD` and the binary has no idea
    // which host dir invoked it. Bail with a clear remedy instead
    // of silently writing every project to `default/work`.
    let cwd = std::env::current_dir().context("getting CWD for project auto-detect")?;
    if cwd.as_os_str() == "/work" {
        bail!(
            "the `ai-memory` wrapper at ~/.local/bin/ai-memory looks stale \
             (it didn't pass AI_MEMORY_HOST_CWD into the container). Without \
             this, every project would land in `default/work` regardless of \
             which host dir you ran from. Reinstall the checksum-verified \
             wrapper from the latest GitHub Release as documented in README.md,\n  \
             (or run `ai-memory upgrade` if your existing wrapper is recent enough \
             to know that command)"
        );
    }

    // Shared with the hook router via `derive_project_name` so the CLI
    // and hooks agree on what "the project for this cwd" means. The
    // `MainRepoRoot` strategy walks worktrees back to the main repo
    // — a session in `~/repo-worktrees/feature-x/` and one in the
    // main checkout resolve to the same project name (the main repo's
    // basename), instead of fragmenting into separate projects.
    // Aligned change from the earlier CLI behaviour (which used the
    // worktree-local `discover_repo_root`).
    if let Some((name, _)) = ai_memory_consolidate::derive_project_name(
        &cwd,
        ai_memory_consolidate::ProjectNameStrategy::MainRepoRoot,
    ) {
        return Ok(name);
    }
    Err(anyhow!(
        "could not derive project name from CWD ({}); \
         pass --project explicitly",
        cwd.display()
    ))
}

/// Human-readable lines for the proposals the server refused to stage.
///
/// Empty when nothing was skipped, so a clean run prints nothing extra. Every
/// staging command shares this: a skipped proposal is otherwise
/// indistinguishable from one the reviewer never produced — the run reports
/// success either way, only with one proposal fewer — and the operator has no
/// way to learn that a paid review result was dropped.
pub(crate) fn skipped_proposal_lines(skipped: &[ai_memory_store::SkippedProposal]) -> Vec<String> {
    if skipped.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::with_capacity(skipped.len() + 1);
    lines.push(format!("Skipped {} proposal(s):", skipped.len()));
    lines.extend(
        skipped
            .iter()
            .map(|s| format!("  - {}: {}", s.target_path, s.reason)),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeEnv;

    #[test]
    fn resolve_project_name_prefers_explicit_value() {
        let config = Config {
            runtime_env: RuntimeEnv::with_host_cwd_for_tests("/host/ignored"),
            ..Config::default()
        };

        assert_eq!(
            resolve_project_name(&config, Some("explicit-project")).unwrap(),
            "explicit-project"
        );
    }

    #[test]
    fn resolve_project_name_uses_host_cwd_basename() {
        let config = Config {
            runtime_env: RuntimeEnv::with_host_cwd_for_tests("/host/my-project"),
            ..Config::default()
        };

        assert_eq!(resolve_project_name(&config, None).unwrap(), "my-project");
    }

    #[test]
    fn skipped_proposal_lines_are_empty_on_a_clean_run() {
        assert!(skipped_proposal_lines(&[]).is_empty());
    }

    #[test]
    fn skipped_proposal_lines_name_the_path_and_the_reason() {
        let lines = skipped_proposal_lines(&[ai_memory_store::SkippedProposal {
            target_path: "procedures/release.md".into(),
            reason: "a proposal is already pending review for this path".into(),
        }]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains('1'), "{lines:?}");
        assert!(lines[1].contains("procedures/release.md"), "{lines:?}");
        assert!(
            lines[1].contains("already pending review"),
            "the reason has to travel with the path, or the operator only \
             learns that something vanished: {lines:?}"
        );
    }

    /// Build a config whose scope resolution walks up from `cwd`, and drop a
    /// marker there when `marker` is given.
    fn config_at(dir: &std::path::Path, marker: Option<&str>) -> Config {
        if let Some(body) = marker {
            std::fs::write(dir.join(".ai-memory.toml"), body).unwrap();
        }
        Config {
            runtime_env: RuntimeEnv::with_host_cwd_for_tests(dir.to_str().unwrap()),
            ..Config::default()
        }
    }

    /// The bug this whole path exists for: a checkout whose marker declares a
    /// workspace used to resolve into `default` for every CLI command, while
    /// the lifecycle hooks — the only marker reader at the time — sent that
    /// same checkout's captures to the declared workspace.
    #[test]
    fn resolve_scope_takes_the_workspace_from_the_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_at(tmp.path(), Some("workspace = \"acme\"\n"));

        let (workspace, project) = resolve_scope(&config, None, None).unwrap();

        assert_eq!(workspace, "acme");
        assert_eq!(
            project,
            tmp.path().file_name().unwrap().to_str().unwrap(),
            "an unpinned project still comes from the cwd chain"
        );
    }

    #[test]
    fn workspace_only_marker_uses_hook_basename_from_a_subdirectory() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".ai-memory.toml"), "workspace = \"acme\"\n").unwrap();
        let subdir = tmp.path().join("crates").join("cli");
        std::fs::create_dir_all(&subdir).unwrap();
        let config = Config {
            runtime_env: RuntimeEnv::with_host_cwd_for_tests(subdir.to_str().unwrap()),
            ..Config::default()
        };

        assert_eq!(
            resolve_scope(&config, None, None).unwrap(),
            ("acme".to_string(), "cli".to_string())
        );
        assert_eq!(
            resolve_scope(&config, Some("flagged"), None).unwrap(),
            ("flagged".to_string(), "cli".to_string()),
            "an explicit workspace must not restore the main-repo fallback"
        );
    }

    #[test]
    fn explicit_path_scope_matches_marker_and_non_repo_fallback_policy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace_tree = tmp.path().join("workspace-tree");
        let workspace_child = workspace_tree.join("child");
        std::fs::create_dir_all(&workspace_child).unwrap();
        std::fs::write(
            workspace_tree.join(".ai-memory.toml"),
            "workspace = \"acme\"\n",
        )
        .unwrap();

        let pinned = tmp.path().join("pinned-checkout");
        std::fs::create_dir(&pinned).unwrap();
        std::fs::write(
            pinned.join(".ai-memory.toml"),
            "project = \"shared-project\"\n",
        )
        .unwrap();

        let plain = tmp.path().join("plain-checkout");
        std::fs::create_dir(&plain).unwrap();
        let config = Config::default();

        assert_eq!(
            resolve_scope_for_path(&config, &workspace_child).unwrap(),
            ("acme".to_owned(), "child".to_owned())
        );
        assert_eq!(
            resolve_scope_for_path(&config, &pinned).unwrap(),
            (
                crate::config::DEFAULT_WORKSPACE.to_owned(),
                "shared-project".to_owned()
            )
        );
        assert_eq!(
            resolve_scope_for_path(&config, &plain).unwrap(),
            (
                crate::config::DEFAULT_WORKSPACE.to_owned(),
                "plain-checkout".to_owned()
            )
        );
    }

    #[test]
    fn workspace_only_marker_uses_linked_worktree_directory_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("feature-worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: /repo/.git/worktrees/feature\n",
        )
        .unwrap();
        std::fs::write(worktree.join(".ai-memory.toml"), "workspace = \"acme\"\n").unwrap();
        let config = Config {
            runtime_env: RuntimeEnv::with_host_cwd_for_tests(worktree.to_str().unwrap()),
            ..Config::default()
        };

        assert_eq!(
            resolve_scope(&config, None, None).unwrap(),
            ("acme".to_string(), "feature-worktree".to_string()),
            "workspace-only markers preserve the hook's basename worktree identity"
        );
    }

    /// A marker that pins `project` overrides the cwd-derived name for its
    /// whole tree.
    #[test]
    fn resolve_scope_takes_a_pinned_project_from_the_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_at(
            tmp.path(),
            Some("workspace = \"acme\"\nproject = \"pinned\"\n"),
        );

        assert_eq!(
            resolve_scope(&config, None, None).unwrap(),
            ("acme".to_string(), "pinned".to_string())
        );
    }

    /// Explicit flags are rung 1: they beat the marker on both halves, and
    /// each half is resolved independently.
    #[test]
    fn resolve_scope_prefers_explicit_flags_over_the_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_at(
            tmp.path(),
            Some("workspace = \"acme\"\nproject = \"pinned\"\n"),
        );

        assert_eq!(
            resolve_scope(&config, Some("flagged"), Some("flagged-proj")).unwrap(),
            ("flagged".to_string(), "flagged-proj".to_string())
        );
        assert_eq!(
            resolve_scope(&config, Some("flagged"), None).unwrap(),
            ("flagged".to_string(), "pinned".to_string()),
            "an explicit workspace must not drag the project along with it"
        );
        assert_eq!(
            resolve_scope(&config, None, Some("flagged-proj")).unwrap(),
            ("acme".to_string(), "flagged-proj".to_string()),
            "and vice versa"
        );
    }

    /// No marker means the previous behaviour, byte for byte.
    #[test]
    fn resolve_scope_without_a_marker_keeps_the_old_fallbacks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_at(tmp.path(), None);

        let (workspace, project) = resolve_scope(&config, None, None).unwrap();

        assert_eq!(workspace, crate::config::DEFAULT_WORKSPACE);
        assert_eq!(project, tmp.path().file_name().unwrap().to_str().unwrap());
    }

    /// A marker carrying only `[capture]` rules declares no scope, so it must
    /// not pull the command out of the default workspace.
    #[test]
    fn resolve_scope_ignores_a_marker_without_scope_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_at(
            tmp.path(),
            Some("[capture]\nignore_paths = [\"secret/**\"]\n"),
        );

        let (workspace, _) = resolve_scope(&config, None, None).unwrap();

        assert_eq!(workspace, crate::config::DEFAULT_WORKSPACE);
    }
}
