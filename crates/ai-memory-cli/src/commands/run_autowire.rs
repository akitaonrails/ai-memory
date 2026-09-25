//! Auto-install a harness's ai-memory hooks + MCP the first time it is launched
//! through `ai-memory run`.
//!
//! Managed launch is the recommended way to start a harness ("if in doubt, run
//! with ai-memory"), so it should be the path that makes capture *work* without
//! a separate manual `install-hooks` / `install-mcp` step. Motivating bug: a
//! user ran `ai-memory run kimi` and nothing was captured because the Kimi hooks
//! and MCP had never been installed.
//!
//! This runs at most once per (harness, binary version, install location): a
//! sentinel under `<data_dir>/autowire-state/` keeps the run hot-path fast on
//! every subsequent launch, keying on the version means an upgrade re-stages the
//! fresh hook bundle, and keying on where the hook and MCP configs resolve means
//! a second config home (another `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, ...) gets
//! its own first launch instead of being skipped. Those locations follow
//! `run --env` / `--env-file` first, then ai-memory's own environment. Both
//! installs are idempotent (they no-op when already up to date), and the whole
//! step is best-effort — a failure warns and the harness still launches. Opt out
//! with `run --no-autowire` or `AI_MEMORY_RUN_AUTOWIRE=false`.

use std::cell::Cell;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use ai_memory_workstream::ManagedHarness;
use sha2::{Digest as _, Sha256};

use crate::cli::{AgentChoice, InstallHooksArgs, InstallMcpArgs, McpClient};
use crate::config::Config;

use super::{install_hooks, install_mcp};

/// The install-hooks `AgentChoice` for a launchable harness, or `None` for a
/// harness with no install support (Crush has no `AgentChoice`), which is
/// skipped cleanly.
pub(crate) fn agent_choice_for_harness(harness: ManagedHarness) -> Option<AgentChoice> {
    Some(match harness {
        ManagedHarness::Claude => AgentChoice::ClaudeCode,
        ManagedHarness::Codex => AgentChoice::Codex,
        ManagedHarness::OpenCode => AgentChoice::OpenCode,
        ManagedHarness::OpenCode2 => AgentChoice::OpenCode2,
        ManagedHarness::Pi => AgentChoice::Pi,
        ManagedHarness::Omp => AgentChoice::Omp,
        ManagedHarness::Kimi => AgentChoice::KimiCode,
        ManagedHarness::CommandCode => AgentChoice::CommandCode,
        ManagedHarness::Kiro => AgentChoice::KiroCli,
        ManagedHarness::KiroV3 => AgentChoice::KiroCliV3,
        ManagedHarness::Grok => AgentChoice::Grok,
        ManagedHarness::Antigravity => AgentChoice::AntigravityCli,
        // No AgentChoice / installer support.
        ManagedHarness::Crush => return None,
    })
}

/// Where auto-wire records its sentinels. `uninstall` clears it whenever it
/// removes hooks or MCP, so the next managed launch wires again.
pub(crate) fn autowire_state_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("autowire-state")
}

/// Sentinel marking that auto-wire already ran for this agent, binary version
/// and install location. The version re-wires the fresh hook bundle after an
/// upgrade; the hashed hook and MCP config locations give each config home its
/// own first launch.
fn sentinel_path(data_dir: &Path, agent: AgentChoice, targets: &[String]) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(targets.join("\n").as_bytes()));
    let name = format!(
        "{}-{}-{}",
        agent.kind().as_str(),
        env!("CARGO_PKG_VERSION"),
        &digest[..16]
    );
    autowire_state_dir(data_dir).join(name)
}

fn write_sentinel(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Best-effort: a missing sentinel only costs a redundant idempotent re-apply
    // on the next launch, never wrong behavior.
    let _ = std::fs::write(path, b"");
}

/// Test-only path injections so the wiring can be exercised without resolving —
/// and writing to — the developer's real `$HOME` harness config. In production
/// every field is `None`, so the installers resolve their real per-agent paths.
#[derive(Default)]
pub(crate) struct WireOverrides {
    /// Source directory of the hook script bundle to stage from.
    pub hooks_dir: Option<PathBuf>,
    /// Destination agent hook-config file (else the agent's real path).
    pub hooks_config_file: Option<PathBuf>,
    /// Destination agent MCP-config file (else the agent's real path).
    pub mcp_config_file: Option<PathBuf>,
    /// Refuse every install whose target is not an explicit path under this
    /// directory, instead of letting it reach the real `$HOME` (an installer
    /// default, or a relocation that resolved back to the home). Lets a test
    /// drive `run` without injected targets and still never write outside
    /// its temp dirs.
    pub confine_to: Option<PathBuf>,
}

/// `path` with every existing part resolved through symlinks, so
/// `<root>/link/settings.json` with `link` pointing elsewhere is not under
/// `<root>`; the missing tail is folded lexically. `None` for a relative path
/// or a dangling link, which cannot be placed.
fn resolve_through_links(path: &Path) -> Option<PathBuf> {
    for existing in path.ancestors() {
        match std::fs::canonicalize(existing) {
            Ok(resolved) => {
                let rest = path.strip_prefix(existing).ok()?;
                let joined = ai_memory_workstream::clean_path(&resolved.join(rest));
                // Folding `missing/../link` lands on a part that exists, and
                // creating `missing` lets the write follow `link`: resolve
                // again from there.
                return if rest.components().any(|part| part == Component::ParentDir) {
                    resolve_through_links(&joined)
                } else {
                    Some(joined)
                };
            }
            // A link whose target is missing would be written through.
            Err(_) if std::fs::symlink_metadata(existing).is_ok() => return None,
            Err(_) => {}
        }
    }
    None
}

/// Reads the relocation variables for auto-wire: `run --env` / `--env-file`
/// entries first, then ai-memory's own environment. That is the order the
/// launch plan resolves the native session store in, so hooks, MCP and
/// transcript import agree on one config home. It also records whether any
/// value it handed out came from `--env`.
struct LaunchEnv<'a> {
    run_env: &'a [(String, String)],
    from_run_env: Cell<bool>,
}

impl<'a> LaunchEnv<'a> {
    fn new(run_env: &'a [(String, String)]) -> Self {
        Self {
            run_env,
            from_run_env: Cell::new(false),
        }
    }

    fn var_os(&self, name: &str) -> Option<OsString> {
        if let Some((_, value)) = self.run_env.iter().find(|(key, _)| key == name) {
            self.from_run_env.set(true);
            return Some(OsString::from(value));
        }
        std::env::var_os(name)
    }
}

/// Where this launch's auto-wire installs, resolved without touching the disk.
struct WireTargets {
    agent: AgentChoice,
    sentinel: PathBuf,
    hook_target: anyhow::Result<PathBuf>,
    /// Whether any relocation variable behind the hook target came from `--env`.
    hook_relocated: bool,
    mcp_client: Option<McpClient>,
    mcp_target: Option<anyhow::Result<PathBuf>>,
    mcp_relocated: bool,
    /// The extensions directory Pi and OMP share under the launch env, when
    /// the installer's own check (which reads ai-memory's environment) would
    /// miss it.
    shared_extensions_dir: Option<PathBuf>,
}

fn wire_targets(
    config: &Config,
    harness: ManagedHarness,
    overrides: &WireOverrides,
    run_env: &[(String, String)],
) -> Option<WireTargets> {
    let agent = agent_choice_for_harness(harness)?;
    let hook_env = LaunchEnv::new(run_env);
    let hook_target = install_hooks::hook_config_target_with(agent, &|name| hook_env.var_os(name));
    let mcp_client = install_hooks::mcp_client_for_agent(agent);
    let mcp_env = LaunchEnv::new(run_env);
    let mcp_target = mcp_client
        .map(|client| install_mcp::mcp_config_path_with(client, &|name| mcp_env.var_os(name)));
    let sentinel = sentinel_path(
        &config.data_dir,
        agent,
        &[
            target_key(overrides.hooks_config_file.as_deref(), Some(&hook_target)),
            target_key(overrides.mcp_config_file.as_deref(), mcp_target.as_ref()),
        ],
    );
    let shared_extensions_dir = match (&overrides.hooks_config_file, &hook_target) {
        (None, Ok(target)) => {
            let launch = LaunchEnv::new(run_env);
            let process = LaunchEnv::new(&[]);
            install_hooks::shared_extensions_dir(agent, target, &|name| launch.var_os(name)).filter(
                |_| {
                    install_hooks::shared_extensions_dir(agent, target, &|name| {
                        process.var_os(name)
                    })
                    .is_none()
                },
            )
        }
        _ => None,
    };
    Some(WireTargets {
        agent,
        sentinel,
        hook_target,
        hook_relocated: hook_env.from_run_env.get(),
        mcp_client,
        mcp_target,
        mcp_relocated: mcp_env.from_run_env.get(),
        shared_extensions_dir,
    })
}

/// One sentinel component: the injected path, else the resolved one. A
/// relative `--env` value is made absolute against the directory the installer
/// writes from, so the same relative name in two checkouts keys two sentinels.
fn target_key(injected: Option<&Path>, resolved: Option<&anyhow::Result<PathBuf>>) -> String {
    let key = |path: &Path| {
        std::path::absolute(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
            .to_string()
    };
    match (injected, resolved) {
        (Some(path), _) => key(path),
        (None, Some(Ok(path))) => key(path),
        (None, Some(Err(_))) => "unresolved".to_string(),
        (None, None) => "none".to_string(),
    }
}

/// The installer calls one auto-wire makes. A test override always wins;
/// otherwise a target is pinned only when `--env` moved it, so a launch without
/// `--env` installs exactly where the installers pick on their own.
struct WireInstalls {
    /// One hook install, or one per agent config for a Kiro CLI v2 home that
    /// `--env` relocated.
    hooks: Vec<InstallHooksArgs>,
    /// Why no hook install could be planned, reported like an install failure.
    hooks_skipped: Option<String>,
    mcp: Option<InstallMcpArgs>,
    /// Why the MCP install was not planned, reported like an install failure.
    mcp_skipped: Option<String>,
    shared_extensions_dir: Option<PathBuf>,
}

fn wire_installs(config: &Config, targets: WireTargets, overrides: &WireOverrides) -> WireInstalls {
    let WireTargets {
        agent,
        hook_target,
        hook_relocated,
        mcp_client,
        mcp_target,
        mcp_relocated,
        shared_extensions_dir,
        ..
    } = targets;
    let server_url = Some(config.server_url.clone());
    let auth_token = config.auth.bearer_token.clone();
    let hook_install = |config_file: Option<PathBuf>| InstallHooksArgs {
        agent,
        hooks_dir: overrides.hooks_dir.clone(),
        server_url: server_url.clone(),
        auth_token: auth_token.clone(),
        as_user: None,
        apply: true,
        config_file,
        project_strategy: None,
        capture_assistant: false,
        capture_mode: None,
        no_capture_prompts: false,
        capture_prompts: false,
        profile: None,
    };
    let (hooks, hooks_skipped) = match (&overrides.hooks_config_file, hook_target) {
        (Some(file), _) => (vec![hook_install(Some(file.clone()))], None),
        // Kiro CLI v2 keeps hooks inside each agent config. The installer lists
        // them only under its own KIRO_HOME, and its --config-file names a single
        // config, so a relocated agents directory is expanded here.
        (None, Ok(agents_dir)) if hook_relocated && agent == AgentChoice::KiroCli => {
            match install_hooks::list_kiro_cli_agent_configs(&agents_dir) {
                Ok(files) if !files.is_empty() => (
                    files
                        .into_iter()
                        .map(|file| hook_install(Some(file)))
                        .collect(),
                    None,
                ),
                Ok(_) => (
                    Vec::new(),
                    Some(format!(
                        "no Kiro CLI agent configs found in {}",
                        agents_dir.display()
                    )),
                ),
                Err(error) => (Vec::new(), Some(format!("{error:#}"))),
            }
        }
        (None, Ok(target)) if hook_relocated => (vec![hook_install(Some(target))], None),
        // `--env` relocated a target the resolver could not place. The
        // installer would fall back to ai-memory's own environment and wire a
        // home this launch does not use.
        (None, Err(error)) if hook_relocated => (Vec::new(), Some(format!("{error:#}"))),
        (None, _) => (vec![hook_install(None)], None),
    };
    let outside_confinement = |file: Option<&PathBuf>| {
        overrides.confine_to.as_deref().is_some_and(|root| {
            let root = std::fs::canonicalize(root)
                .unwrap_or_else(|_| ai_memory_workstream::clean_path(root));
            !file
                .and_then(|file| resolve_through_links(file))
                .is_some_and(|file| file.starts_with(root))
        })
    };
    let confinement_refusal = || {
        overrides
            .confine_to
            .as_deref()
            .map_or_else(String::new, |root| {
                format!("refused a target outside {}", root.display())
            })
    };
    let (hooks, hooks_skipped) = if hooks
        .iter()
        .any(|args| outside_confinement(args.config_file.as_ref()))
    {
        (Vec::new(), Some(confinement_refusal()))
    } else {
        (hooks, hooks_skipped)
    };
    let (mcp_file, mcp_error) = match (&overrides.mcp_config_file, mcp_target) {
        (Some(file), _) => (Some(file.clone()), None),
        (None, Some(Ok(target))) if mcp_relocated => (Some(target), None),
        (None, Some(Err(error))) if mcp_relocated => (None, Some(format!("{error:#}"))),
        _ => (None, None),
    };
    let mcp_skipped = mcp_client.and(
        mcp_error.or_else(|| outside_confinement(mcp_file.as_ref()).then(confinement_refusal)),
    );
    let mcp = mcp_client
        .filter(|_| mcp_skipped.is_none())
        .map(|client| InstallMcpArgs {
            client,
            server_url: server_url.clone(),
            name: "ai-memory".to_string(),
            auth_token: auth_token.clone(),
            apply: true,
            config_file: mcp_file,
            session_aware: false,
            flavor: None,
        });
    WireInstalls {
        hooks,
        hooks_skipped,
        mcp,
        mcp_skipped,
        shared_extensions_dir,
    }
}

/// Ensure the launched harness has ai-memory hooks + MCP installed where this
/// launch's harness will look for them. Best-effort and one-time per install
/// location; never blocks or fails the launch.
///
/// `run_env` is the resolved `--env` / `--env-file` list, so a relocated config
/// home (`--env CLAUDE_CONFIG_DIR=...`) is wired where the harness will read it.
/// Production launches pass [`WireOverrides::default()`] (via
/// [`run_from`](super::run::run_from)); the overrides exist only so the seam can
/// be exercised without writing to the developer's real `$HOME`.
pub(crate) fn ensure_wired_with(
    config: &Config,
    harness: ManagedHarness,
    overrides: &WireOverrides,
    run_env: &[(String, String)],
) {
    let Some(targets) = wire_targets(config, harness, overrides, run_env) else {
        return;
    };
    if targets.sentinel.exists() {
        return;
    }
    let agent = targets.agent;
    let sentinel = targets.sentinel.clone();

    eprintln!(
        "ai-memory: first managed launch of {} here — wiring its ai-memory hooks + MCP so \
         capture and recall work (disable with --no-autowire or AI_MEMORY_RUN_AUTOWIRE=false).",
        harness.as_str()
    );

    let installs = wire_installs(config, targets, overrides);
    let warn_hooks = |reason: &str| {
        eprintln!(
            "ai-memory: could not auto-install {} hooks ({reason}); continuing launch. \
             Wire them manually with `ai-memory install-hooks --agent {} --apply`.",
            harness.as_str(),
            agent.kind().as_str()
        );
    };
    for args in installs.hooks {
        match install_hooks::run(config, args) {
            Ok(()) => {
                if let Some(dir) = &installs.shared_extensions_dir {
                    install_hooks::warn_agents_share_extensions_dir(dir);
                }
            }
            Err(error) => warn_hooks(&format!("{error:#}")),
        }
    }
    if let Some(reason) = &installs.hooks_skipped {
        warn_hooks(reason);
    }

    // Not every hook-capable harness has an MCP client the installer can write
    // (e.g. Pi bridges MCP through its generated extension); skip those quietly.
    let mcp_failure = match installs.mcp {
        // The sentinel is version-keyed, so this whole step re-runs on every
        // upgrade. install-mcp replaces the `ai-memory` entry wholesale, so a
        // plain re-run would overwrite a session-aware Claude Code bridge the
        // user installed with `install-mcp --session-aware` back to static HTTP,
        // silently disabling per_session for their MCP calls. Preserve it, in
        // the file this launch would write.
        Some(args)
            if install_mcp::existing_entry_is_session_aware(
                args.client,
                args.config_file.as_deref(),
                &args.name,
            ) =>
        {
            eprintln!(
                "ai-memory: keeping the existing session-aware {} MCP bridge; not \
                 overwriting it with the static HTTP registration.",
                harness.as_str()
            );
            None
        }
        Some(args) => install_mcp::run(config, args)
            .err()
            .map(|error| format!("{error:#}")),
        None => installs.mcp_skipped,
    };
    if let Some(reason) = mcp_failure {
        eprintln!(
            "ai-memory: could not auto-install the {} MCP server ({reason}); continuing \
             launch. Wire it manually with `ai-memory install-mcp --client {} --apply`.",
            harness.as_str(),
            agent.kind().as_str()
        );
    }

    // Record the attempt even on partial failure: re-applying an idempotent
    // install on every launch would nag and churn config. A user who wants a
    // retry can re-run install-hooks manually or delete this sentinel under
    // `<data_dir>/autowire-state/`; `ai-memory uninstall` clears them all when
    // it removes hooks or MCP.
    write_sentinel(&sentinel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_launchable_harness_maps_or_is_deliberately_skipped() {
        // Crush is the one launchable harness with no installer support.
        assert!(agent_choice_for_harness(ManagedHarness::Crush).is_none());
        for harness in [
            ManagedHarness::Claude,
            ManagedHarness::Codex,
            ManagedHarness::OpenCode,
            ManagedHarness::OpenCode2,
            ManagedHarness::Pi,
            ManagedHarness::Omp,
            ManagedHarness::Kimi,
            ManagedHarness::CommandCode,
            ManagedHarness::Kiro,
            ManagedHarness::KiroV3,
            ManagedHarness::Grok,
            ManagedHarness::Antigravity,
        ] {
            assert!(
                agent_choice_for_harness(harness).is_some(),
                "{harness:?} must map to an AgentChoice for autowire"
            );
        }
    }

    #[test]
    fn kimi_maps_to_the_kimi_agent_and_client() {
        let agent = agent_choice_for_harness(ManagedHarness::Kimi).unwrap();
        assert_eq!(agent.kind().as_str(), "kimi-code");
        assert!(
            install_hooks::mcp_client_for_agent(agent).is_some(),
            "Kimi has an MCP client the installer can write"
        );
    }

    #[test]
    fn pi_wires_hooks_but_has_no_mcp_client() {
        let agent = agent_choice_for_harness(ManagedHarness::Pi).unwrap();
        assert!(
            install_hooks::mcp_client_for_agent(agent).is_none(),
            "Pi bridges MCP through its extension, not a native mcp.json"
        );
    }

    #[test]
    fn sentinel_keys_on_agent_version_and_install_targets() {
        let dir = Path::new("/data");
        let targets = |hooks: &str| vec![hooks.to_string(), "/cfg/.claude.json".to_string()];
        let claude = sentinel_path(dir, AgentChoice::ClaudeCode, &targets("/a/settings.json"));
        let codex = sentinel_path(dir, AgentChoice::Codex, &targets("/a/settings.json"));
        assert_ne!(claude, codex, "different agents get distinct sentinels");
        assert_eq!(
            claude,
            sentinel_path(dir, AgentChoice::ClaudeCode, &targets("/a/settings.json")),
            "the same install targets reuse the sentinel"
        );
        assert_ne!(
            claude,
            sentinel_path(dir, AgentChoice::ClaudeCode, &targets("/b/settings.json")),
            "another config home gets its own first launch"
        );
        assert!(claude.starts_with("/data/autowire-state/"));
        let prefix = format!("claude-code-{}-", env!("CARGO_PKG_VERSION"));
        assert!(
            claude
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix)),
            "sentinel names the agent and version first: {claude:?}"
        );
    }

    /// `--env CLAUDE_CONFIG_DIR=.claude-work` names a different home in each
    /// checkout, so the sentinel must key on the absolute path.
    #[test]
    fn a_relative_target_keys_on_its_absolute_path() {
        let relative = PathBuf::from(".claude-work").join("settings.json");
        let key = target_key(None, Some(&Ok(relative.clone())));
        assert!(Path::new(&key).is_absolute(), "{key}");
        assert!(Path::new(&key).ends_with(&relative), "{key}");
    }

    /// The planned installs for a launch, without running them.
    fn planned_installs(
        config: &Config,
        harness: ManagedHarness,
        run_env: &[(String, String)],
    ) -> WireInstalls {
        let overrides = WireOverrides::default();
        let targets = wire_targets(config, harness, &overrides, run_env).unwrap();
        wire_installs(config, targets, &overrides)
    }

    fn env_pair(name: &str, value: &Path) -> Vec<(String, String)> {
        vec![(name.to_string(), value.display().to_string())]
    }

    /// `run --env` moves a config home, so auto-wire must install where that
    /// harness will read. Only the plan is built: nothing is written, so a
    /// regression here cannot reach the developer's real config.
    #[test]
    fn run_env_points_hooks_and_mcp_at_the_relocated_config_home() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let root = data.path().join("relocated");
        for (harness, var, hooks, mcp) in [
            (
                ManagedHarness::Claude,
                "CLAUDE_CONFIG_DIR",
                "settings.json",
                Some(".claude.json"),
            ),
            (
                ManagedHarness::Codex,
                "CODEX_HOME",
                "hooks.json",
                Some("config.toml"),
            ),
            (
                ManagedHarness::Pi,
                "PI_CODING_AGENT_DIR",
                "extensions/ai-memory-pi.ts",
                None,
            ),
            // OMP's `mcp.json` stays in `~/.omp/agent`, so nothing pins it.
            (
                ManagedHarness::Omp,
                "PI_CODING_AGENT_DIR",
                "extensions/ai-memory-omp.ts",
                None,
            ),
            (
                ManagedHarness::Kimi,
                "KIMI_CODE_HOME",
                "config.toml",
                Some("mcp.json"),
            ),
            (
                ManagedHarness::KiroV3,
                "KIRO_HOME",
                "hooks/ai-memory.json",
                Some("settings/mcp.json"),
            ),
            (
                ManagedHarness::Grok,
                "GROK_HOME",
                "hooks/ai-memory.json",
                Some("config.toml"),
            ),
        ] {
            let installs = planned_installs(&config, harness, &env_pair(var, &root));
            assert_eq!(installs.hooks.len(), 1, "{harness:?}");
            assert_eq!(
                installs.hooks[0].config_file,
                Some(root.join(hooks)),
                "{harness:?} hooks must follow --env {var}"
            );
            assert_eq!(
                installs.mcp.and_then(|args| args.config_file),
                mcp.map(|relative| root.join(relative)),
                "{harness:?} MCP must follow --env {var}"
            );
        }
    }

    /// `--env` wins over ai-memory's own environment, and an empty value still
    /// masks the process one, the same layering native-session resolution uses.
    #[test]
    fn launch_env_prefers_run_env_and_records_it() {
        let process_only = LaunchEnv::new(&[]);
        assert_eq!(process_only.var_os("PATH"), std::env::var_os("PATH"));
        assert!(!process_only.from_run_env.get());

        let run_env = [("PATH".to_string(), "/from-run-env".to_string())];
        let layered = LaunchEnv::new(&run_env);
        assert_eq!(
            layered.var_os("PATH"),
            Some(OsString::from("/from-run-env"))
        );
        assert!(layered.from_run_env.get());

        let empty = [("PATH".to_string(), String::new())];
        assert_eq!(LaunchEnv::new(&empty).var_os("PATH"), Some(OsString::new()));
    }

    /// A blank `--env` value masks the process one and counts as unset, so
    /// hooks, MCP and native session import all fall back to the defaults the
    /// installers pick on their own, never to a whitespace-named directory.
    #[test]
    fn blank_run_env_relocation_falls_back_to_defaults_everywhere() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        for (harness, vars) in [
            (ManagedHarness::Claude, &["CLAUDE_CONFIG_DIR"][..]),
            (ManagedHarness::Codex, &["CODEX_HOME"][..]),
            (
                ManagedHarness::Pi,
                &["PI_CODING_AGENT_SESSION_DIR", "PI_CODING_AGENT_DIR"][..],
            ),
            (ManagedHarness::Omp, &["PI_CODING_AGENT_DIR"][..]),
            (ManagedHarness::Kimi, &["KIMI_CODE_HOME"][..]),
            (ManagedHarness::KiroV3, &["KIRO_HOME"][..]),
            (ManagedHarness::Grok, &["GROK_HOME"][..]),
        ] {
            let run_env: Vec<_> = vars
                .iter()
                .map(|name| (name.to_string(), "   ".to_string()))
                .collect();
            let agent = agent_choice_for_harness(harness).unwrap();
            let installs = planned_installs(&config, harness, &run_env);
            assert_eq!(
                installs.hooks[0].config_file,
                install_hooks::hook_config_target_with(agent, &|_| None).ok(),
                "{harness:?} hooks"
            );
            // OMP's `mcp.json` reads no relocation variable, so nothing pins it.
            if let Some(client) = install_hooks::mcp_client_for_agent(agent)
                && harness != ManagedHarness::Omp
            {
                assert_eq!(
                    installs.mcp.and_then(|args| args.config_file),
                    install_mcp::mcp_config_path_with(client, &|_| None).ok(),
                    "{harness:?} MCP"
                );
            }
            let plan = ai_memory_workstream::build_launch_plan_with_env(
                harness,
                None,
                Vec::new(),
                None,
                &run_env,
            )
            .unwrap();
            assert_eq!(plan.session_dir, None, "{harness:?} session store");
        }
    }

    /// `run pi|omp --env PI_CODING_AGENT_DIR=...` can put both agents in one
    /// extensions directory that ai-memory's own environment never sees, so
    /// auto-wire must report the double capture itself.
    #[test]
    fn run_env_shared_pi_family_dir_is_reported() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let root = data.path().join("pi-family");
        let overrides = WireOverrides::default();
        for harness in [ManagedHarness::Pi, ManagedHarness::Omp] {
            let shared = |run_env: &[(String, String)]| {
                wire_targets(&config, harness, &overrides, run_env)
                    .unwrap()
                    .shared_extensions_dir
            };
            assert_eq!(
                shared(&env_pair("PI_CODING_AGENT_DIR", &root)),
                Some(root.join("extensions")),
                "{harness:?}"
            );
        }
        assert_eq!(
            wire_targets(
                &config,
                ManagedHarness::Claude,
                &overrides,
                &env_pair("PI_CODING_AGENT_DIR", &root)
            )
            .unwrap()
            .shared_extensions_dir,
            None
        );
    }

    /// The test guard plans only installs whose target is an explicit path
    /// under its root, so a test that drives `run` without injected paths
    /// cannot reach the real home, including through a relocation that
    /// resolves back to it.
    #[test]
    fn confined_installs_refuse_targets_outside_the_root() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let overrides = WireOverrides {
            confine_to: Some(data.path().to_path_buf()),
            ..WireOverrides::default()
        };
        let plan = |harness: ManagedHarness, run_env: &[(String, String)]| {
            let targets = wire_targets(&config, harness, &overrides, run_env).unwrap();
            wire_installs(&config, targets, &overrides)
        };
        let blank = [("CLAUDE_CONFIG_DIR".to_string(), "  ".to_string())];
        let profiled = [("OMP_PROFILE".to_string(), "work".to_string())];
        let escaping = env_pair(
            "CLAUDE_CONFIG_DIR",
            &data.path().join("..").join("elsewhere"),
        );
        for (harness, run_env) in [
            (ManagedHarness::Claude, &[][..]),
            (ManagedHarness::Claude, &blank[..]),
            (ManagedHarness::Omp, &profiled[..]),
            (ManagedHarness::Claude, &escaping[..]),
        ] {
            let refused = plan(harness, run_env);
            assert!(refused.hooks.is_empty(), "{harness:?} {run_env:?}");
            assert!(refused.mcp.is_none(), "{harness:?} {run_env:?}");
            assert!(
                refused.hooks_skipped.is_some() && refused.mcp_skipped.is_some(),
                "{harness:?} {run_env:?}"
            );
        }

        let root = data.path().join("claude-home");
        let pinned = plan(
            ManagedHarness::Claude,
            &env_pair("CLAUDE_CONFIG_DIR", &root),
        );
        assert_eq!(pinned.hooks.len(), 1);
        assert_eq!(
            pinned.hooks[0].config_file,
            Some(root.join("settings.json"))
        );
        assert_eq!(
            pinned.mcp.and_then(|args| args.config_file),
            Some(root.join(".claude.json"))
        );
        assert_eq!(pinned.hooks_skipped, None);
        assert_eq!(pinned.mcp_skipped, None);
    }

    /// A symlink under the root leads out of it, and a dangling one would be
    /// written through, so neither counts as inside.
    #[cfg(unix)]
    #[test]
    fn confined_installs_refuse_symlinks_out_of_the_root() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let overrides = WireOverrides {
            confine_to: Some(data.path().to_path_buf()),
            ..WireOverrides::default()
        };
        let linked = data.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        let dangling = data.path().join("dangling");
        std::os::unix::fs::symlink(outside.path().join("missing"), &dangling).unwrap();
        let inside = data.path().join("inside");
        std::fs::create_dir(&inside).unwrap();
        let looped = data.path().join("looped");
        std::os::unix::fs::symlink(&inside, &looped).unwrap();
        let plan = |dir: &Path| {
            let run_env = env_pair("CLAUDE_CONFIG_DIR", dir);
            let targets =
                wire_targets(&config, ManagedHarness::Claude, &overrides, &run_env).unwrap();
            wire_installs(&config, targets, &overrides)
        };
        let detour = data.path().join("missing").join("..").join("linked");
        for dir in [
            &linked,
            &dangling,
            &linked.join("..").join("elsewhere"),
            &detour,
        ] {
            let refused = plan(dir);
            assert!(refused.hooks.is_empty(), "{dir:?}");
            assert!(refused.mcp.is_none(), "{dir:?}");
        }
        let kept = plan(&looped);
        assert_eq!(kept.hooks.len(), 1);
        assert!(kept.mcp.is_some());
    }

    /// Without `--env` relocating anything, auto-wire leaves target selection to
    /// the installers, exactly as before `--env` reached it.
    #[test]
    fn without_a_relocating_run_env_the_installers_pick_their_own_targets() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let unrelated = [("SOME_PROVIDER_KEY".to_string(), "x".to_string())];
        for run_env in [&[][..], &unrelated[..]] {
            for harness in [ManagedHarness::Claude, ManagedHarness::Codex] {
                let installs = planned_installs(&config, harness, run_env);
                assert_eq!(installs.hooks.len(), 1);
                assert_eq!(installs.hooks[0].config_file, None, "{harness:?}");
                assert_eq!(
                    installs.mcp.and_then(|args| args.config_file),
                    None,
                    "{harness:?}"
                );
            }
        }
    }

    #[test]
    fn each_config_home_gets_its_own_sentinel() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let overrides = WireOverrides::default();
        let sentinel = |dir: &str| {
            let run_env = env_pair("CLAUDE_CONFIG_DIR", &data.path().join(dir));
            wire_targets(&config, ManagedHarness::Claude, &overrides, &run_env)
                .unwrap()
                .sentinel
        };
        assert_eq!(sentinel("work"), sentinel("work"));
        assert_ne!(
            sentinel("work"),
            sentinel("personal"),
            "a second account must not reuse the first account's sentinel"
        );
    }

    /// The multi-account sentinel bug: wiring one config home used to mark the
    /// agent as wired for every other one. Both paths are injected, so the real
    /// installers run without leaving the temp dirs; how `--env` picks those
    /// paths is covered by the plan tests above.
    #[test]
    fn a_second_config_home_is_wired_after_the_first() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let account = |name: &str| {
            let dir = data.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            WireOverrides {
                hooks_dir: Some(repo_hooks()),
                hooks_config_file: Some(dir.join("settings.json")),
                mcp_config_file: Some(dir.join(".claude.json")),
                ..WireOverrides::default()
            }
        };
        let work = account("work");
        let personal = account("personal");
        ensure_wired_with(&config, ManagedHarness::Claude, &work, &[]);
        ensure_wired_with(&config, ManagedHarness::Claude, &personal, &[]);
        for overrides in [&work, &personal] {
            let settings = overrides.hooks_config_file.as_ref().unwrap();
            let mcp = overrides.mcp_config_file.as_ref().unwrap();
            assert!(
                std::fs::read_to_string(settings)
                    .is_ok_and(|s| s.contains("ai-memory") || s.contains("ai_memory")),
                "hooks missing in {}",
                settings.display()
            );
            assert!(
                std::fs::read_to_string(mcp).is_ok_and(|s| s.contains("ai-memory")),
                "MCP missing in {}",
                mcp.display()
            );
        }
    }

    /// Kiro CLI v2 has no single hook file: a relocated home is expanded into one
    /// install per existing agent config, and an empty one is reported instead
    /// of silently wiring nothing.
    #[test]
    fn kiro_v2_relocated_by_run_env_wires_each_agent_config() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let kiro_home = data.path().join("kiro");
        let agents = kiro_home.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let run_env = env_pair("KIRO_HOME", &kiro_home);

        let empty = planned_installs(&config, ManagedHarness::Kiro, &run_env);
        assert!(empty.hooks.is_empty());
        assert!(
            empty
                .hooks_skipped
                .as_deref()
                .is_some_and(|reason| reason.contains("no Kiro CLI agent configs")),
            "an empty relocated agents dir must be reported: {:?}",
            empty.hooks_skipped
        );

        for name in ["default.json", "review.json"] {
            std::fs::write(agents.join(name), "{}").unwrap();
        }
        std::fs::write(agents.join("notes.txt"), "not an agent").unwrap();
        let installs = planned_installs(&config, ManagedHarness::Kiro, &run_env);
        let mut files: Vec<_> = installs
            .hooks
            .iter()
            .filter_map(|args| args.config_file.clone())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![agents.join("default.json"), agents.join("review.json")]
        );
        assert_eq!(installs.hooks_skipped, None);
        assert_eq!(
            installs.mcp.and_then(|args| args.config_file),
            Some(kiro_home.join("settings").join("mcp.json"))
        );
    }

    /// The autowire target table must agree with the default each installer
    /// resolves on its own, or the sentinel would key on a path nothing writes.
    #[test]
    fn every_auto_wired_agent_resolves_its_installer_default() {
        let process_env = |name: &str| std::env::var_os(name);
        for (agent, installer_default) in [
            (
                AgentChoice::ClaudeCode,
                install_hooks::claude_settings_path(),
            ),
            (AgentChoice::Codex, install_hooks::codex_hooks_path()),
            (AgentChoice::OpenCode, install_hooks::opencode_plugin_path()),
            (
                AgentChoice::OpenCode2,
                install_hooks::opencode2_plugin_path(),
            ),
            (AgentChoice::Pi, install_hooks::pi_extension_path()),
            (AgentChoice::Omp, install_hooks::omp_extension_path(None)),
            (
                AgentChoice::KimiCode,
                install_hooks::kimi_code_config_path(),
            ),
            (
                AgentChoice::CommandCode,
                install_hooks::command_code_settings_path(),
            ),
            (AgentChoice::KiroCli, install_hooks::kiro_cli_agents_dir()),
            (
                AgentChoice::KiroCliV3,
                install_hooks::kiro_cli_v3_hooks_path(),
            ),
            (AgentChoice::Grok, install_hooks::grok_hooks_path()),
            (
                AgentChoice::AntigravityCli,
                install_hooks::antigravity_hooks_path(),
            ),
        ] {
            assert_eq!(
                install_hooks::hook_config_target_with(agent, &process_env).ok(),
                installer_default.ok(),
                "{agent:?}"
            );
        }
        for harness in [
            ManagedHarness::Claude,
            ManagedHarness::Codex,
            ManagedHarness::OpenCode,
            ManagedHarness::OpenCode2,
            ManagedHarness::Pi,
            ManagedHarness::Omp,
            ManagedHarness::Kimi,
            ManagedHarness::CommandCode,
            ManagedHarness::Kiro,
            ManagedHarness::KiroV3,
            ManagedHarness::Grok,
            ManagedHarness::Antigravity,
        ] {
            let agent = agent_choice_for_harness(harness).unwrap();
            assert!(
                install_hooks::hook_config_target_with(agent, &|_| None).is_ok()
                    || crate::commands::path_util::home_dir().is_none(),
                "{harness:?} is auto-wired but has no hook target"
            );
        }
    }

    const SESSION_AWARE_BRIDGE: &str = r#"{
  "mcpServers": {
    "ai-memory": {
      "type": "stdio",
      "command": "ai-memory",
      "args": ["mcp-bridge", "--server-url", "http://127.0.0.1:49374/mcp"]
    }
  }
}"#;

    fn test_config(home: &Path, data_dir: &Path) -> Config {
        let mut config = Config::load(None, Some(home.to_path_buf())).unwrap();
        config.data_dir = data_dir.to_path_buf();
        config.run_autowire = true;
        config
    }

    fn repo_hooks() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks")
    }

    /// The load-bearing "won't mess up any harness" guarantee: auto-wire installs
    /// the harness's hooks + MCP while preserving unrelated user config, and a
    /// second launch is a clean no-op (no duplication, no churn). Paths are
    /// injected so the test never touches the developer's real `$HOME`.
    #[test]
    fn wiring_installs_and_preserves_user_config_then_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = data.path().join("claude-settings.json");
        std::fs::write(&settings, r#"{"existingUserKey":"keep me"}"#).unwrap();
        let mcp = data.path().join("claude.json");
        std::fs::write(&mcp, r#"{"existingMcpKey":"keep me too"}"#).unwrap();

        let config = test_config(home.path(), data.path());
        let overrides = WireOverrides {
            hooks_dir: Some(repo_hooks()),
            hooks_config_file: Some(settings.clone()),
            mcp_config_file: Some(mcp.clone()),
            ..WireOverrides::default()
        };
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides, &[]);

        let hooks_json = std::fs::read_to_string(&settings).unwrap();
        assert!(
            hooks_json.contains("existingUserKey"),
            "unrelated user settings must be preserved: {hooks_json}"
        );
        assert!(
            hooks_json.contains("ai-memory") || hooks_json.contains("ai_memory"),
            "the ai-memory hook must be installed: {hooks_json}"
        );
        let mcp_json = std::fs::read_to_string(&mcp).unwrap();
        assert!(
            mcp_json.contains("existingMcpKey"),
            "unrelated MCP config must be preserved: {mcp_json}"
        );
        assert!(
            mcp_json.contains("ai-memory"),
            "the ai-memory MCP server must be installed: {mcp_json}"
        );
        assert!(
            wire_targets(&config, ManagedHarness::Claude, &overrides, &[])
                .unwrap()
                .sentinel
                .exists(),
            "the attempt must be recorded"
        );

        // Second launch: the sentinel gates it, so the files are byte-identical.
        let before_hooks = std::fs::read(&settings).unwrap();
        let before_mcp = std::fs::read(&mcp).unwrap();
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides, &[]);
        assert_eq!(
            std::fs::read(&settings).unwrap(),
            before_hooks,
            "a gated re-launch must not rewrite hook config"
        );
        assert_eq!(
            std::fs::read(&mcp).unwrap(),
            before_mcp,
            "a gated re-launch must not rewrite MCP config"
        );
    }

    /// Auto-wire must not downgrade a deliberately-installed session-aware
    /// bridge to static HTTP. The sentinel is version-keyed, so this step
    /// re-runs on every upgrade; without the guard that re-run rewrites the
    /// Claude Code MCP entry wholesale, silently disabling the per_session
    /// isolation the user opted into with `install-mcp --session-aware`.
    #[test]
    fn wiring_preserves_an_existing_session_aware_bridge() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let settings = data.path().join("claude-settings.json");
        std::fs::write(&settings, "{}").unwrap();
        let mcp = data.path().join("claude.json");
        std::fs::write(&mcp, SESSION_AWARE_BRIDGE).unwrap();

        let config = test_config(home.path(), data.path());
        let overrides = WireOverrides {
            hooks_dir: Some(repo_hooks()),
            hooks_config_file: Some(settings.clone()),
            mcp_config_file: Some(mcp.clone()),
            ..WireOverrides::default()
        };
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides, &[]);

        let mcp_json = std::fs::read_to_string(&mcp).unwrap();
        let entry: serde_json::Value = serde_json::from_str(&mcp_json).unwrap();
        let server = &entry["mcpServers"]["ai-memory"];
        assert_eq!(
            server["type"].as_str(),
            Some("stdio"),
            "auto-wire must keep the session-aware bridge, not downgrade it to http: {mcp_json}"
        );
        assert!(
            server["args"]
                .as_array()
                .is_some_and(|args| args.iter().any(|arg| arg == "mcp-bridge")),
            "the preserved entry must still be the mcp-bridge: {mcp_json}"
        );
        // The hooks still install, and the attempt is still recorded, so a plain
        // re-launch stays gated.
        assert!(
            wire_targets(&config, ManagedHarness::Claude, &overrides, &[])
                .unwrap()
                .sentinel
                .exists(),
            "the attempt must be recorded even when the MCP bridge is preserved"
        );
    }

    /// Under `--env CLAUDE_CONFIG_DIR` the guard reads the relocated
    /// `.claude.json` this launch writes, not the default one, so a bridge
    /// installed for that account is kept too.
    #[test]
    fn wiring_preserves_a_session_aware_bridge_in_the_relocated_home() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = data.path().join("claude-home");
        std::fs::create_dir_all(&root).unwrap();
        let mcp = root.join(".claude.json");
        std::fs::write(&mcp, SESSION_AWARE_BRIDGE).unwrap();

        let config = test_config(home.path(), data.path());
        let overrides = WireOverrides {
            hooks_dir: Some(repo_hooks()),
            confine_to: Some(data.path().to_path_buf()),
            ..WireOverrides::default()
        };
        ensure_wired_with(
            &config,
            ManagedHarness::Claude,
            &overrides,
            &env_pair("CLAUDE_CONFIG_DIR", &root),
        );

        let mcp_json = std::fs::read_to_string(&mcp).unwrap();
        let entry: serde_json::Value = serde_json::from_str(&mcp_json).unwrap();
        assert_eq!(
            entry["mcpServers"]["ai-memory"]["type"].as_str(),
            Some("stdio"),
            "the relocated session-aware bridge must be kept: {mcp_json}"
        );
        assert!(
            std::fs::read_to_string(root.join("settings.json"))
                .is_ok_and(|s| s.contains("ai-memory") || s.contains("ai_memory")),
            "the hooks still install in the relocated home"
        );
    }

    /// A harness with no installer support (Crush) is skipped before anything is
    /// written — no config touched, no sentinel churn.
    #[test]
    fn unsupported_harness_wires_nothing() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        ensure_wired_with(
            &config,
            ManagedHarness::Crush,
            &WireOverrides::default(),
            &[],
        );
        assert!(
            !autowire_state_dir(data.path()).exists(),
            "an unsupported harness must not create autowire state"
        );
    }

    /// A pre-existing sentinel means the harness config is never touched, even if
    /// the wiring logic were otherwise reached.
    #[test]
    fn a_present_sentinel_leaves_config_untouched() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let settings = data.path().join("claude-settings.json");
        std::fs::write(&settings, r#"{"existingUserKey":1}"#).unwrap();
        let mcp = data.path().join("claude.json");
        std::fs::write(&mcp, r#"{"existingMcpKey":1}"#).unwrap();
        let overrides = WireOverrides {
            hooks_dir: Some(repo_hooks()),
            hooks_config_file: Some(settings.clone()),
            mcp_config_file: Some(mcp.clone()),
            ..WireOverrides::default()
        };
        let sentinel = wire_targets(&config, ManagedHarness::Claude, &overrides, &[])
            .unwrap()
            .sentinel;
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"").unwrap();

        ensure_wired_with(&config, ManagedHarness::Claude, &overrides, &[]);
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            r#"{"existingUserKey":1}"#,
            "a present sentinel must short-circuit before any install"
        );
        assert_eq!(
            std::fs::read_to_string(&mcp).unwrap(),
            r#"{"existingMcpKey":1}"#,
            "a present sentinel must short-circuit before the MCP install"
        );
    }

    /// Clearing the auto-wire state (what `uninstall` does) makes the next
    /// launch wire again; `uninstall` and the gate must agree on the directory.
    #[test]
    fn clearing_autowire_state_rewires_the_next_launch() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        let settings = data.path().join("claude-settings.json");
        let mcp = data.path().join("claude.json");
        let user_settings = r#"{"existingUserKey":1}"#;
        let user_mcp = r#"{"existingMcpKey":1}"#;
        std::fs::write(&settings, user_settings).unwrap();
        std::fs::write(&mcp, user_mcp).unwrap();
        let overrides = WireOverrides {
            hooks_dir: Some(repo_hooks()),
            hooks_config_file: Some(settings.clone()),
            mcp_config_file: Some(mcp.clone()),
            ..WireOverrides::default()
        };
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides, &[]);

        // The wiring is removed by hand; the sentinel alone keeps it that way.
        std::fs::write(&settings, user_settings).unwrap();
        std::fs::write(&mcp, user_mcp).unwrap();
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides, &[]);
        assert_eq!(std::fs::read_to_string(&mcp).unwrap(), user_mcp);

        // Moving the state aside is, for the gate, the same as clearing it.
        std::fs::rename(
            autowire_state_dir(data.path()),
            data.path().join("cleared-autowire-state"),
        )
        .unwrap();
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides, &[]);
        assert!(
            std::fs::read_to_string(&settings)
                .is_ok_and(|s| s.contains("ai-memory") || s.contains("ai_memory"))
        );
        assert!(std::fs::read_to_string(&mcp).is_ok_and(|s| s.contains("ai-memory")));
    }
}
