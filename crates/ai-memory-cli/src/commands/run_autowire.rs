//! Auto-install a harness's ai-memory hooks + MCP the first time it is launched
//! through `ai-memory run`.
//!
//! Managed launch is the recommended way to start a harness ("if in doubt, run
//! with ai-memory"), so it should be the path that makes capture *work* without
//! a separate manual `install-hooks` / `install-mcp` step. Motivating bug: a
//! user ran `ai-memory run kimi` and nothing was captured because the Kimi hooks
//! and MCP had never been installed.
//!
//! This runs at most once per (harness, binary version): a per-agent sentinel
//! under `<data_dir>/autowire-state/` keeps the run hot-path fast on every
//! subsequent launch, while keying on the version means an upgrade re-stages the
//! fresh hook bundle. Both installs are idempotent (they no-op when already up
//! to date), and the whole step is best-effort — a failure warns and the harness
//! still launches. Opt out with `run --no-autowire` or `AI_MEMORY_RUN_AUTOWIRE=false`.

use std::path::{Path, PathBuf};

use ai_memory_workstream::ManagedHarness;

use crate::cli::{AgentChoice, InstallHooksArgs, InstallMcpArgs};
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

/// Per-(agent, version) sentinel marking that auto-wire already ran for this
/// harness on this machine with this binary. Keyed by the agent's stable kebab
/// name plus the crate version so an upgrade re-wires the fresh hook bundle.
fn sentinel_path(data_dir: &Path, agent: AgentChoice) -> PathBuf {
    let name = format!("{}-{}", agent.kind().as_str(), env!("CARGO_PKG_VERSION"));
    data_dir.join("autowire-state").join(name)
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
}

/// Ensure the launched harness has ai-memory hooks + MCP installed. Best-effort
/// and one-time; never blocks or fails the launch.
///
/// Production launches pass [`WireOverrides::default()`] (via
/// [`run_from`](super::run::run_from)); the overrides exist only so the seam can
/// be exercised without writing to the developer's real `$HOME`.
pub(crate) fn ensure_wired_with(
    config: &Config,
    harness: ManagedHarness,
    overrides: &WireOverrides,
) {
    let Some(agent) = agent_choice_for_harness(harness) else {
        return;
    };
    let sentinel = sentinel_path(&config.data_dir, agent);
    if sentinel.exists() {
        return;
    }

    eprintln!(
        "ai-memory: first managed launch of {} here — wiring its ai-memory hooks + MCP so \
         capture and recall work (disable with --no-autowire or AI_MEMORY_RUN_AUTOWIRE=false).",
        harness.as_str()
    );

    let server_url = Some(config.server_url.clone());
    let auth_token = config.auth.bearer_token.clone();

    if let Err(error) = install_hooks::run(
        config,
        InstallHooksArgs {
            agent,
            hooks_dir: overrides.hooks_dir.clone(),
            server_url: server_url.clone(),
            auth_token: auth_token.clone(),
            as_user: None,
            apply: true,
            config_file: overrides.hooks_config_file.clone(),
            project_strategy: None,
            capture_assistant: false,
            capture_mode: None,
            no_capture_prompts: false,
            capture_prompts: false,
            profile: None,
        },
    ) {
        eprintln!(
            "ai-memory: could not auto-install {} hooks ({error:#}); continuing launch. \
             Wire them manually with `ai-memory install-hooks --agent {} --apply`.",
            harness.as_str(),
            agent.kind().as_str()
        );
    }

    // Not every hook-capable harness has an MCP client the installer can write
    // (e.g. Pi bridges MCP through its generated extension); skip those quietly.
    if let Some(client) = install_hooks::mcp_client_for_agent(agent)
        && let Err(error) = install_mcp::run(
            config,
            InstallMcpArgs {
                client,
                server_url,
                name: "ai-memory".to_string(),
                auth_token,
                apply: true,
                config_file: overrides.mcp_config_file.clone(),
                session_aware: false,
                flavor: None,
            },
        )
    {
        eprintln!(
            "ai-memory: could not auto-install the {} MCP server ({error:#}); continuing \
             launch. Wire it manually with `ai-memory install-mcp --client {} --apply`.",
            harness.as_str(),
            agent.kind().as_str()
        );
    }

    // Record the attempt even on partial failure: re-applying an idempotent
    // install on every launch would nag and churn config. A user who wants a
    // retry can re-run install-hooks manually or clear the sentinel.
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
    fn sentinel_is_agent_and_version_specific() {
        let dir = Path::new("/data");
        let claude = sentinel_path(dir, AgentChoice::ClaudeCode);
        let codex = sentinel_path(dir, AgentChoice::Codex);
        assert_ne!(claude, codex, "different agents get distinct sentinels");
        assert!(claude.starts_with("/data/autowire-state/"));
        assert!(
            claude
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(
                    |n| n.starts_with("claude-code-") && n.ends_with(env!("CARGO_PKG_VERSION"))
                ),
            "sentinel keys on agent + version: {claude:?}"
        );
    }

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
        };
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides);

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
            sentinel_path(&config.data_dir, AgentChoice::ClaudeCode).exists(),
            "the attempt must be recorded"
        );

        // Second launch: the sentinel gates it, so the files are byte-identical.
        let before_hooks = std::fs::read(&settings).unwrap();
        let before_mcp = std::fs::read(&mcp).unwrap();
        ensure_wired_with(&config, ManagedHarness::Claude, &overrides);
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

    /// A harness with no installer support (Crush) is skipped before anything is
    /// written — no config touched, no sentinel churn.
    #[test]
    fn unsupported_harness_wires_nothing() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let config = test_config(home.path(), data.path());
        ensure_wired_with(&config, ManagedHarness::Crush, &WireOverrides::default());
        assert!(
            !data.path().join("autowire-state").exists(),
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
        let sentinel = sentinel_path(&config.data_dir, AgentChoice::ClaudeCode);
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"").unwrap();

        let settings = data.path().join("claude-settings.json");
        std::fs::write(&settings, r#"{"existingUserKey":1}"#).unwrap();
        ensure_wired_with(
            &config,
            ManagedHarness::Claude,
            &WireOverrides {
                hooks_dir: Some(repo_hooks()),
                hooks_config_file: Some(settings.clone()),
                mcp_config_file: None,
            },
        );
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            r#"{"existingUserKey":1}"#,
            "a present sentinel must short-circuit before any install"
        );
    }
}
