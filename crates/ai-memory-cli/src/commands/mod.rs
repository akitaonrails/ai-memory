//! Subcommand implementations.

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;

pub mod apply_shared;
pub mod auth;
pub mod backup;
pub mod bootstrap;
pub mod commit;
pub mod data_purge;
pub mod embed;
pub mod forget_sweep;
pub mod generate_auth_token;
pub mod init;
pub mod install_hooks;
pub mod install_instructions;
pub mod install_mcp;
pub mod lint;
pub mod llm_test;
pub mod openclaw_plugin;
pub mod purge_project;
pub mod read_page;
pub mod rename_project;
pub mod render_shared;
pub mod reorg;
pub mod reset;
pub mod restore;
pub mod search;
pub mod serve;
pub mod setup_agent;
pub mod status;
pub mod uninstall;
pub mod write_page;

/// Resolve the effective project name for a client command.
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
             which host dir you ran from. Fix:\n  \
             curl -fsSL https://raw.githubusercontent.com/akitaonrails/ai-memory/main/bin/ai-memory \\\n    \
               -o ~/.local/bin/ai-memory && chmod +x ~/.local/bin/ai-memory\n  \
             (or run `ai-memory upgrade` if your existing wrapper is recent enough \
             to know that command)"
        );
    }

    if let Ok(root) = ai_memory_consolidate::discover_repo_root(std::path::Path::new("."))
        && let Some(name) = root
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
    {
        return Ok(name.to_string());
    }
    cwd.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "could not derive project name from CWD ({}); \
                 pass --project explicitly",
                cwd.display()
            )
        })
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
}
