//! `ai-memory install-hooks` — install lifecycle-hook configuration for
//! the chosen agent CLI.
//!
//! Two modes:
//!
//! - **Default (print):** renders the JSON/TOML/TypeScript snippet the
//!   user should merge into their agent CLI's settings file, plus the
//!   absolute paths to the vendored shell scripts. Nothing is written to
//!   disk.
//!
//! - **`--apply` (recommended):** performs an atomic in-place merge into
//!   the target config file. A timestamped backup (`.bak-<unix-ts>`) is
//!   written next to the file before any mutation. Re-runs are
//!   idempotent — a second `--apply` with unchanged content is a no-op
//!   and produces no backup.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{AgentChoice, CaptureModeArg, InstallHooksArgs, McpClient, ProjectStrategyArg};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json, mutate_toml};
use crate::commands::install_mcp;
use crate::commands::openclaw_plugin;
use crate::commands::path_util::home_dir;
use crate::commands::render_shared::{
    ANTIGRAVITY_LIFECYCLE_EVENTS, ANTIGRAVITY_TOOL_EVENTS, CODEX_PROFILE, COMMAND_CODE_PROFILE,
    CURSOR_PROFILE, GEMINI_PROFILE, KIMI_CODE_EVENTS, KIRO_CLI_V2_EVENTS, KIRO_CLI_V3_EVENTS,
    POOL_EVENTS, build_antigravity_payload_with_data_dir, build_claude_code_payload_with_data_dir,
    build_devin_payload_with_data_dir, build_grok_payload_with_data_dir,
    build_kiro_cli_v2_hooks_value, build_kiro_cli_v3_hooks_value,
    build_pool_settings_yaml_with_data_dir, build_profile_payload_for_agent,
    hook_script_for_claude_code, hook_script_for_current_platform, kimi_code_hook_commands,
    local_hook_policy_v1_supported, ts_capture_policy_v1, ts_string_literal,
};
use crate::commands::uninstall::hook_command_is_ours;
use crate::config::{Config, DEFAULT_SERVER_URL};

const CLAUDE_PROMPT_EVENT: &str = "UserPromptSubmit";

/// Claude Code's settings file — hooks live under `hooks`.
/// `$CLAUDE_CONFIG_DIR/settings.json` when the var is set, else
/// `~/.claude/settings.json`.
pub(crate) fn claude_settings_path() -> anyhow::Result<std::path::PathBuf> {
    claude_settings_path_in(std::env::var_os("CLAUDE_CONFIG_DIR"))
}

/// The env value comes in as a parameter so tests can exercise both
/// branches without mutating process env (mirrors
/// `install_mcp::kimi_code_home`).
fn claude_settings_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = crate::commands::path_util::claude_config_dir(env_override) {
        return Ok(dir.join("settings.json"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.claude/settings.json")?
        .join(".claude")
        .join("settings.json"))
}

/// Codex's hooks file — `$CODEX_HOME/hooks.json` when the var is set, else
/// `~/.codex/hooks.json`.
///
/// `CODEX_HOME` relocates Codex's whole config home, so hooks written to the
/// default path are simply never loaded by a Codex configured that way: capture
/// silently does nothing, with a successful-looking install. ai-memory already
/// honors the variable when it resolves that Codex's transcripts
/// (`ManagedHarness::Codex` in `ai-memory-workstream`), so ignoring it here left
/// one half of the same install pointing somewhere the other half did not.
pub(crate) fn codex_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    codex_hooks_path_in(std::env::var_os("CODEX_HOME"))
}

/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env (mirrors [`claude_settings_path_in`]).
fn codex_hooks_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = crate::commands::path_util::agent_config_home(env_override) {
        return Ok(dir.join("hooks.json"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.codex/hooks.json")?
        .join(".codex")
        .join("hooks.json"))
}

/// `~/.commandcode/settings.json`.
pub(crate) fn command_code_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.commandcode/settings.json")?
        .join(".commandcode")
        .join("settings.json"))
}

/// `~/.cursor/hooks.json`.
pub(crate) fn cursor_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.cursor/hooks.json")?
        .join(".cursor")
        .join("hooks.json"))
}

/// `~/.gemini/settings.json`.
pub(crate) fn gemini_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.gemini/settings.json")?
        .join(".gemini")
        .join("settings.json"))
}

/// `~/.gemini/config/hooks.json` — Antigravity CLI lifecycle hooks.
pub(crate) fn antigravity_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.gemini/config/hooks.json")?
        .join(".gemini")
        .join("config")
        .join("hooks.json"))
}

/// `~/.grok/hooks/ai-memory.json` — Grok Build CLI lifecycle hooks.
pub(crate) fn grok_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(install_mcp::grok_home()?
        .join("hooks")
        .join("ai-memory.json"))
}

/// `~/.config/zero/hooks.json` — Zero's user-level lifecycle hook config
/// (issue #156). Zero resolves it under `$XDG_CONFIG_HOME` falling back to
/// `~/.config`; like the OpenCode plugin path below we target the default
/// location and `--config-file` covers non-default XDG setups.
pub(crate) fn zero_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.config/zero/hooks.json")?
        .join(".config")
        .join("zero")
        .join("hooks.json"))
}

/// `~/.zcode/cli/config.json` — ZCode's user-scope config. The root
/// `hooks` block lives in the same file `install-mcp --client zcode`
/// registers MCP servers in; user scope runs headless with no trust
/// gate, while ZCode's workspace scopes (`.zcode/config.json`,
/// `zcode.json`) are trust-gated and off by default, so they are never
/// targeted (#512).
pub(crate) fn zcode_config_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.zcode/cli/config.json")?
        .join(".zcode")
        .join("cli")
        .join("config.json"))
}

/// `~/.devin/hooks.v1.json` — Devin CLI lifecycle hooks (default target).
pub(crate) fn devin_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.devin/hooks.v1.json")?
        .join(".devin")
        .join("hooks.v1.json"))
}

/// `~/.devin/config.json` — Devin CLI lifecycle hooks (alternative target under `hooks` key).
pub(crate) fn devin_config_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.devin/config.json")?
        .join(".devin")
        .join("config.json"))
}

/// `~/.config/opencode/plugins/ai-memory.ts` — OpenCode's plugin file.
pub(crate) fn opencode_plugin_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.config/opencode")?
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("ai-memory.ts"))
}

/// `$PI_CODING_AGENT_DIR/extensions/ai-memory.ts` when the var is set, else
/// `~/.omp/agent/extensions/ai-memory.ts` — OMP lifecycle extension.
///
/// `PI_CODING_AGENT_DIR` relocates OMP's whole agent config home
/// (`~/.omp/agent`), so extensions written to the default path are never
/// loaded by an OMP configured that way: capture silently does nothing, with
/// a successful-looking install. ai-memory already honors the variable when
/// it resolves OMP transcripts (`ManagedHarness::Omp` in
/// `ai-memory-workstream`), so ignoring it here left one half of the same
/// install pointing somewhere the other half did not.
pub(crate) fn omp_extension_path(profile: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    // Bind the OsString before borrowing from it: `var_os(..).as_ref()` in a
    // single expression drops the temporary at the end of the statement.
    let env_profile = std::env::var_os("OMP_PROFILE");
    let profile = profile.or_else(|| env_profile.as_ref().and_then(|s| s.to_str()));
    omp_extension_path_in(std::env::var_os("PI_CODING_AGENT_DIR"), profile)
}

/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env (mirrors [`codex_hooks_path_in`]).
fn omp_extension_path_in(
    env_override: Option<std::ffi::OsString>,
    profile: Option<&str>,
) -> anyhow::Result<std::path::PathBuf> {
    // Precedence: `PI_CODING_AGENT_DIR` wins over `--profile`.
    //
    // OMP documents each separately — the variable "can override the agent
    // directory", and `--profile <name>` moves it to
    // `~/.omp/profiles/<name>/agent` — but does not define what happens when
    // both are set. Rather than guess at a composition (an earlier revision
    // derived `$DIR/profiles/<name>/agent`, and re-rooted differently
    // depending on whether the path's last component happened to be named
    // "agent", so the result changed with the directory's *name*), this
    // takes the reading each is documented to have: the variable names the
    // agent directory outright, so when it is set that is the agent
    // directory. The profile then has nothing left to relocate, and the
    // caller says so instead of silently picking one.
    let base_dir = if let Some(dir) = crate::commands::path_util::agent_config_home(env_override) {
        dir
    } else {
        let root = home_dir()
            .context("could not locate $HOME for ~/.omp")?
            .join(".omp");
        if let Some(profile_name) = profile {
            root.join("profiles").join(profile_name).join("agent")
        } else {
            root.join("agent")
        }
    };
    Ok(base_dir.join("extensions").join("ai-memory-omp.ts"))
}

/// `$PI_CODING_AGENT_DIR/extensions/ai-memory-pi.ts` when the var is set, else
/// `~/.pi/agent/extensions/ai-memory-pi.ts` — Pi lifecycle + MCP bridge extension.
///
/// `PI_CODING_AGENT_DIR` relocates Pi's whole agent config home
/// (`~/.pi/agent`), so extensions written to the default path are never
/// loaded by a Pi configured that way: capture silently does nothing, with a
/// successful-looking install. ai-memory already honors the variable when it
/// resolves Pi transcripts (`ManagedHarness::Pi` in `ai-memory-workstream`),
/// so ignoring it here left one half of the same install pointing somewhere
/// the other half did not.
pub(crate) fn pi_extension_path() -> anyhow::Result<std::path::PathBuf> {
    pi_extension_path_in(std::env::var_os("PI_CODING_AGENT_DIR"))
}

/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env (mirrors [`codex_hooks_path_in`]).
fn pi_extension_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = crate::commands::path_util::agent_config_home(env_override) {
        return Ok(dir.join("extensions").join("ai-memory-pi.ts"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.pi/agent/extensions")?
        .join(".pi")
        .join("agent")
        .join("extensions")
        .join("ai-memory-pi.ts"))
}

/// `$KIMI_CODE_HOME/config.toml` when set, else `~/.kimi-code/config.toml`.
/// Kimi Code keeps hooks as `[[hooks]]` entries inside the same
/// config.toml that stores the user's providers/model, so the apply
/// path merges TOML-aware instead of rewriting the file.
pub(crate) fn kimi_code_config_path() -> anyhow::Result<std::path::PathBuf> {
    kimi_code_config_path_in(std::env::var_os("KIMI_CODE_HOME"))
}

/// The env value comes in as a parameter so tests can exercise both
/// branches without mutating process env (mirrors
/// `install_mcp::kimi_code_home`).
fn kimi_code_config_path_in(
    env_override: Option<std::ffi::OsString>,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = env_override.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir).join("config.toml"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.kimi-code/config.toml")?
        .join(".kimi-code")
        .join("config.toml"))
}

/// `$KIRO_HOME/agents` when set, else `~/.kiro/agents`. The Kiro CLI v2
/// engine embeds hooks inside per-agent config JSONs in this directory;
/// there is no global v2 hook surface, and the built-in default agent
/// has no file on disk. `KIRO_HOME` relocation is honored by the real
/// binary (verified against kiro-cli 2.16.0: settings are written under
/// `$KIRO_HOME` when set).
pub(crate) fn kiro_cli_agents_dir() -> anyhow::Result<std::path::PathBuf> {
    kiro_cli_home_join(std::env::var_os("KIRO_HOME"), "agents")
}

/// `$KIRO_HOME/hooks/ai-memory.json` when set, else
/// `~/.kiro/hooks/ai-memory.json`. Kiro v3 loads each standalone JSON file in
/// this directory; using one ai-memory-owned file avoids mutating unrelated
/// registrations.
pub(crate) fn kiro_cli_v3_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(kiro_cli_home_join(std::env::var_os("KIRO_HOME"), "hooks")?.join("ai-memory.json"))
}

/// The env value comes in as a parameter so tests can exercise both
/// branches without mutating process env (mirrors
/// `kimi_code_config_path_in`).
fn kiro_cli_home_join(
    env_override: Option<std::ffi::OsString>,
    child: &str,
) -> anyhow::Result<std::path::PathBuf> {
    if let Some(dir) = env_override.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir).join(child));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.kiro")?
        .join(".kiro")
        .join(child))
}

/// Run the `install-hooks` subcommand.
///
/// # Errors
/// Returns an error if the hook script directory cannot be located.
pub fn run(config: &Config, mut args: InstallHooksArgs) -> Result<()> {
    let inferred = if args.server_url.is_none() {
        infer_installed_mcp_config(args.agent)?
    } else {
        None
    };
    let server_url = effective_hook_server_url(config, &args, inferred.as_ref());
    let auth_token_owned = args
        .auth_token
        .clone()
        .or_else(|| config.auth.bearer_token.clone())
        .or_else(|| inferred.as_ref().and_then(|mcp| mcp.auth_token.clone()));
    // #552: persist the bearer under the data dir (0600) and keep it OUT of
    // the rendered config. Before this it went onto every hook's command line —
    // as `--auth-token <token>` for native hooks, as an `AI_MEMORY_AUTH_TOKEN=`
    // shell prefix for the script hooks — so it was readable through
    // `/proc/<pid>/cmdline` by any local user for as long as each hook ran, on
    // every tool call.
    //
    // Only `--apply` persists: a printed snippet is the operator's to place, and
    // writing a credential as a side effect of a dry run would be a surprise.
    let persisted = if args.apply
        && let Some(token) = auth_token_owned.as_deref()
    {
        crate::config::store_hook_auth_token(&config.data_dir, token).with_context(|| {
            format!(
                "storing the hook auth token under {}",
                config.data_dir.display()
            )
        })?;
        true
    } else {
        false
    };
    // Renderers take `Option<&str>` and omit the credential entirely when it is
    // `None`, so one decision here covers every agent instead of twenty edits.
    let auth = if persisted {
        None
    } else {
        auth_token_owned.as_deref()
    };
    // P1.8 multi-user attribution: `--as-user` is metadata only — the
    // token stamped into the hook env block is whatever the operator
    // passed via `--auth-token` (typically a native key from
    // `ai-memory api-key add`). We surface the username to stderr so the
    // operator can confirm which identity their writes will attribute
    // to. Mismatch between `--as-user` and the actual token's owner is
    // the operator's concern; we don't reach back to the server to
    // verify (keeps install-hooks offline-capable).
    validate_as_user(args.as_user.as_deref(), auth)?;
    if let Some(user) = args.as_user.as_deref().filter(|s| !s.trim().is_empty()) {
        eprintln!("[ai-memory] hooks installing for user: {user}");
    }
    let generated = matches!(
        args.agent,
        AgentChoice::OpenCode | AgentChoice::Omp | AgentChoice::Pi | AgentChoice::Openclaw
    );
    if generated || local_hook_policy_v1_supported() {
        eprintln!(
            "[ai-memory] capture-policy capability v1 enforced by this selected integration; re-run --apply to refresh existing installs."
        );
    } else {
        eprintln!(
            "[ai-memory] selected shell/PowerShell compatibility path does not enforce capture-policy v1; use a native platform selection or generated integration."
        );
    }
    // Assistant/Stop capture is Claude Code + native-platform only (#196). No
    // silent fallback: bail so an operator on a script-fallback platform or a
    // different agent is told the flag has no effect instead of installing a
    // command whose capture would be silently dropped.
    if args.capture_assistant && !capture_assistant_allowed(args.agent) {
        anyhow::bail!(
            "--capture-assistant requires --agent claude-code on a native hook platform \
             (PosixNative/WindowsNative). The current selection uses the script fallback or a \
             different agent, where the opt-in cannot take effect. Remove --capture-assistant or \
             switch to a native Claude Code install."
        );
    }
    if (args.no_capture_prompts || args.capture_prompts)
        && !prompt_capture_options_allowed(args.agent)
    {
        anyhow::bail!(
            "--no-capture-prompts and --capture-prompts require --agent claude-code. Other \
             agents may use their prompt hook to deliver handoff context, so removing it could \
             break cross-agent continuity."
        );
    }
    if args.apply {
        // #446: settle the capture failure mode before any agent-specific
        // work, and say which mode is in force. A protection the operator
        // cannot see is one they cannot trust.
        let capture_mode = persist_capture_mode(&config.data_dir, args.capture_mode)?;
        println!("capture mode: {capture_mode}");
        if capture_mode == "allowlist" {
            println!("  repositories without a .ai-memory.toml marker emit no lifecycle events");
            // The gate lives inside the native hook binary, immediately before
            // the spool write. A script install POSTs to the server directly
            // and never runs it, so the mode is stored but unenforced. Saying
            // nothing would leave an operator trusting a protection this
            // install does not have — the exact failure #446 is about.
            if !local_hook_policy_v1_supported() {
                println!(
                    "  WARNING: this install uses script hooks, which POST directly and \
                     cannot enforce the mode. Allowlist is stored but NOT in force here."
                );
            }
        }
        // Preserve a project-strategy an earlier `--apply` baked when this run
        // did not pass `--project-strategy`. Without this, a bare re-apply —
        // notably the auto-refresh in `ai-memory upgrade` — re-renders the hook
        // commands with no strategy and silently reverts a `repo-root` install
        // to `basename`. An explicit `--project-strategy` (including `basename`)
        // is honored as-is, so an intentional downgrade still works. Resolving
        // into `args` here means every downstream renderer picks it up with no
        // per-agent plumbing.
        args.project_strategy = install_project_strategy(&args);
        return match args.agent {
            AgentChoice::OpenCode => apply_to_opencode_plugin(&server_url, auth, &args),
            AgentChoice::Pi => apply_to_pi_extension(&server_url, auth, &args),
            AgentChoice::Omp => apply_to_omp_extension(&server_url, auth, &args),
            AgentChoice::ClaudeCode => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_claude_code_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Codex => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_codex_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::CommandCode => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_command_code_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Cursor => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_cursor_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::GeminiCli => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_gemini_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::AntigravityCli => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_antigravity_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Grok => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_grok_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Zero => apply_to_zero_hooks(&server_url, auth, &config.data_dir, &args),
            AgentChoice::Zcode => apply_to_zcode_hooks(&server_url, auth, &config.data_dir, &args),
            AgentChoice::Devin => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_devin_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Openclaw => openclaw_plugin::apply(&server_url, auth, &args),
            AgentChoice::KimiCode => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_kimi_code_config(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::KiroCli => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_kiro_cli_agent_configs(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::KiroCliV3 => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_kiro_cli_v3_hooks(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Pool => {
                let hooks_dir =
                    resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
                apply_to_pool(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
        };
    }
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    match args.agent {
        AgentChoice::OpenCode => render_opencode_plugin(&server_url, auth, strategy),
        AgentChoice::Pi => render_pi_extension(&server_url, auth, strategy),
        AgentChoice::Omp => {
            render_omp_extension(&server_url, auth, strategy, args.profile.as_deref())
        }
        AgentChoice::ClaudeCode => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            let settings_path = match &args.config_file {
                Some(p) => p.clone(),
                None => claude_settings_path()?,
            };
            render_claude_code(
                &hooks_dir,
                &server_url,
                auth,
                &config.data_dir,
                strategy,
                &settings_path,
                ClaudeCaptureScope {
                    assistant: args.capture_assistant,
                    prompts: install_claude_prompt_capture(&args),
                },
            )
        }
        AgentChoice::Codex => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "codex",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[CODEX_PROFILE.events],
            )
        }
        AgentChoice::CommandCode => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "command-code",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[COMMAND_CODE_PROFILE.events],
            )
        }
        AgentChoice::Cursor => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "cursor",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[CURSOR_PROFILE.events],
            )
        }
        AgentChoice::GeminiCli => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "gemini-cli",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[GEMINI_PROFILE.events],
            )
        }
        AgentChoice::AntigravityCli => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_agent(
                "antigravity-cli",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[&ANTIGRAVITY_TOOL_EVENTS, &ANTIGRAVITY_LIFECYCLE_EVENTS],
            )
        }
        AgentChoice::Grok => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_grok(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Zero => render_zero(&server_url, auth, &config.data_dir, strategy),
        AgentChoice::Zcode => render_zcode(&server_url, auth, &config.data_dir, strategy),
        AgentChoice::Devin => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_devin(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Openclaw => {
            openclaw_plugin::render(&server_url, auth, strategy);
            Ok(())
        }
        AgentChoice::KimiCode => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_kimi_code(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::KiroCli => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_kiro_cli(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::KiroCliV3 => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_kiro_cli_v3(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Pool => {
            let hooks_dir =
                resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent, &config.data_dir)?;
            render_pool(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
    }
}

#[derive(Debug, Clone, Default)]
struct InferredMcpConfig {
    hook_server_url: Option<String>,
    auth_token: Option<String>,
}

/// The project-strategy to install for `args`: the explicit `--project-strategy`
/// when one was given, otherwise the strategy an earlier `--apply` baked into
/// the agent's existing config (so a bare re-apply preserves it instead of
/// reverting to basename). `None` means "bake nothing", unchanged from before.
fn install_project_strategy(args: &InstallHooksArgs) -> Option<ProjectStrategyArg> {
    if args.project_strategy.is_some() {
        return args.project_strategy;
    }
    // Kiro v2 can update several agent configs in one run. Each config may
    // carry a different previously-baked strategy, so recover it per target in
    // `apply_to_kiro_cli_agent_configs` instead of choosing one globally.
    if args.agent == AgentChoice::KiroCli {
        return None;
    }
    existing_agent_config(args)
        .as_deref()
        .and_then(|existing| baked_project_strategy(args.agent, existing))
}

/// Settle and persist the capture failure mode (#446), returning the mode now
/// in force.
///
/// An explicit flag writes the file. Omitting the flag *reads* the stored
/// value rather than defaulting to it, so a bare `--apply` — including the
/// auto-refresh inside `ai-memory upgrade` — can never quietly downgrade an
/// existing opt-in back to capture-by-default.
fn persist_capture_mode(data_dir: &Path, requested: Option<CaptureModeArg>) -> Result<String> {
    let path = data_dir.join(crate::commands::hook::CAPTURE_MODE_FILE);
    let Some(requested) = requested else {
        return Ok(match fs::read_to_string(&path) {
            Ok(text) if text.trim().eq_ignore_ascii_case("allowlist") => "allowlist".to_string(),
            _ => "denylist".to_string(),
        });
    };
    let value = match requested {
        CaptureModeArg::Allowlist => "allowlist",
        CaptureModeArg::Denylist => "denylist",
    };
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    // Atomically, because every reader maps an unrecognised value onto
    // `denylist`: a torn write would not corrupt the opt-in, it would silently
    // revert it to capture-by-default (`CONTRIBUTING.md`, "Atomic file writes
    // only").
    ai_memory_wiki::write_atomic(&path, format!("{value}\n").as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(value.to_string())
}

/// Whether the Claude Code install should include its prompt-capture hook.
/// Explicit flags win. A bare apply preserves the state of an existing
/// ai-memory install so `upgrade` cannot silently restore a privacy opt-out.
fn install_claude_prompt_capture(args: &InstallHooksArgs) -> bool {
    if args.no_capture_prompts {
        return false;
    }
    if args.capture_prompts || !args.apply {
        return true;
    }
    existing_agent_config(args)
        .as_deref()
        .and_then(baked_claude_prompt_capture)
        .unwrap_or(true)
}

/// Recover prompt-capture state only from hook entries owned by ai-memory.
/// `None` means this is not an existing ai-memory Claude Code install.
fn baked_claude_prompt_capture(existing: &str) -> Option<bool> {
    let document: serde_json::Value = serde_json::from_str(existing).ok()?;
    let hooks = document.get("hooks")?.as_object()?;
    let has_ai_memory_hooks = hooks.values().any(|value| {
        value
            .as_array()
            .is_some_and(|entries| entries.iter().any(is_ai_memory_hook_entry))
    });
    if !has_ai_memory_hooks {
        return None;
    }
    Some(
        hooks
            .get(CLAUDE_PROMPT_EVENT)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| entries.iter().any(is_ai_memory_hook_entry)),
    )
}

/// Read the config file `--apply` will update for the selected agent.
fn existing_agent_config(args: &InstallHooksArgs) -> Option<String> {
    let path = if let Some(path) = &args.config_file {
        if args.agent == AgentChoice::Openclaw {
            path.join(openclaw_plugin::ENTRYPOINT_TS)
        } else {
            path.clone()
        }
    } else {
        match args.agent {
            AgentChoice::ClaudeCode => claude_settings_path().ok()?,
            AgentChoice::Codex => codex_hooks_path().ok()?,
            AgentChoice::CommandCode => command_code_settings_path().ok()?,
            AgentChoice::Cursor => cursor_hooks_path().ok()?,
            AgentChoice::GeminiCli => gemini_settings_path().ok()?,
            AgentChoice::OpenCode => opencode_plugin_path().ok()?,
            AgentChoice::Pi => pi_extension_path().ok()?,
            AgentChoice::Omp => omp_extension_path(args.profile.as_deref()).ok()?,
            AgentChoice::Openclaw => openclaw_plugin::default_plugin_dir()
                .ok()?
                .join(openclaw_plugin::ENTRYPOINT_TS),
            AgentChoice::AntigravityCli => antigravity_hooks_path().ok()?,
            AgentChoice::Grok => grok_hooks_path().ok()?,
            AgentChoice::Zero => zero_hooks_path().ok()?,
            AgentChoice::Zcode => zcode_config_path().ok()?,
            AgentChoice::Devin => devin_hooks_path().ok()?,
            AgentChoice::KimiCode => kimi_code_config_path().ok()?,
            AgentChoice::KiroCli => return None,
            // Pool's hook config is per-repo (`.poolside/settings.yaml`), not a
            // user-global file the installer could re-read a baked strategy from.
            AgentChoice::Pool => return None,
            AgentChoice::KiroCliV3 => kiro_cli_v3_hooks_path().ok()?,
        }
    };
    std::fs::read_to_string(path).ok()
}

/// Recover a strategy only from configuration entries ai-memory owns. Shared
/// JSON/TOML config files may contain unrelated hooks, while generated
/// TypeScript files carry an explicit ownership header.
fn baked_project_strategy(agent: AgentChoice, existing: &str) -> Option<ProjectStrategyArg> {
    match agent {
        AgentChoice::OpenCode | AgentChoice::Pi | AgentChoice::Omp | AgentChoice::Openclaw => {
            let marker = match agent {
                AgentChoice::OpenCode => "--agent opencode --apply`.",
                AgentChoice::Pi => "--agent pi --apply`.",
                AgentChoice::Omp => "--agent omp --apply`.",
                AgentChoice::Openclaw => "--agent openclaw --apply`.",
                _ => return None,
            };
            existing
                .lines()
                .next()
                .filter(|line| {
                    line.starts_with("// Auto-generated by `ai-memory install-hooks ")
                        && line.ends_with(marker)
                })
                .and_then(|_| project_strategy_from_text(existing))
        }
        AgentChoice::KimiCode => {
            let document: toml_edit::DocumentMut = existing.parse().ok()?;
            document
                .get("hooks")
                .and_then(toml_edit::Item::as_array_of_tables)
                .and_then(|hooks| {
                    hooks.iter().find_map(|entry| {
                        if !is_ai_memory_toml_hook_entry(entry) {
                            return None;
                        }
                        entry
                            .get("command")
                            .and_then(|item| item.as_str())
                            .and_then(project_strategy_from_text)
                    })
                })
        }
        AgentChoice::KiroCliV3 => {
            let root: serde_json::Value = serde_json::from_str(existing).ok()?;
            root.get("hooks")
                .and_then(serde_json::Value::as_array)
                .and_then(|hooks| {
                    hooks.iter().find_map(|entry| {
                        is_ai_memory_kiro_v3_hook_entry(entry)
                            .then(|| {
                                entry
                                    .pointer("/action/command")
                                    .and_then(serde_json::Value::as_str)
                                    .and_then(project_strategy_from_text)
                            })
                            .flatten()
                    })
                })
        }
        _ => serde_json::from_str(existing)
            .ok()
            .as_ref()
            .and_then(project_strategy_from_json),
    }
}

fn project_strategy_from_json(value: &serde_json::Value) -> Option<ProjectStrategyArg> {
    if is_ai_memory_hook_entry(value) {
        if let Some(strategy) = value
            .get("command")
            .and_then(|command| command.as_str())
            .and_then(project_strategy_from_text)
        {
            return Some(strategy);
        }
        if let Some(args) = value.get("args").and_then(|args| args.as_array()) {
            let args: Vec<&str> = args.iter().filter_map(|arg| arg.as_str()).collect();
            for (index, arg) in args.iter().enumerate() {
                if let Some(value) = arg.strip_prefix("--project-strategy=")
                    && is_repo_root_strategy(value)
                {
                    return Some(ProjectStrategyArg::RepoRoot);
                }
                if *arg == "--project-strategy"
                    && args
                        .get(index + 1)
                        .is_some_and(|value| is_repo_root_strategy(value))
                {
                    return Some(ProjectStrategyArg::RepoRoot);
                }
            }
        }
    }

    match value {
        serde_json::Value::Array(values) => values.iter().find_map(project_strategy_from_json),
        serde_json::Value::Object(values) => values.values().find_map(project_strategy_from_json),
        _ => None,
    }
}

/// Only `repo-root` is baked (`basename` removes the marker), so it is the sole
/// value recovered from legacy shell commands and generated source.
fn project_strategy_from_text(existing: &str) -> Option<ProjectStrategyArg> {
    for marker in [
        "AI_MEMORY_PROJECT_STRATEGY=",
        "--project-strategy=",
        "--project-strategy ",
        "const DEFAULT_PROJECT_STRATEGY =",
    ] {
        for rest in existing.split(marker).skip(1) {
            let token: String = rest
                .trim_start_matches(|character: char| {
                    character.is_ascii_whitespace() || matches!(character, '\'' | '"')
                })
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
                .collect();
            if is_repo_root_strategy(&token) {
                return Some(ProjectStrategyArg::RepoRoot);
            }
        }
    }
    None
}

fn is_repo_root_strategy(value: &str) -> bool {
    matches!(value, "repo-root" | "repo_root")
}

/// Reject `--as-user X` without a usable `--auth-token`. P1.8
/// metadata flag — without a token, the hook scripts would still
/// authenticate anonymously (or as root if the operator reused the
/// config bearer), so the `--as-user X` label would be misleading.
/// Trims whitespace; empty / whitespace-only `--as-user` is treated
/// as not-set so an accidental `--as-user ""` doesn't bail.
///
/// # Errors
/// Returns an error when `as_user` is set but `auth_token` is `None`
/// (or whitespace-only). The error message names the user so
/// operators see which arg they meant to pair with `--auth-token`.
fn validate_as_user(as_user: Option<&str>, auth_token: Option<&str>) -> Result<()> {
    let Some(user) = as_user.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if auth_token.map(str::trim).is_none_or(str::is_empty) {
        anyhow::bail!(
            "--as-user '{user}' requires --auth-token \
             (issue one with `ai-memory api-key add --username {user} --label <label>`)"
        );
    }
    Ok(())
}

fn effective_hook_server_url(
    config: &Config,
    args: &InstallHooksArgs,
    inferred: Option<&InferredMcpConfig>,
) -> String {
    let raw = if let Some(url) = &args.server_url {
        url.clone()
    } else if config.server_url_configured() {
        config.server_url.clone()
    } else if let Some(url) = inferred.and_then(|mcp| mcp.hook_server_url.clone()) {
        url
    } else {
        return DEFAULT_SERVER_URL.to_string();
    };
    apply_base_path_to_hook_url(&normalise_hook_server_url(&raw), &config.base_path)
}

fn normalise_hook_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Thread `Config::base_path` into the URL baked into hook commands so
/// the native hook subcommand (`ai-memory hook`) and the POSIX
/// `.sh`/`.ps1` scripts POST to `<origin><base>/hook` instead of
/// 404'ing under a reverse proxy.
///
/// Skip when the resolved URL already carries a path component — that
/// means the operator put the prefix into `AI_MEMORY_SERVER_URL`
/// directly (`http://host:49374/wiki`) and we'd double it otherwise.
fn apply_base_path_to_hook_url(url: &str, base_path: &str) -> String {
    let (origin, existing_path) = crate::http_client::split_origin_and_path(url);
    if !existing_path.is_empty() {
        return url.to_string();
    }
    let prefix = ai_memory_web::normalize_prefix(base_path);
    if prefix.is_empty() {
        origin
    } else {
        format!("{origin}{prefix}")
    }
}

fn infer_installed_mcp_config(agent: AgentChoice) -> Result<Option<InferredMcpConfig>> {
    if agent == AgentChoice::Grok {
        let cwd = std::env::current_dir()
            .context("could not resolve current dir for Grok project configuration")?;
        let repo_root = ai_memory_consolidate::discover_repo_root(&cwd).ok();
        if let Some(project_config) = find_grok_project_overlay(&cwd, repo_root.as_deref()) {
            anyhow::bail!(
                "Grok project configuration {} may override GROK_HOME; pass explicit --server-url and --auth-token rather than inferring MCP settings",
                project_config.display()
            );
        }
    }
    let Some(client) = mcp_client_for_agent(agent) else {
        return Ok(None);
    };
    let path = install_mcp::mcp_config_path(client)?;
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(None);
    };
    match client {
        McpClient::ClaudeCode => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        // Codex uses `http_headers`; Grok uses `headers`. The shared TOML
        // inferencer accepts both.
        McpClient::Codex => Ok(infer_toml_mcp_config(&content)),
        McpClient::CommandCode => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        McpClient::Grok => infer_grok_mcp_config(&content),
        McpClient::OpenCode => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "ai-memory"],
            "url",
        )),
        McpClient::Cursor => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        McpClient::GeminiCli => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "httpUrl",
        )),
        McpClient::Openclaw => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "ai-memory"],
            "url",
        )),
        McpClient::Omp => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        McpClient::Zero => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "ai-memory"],
            "url",
        )),
        McpClient::Zcode => Ok(infer_json_mcp_config(
            &content,
            &["mcp", "servers", "ai-memory"],
            "url",
        )),
        McpClient::Pi => Ok(None),
        McpClient::AntigravityCli => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "serverUrl",
        )),
        McpClient::Devin => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        McpClient::KimiCode | McpClient::KiroCli => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        McpClient::ClaudeDesktop => Ok(None),
        // MCP-only client: no AgentChoice counterpart routes here.
        McpClient::Swival => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        // MCP-only client: no AgentChoice counterpart routes here.
        // Reachable only if a future install_hooks flow targets VS
        // Code Copilot directly.
        McpClient::VsCodeCopilot => Ok(infer_json_mcp_config(
            &content,
            &["servers", "ai-memory"],
            "url",
        )),
        // MCP-only client: no AgentChoice counterpart routes here.
        McpClient::Zed => Ok(infer_json_mcp_config(
            &content,
            &["context_servers", "ai-memory"],
            "url",
        )),
    }
}

/// Grok loads project settings from every directory between the repository
/// root and CWD. Outside a repository, only the active directory is relevant.
fn grok_project_overlay_paths(cwd: &Path, repo_root: Option<&Path>) -> Vec<PathBuf> {
    let start = repo_root
        .filter(|root| cwd.starts_with(root))
        .unwrap_or(cwd);
    let mut directories = Vec::new();
    let mut current = cwd;
    loop {
        directories.push(current.to_path_buf());
        if current == start {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    directories.reverse();
    directories
        .into_iter()
        .map(|directory| directory.join(".grok").join("config.toml"))
        .collect()
}

fn find_grok_project_overlay(cwd: &Path, repo_root: Option<&Path>) -> Option<PathBuf> {
    grok_project_overlay_paths(cwd, repo_root)
        .into_iter()
        .find(|path| path.exists())
}

fn mcp_client_for_agent(agent: AgentChoice) -> Option<McpClient> {
    match agent {
        AgentChoice::ClaudeCode => Some(McpClient::ClaudeCode),
        AgentChoice::Codex => Some(McpClient::Codex),
        AgentChoice::CommandCode => Some(McpClient::CommandCode),
        AgentChoice::Cursor => Some(McpClient::Cursor),
        AgentChoice::GeminiCli => Some(McpClient::GeminiCli),
        AgentChoice::OpenCode => Some(McpClient::OpenCode),
        AgentChoice::Omp => Some(McpClient::Omp),
        AgentChoice::Openclaw => Some(McpClient::Openclaw),
        AgentChoice::AntigravityCli => Some(McpClient::AntigravityCli),
        AgentChoice::Zero => Some(McpClient::Zero),
        AgentChoice::Grok => Some(McpClient::Grok),
        AgentChoice::Devin => Some(McpClient::Devin),
        AgentChoice::KimiCode => Some(McpClient::KimiCode),
        // Pi bridges MCP through its generated extension, not a native
        // mcp.json the installer can scrape.
        AgentChoice::Pi => None,
        AgentChoice::KiroCli | AgentChoice::KiroCliV3 => Some(McpClient::KiroCli),
        // No first-party Pool MCP installer ships yet, so there is no
        // config file to infer a server URL or token from.
        AgentChoice::Pool => None,
        // The ZCode MCP client ships with #511; until that lands there is
        // no `McpClient::Zcode` whose config the installer could scrape a
        // server URL or token from.
        AgentChoice::Zcode => None,
    }
}

fn infer_json_mcp_config(
    content: &str,
    entry_path: &[&str],
    url_key: &str,
) -> Option<InferredMcpConfig> {
    let root: serde_json::Value = serde_json::from_str(content).ok()?;
    let mut entry = &root;
    for key in entry_path {
        entry = entry.get(*key)?;
    }
    let hook_server_url = entry
        .get(url_key)
        .and_then(|v| v.as_str())
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = entry
        .get("headers")
        .and_then(|headers| headers.get("Authorization"))
        .and_then(|v| v.as_str())
        .and_then(bearer_token_from_header);
    Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    })
}

/// Infer hook server URL + bearer from a TOML `[mcp_servers.ai-memory]`
/// entry (Codex `http_headers` or Grok `headers`).
fn infer_toml_mcp_config(content: &str) -> Option<InferredMcpConfig> {
    let doc: toml_edit::DocumentMut = content.parse().ok()?;
    // `toml_edit::Item`'s `Index` impl panics on missing keys, so this
    // walks the table chain with `.get()` instead. A user with
    // `[mcp_servers.context7]` but no `[mcp_servers.ai-memory]` is a
    // perfectly valid hooks-only Codex/Grok setup (issue #53) — return None
    // rather than abort the whole install with a stack trace.
    let server = doc.get("mcp_servers")?.get("ai-memory")?;

    let hook_server_url = server
        .get("url")
        .and_then(|v| v.as_str())
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = server
        .get("http_headers")
        .and_then(|h| h.get("Authorization"))
        .or_else(|| server.get("headers").and_then(|h| h.get("Authorization")))
        .and_then(|v| v.as_str())
        .and_then(bearer_token_from_header);
    if hook_server_url.is_none() && auth_token.is_none() {
        return None;
    }
    Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    })
}

/// Infer Grok's TOML entry after resolving its supported `${VAR}` and
/// `${VAR:-default}` placeholders. Refuse missing variables so generated hook
/// commands never contain a literal placeholder.
fn infer_grok_mcp_config(content: &str) -> Result<Option<InferredMcpConfig>> {
    let doc: toml_edit::DocumentMut = match content.parse() {
        Ok(doc) => doc,
        Err(_) => return Ok(None),
    };
    let Some(server) = doc
        .get("mcp_servers")
        .and_then(|servers| servers.get("ai-memory"))
    else {
        return Ok(None);
    };
    let hook_server_url = server
        .get("url")
        .and_then(|value| value.as_str())
        .map(expand_grok_placeholders)
        .transpose()?
        .as_deref()
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = server
        .get("headers")
        .and_then(|headers| headers.get("Authorization"))
        .and_then(|value| value.as_str())
        .map(expand_grok_placeholders)
        .transpose()?
        .as_deref()
        .and_then(bearer_token_from_header);
    if hook_server_url.is_none() && auth_token.is_none() {
        return Ok(None);
    }
    Ok(Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    }))
}

fn expand_grok_placeholders(value: &str) -> Result<String> {
    expand_grok_placeholders_with(value, |name| std::env::var(name).ok())
}

fn expand_grok_placeholders_with(
    value: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let after_start = &remaining[start + 2..];
        let end = after_start
            .find('}')
            .context("unterminated Grok ${...} placeholder")?;
        let expression = &after_start[..end];
        let (name, default) = expression
            .split_once(":-")
            .map_or((expression, None), |(name, default)| (name, Some(default)));
        if name.is_empty()
            || !name.chars().enumerate().all(|(index, ch)| {
                ch == '_' || ch.is_ascii_alphanumeric() && (index > 0 || ch.is_ascii_alphabetic())
            })
        {
            anyhow::bail!("invalid Grok environment placeholder `${{{expression}}}`");
        }
        let replacement = lookup(name)
            .filter(|value| !value.is_empty())
            .or_else(|| default.map(ToOwned::to_owned))
            .with_context(|| format!("Grok MCP placeholder `${{{name}}}` is unset; pass explicit --server-url and --auth-token"))?;
        output.push_str(&replacement);
        remaining = &after_start[end + 1..];
    }
    output.push_str(remaining);
    Ok(output)
}

fn hook_server_url_from_mcp_url(url: &str) -> Option<String> {
    // Drop query/fragment BEFORE the `/mcp` peel: Kimi Code's
    // `?flavor=moonshot` marker must never leak into hook URLs (and it
    // would stop the suffix from matching).
    let base = url
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    Some(base.strip_suffix("/mcp").unwrap_or(base).to_string())
}

fn bearer_token_from_header(header: &str) -> Option<String> {
    header
        .trim()
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// True if a hook-array entry belongs to ai-memory — i.e. some command handler
/// inside it is one of our legacy command strings or one of our exec-form native
/// hook handlers. Used to replace our own entries on re-apply while preserving
/// hooks that other tools registered under the same event.
fn is_ai_memory_hook_entry(entry: &serde_json::Value) -> bool {
    fn mentions_ai_memory(value: &serde_json::Value) -> bool {
        let Some(command) = value.get("command").and_then(|c| c.as_str()) else {
            return false;
        };
        let lower = command.to_ascii_lowercase();
        let args = value.get("args").and_then(|a| a.as_array());
        let Some(args) = args else {
            return hook_command_is_ours(command);
        };
        let tokens: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
        // Exec form: require both an ai-memory-ish executable and our hook argv
        // signature so unrelated helpers such as `ai-memory-helper.exe` are not
        // removed just because their executable name contains ai-memory.
        (lower.contains("ai-memory") || lower.contains("ai_memory"))
            && tokens.contains(&"hook")
            && tokens.contains(&"--event")
            && tokens.contains(&"--agent")
            && tokens.contains(&"--server-url")
    }
    // Flat shape (Cursor): `{ "type":"command", "command":"…" }`.
    // Nested shape (Claude Code / Codex / Gemini):
    // `{ "matcher":"", "hooks":[ {"command":"…"} ] }`.
    mentions_ai_memory(entry)
        || entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|inner| inner.iter().any(mentions_ai_memory))
}

/// Overlay our hook entries for one event onto the user's existing array
/// for that event: drop any prior ai-memory entries (so re-running
/// `install-hooks` never duplicates them) and append ours, while keeping
/// every third-party hook registered under the same event. Replaces a
/// blind `map.insert(event, value)`, which discarded co-located hooks
/// from other tools (e.g. a context-mode SessionStart hook).
fn overlay_event_hooks(
    map: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    our_value: &serde_json::Value,
) {
    let mut entries: Vec<serde_json::Value> = map
        .get(event)
        .and_then(|v| v.as_array())
        .map(|existing| {
            existing
                .iter()
                .filter(|e| !is_ai_memory_hook_entry(e))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(ours) = our_value.as_array() {
        entries.extend(ours.iter().cloned());
    }
    map.insert(event.to_string(), serde_json::Value::Array(entries));
}

/// Remove only ai-memory's entries for one event. Delete the event key when
/// no third-party hooks remain so a rendered opt-out stays minimal.
fn remove_ai_memory_event_hooks(map: &mut serde_json::Map<String, serde_json::Value>, event: &str) {
    overlay_event_hooks(map, event, &serde_json::Value::Array(Vec::new()));
    if map
        .get(event)
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        map.remove(event);
    }
}

fn overlay_kiro_cli_event_hooks(
    map: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    our_value: &serde_json::Value,
) {
    let mut entries: Vec<serde_json::Value> = map
        .get(event)
        .and_then(|value| value.as_array())
        .map(|existing| {
            existing
                .iter()
                .filter(|entry| {
                    !entry
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(is_ai_memory_kiro_hook_command)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Some(ours) = our_value.as_array() {
        entries.extend(ours.iter().cloned());
    }
    map.insert(event.to_string(), serde_json::Value::Array(entries));
}

/// Mutate `~/.claude/settings.json` in place: replace the hook entries
/// ai-memory cares about (`CLAUDE_CODE_EVENTS`); preserve every other hook the
/// user has wired up to other tools.
/// Whether `--capture-assistant` may take effect for this agent + platform
/// (#196): Claude Code on a native hook platform only. Any other agent or a
/// script-fallback platform cannot honor the opt-in, so the installer bails
/// instead of enabling it silently.
fn capture_assistant_allowed(agent: AgentChoice) -> bool {
    matches!(agent, AgentChoice::ClaudeCode) && local_hook_policy_v1_supported()
}

fn prompt_capture_options_allowed(agent: AgentChoice) -> bool {
    agent == AgentChoice::ClaudeCode
}

fn configure_claude_prompt_capture(
    mut payload: serde_json::Value,
    capture_prompts: bool,
) -> serde_json::Value {
    if !capture_prompts
        && let Some(hooks) = payload
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
    {
        hooks.remove(CLAUDE_PROMPT_EVENT);
    }
    payload
}

fn apply_to_claude_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "claude-code", data_dir)?;
    apply_to_claude_code_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_claude_code_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "claude-code", staging_data_local)?;
    let command_dir = staged_command_dir(&staged, "claude-code");
    let capture_prompts = install_claude_prompt_capture(args);
    let payload = configure_claude_prompt_capture(
        crate::commands::render_shared::build_claude_code_script_payload_for_test(
            &command_dir,
            server_url,
            auth_token,
            Some(data_dir),
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            args.capture_assistant,
        ),
        capture_prompts,
    );
    apply_to_claude_code_settings_with_payload(payload, args, capture_prompts)
}

fn apply_to_claude_code_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let command_dir = staged_command_dir(staged, "claude-code");
    let capture_prompts = install_claude_prompt_capture(args);
    let payload = configure_claude_prompt_capture(
        build_claude_code_payload_with_data_dir(
            &command_dir,
            server_url,
            auth_token,
            Some(data_dir),
            args.project_strategy.and_then(ProjectStrategyArg::baked),
            args.capture_assistant,
        ),
        capture_prompts,
    );
    apply_to_claude_code_settings_with_payload(payload, args, capture_prompts)
}

fn apply_to_claude_code_settings_with_payload(
    payload: serde_json::Value,
    args: &InstallHooksArgs,
    capture_prompts: bool,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => claude_settings_path()?,
    };
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_claude_code_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // Get-or-create the top-level `hooks` table, then merge our
            // event keys in via `overlay_event_hooks`: our entries replace
            // any prior ai-memory entries, while hooks the user (or another
            // tool) wired under the same event — or under a non-overlapping
            // event name (e.g. a hand-written "Notification" hook) — survive.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            if !capture_prompts {
                remove_ai_memory_event_hooks(hooks, CLAUDE_PROMPT_EVENT);
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate `~/.grok/hooks/ai-memory.json` so Grok Build CLI fires the ai-memory
/// lifecycle hooks. Grok's hook config is structurally identical to Claude
/// Code's nested hook JSON and uses the same CamelCase event names, but
/// its script bundle carries `agent=grok` and skips destructive SessionStart
/// handoff fetches. We merge into a dedicated `ai-memory.json` (Grok discovers
/// every `~/.grok/hooks/*.json`), so a pre-existing third-party hook file is
/// left untouched.
fn apply_to_grok_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => grok_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "grok", data_dir)?;
    let command_dir = staged_command_dir(&staged, "grok");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = build_grok_payload_with_data_dir(
        &command_dir,
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_grok_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in ai-memory.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate Devin's hook config (either `~/.devin/hooks.v1.json` or `~/.devin/config.json` hooks key).
/// Devin uses the same nested hook JSON shape as Claude Code/Grok, but with DEVIN_EVENTS
/// (PostCompaction instead of PreCompact, no subagent events). SessionStart injects
/// the handoff via hookSpecificOutput.additionalContext.
fn apply_to_devin_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "devin", data_dir)?;
    apply_to_devin_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_devin_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "devin", staging_data_local)?;
    apply_to_devin_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

fn apply_to_devin_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => devin_hooks_path()?,
    };
    let command_dir = staged_command_dir(staged, "devin");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = build_devin_payload_with_data_dir(
        &command_dir,
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_devin_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        // For hooks.v1.json, the entire file IS the hooks object
        // For config.json hooks key, we merge into the hooks object
        if path.file_name() == Some("hooks.v1.json".as_ref()) {
            mutate_json(existing, |root| {
                for (event, value) in &our_hooks {
                    overlay_event_hooks(root, event, value);
                }
                Ok(())
            })
        } else {
            mutate_json(existing, |root| {
                let hooks = root
                    .entry("hooks")
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                    .context("`hooks` is present in config.json but not an object")?;
                for (event, value) in &our_hooks {
                    overlay_event_hooks(hooks, event, value);
                }
                Ok(())
            })
        }
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate `~/.commandcode/settings.json` so Command Code fires the stable
/// four-event lifecycle integration. Command Code's schema requires the outer
/// `matcher` to be omitted for SessionStart and Stop, so its profile must not
/// reuse the otherwise similar Claude/Codex payload byte-for-byte.
fn apply_to_command_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "command-code", data_dir)?;
    apply_to_command_code_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_command_code_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "command-code", staging_data_local)?;
    let command_dir = staged_command_dir(&staged, "command-code");
    let payload = crate::commands::render_shared::build_profile_script_payload_for_test(
        &COMMAND_CODE_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "command-code",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_command_code_settings_with_payload(payload, args)
}

fn apply_to_command_code_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let command_dir = staged_command_dir(staged, "command-code");
    let payload = build_profile_payload_for_agent(
        &COMMAND_CODE_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "command-code",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_command_code_settings_with_payload(payload, args)
}

fn apply_to_command_code_settings_with_payload(
    payload: serde_json::Value,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(path) => path.clone(),
        None => command_code_settings_path()?,
    };
    let our_hooks = payload
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .context("internal: Command Code payload did not return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate `~/.codex/hooks.json` (creating it if absent) so Codex's
/// lifecycle hook runner fires the ai-memory scripts on every
/// session/prompt/tool event.
///
/// Codex's hook config is structurally identical to Claude Code's
/// (verified against `openai/codex/codex-rs/config/src/hooks_tests.rs`):
///
///   { "hooks": {
///       "SessionStart": [
///         { "matcher": "",
///           "hooks": [ {"type":"command", "command":"..."} ]
///         }
///       ], ...
///   } }
///
/// Codex looks for hooks in `~/.codex/hooks.json` by default (or
/// wherever `hooks = "./relative-path.json"` in config.toml points).
/// We write the standalone file and don't touch config.toml — Codex
/// picks it up automatically.
///
/// Trust note: Codex refuses to RUN new hooks until the user accepts
/// them in the TUI ("Trust all and continue") or sets
/// `--dangerously-bypass-hook-trust`. We print a reminder.
fn apply_to_codex_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "codex", data_dir)?;
    apply_to_codex_settings_with_staged(&staged, server_url, auth_token, data_dir, args)
}

#[cfg(test)]
fn apply_to_codex_settings_in(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    staging_data_local: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts_in(hooks_dir, "codex", staging_data_local)?;
    let command_dir = staged_command_dir(&staged, "codex");
    let payload = crate::commands::render_shared::build_profile_script_payload_for_test(
        &super::render_shared::CODEX_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "codex",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_codex_settings_with_payload(payload, args)
}

fn apply_to_codex_settings_with_staged(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let command_dir = staged_command_dir(staged, "codex");
    let payload = build_profile_payload_for_agent(
        &super::render_shared::CODEX_PROFILE,
        &command_dir,
        server_url,
        auth_token,
        "codex",
        Some(data_dir),
        args.project_strategy.and_then(ProjectStrategyArg::baked),
    );
    apply_to_codex_settings_with_payload(payload, args)
}

fn apply_to_codex_settings_with_payload(
    payload: serde_json::Value,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => codex_hooks_path()?,
    };
    let outcome = merge_codex_payload(payload, &path)?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    // First-time trust reminder. Codex's TUI flags new/changed
    // hooks on startup; users must explicitly trust them before
    // they fire.
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("Codex requires explicit trust for new hooks. Next time you start `codex`:");
        println!("  → the TUI will surface 'Hooks need review' for each new event");
        println!("  → choose 'Trust all and continue' (or trust individually)");
        println!("To bypass the prompt for automated installs, start with");
        println!("`codex --dangerously-bypass-hook-trust` (review hook scripts first).");
    }
    Ok(())
}

#[cfg(test)]
fn merge_codex_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    // Build the Codex-flavoured payload. The JSON shape is identical
    // to Claude Code's matcher + nested hooks form — only the event
    // list differs (no `SessionEnd`, which Codex doesn't recognise).
    let payload = build_profile_payload_for_agent(
        &super::render_shared::CODEX_PROFILE,
        staged,
        server_url,
        auth_token,
        "codex",
        Some(data_dir),
        project_strategy,
    );
    merge_codex_payload(payload, config_path)
}

fn merge_codex_payload(payload: serde_json::Value, config_path: &Path) -> Result<ApplyOutcome> {
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: payload builder didn't return a hooks object")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in hooks.json but not an object")?;
            // Remove any stale `SessionEnd` entry left behind by an
            // earlier version of install-hooks that mistakenly wrote
            // the Claude-Code-only event into Codex's file. Codex
            // ignores unknown events but the file looks cleaner
            // without dead keys.
            hooks.remove("SessionEnd");
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate `~/.cursor/hooks.json` (creating it if absent) so Cursor's
/// agent fires the ai-memory scripts on lifecycle events.
///
/// Cursor's hook schema (per <https://cursor.com/docs/agent/hooks>) is
/// *flatter* than Claude Code's / Codex's:
///
///   { "version": 1,
///     "hooks": {
///       "sessionStart": [
///         { "type": "command", "command": "...", "matcher": "" }
///       ]
///     }
///   }
///
/// — no inner `hooks: [...]` array, camelCase event names, plus a
/// required top-level `version: 1` key. We use `CURSOR_PROFILE`
/// (HookShape::Flat) to produce the right payload, then merge into
/// the existing file (preserving any non-overlapping events the
/// user has wired up to other tools).
fn apply_to_cursor_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => cursor_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "cursor", data_dir)?;
    let command_dir = staged_command_dir(&staged, "cursor");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_cursor_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_cursor_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_profile_payload_for_agent(
        &CURSOR_PROFILE,
        staged,
        server_url,
        auth_token,
        "cursor",
        Some(data_dir),
        project_strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: payload builder didn't return a hooks object")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            // Cursor requires "version": 1 at the top level.
            // Overwrite unconditionally — the schema is versioned
            // so future Cursor releases can bump this; we'll bump
            // here too when that happens.
            root.insert("version".into(), serde_json::json!(1));
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in hooks.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate `~/.gemini/settings.json` so Gemini CLI fires the ai-memory
/// scripts on its (Gemini-specific) lifecycle events.
///
/// Gemini's schema (per <https://geminicli.com/docs/hooks/reference>)
/// is the same nested shape as Claude Code's (`matcher` +
/// `hooks: [{type, command}]`), but the event vocabulary differs:
///
///   - `BeforeTool` / `AfterTool`  (ai-memory: `pre-tool-use` / `post-tool-use`)
///   - `PreCompress`               (ai-memory: `pre-compact`)
///   - `SessionStart` / `SessionEnd` line up with Claude Code's
///   - No `UserPromptSubmit` / `Stop` equivalents — skipped
///
/// Like Claude Code, Gemini doesn't honour an `env` field at the
/// inner-hook level, so the env vars get inlined into the command
/// string by the shared payload builder.
fn apply_to_gemini_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => gemini_settings_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "gemini-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "gemini-cli");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_gemini_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_gemini_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_profile_payload_for_agent(
        &GEMINI_PROFILE,
        staged,
        server_url,
        auth_token,
        "gemini-cli",
        Some(data_dir),
        project_strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: payload builder didn't return a hooks object")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            // Gemini's settings.json mixes MCP servers, hooks, and
            // other config under one document. Get-or-create the
            // `hooks` table; overlay our events; preserve siblings.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                overlay_event_hooks(hooks, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate `~/.gemini/config/hooks.json` so Antigravity CLI (`agy`)
/// fires the ai-memory scripts on its lifecycle events.
///
/// Antigravity CLI uses a named-groups format where hook groups are
/// top-level keys (e.g. `"ai-memory"`) containing event arrays. Tool
/// events (`PreToolUse`, `PostToolUse`) use nested shape with matcher;
/// lifecycle events (`PreInvocation`, `Stop`) use flat shape.
///
/// Config file: `~/.gemini/config/hooks.json`
fn apply_to_antigravity_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => antigravity_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "antigravity-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "antigravity-cli");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_antigravity_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    println!();
    print!("{ANTIGRAVITY_FINALIZATION_GUIDANCE}");
    Ok(())
}

const ANTIGRAVITY_FINALIZATION_GUIDANCE: &str = "\
Antigravity `Stop` ends one execution loop, not the conversation. After the\n\
final turn, run `ai-memory finalize-session --agent antigravity-cli` to create\n\
the final summary and handoff and to queue opt-in SessionEnd consolidation.\n";

fn merge_antigravity_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_antigravity_payload_with_data_dir(
        staged,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let our_group = payload
        .get("ai-memory")
        .and_then(|v| v.as_object())
        .context("internal: build_antigravity_payload didn't return an ai-memory group")?
        .clone();
    apply_atomic(config_path, |existing| {
        mutate_json(existing, |root| {
            // Get-or-create the "ai-memory" named group; overlay
            // our events. Other named groups survive untouched.
            let group = root
                .entry("ai-memory")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`ai-memory` is present in hooks.json but not an object")?;
            for (event, value) in &our_group {
                overlay_event_hooks(group, event, value);
            }
            Ok(())
        })
    })
}

/// Mutate Kimi Code's `config.toml` (`$KIMI_CODE_HOME/config.toml` when
/// the var is set, else `~/.kimi-code/config.toml`) so Kimi Code fires
/// the ai-memory scripts on its lifecycle events.
///
/// Kimi Code stores hooks as `[[hooks]]` array-of-tables entries inside
/// the SAME config.toml that carries the user's providers and model,
/// and any unknown key on a hook entry makes the whole config fail to
/// load. The merge is therefore TOML-aware via `toml_edit` (the rest of
/// the document is preserved as parsed) and each entry carries exactly
/// `event` + `command` — `matcher` omitted (Kimi Code matches
/// everything) and `timeout` omitted (30s default).
fn apply_to_kimi_code_config(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => kimi_code_config_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "kimi-code", data_dir)?;
    let command_dir = staged_command_dir(&staged, "kimi-code");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let outcome = merge_kimi_code_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_kimi_code_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let commands = kimi_code_hook_commands(
        staged,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    apply_atomic(config_path, |existing| {
        mutate_toml(existing, |doc| upsert_kimi_code_hooks(doc, &commands))
    })
}

/// Insert or refresh ai-memory's `[[hooks]]` entries: drop any prior
/// entry whose command references our staged scripts (so re-runs update
/// in place instead of duplicating), append the fresh set, and leave
/// every third-party `[[hooks]]` entry — and every other table in
/// config.toml — untouched.
fn upsert_kimi_code_hooks(
    doc: &mut toml_edit::DocumentMut,
    commands: &[(&str, String)],
) -> Result<()> {
    use toml_edit::{ArrayOfTables, Item, Table, value};

    if doc.get("hooks").is_none() {
        doc.insert("hooks", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let hooks = doc
        .get_mut("hooks")
        .and_then(Item::as_array_of_tables_mut)
        .context("`hooks` is present in config.toml but not an array of tables")?;

    let stale: Vec<usize> = hooks
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| is_ai_memory_toml_hook_entry(entry).then_some(i))
        .collect();
    for i in stale.into_iter().rev() {
        hooks.remove(i);
    }
    for (event, command) in commands {
        let mut entry = Table::new();
        entry["event"] = value(*event);
        entry["command"] = value(command);
        hooks.push(entry);
    }
    Ok(())
}

/// True if a `[[hooks]]` table belongs to ai-memory. Keep the ownership
/// signature shared with JSON hook overlays and uninstall so string commands
/// from other tools are not removed during re-apply.
fn is_ai_memory_toml_hook_entry(entry: &toml_edit::Table) -> bool {
    let Some(command) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    hook_command_is_ours(command)
}

/// Merge ai-memory's camelCase hook entries into every existing Kiro CLI
/// v2 agent config (`~/.kiro/agents/*.json`, or the single file passed
/// via `--config-file`).
///
/// The v2 engine has no global hook surface: hooks fire only for the
/// agent config that is active, so the installer updates each config the
/// user already has. It never invents one — a fabricated agent config
/// would not be the active agent (the built-in default has no file on
/// disk), so hooks in it would never fire and the install would look
/// successful while capturing nothing.
fn apply_to_kiro_cli_agent_configs(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    // Resolve targets before staging anything so a missing-agent-config
    // bail leaves no half-done install behind.
    let targets: Vec<PathBuf> = match &args.config_file {
        Some(p) => {
            anyhow::ensure!(
                p.is_file(),
                "{} does not exist. Kiro CLI v2 hooks live inside an existing agent config; \
                 create one first (`kiro-cli agent create`) and re-run. For Kiro v3, select \
                 the explicit `--agent kiro-cli-v3` standalone-hook target instead.",
                p.display()
            );
            vec![p.clone()]
        }
        None => list_kiro_cli_agent_configs(&kiro_cli_agents_dir()?)?,
    };
    anyhow::ensure!(
        !targets.is_empty(),
        "no Kiro CLI agent configs found in {}. The v2 engine only fires hooks defined in an \
         agent config and the built-in default agent has no file on disk, so create an agent \
         first (`kiro-cli agent create`, then `kiro-cli agent set-default <name>`) and re-run. \
         For Kiro v3, select the explicit `--agent kiro-cli-v3` standalone-hook target instead.",
        kiro_cli_agents_dir()?.display()
    );
    let prepared = preflight_kiro_cli_agent_configs(
        targets,
        hooks_dir,
        server_url,
        auth_token,
        data_dir,
        args.project_strategy,
    )?;

    let staged = stage_hook_scripts(hooks_dir, "kiro-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "kiro-cli");
    for (path, strategy) in prepared {
        let outcome = merge_kiro_cli_agent_hooks(
            &command_dir,
            server_url,
            auth_token,
            data_dir,
            strategy.and_then(ProjectStrategyArg::baked),
            &path,
        )?;
        println!(
            "✓ {} {} ({})",
            outcome.verb(),
            path.display(),
            match outcome {
                ApplyOutcome::Created => "new file",
                ApplyOutcome::Updated => "backup written next to it",
                ApplyOutcome::NoOp => "already up to date",
            }
        );
    }
    Ok(())
}

/// Parse and validate every Kiro v2 target before staging scripts or writing
/// any config. The strategy is recovered per file because Kiro can update
/// several independently-configured agents in one invocation.
fn preflight_kiro_cli_agent_configs(
    targets: Vec<PathBuf>,
    command_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    explicit_strategy: Option<ProjectStrategyArg>,
) -> Result<Vec<(PathBuf, Option<ProjectStrategyArg>)>> {
    let mut prepared = Vec::with_capacity(targets.len());
    for path in targets {
        let existing =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let strategy =
            explicit_strategy.or_else(|| baked_project_strategy(AgentChoice::KiroCli, &existing));
        render_kiro_cli_agent_hooks(
            &existing,
            command_dir,
            server_url,
            auth_token,
            data_dir,
            strategy.and_then(ProjectStrategyArg::baked),
            &path,
        )?;
        prepared.push((path, strategy));
    }
    Ok(prepared)
}

/// Merge ai-memory's v2 hook entries into one Kiro agent config,
/// preserving every third-party entry under the same triggers and every
/// non-hook field of the agent definition.
fn merge_kiro_cli_agent_hooks(
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    apply_atomic(config_path, |existing| {
        render_kiro_cli_agent_hooks(
            existing,
            staged,
            server_url,
            auth_token,
            data_dir,
            project_strategy,
            config_path,
        )
    })
}

fn render_kiro_cli_agent_hooks(
    existing: &str,
    staged: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<String> {
    let our_hooks = build_kiro_cli_v2_hooks_value(
        staged,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    mutate_json(existing, |root| {
        let hooks = root
            .entry("hooks")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .with_context(|| {
                format!(
                    "`hooks` in {} is present but not an object",
                    config_path.display()
                )
            })?;
        for (event, value) in &our_hooks {
            overlay_kiro_cli_event_hooks(hooks, event, value);
        }
        Ok(())
    })
}

fn apply_to_kiro_cli_v3_hooks(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(path) => path.clone(),
        None => kiro_cli_v3_hooks_path()?,
    };
    let existing = match fs::read_to_string(&path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);

    // Validate before staging scripts so an incompatible registration file
    // cannot leave a partial installation behind.
    render_kiro_cli_v3_hooks(
        &existing, hooks_dir, server_url, auth_token, data_dir, strategy, &path,
    )?;

    let staged = stage_hook_scripts(hooks_dir, "kiro-cli", data_dir)?;
    let command_dir = staged_command_dir(&staged, "kiro-cli");
    let outcome = merge_kiro_cli_v3_hooks(
        &command_dir,
        server_url,
        auth_token,
        data_dir,
        strategy,
        &path,
    )?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

fn merge_kiro_cli_v3_hooks(
    command_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<ApplyOutcome> {
    apply_atomic(config_path, |existing| {
        render_kiro_cli_v3_hooks(
            existing,
            command_dir,
            server_url,
            auth_token,
            data_dir,
            project_strategy,
            config_path,
        )
    })
}

fn render_kiro_cli_v3_hooks(
    existing: &str,
    command_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    config_path: &Path,
) -> Result<String> {
    let desired = build_kiro_cli_v3_hooks_value(
        command_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let desired_hooks = desired
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .expect("static Kiro v3 hook payload has a hooks array")
        .clone();

    mutate_json(existing, |root| {
        match root.get("version") {
            Some(serde_json::Value::String(version)) if version == "v1" => {}
            Some(serde_json::Value::String(version)) => anyhow::bail!(
                "unsupported Kiro hook schema version `{version}` in {}; expected `v1`",
                config_path.display()
            ),
            Some(_) => anyhow::bail!(
                "`version` in {} is present but not a string",
                config_path.display()
            ),
            None => {
                root.insert(
                    "version".to_string(),
                    serde_json::Value::String("v1".to_string()),
                );
            }
        }

        let hooks = root
            .entry("hooks")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| {
                format!(
                    "`hooks` in {} is present but not an array",
                    config_path.display()
                )
            })?;

        for entry in hooks.iter() {
            if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str)
                && kiro_cli_v3_hook_name_is_reserved(name)
                && !is_ai_memory_kiro_v3_hook_entry(entry)
            {
                anyhow::bail!(
                    "Kiro v3 hook `{name}` in {} uses ai-memory's reserved name but is not an \
                     ai-memory hook; rename it before installing",
                    config_path.display()
                );
            }
        }
        hooks.retain(|entry| !is_ai_memory_kiro_v3_hook_entry(entry));
        hooks.extend(desired_hooks);
        Ok(())
    })
}

fn kiro_cli_v3_hook_name_is_reserved(name: &str) -> bool {
    KIRO_CLI_V3_EVENTS.iter().any(|(_, script)| {
        script
            .strip_suffix(".sh")
            .is_some_and(|stem| name == format!("ai-memory-{stem}"))
    })
}

pub(crate) fn is_ai_memory_kiro_v3_hook_entry(entry: &serde_json::Value) -> bool {
    let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(trigger) = entry.get("trigger").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let known_pair = KIRO_CLI_V3_EVENTS.iter().any(|(expected_trigger, script)| {
        *expected_trigger == trigger
            && script
                .strip_suffix(".sh")
                .is_some_and(|stem| name == format!("ai-memory-{stem}"))
    });
    known_pair
        && entry
            .pointer("/action/type")
            .and_then(serde_json::Value::as_str)
            == Some("command")
        && entry
            .pointer("/action/command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_ai_memory_kiro_hook_command)
}

/// Every `*.json` agent config in the Kiro CLI global agents directory,
/// sorted for deterministic apply order. A missing directory is an empty
/// list, not an error — the caller decides how to report it.
pub(crate) fn list_kiro_cli_agent_configs(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", dir.display()));
        }
    };
    let mut configs = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", dir.display()))?;
        let path = entry.path();
        let metadata = fs::metadata(&path)
            .with_context(|| format!("reading metadata for {}", path.display()))?;
        if metadata.is_file() && path.extension().and_then(|e| e.to_str()) == Some("json") {
            configs.push(path);
        }
    }
    configs.sort();
    Ok(configs)
}

pub(crate) fn is_ai_memory_kiro_hook_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let generated_script =
        lower.contains("/hooks/kiro-cli/") || lower.contains("\\hooks\\kiro-cli\\");
    let ai_memory_executable = lower.contains("ai-memory") || lower.contains("ai_memory");
    let native_hook = ai_memory_executable
        && lower.contains(" hook ")
        && lower.contains("--agent kiro-cli")
        && lower.contains("--server-url");
    (generated_script || native_hook)
        && KIRO_CLI_V2_EVENTS.iter().any(|(_, script)| {
            let stem = script.trim_end_matches(".sh");
            lower.contains(script)
                || lower.contains(&format!("{stem}.ps1"))
                || lower.contains(&format!("--event {stem}"))
        })
}

/// Generate an OpenCode plugin at `~/.config/opencode/plugins/ai-memory.ts`.
///
/// OpenCode's integration surface is a TypeScript plugin, not a JSON
/// hook table. The plugin posts normalized lifecycle payloads directly
/// to `/hook` and injects pending handoffs through
/// `experimental.chat.system.transform`, because plugin shell stdout is
/// not prepended to the model context the way Claude Code hook stdout is.
fn apply_to_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => opencode_plugin_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_opencode_plugin(server_url, auth_token, strategy);

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new plugin file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("OpenCode auto-loads plugins from ~/.config/opencode/plugins/ on next start.");
        println!("If you're already inside an `opencode` session, restart it for the");
        println!("new plugin to take effect.");
    }
    Ok(())
}

fn render_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> Result<()> {
    println!("// OpenCode plugin — write to ~/.config/opencode/plugins/ai-memory.ts");
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OpenCode after changing plugins; config is loaded at startup.");
    println!();
    println!(
        "{}",
        build_opencode_plugin(server_url, auth_token, project_strategy)
    );
    Ok(())
}

/// Emit the `applyMarkerParams` TypeScript function shared verbatim by the
/// OpenCode plugin and the OMP extension.
///
/// `None` reproduces the historical marker-only function byte-for-byte, so
/// existing generated files and golden tests are unchanged. `Some(default)`
/// prepends a `DEFAULT_PROJECT_STRATEGY` const and emits a variant that applies
/// that install-time default when no marker pins a `project_strategy` (#128).
/// A marker's own `project` / `project_strategy` still take precedence (§3.3),
/// and repo-root is resolved host-side via `repoRootProject`.
fn ts_apply_marker_params(default_strategy: Option<&str>) -> String {
    let Some(default) = default_strategy else {
        return format!(
            "{TS_TOML_FLAG}\n{}",
            r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  const managedRun = process.env.AI_MEMORY_RUN_ID;
  if (managedRun) url.searchParams.set("managed_run", managedRun);
  const marker = findMarker(cwd);
  if (!marker || !cwd) return;
  url.searchParams.set("cwd", cwd);
  try {
    const body = readFileSync(marker, "utf8");
    const workspace = tomlKey(body, "workspace");
    const project = tomlKey(body, "project");
    const projectStrategy = tomlKey(body, "project_strategy");
    const dropSubagent = tomlKey(body, "drop_subagent_captures");
    const defaultGlobal = tomlFlag(body, "default_global");
    const briefing = tomlFlag(body, "inject_on_session_start");
    const briefingBudget = tomlFlag(body, "max_chars");
    if (workspace) url.searchParams.set("workspace", workspace);
    if (project) url.searchParams.set("project", project);
    if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
    if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
    if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
    if (briefing) url.searchParams.set("briefing", briefing);
    if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
    if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
      const repoProject = repoRootProject(cwd);
      if (repoProject) url.searchParams.set("project", repoProject);
    }
  } catch (_e) {
  }
}"#
        );
    };
    let body = r#"function applyMarkerParams(url: URL, cwd: string | undefined): void {
  const managedRun = process.env.AI_MEMORY_RUN_ID;
  if (managedRun) url.searchParams.set("managed_run", managedRun);
  if (!cwd) return;
  url.searchParams.set("cwd", cwd);
  let workspace: string | undefined;
  let project: string | undefined;
  let projectStrategy: string | undefined;
  let dropSubagent: string | undefined;
  let defaultGlobal: string | undefined;
  let briefing: string | undefined;
  let briefingBudget: string | undefined;
  const marker = findMarker(cwd);
  if (marker) {
    try {
      const body = readFileSync(marker, "utf8");
      workspace = tomlKey(body, "workspace");
      project = tomlKey(body, "project");
      projectStrategy = tomlKey(body, "project_strategy");
      dropSubagent = tomlKey(body, "drop_subagent_captures");
      defaultGlobal = tomlFlag(body, "default_global");
      briefing = tomlFlag(body, "inject_on_session_start");
      briefingBudget = tomlFlag(body, "max_chars");
    } catch (_e) {
    }
  }
  if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;
  if (!project && (projectStrategy === "repo-root" || projectStrategy === "repo_root")) {
    const repoProject = repoRootProject(cwd);
    if (repoProject) project = repoProject;
  }
  if (workspace) url.searchParams.set("workspace", workspace);
  if (project) url.searchParams.set("project", project);
  if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
  if (dropSubagent) url.searchParams.set("drop_subagent", dropSubagent);
  if (defaultGlobal) url.searchParams.set("default_global", defaultGlobal);
  if (briefing) url.searchParams.set("briefing", briefing);
  if (briefingBudget) url.searchParams.set("briefing_budget", briefingBudget);
}"#;
    format!(
        "const DEFAULT_PROJECT_STRATEGY = {};\n{TS_TOML_FLAG}\n{body}",
        ts_string_literal(default)
    )
}

/// `tomlFlag` mirrors the native hook's `parse_toml_flag`: unlike `tomlKey`
/// (quoted strings only) it also accepts a bare token (`default_global =
/// true`, `max_chars = 4000`), so section-style marker keys work whether or
/// not the operator quotes the value. Emitted next to `applyMarkerParams`
/// in every generated TypeScript integration.
pub(crate) const TS_TOML_FLAG: &str = r#"function tomlFlag(text: string, key: string): string | undefined {
  const re = new RegExp(`^\\s*${key}\\s*=\\s*(?:"([^"]*)"|([^#\\s]+))`);
  for (const line of text.split(/\r?\n/)) {
    const match = re.exec(line);
    if (match) return match[1] ?? match[2];
  }
  return undefined;
}"#;

/// Post-process a generated TypeScript integration so failed hook
/// deliveries SPOOL instead of vanishing (#580). The shell hooks have
/// spooled since day one; the TS plugins fire-and-forgot, so a laptop
/// off the server's network lost whole sessions of capture. Entries are
/// written in the CLI spool's exact on-disk contract
/// (`{ms:013}-{pid}-{seq:016x}.json`, `SpoolEntry` fields, 0700 dir /
/// 0600 files, tmp+rename) so `ai-memory hook-drain` and the shell
/// hooks' piggyback drain deliver them too; the plugin also drains its
/// own spool on first use and after any queue flush. An `ingest_key`
/// is minted at spool time, so a double drain (plugin + CLI) dedupes
/// server-side instead of double-ingesting.
///
/// Implemented as a transformation over the rendered source (rather
/// than another copy of the template) so all TS integrations share one
/// audited implementation; it fails loudly if the delivery block it
/// patches ever drifts.
fn add_hook_spooling(source: String) -> Result<String> {
    const FETCH_BLOCK: &str = r#"      try {
        await fetch(item.url, {
          method: "POST",
          headers: { "Content-Type": "application/json", ...authHeaders() },
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }).catch(() => undefined);
      } catch (_e) {
        // Best-effort capture. Hooks must never block the agent.
      }"#;
    const FETCH_REPLACEMENT: &str = r#"      try {
        const resp = await fetch(item.url, {
          method: "POST",
          headers: { "Content-Type": "application/json", ...authHeaders() },
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }).catch(() => undefined);
        // Unreachable server or 5xx: keep the event on disk for a later
        // drain (#580). 4xx is permanent - spooling it would retry a
        // rejection forever.
        if (!resp || resp.status >= 500) spoolFailedHook(item.url, item.payload);
      } catch (_e) {
        try { spoolFailedHook(item.url, item.payload); } catch (_e2) {}
      }"#;
    const ENQUEUE_ANCHOR: &str = "function enqueueHook(";
    let runtime = crate::commands::render_shared::ts_spool_runtime();
    if !source.contains(FETCH_BLOCK) {
        anyhow::bail!(
            "TS integration template drifted: the hook delivery block the \
             spool patch targets was not found"
        );
    }
    if source.matches(ENQUEUE_ANCHOR).count() != 1 {
        anyhow::bail!("TS integration template drifted: enqueueHook anchor not unique");
    }
    let mut out = source.replacen(FETCH_BLOCK, FETCH_REPLACEMENT, 1);
    out = out.replacen(ENQUEUE_ANCHOR, &format!("{runtime}{ENQUEUE_ANCHOR}"), 1);
    // Drain the offline spool alongside every queue flush, and extend the
    // imports the runtime needs.
    let drain_anchor =
        "async function drainHookQueue(): Promise<void> {\n  if (hookDraining) return;";
    if out.matches(drain_anchor).count() != 1 {
        anyhow::bail!("TS integration template drifted: drainHookQueue anchor not unique");
    }
    out = out.replacen(
        drain_anchor,
        "async function drainHookQueue(): Promise<void> {\n  if (hookDraining) return;\n  requestSpoolDrain();",
        1,
    );
    let import_anchor = "import { closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync } from \"node:fs\";";
    if out.matches(import_anchor).count() != 1 {
        anyhow::bail!("TS integration template drifted: node:fs import anchor not unique");
    }
    out = out.replacen(
        import_anchor,
        "import { closeSync, existsSync, mkdirSync, openSync, readFileSync as readMarkerText, readSync, readdirSync, renameSync, unlinkSync, writeFileSync } from \"node:fs\";",
        1,
    );
    Ok(out)
}

fn build_opencode_plugin(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    let apply_marker_params = ts_apply_marker_params(project_strategy);
    let capture_policy = ts_capture_policy_v1();
    let body = format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.
// Edit by re-running the command, not by hand — install-hooks
// will overwrite this file (with a `.bak-<ts>` backup) on each
// re-run.

import type {{ Plugin }} from "@opencode-ai/plugin";
import {{ execFileSync }} from "node:child_process";
import {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync }} from "node:fs";
import {{ basename, dirname, join, resolve, sep }} from "node:path";
import {{ homedir }} from "node:os";

const SERVER = {server_literal}.replace(/\/+$/, "");
const AGENT = "open-code";
{token_line}
{capture_policy}

function timeoutSignal(ms: number): AbortSignal | undefined {{
  if (typeof AbortSignal === "undefined") return undefined;
  const factory = (AbortSignal as unknown as {{ timeout?: (ms: number) => AbortSignal }}).timeout;
  return factory ? factory(ms) : undefined;
}}

function authHeaders(): Record<string, string> {{
  return TOKEN ? {{ Authorization: `Bearer ${{TOKEN}}` }} : {{}};
}}

const HOOK_QUEUE_MAX = 100;
const HOOK_FLUSH_INTERVAL_MS = 2000;
const HOOK_FLUSH_THRESHOLD = 20;
const HOOK_INTER_REQUEST_DELAY_MS = 50;
const HOOK_REQUEST_TIMEOUT_MS = 2000;
const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;
const HOOK_IMMEDIATE_EVENTS = new Set(["session-start", "stop", "session-end", "pre-compact"]);

type HookQueueItem = {{ event: string; url: URL; payload: Record<string, unknown> }};
const hookQueue: HookQueueItem[] = [];
let hookFlushTimer: ReturnType<typeof setTimeout> | undefined;
let hookDraining = false;
let hookDrainPromise: Promise<void> | undefined;

function sleep(ms: number): Promise<void> {{
  return new Promise((resolve) => setTimeout(resolve, ms));
}}

function scheduleHookFlush(): void {{
  if (hookFlushTimer) return;
  hookFlushTimer = setTimeout(() => {{
    hookFlushTimer = undefined;
    void requestHookDrain();
  }}, HOOK_FLUSH_INTERVAL_MS);
  hookFlushTimer.unref?.();
}}

function requestHookDrain(): Promise<void> {{
  if (!hookDrainPromise) {{
    hookDrainPromise = drainHookQueue().finally(() => {{
      hookDrainPromise = undefined;
      if (hookQueue.length > 0) void requestHookDrain();
    }});
  }}
  return hookDrainPromise;
}}

function disposeDrainTimeout(): Promise<void> {{
  return new Promise((resolve) => {{
    const timer = setTimeout(resolve, HOOK_DISPOSE_DRAIN_BUDGET_MS);
    timer.unref?.();
  }});
}}

async function drainHookQueueForDispose(): Promise<void> {{
  await Promise.race([requestHookDrain(), disposeDrainTimeout()]);
}}

function enqueueHook(event: string, url: URL, payload: Record<string, unknown>): void {{
  if (hookQueue.length >= HOOK_QUEUE_MAX) hookQueue.shift();
  hookQueue.push({{ event, url, payload }});
  if (HOOK_IMMEDIATE_EVENTS.has(event) || hookQueue.length >= HOOK_FLUSH_THRESHOLD) {{
    void requestHookDrain();
  }} else {{
    scheduleHookFlush();
  }}
}}

async function drainHookQueue(): Promise<void> {{
  if (hookDraining) return;
  hookDraining = true;
  if (hookFlushTimer) {{
    clearTimeout(hookFlushTimer);
    hookFlushTimer = undefined;
  }}
  try {{
    while (hookQueue.length > 0) {{
      const item = hookQueue.shift();
      if (!item) break;
      try {{
        await fetch(item.url, {{
          method: "POST",
          headers: {{ "Content-Type": "application/json", ...authHeaders() }},
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }}).catch(() => undefined);
      }} catch (_e) {{
        // Best-effort capture. Hooks must never block the agent.
      }}
      if (hookQueue.length > 0) await sleep(HOOK_INTER_REQUEST_DELAY_MS);
    }}
  }} finally {{
    hookDraining = false;
  }}
}}

function findMarker(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  let dir = resolve(cwd);
  const home = homedir();
  let boundary: string | undefined;
  if (home && (dir === home || dir.startsWith(home.endsWith(sep) ? home : home + sep))) {{
    boundary = home;
  }} else if (home) {{
    let probe = dir;
    while (probe && probe !== dirname(probe)) {{
      if (existsSync(join(probe, ".git"))) {{
        boundary = probe;
        break;
      }}
      probe = dirname(probe);
    }}
    boundary ??= dir;
  }}
  while (dir && dir !== dirname(dir)) {{
    const marker = join(dir, ".ai-memory.toml");
    if (existsSync(marker)) return marker;
    if (boundary && dir === boundary) return undefined;
    dir = dirname(dir);
  }}
  return undefined;
}}

function tomlKey(text: string, key: string): string | undefined {{
  const re = new RegExp(`^\\s*${{key}}\\s*=\\s*"([^"]*)"`);
  for (const line of text.split(/\r?\n/)) {{
    const match = re.exec(line);
    if (match) return match[1];
  }}
  return undefined;
}}


function repoRootProject(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  try {{
    const inside = execFileSync("git", ["-C", cwd, "rev-parse", "--is-inside-work-tree"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (inside !== "true") return undefined;
    const common = execFileSync("git", ["-C", cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (!common) return undefined;
    const root = dirname(common);
    if (!root || root === dirname(root)) return undefined;
    return basename(root);
  }} catch (_e) {{
    return undefined;
  }}
}}
{apply_marker_params}

function sessionID(input: unknown): string | undefined {{
  const value = input as any;
  return value?.sessionID ?? value?.sessionId ?? value?.session_id ?? value?.info?.id;
}}

function textFromParts(parts: unknown): string {{
  if (!Array.isArray(parts)) return "";
  return parts
    .map((part: any) => {{
      if (part?.type === "text" && typeof part.text === "string") return part.text;
      if (part?.type === "subtask" && typeof part.prompt === "string") return part.prompt;
      if (part?.type === "file" && typeof part.filename === "string") return `[file: ${{part.filename}}]`;
      return "";
    }})
    .filter(Boolean)
    .join("\n\n")
    .trim();
}}

const sessionCwds = new Map<string, string>();
const startedSessions = new Set<string>();
const handoffFetches = new Map<string, Promise<string | undefined>>();
const preCompactLast = new Map<string, number>();

function cwdFor(id: string | undefined, directory: string): string {{
  return (id && sessionCwds.get(id)) || directory;
}}

function rememberCwd(id: string | undefined, cwd: string | undefined): void {{
  if (id && cwd) sessionCwds.set(id, cwd);
}}

function startSession(id: string | undefined, cwd: string, extra: Record<string, unknown> = {{}}): void {{
  if (!id || startedSessions.has(id)) return;
  startedSessions.add(id);
  rememberCwd(id, cwd);
  // Generated integrations inject through fetchHandoff below. In managed mode
  // a queued SessionStart response is not model-visible and must not consume
  // the workstream context before that synchronous fetch receives it.
  if (!process.env.AI_MEMORY_RUN_ID) {{
    postHook("session-start", {{ sessionID: id, cwd, ...extra }});
  }}
}}

function endSession(id: string | undefined, directory: string, cwd?: string): void {{
  if (!id || !startedSessions.delete(id)) return;
  const resolvedCwd = cwd || cwdFor(id, directory);
  postHook("session-end", {{ sessionID: id, cwd: resolvedCwd }});
  sessionCwds.delete(id);
  handoffFetches.delete(id);
  preCompactLast.delete(id);
}}

function postPreCompact(id: string | undefined, directory: string): void {{
  startSession(id, cwdFor(id, directory));
  const key = id || "unknown";
  const now = Date.now();
  const last = preCompactLast.get(key) ?? 0;
  if (now - last < 1000) return;
  preCompactLast.set(key, now);
  postHook("pre-compact", {{ sessionID: id, cwd: cwdFor(id, directory) }});
}}

function postHook(event: string, payload: Record<string, unknown>): void {{
  const url = new URL(`${{SERVER}}/hook`);
  url.searchParams.set("event", event);
  url.searchParams.set("agent", AGENT);
  applyMarkerParams(url, typeof payload.cwd === "string" ? payload.cwd : undefined);
  const policy = capturePolicy(payload, typeof payload.cwd === "string" ? payload.cwd : undefined);
  if (policy.disposition === "drop") return;
  try {{
    enqueueHook(event, url, policy.payload);
  }} catch (_e) {{
    // Best-effort capture. Hooks must never block the agent.
  }}
}}

async function fetchHandoff(cwd: string, id: string | undefined): Promise<string | undefined> {{
  const url = new URL(`${{SERVER}}/handoff`);
  url.searchParams.set("agent", AGENT);
  url.searchParams.set("cwd", cwd);
  if (id) url.searchParams.set("session_id", id);
  applyMarkerParams(url, cwd);
  try {{
    const response = await fetch(url, {{
      headers: authHeaders(),
      signal: timeoutSignal(1000),
    }});
    const text = (await response.text()).trim();
    return text.length > 0 ? text : undefined;
  }} catch (_e) {{
    return undefined;
  }}
}}

export const AiMemoryHooks: Plugin = async ({{ directory }}) => {{
  return {{
    dispose: async () => {{
      for (const id of Array.from(startedSessions)) {{
        endSession(id, directory);
      }}
      await drainHookQueueForDispose();
    }},
    event: async (input) => {{
      const event = (input as any).event;
      const properties = event?.properties ?? {{}};
      if (event?.type === "session.created") {{
        const info = properties.info ?? {{}};
        const id = properties.sessionID ?? info.id;
        const cwd = info.directory ?? directory;
        startSession(id, cwd, {{
          title: info.title,
          projectID: info.projectID,
        }});
      }}
      if (event?.type === "session.idle") {{
        const id = properties.sessionID;
        startSession(id, cwdFor(id, directory));
        postHook("stop", {{ sessionID: id, cwd: cwdFor(id, directory) }});
      }}
      if (event?.type === "session.deleted") {{
        const info = properties.info ?? {{}};
        const id = properties.sessionID ?? info.id;
        endSession(id, directory, info.directory);
      }}
      if (event?.type === "session.compacted") {{
        const id = properties.sessionID;
        postPreCompact(id, directory);
      }}
    }},
    "chat.message": async (input, output) => {{
      const id = sessionID(input);
      const cwd = cwdFor(id, directory);
      startSession(id, cwd, {{ agent: (input as any).agent, model: (input as any).model }});
      postHook("user-prompt", {{
        sessionID: id,
        cwd,
        agent: (input as any).agent,
        model: (input as any).model,
        messageID: (input as any).messageID,
        prompt: textFromParts((output as any).parts),
      }});
    }},
    "tool.execute.before": async (input, output) => {{
      const id = sessionID(input);
      startSession(id, cwdFor(id, directory));
      postHook("pre-tool-use", {{
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: (input as any).tool,
        callID: (input as any).callID,
        args: (output as any).args,
      }});
    }},
    "tool.execute.after": async (input, output) => {{
      const id = sessionID(input);
      startSession(id, cwdFor(id, directory));
      postHook("post-tool-use", {{
        sessionID: id,
        cwd: cwdFor(id, directory),
        tool: (input as any).tool,
        callID: (input as any).callID,
        args: (input as any).args,
        title: (output as any).title,
        output: (output as any).output,
        metadata: (output as any).metadata,
      }});
    }},
    "experimental.session.compacting": async (input) => {{
      const id = sessionID(input);
      postPreCompact(id, directory);
    }},
    "experimental.chat.system.transform": async (input, output) => {{
      const id = sessionID(input);
      if (!id) return;
      startSession(id, cwdFor(id, directory));
      let pending = handoffFetches.get(id);
      if (!pending) {{
        pending = fetchHandoff(cwdFor(id, directory), id);
        handoffFetches.set(id, pending);
      }}
      const handoff = await pending;
      if (handoff) (output as any).system.push(handoff);
    }},
  }};
}};

export default AiMemoryHooks;
"#,
        server_literal = ts_string_literal(server_url),
        token_line = token_line,
    );
    // Anchors are compile-time constants over this same file's template;
    // a mismatch is a template edit that forgot the spool patch.
    add_hook_spooling(body).expect("generated TS integration lost its hook-spool anchors")
}

/// Warn when Pi and OMP resolve to the same extensions directory.
///
/// Agent-distinct filenames stop the two installs from overwriting each
/// other, but they do not make sharing a directory safe. OMP discovers
/// **every** direct `*.ts` under its extensions directory, so with both
/// files present each agent loads both extensions — and since the Pi
/// extension is the OMP one with `const AGENT` swapped, and neither guards
/// on its host, every lifecycle event is captured twice under two different
/// agent identities.
///
/// That is not a regression this installer can fix by itself: both agents
/// honour the same `PI_CODING_AGENT_DIR`, so the collision is upstream. What
/// it can do is refuse to be silent about it, and name the two ways out.
fn warn_if_agents_share_extensions_dir(this_agent: &str, this_path: &Path) {
    let other = match this_agent {
        "pi" => omp_extension_path(None),
        "omp" => pi_extension_path(),
        _ => return,
    };
    let Ok(other_path) = other else { return };
    let (Some(a), Some(b)) = (this_path.parent(), other_path.parent()) else {
        return;
    };
    if a != b {
        return;
    }
    eprintln!(
        "warning: Pi and OMP both resolve to {} — they share PI_CODING_AGENT_DIR.\n\
         \x20 Each agent loads every *.ts in that directory, so installing both\n\
         \x20 captures each event twice, once as `pi` and once as `omp`.\n\
         \x20 Give them separate homes, or scope OMP to a profile:\n\
         \x20     ai-memory install-hooks --agent omp --profile <name> --apply",
        a.display()
    );
}

/// Remove a pre-rename `ai-memory.ts` sitting beside the new agent-distinct
/// file, so the agent does not load both.
///
/// **Only removes a file this tool generated.** `uninstall` gates every
/// delete behind `generated_file_is_ours`, and `removal.rs` asserts that a
/// user-owned file which merely happens to be named `ai-memory.ts` survives
/// (`uninstall_omp_extension_deletes_only_generated_file`). An unconditional
/// delete here would let `install-hooks --apply` destroy exactly what
/// `uninstall` refuses to touch — a worse outcome than the stale file,
/// because it is silent and hits a path the operator did not ask to modify.
///
/// The marker is the generated banner plus the agent constant, matching the
/// pair `generated_file_is_ours` checks.
///
/// `agent` scopes the match to the caller's own agent. Pi and OMP can share
/// one extensions directory (both honour `PI_CODING_AGENT_DIR`), and a
/// legacy `ai-memory.ts` there belongs to whichever installed last. Removing
/// the *other* agent's file would silently stop its capture until the
/// operator happened to reinstall it.
fn remove_legacy_extension(new_path: &Path, agent: &str) {
    let legacy = new_path.with_file_name("ai-memory.ts");
    if legacy == new_path || !legacy.exists() {
        return;
    }
    let Ok(content) = std::fs::read_to_string(&legacy) else {
        return;
    };
    let banner = format!("Auto-generated by `ai-memory install-hooks --agent {agent} --apply`");
    let agent_const = format!("const AGENT = \"{agent}\";");
    let ours = content.contains(&banner) && content.contains(&agent_const);
    if !ours {
        println!(
            "note: left {} in place — it was not generated by this agent's \
             installer. Remove it yourself if the agent should stop loading it.",
            legacy.display()
        );
        return;
    }
    match std::fs::remove_file(&legacy) {
        Ok(()) => println!("✓ Removed superseded extension {}", legacy.display()),
        Err(e) => eprintln!(
            "[ai-memory] warning: could not remove superseded extension {}: {e}",
            legacy.display()
        ),
    }
}

/// Generate an Oh My Pi extension at `~/.omp/agent/extensions/ai-memory-omp.ts`.
///
/// OMP discovers direct `*.ts` / `*.js` files under `~/.omp/agent/extensions/`
/// at startup, so no separate settings merge is needed. The extension uses OMP's
/// lifecycle API for capture and `before_agent_start` for handoff injection.
fn apply_to_omp_extension(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = resolve_omp_extension_path(args)?;
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_omp_extension(server_url, auth_token, strategy);

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new extension file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );

    warn_if_agents_share_extensions_dir("omp", &path);
    remove_legacy_extension(&path, "omp");

    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!(
            "OMP auto-loads direct TypeScript extensions from ~/.omp/agent/extensions/ on next start."
        );
        println!("If you're already inside an `omp` session, restart it for the");
        println!("new extension to take effect.");
    }
    Ok(())
}

fn omp_extension_hint_in(
    env_override: Option<std::ffi::OsString>,
    profile: Option<&str>,
) -> String {
    omp_extension_path_in(env_override, profile).map_or_else(
        |_| {
            if let Some(name) = profile {
                format!("~/.omp/profiles/{}/agent/extensions/ai-memory-omp.ts", name)
            } else {
                "~/.omp/agent/extensions/ai-memory-omp.ts".to_string()
            }
        },
        |p| p.display().to_string(),
    )
}

/// See [`omp_extension_hint_in`]; same reasoning for Pi.
fn pi_extension_hint_in(env_override: Option<std::ffi::OsString>) -> String {
    pi_extension_path_in(env_override).map_or_else(
        |_| "~/.pi/agent/extensions/ai-memory-pi.ts".to_string(),
        |p| p.display().to_string(),
    )
}

fn render_omp_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    profile: Option<&str>,
) -> Result<()> {
    println!(
        "// Oh My Pi / OMP extension — write to {}",
        omp_extension_hint_in(std::env::var_os("PI_CODING_AGENT_DIR"), profile)
    );
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OMP after changing extensions; config is loaded at startup.");
    println!();
    println!(
        "{}",
        build_omp_extension(server_url, auth_token, project_strategy)
    );
    Ok(())
}

fn resolve_omp_extension_path(args: &InstallHooksArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    // See `omp_extension_path`: bind before borrowing.
    let env_profile = std::env::var_os("OMP_PROFILE");
    let profile = args
        .profile
        .as_deref()
        .or_else(|| env_profile.as_ref().and_then(|s| s.to_str()));
    omp_extension_path(profile)
}

fn apply_to_pi_extension(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = resolve_pi_extension_path(args)?;
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let body = build_pi_extension(server_url, auth_token, strategy);

    let outcome = apply_atomic(&path, move |_existing| Ok(body.clone()))?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new Pi extension file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );

    warn_if_agents_share_extensions_dir("pi", &path);
    remove_legacy_extension(&path, "pi");

    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("Pi loads TypeScript extensions from ~/.pi/agent/extensions/ on next start.");
        println!("Restart Pi for lifecycle capture and MCP tools to take effect.");
    }
    Ok(())
}

fn render_pi_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> Result<()> {
    println!(
        "// Pi extension — write to {}",
        pi_extension_hint_in(std::env::var_os("PI_CODING_AGENT_DIR"))
    );
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart Pi after changing extensions; MCP tools are bridged by this file.");
    println!();
    println!(
        "{}",
        build_pi_extension(server_url, auth_token, project_strategy)
    );
    Ok(())
}

fn resolve_pi_extension_path(args: &InstallHooksArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    pi_extension_path()
}

fn build_pi_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> String {
    let lifecycle = build_omp_extension(server_url, auth_token, project_strategy)
        .replace("install-hooks --agent omp --apply", "install-hooks --agent pi --apply")
        .replace("const AGENT = \"omp\";", "const AGENT = \"pi\";")
        .replace(
            r#"
  api.on("session.compacting", (_event: any, ctx: any) => {
    postPreCompact(ctx);
  });
"#,
            "\n",
        )
        .replace(
            "export default function AiMemoryExtension(api: any): void {",
            &format!(
                "{}\nexport default function AiMemoryExtension(pi: any): void {{\n  try {{ void bootstrapMcpBridge(pi); }} catch (_e) {{}}",
                pi_mcp_bridge_source()
            ),
        )
        .replace("api.on(\"", "pi.on(\"");
    debug_assert!(!lifecycle.contains(".omp"));
    lifecycle
}

fn pi_mcp_bridge_source() -> &'static str {
    r#"
// ---- MCP bridge ------------------------------------------------------------
const MCP_SERVER = deriveMcpServer(SERVER);
const MCP_REQUEST_TIMEOUT_MS = 10000;
let mcpRequestId = 0;

function deriveMcpServer(server: string): string {
  const trimmed = server.replace(/\/+$/, "");
  return trimmed.endsWith("/mcp") ? trimmed : `${trimmed}/mcp`;
}

function mcpSessionId(ctx: any): string | undefined {
  const id = sessionID(ctx) ?? ctx?.sessionId ?? ctx?.sessionID ?? ctx?.session?.id;
  return typeof id === "string" && id.length > 0 ? id : undefined;
}

function mcpSignal(signal?: AbortSignal): AbortSignal | undefined {
  const timeout = timeoutSignal(MCP_REQUEST_TIMEOUT_MS);
  if (!signal) return timeout;
  if (!timeout) return signal;
  const anyFactory = (AbortSignal as unknown as { any?: (signals: AbortSignal[]) => AbortSignal }).any;
  return anyFactory ? anyFactory([signal, timeout]) : timeout;
}

async function mcpRpc(method: string, params?: unknown, ctx?: any, signal?: AbortSignal): Promise<any> {
  const id = ++mcpRequestId;
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
    "Accept": "application/json, text/event-stream",
    ...authHeaders(),
  };
  const session = mcpSessionId(ctx);
  if (session) {
    headers["X-Memory-Actor-Session-Id"] = session;
    headers["Mcp-Session-Id"] = session;
  }
  const response = await fetch(MCP_SERVER, {
    method: "POST",
    headers,
    body: JSON.stringify({ jsonrpc: "2.0", id, method, params: params ?? {} }),
    signal: mcpSignal(signal),
  });
  if (!response.ok) throw new Error(`ai-memory MCP ${method} failed: HTTP ${response.status}`);
  const payload = await response.json();
  if (payload?.error) throw new Error(`ai-memory MCP ${method} failed: ${payload.error.message ?? JSON.stringify(payload.error)}`);
  if (payload?.result?.isError) throw new Error(`ai-memory MCP ${method} returned isError`);
  return payload?.result;
}

function toolInputSchema(tool: any): any {
  return tool?.inputSchema ?? { type: "object", additionalProperties: true };
}

async function bootstrapMcpBridge(pi: any): Promise<void> {
  try {
    await mcpRpc("initialize", {
      protocolVersion: "2025-03-26",
      capabilities: {},
      clientInfo: { name: "ai-memory-pi-extension", version: "0.0.0" },
    });
    try { await mcpRpc("notifications/initialized"); } catch (_e) {}
    const listed = await mcpRpc("tools/list");
    for (const tool of listed?.tools ?? []) {
      try {
        pi.registerTool({
          name: tool.name,
          label: tool.name,
          description: tool.description,
          parameters: toolInputSchema(tool),
          execute: async (_toolCallId: string, params: unknown, signal?: AbortSignal, _onUpdate?: unknown, ctx?: any) => {
            const result = await mcpRpc("tools/call", { name: tool.name, arguments: params ?? {} }, ctx, signal);
            return { content: result?.content ?? [], details: result };
          },
        });
      } catch (_e) {
        // Duplicate registration or tool-shape mismatch must not break lifecycle capture.
      }
    }
  } catch (_e) {
    // MCP bridge is best-effort; extension load and lifecycle capture must survive.
  }
}
"#
}

fn build_omp_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    let apply_marker_params = ts_apply_marker_params(project_strategy);
    let capture_policy = ts_capture_policy_v1();
    let body = format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent omp --apply`.
// Edit by re-running the command, not by hand — install-hooks
// will overwrite this file (with a `.bak-<ts>` backup) on each
// re-run.

import {{ execFileSync }} from "node:child_process";
import {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync }} from "node:fs";
import {{ basename, dirname, join, resolve, sep }} from "node:path";
import {{ homedir }} from "node:os";

const SERVER = {server_literal}.replace(/\/+$/, "");
const AGENT = "omp";
{token_line}
{capture_policy}

function timeoutSignal(ms: number): AbortSignal | undefined {{
  if (typeof AbortSignal === "undefined") return undefined;
  const factory = (AbortSignal as unknown as {{ timeout?: (ms: number) => AbortSignal }}).timeout;
  return factory ? factory(ms) : undefined;
}}

function authHeaders(): Record<string, string> {{
  return TOKEN ? {{ Authorization: `Bearer ${{TOKEN}}` }} : {{}};
}}

const HOOK_QUEUE_MAX = 100;
const HOOK_FLUSH_INTERVAL_MS = 2000;
const HOOK_FLUSH_THRESHOLD = 20;
const HOOK_INTER_REQUEST_DELAY_MS = 50;
const HOOK_REQUEST_TIMEOUT_MS = 2000;
const HOOK_IMMEDIATE_EVENTS = new Set(["session-start", "stop", "session-end", "pre-compact"]);

type HookQueueItem = {{ event: string; url: URL; payload: Record<string, unknown> }};
const hookQueue: HookQueueItem[] = [];
let hookFlushTimer: ReturnType<typeof setTimeout> | undefined;
let hookDraining = false;

function sleep(ms: number): Promise<void> {{
  return new Promise((resolve) => setTimeout(resolve, ms));
}}

function scheduleHookFlush(): void {{
  if (hookFlushTimer) return;
  hookFlushTimer = setTimeout(() => {{
    hookFlushTimer = undefined;
    void drainHookQueue();
  }}, HOOK_FLUSH_INTERVAL_MS);
  hookFlushTimer.unref?.();
}}

function enqueueHook(event: string, url: URL, payload: Record<string, unknown>): void {{
  if (hookQueue.length >= HOOK_QUEUE_MAX) hookQueue.shift();
  hookQueue.push({{ event, url, payload }});
  if (HOOK_IMMEDIATE_EVENTS.has(event) || hookQueue.length >= HOOK_FLUSH_THRESHOLD) {{
    void drainHookQueue();
  }} else {{
    scheduleHookFlush();
  }}
}}

async function drainHookQueue(): Promise<void> {{
  if (hookDraining) return;
  hookDraining = true;
  if (hookFlushTimer) {{
    clearTimeout(hookFlushTimer);
    hookFlushTimer = undefined;
  }}
  try {{
    while (hookQueue.length > 0) {{
      const item = hookQueue.shift();
      if (!item) break;
      try {{
        await fetch(item.url, {{
          method: "POST",
          headers: {{ "Content-Type": "application/json", ...authHeaders() }},
          body: JSON.stringify(item.payload),
          signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS),
        }}).catch(() => undefined);
      }} catch (_e) {{
        // Best-effort capture. Hooks must never block the agent.
      }}
      if (hookQueue.length > 0) await sleep(HOOK_INTER_REQUEST_DELAY_MS);
    }}
  }} finally {{
    hookDraining = false;
    if (hookQueue.length > 0) void drainHookQueue();
  }}
}}

function findMarker(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  let dir = resolve(cwd);
  const home = homedir();
  let boundary: string | undefined;
  if (home && (dir === home || dir.startsWith(home.endsWith(sep) ? home : home + sep))) {{
    boundary = home;
  }} else if (home) {{
    let probe = dir;
    while (probe && probe !== dirname(probe)) {{
      if (existsSync(join(probe, ".git"))) {{
        boundary = probe;
        break;
      }}
      probe = dirname(probe);
    }}
    boundary ??= dir;
  }}
  while (dir && dir !== dirname(dir)) {{
    const marker = join(dir, ".ai-memory.toml");
    if (existsSync(marker)) return marker;
    if (boundary && dir === boundary) return undefined;
    dir = dirname(dir);
  }}
  return undefined;
}}

function tomlKey(text: string, key: string): string | undefined {{
  const re = new RegExp(`^\\s*${{key}}\\s*=\\s*"([^"]*)"`);
  for (const line of text.split(/\r?\n/)) {{
    const match = re.exec(line);
    if (match) return match[1];
  }}
  return undefined;
}}


function repoRootProject(cwd: string | undefined): string | undefined {{
  if (!cwd) return undefined;
  try {{
    const inside = execFileSync("git", ["-C", cwd, "rev-parse", "--is-inside-work-tree"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (inside !== "true") return undefined;
    const common = execFileSync("git", ["-C", cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"], {{
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }}).trim();
    if (!common) return undefined;
    const root = dirname(common);
    if (!root || root === dirname(root)) return undefined;
    return basename(root);
  }} catch (_e) {{
    return undefined;
  }}
}}
{apply_marker_params}

function sessionID(ctx: any): string | undefined {{
  const id = ctx?.sessionManager?.getSessionId?.();
  return typeof id === "string" && id.length > 0 ? id : undefined;
}}

function modelName(model: any): string | undefined {{
  const name = model?.id ?? model?.name ?? model?.model;
  return typeof name === "string" && name.length > 0 ? name : undefined;
}}

function sessionPayload(ctx: any): Record<string, unknown> {{
  return {{
    sessionID: sessionID(ctx),
    cwd: ctx?.cwd,
    model: modelName(ctx?.model),
  }};
}}

function stringify(value: unknown): string {{
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  try {{
    return JSON.stringify(value);
  }} catch (_e) {{
    return String(value);
  }}
}}

function contentToText(content: unknown): string {{
  if (content === null || content === undefined) return "";
  if (!Array.isArray(content)) return stringify(content);
  return content
    .map((part: any) => {{
      if (typeof part?.text === "string") return part.text;
      if (typeof part?.content === "string") return part.content;
      if (typeof part?.type === "string") return `[${{part.type}}]`;
      return stringify(part);
    }})
    .filter(Boolean)
    .join("\n\n")
    .trim();
}}

const startedSessions = new Set<string>();
const handoffChecked = new Set<string>();
const preCompactLast = new Map<string, number>();

function startSession(ctx: any, extra: Record<string, unknown> = {{}}): void {{
  const id = sessionID(ctx);
  if (!id || startedSessions.has(id)) return;
  startedSessions.add(id);
  // Generated integrations inject through fetchHandoff below. In managed mode
  // a queued SessionStart response is not model-visible and must not consume
  // the workstream context before that synchronous fetch receives it.
  if (!process.env.AI_MEMORY_RUN_ID) {{
    postHook("session-start", {{ ...sessionPayload(ctx), ...extra }});
  }}
}}

function postPreCompact(ctx: any): void {{
  startSession(ctx);
  const key = sessionID(ctx) || "unknown";
  const now = Date.now();
  const last = preCompactLast.get(key) ?? 0;
  if (now - last < 1000) return;
  preCompactLast.set(key, now);
  postHook("pre-compact", sessionPayload(ctx));
}}

function postHook(event: string, payload: Record<string, unknown>): void {{
  const url = new URL(`${{SERVER}}/hook`);
  url.searchParams.set("event", event);
  url.searchParams.set("agent", AGENT);
  applyMarkerParams(url, typeof payload.cwd === "string" ? payload.cwd : undefined);
  const policy = capturePolicy(payload, typeof payload.cwd === "string" ? payload.cwd : undefined);
  if (policy.disposition === "drop") return;
  try {{
    enqueueHook(event, url, policy.payload);
  }} catch (_e) {{
    // Best-effort capture. Hooks must never block the agent.
  }}
}}

async function fetchHandoff(cwd: string, id: string | undefined): Promise<string | undefined> {{
  const url = new URL(`${{SERVER}}/handoff`);
  url.searchParams.set("agent", AGENT);
  url.searchParams.set("cwd", cwd);
  if (id) url.searchParams.set("session_id", id);
  applyMarkerParams(url, cwd);
  try {{
    const response = await fetch(url, {{
      headers: authHeaders(),
      signal: timeoutSignal(1000),
    }});
    const text = (await response.text()).trim();
    return text.length > 0 ? text : undefined;
  }} catch (_e) {{
    return undefined;
  }}
}}

export default function AiMemoryExtension(api: any): void {{
  api.on("session_start", (_event: any, ctx: any) => {{
    startSession(ctx);
  }});

  api.on("before_agent_start", async (event: any, ctx: any) => {{
    startSession(ctx);
    postHook("user-prompt", {{
      ...sessionPayload(ctx),
      prompt: event?.prompt,
      imageCount: Array.isArray(event?.images) ? event.images.length : undefined,
    }});

    const id = sessionID(ctx);
    if (!id || handoffChecked.has(id)) return;
    handoffChecked.add(id);
    const handoff = await fetchHandoff(ctx?.cwd ?? "", id);
    if (!handoff) return;
    return {{
      message: {{
        customType: "ai-memory-handoff",
        content: handoff,
        display: false,
        attribution: "agent",
      }},
    }};
  }});

  api.on("tool_call", (event: any, ctx: any) => {{
    startSession(ctx);
    postHook("pre-tool-use", {{
      ...sessionPayload(ctx),
      tool: event?.toolName,
      callID: event?.toolCallId,
      args: event?.input,
    }});
  }});

  api.on("tool_result", (event: any, ctx: any) => {{
    startSession(ctx);
    postHook("post-tool-use", {{
      ...sessionPayload(ctx),
      tool: event?.toolName,
      callID: event?.toolCallId,
      args: event?.input,
      output: contentToText(event?.content),
      details: event?.details,
      isError: event?.isError,
    }});
  }});

  api.on("session_before_compact", (_event: any, ctx: any) => {{
    postPreCompact(ctx);
  }});

  api.on("session_compact", (_event: any, ctx: any) => {{
    postPreCompact(ctx);
  }});

  api.on("session.compacting", (_event: any, ctx: any) => {{
    postPreCompact(ctx);
  }});

  api.on("agent_end", (_event: any, ctx: any) => {{
    startSession(ctx);
    postHook("stop", sessionPayload(ctx));
  }});

  api.on("session_shutdown", (_event: any, ctx: any) => {{
    startSession(ctx);
    postHook("session-end", sessionPayload(ctx));
  }});
}}
"#,
        server_literal = ts_string_literal(server_url),
        token_line = token_line,
    );
    // Anchors are compile-time constants over this same file's template;
    // a mismatch is a template edit that forgot the spool patch.
    add_hook_spooling(body).expect("generated TS integration lost its hook-spool anchors")
}

fn render_agent(
    label: &str,
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    event_lists: &[&[(&str, &str)]],
) -> Result<()> {
    print!(
        "{}",
        render_agent_output(
            label,
            hooks_dir,
            server_url,
            auth_token,
            project_strategy,
            event_lists,
        )
    );
    Ok(())
}

fn render_agent_output(
    label: &str,
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
    event_lists: &[&[(&str, &str)]],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {label} hook scripts (manual install — wire each to the matching event)\n"
    ));
    out.push_str(&format!("# Hook scripts: {}\n", hooks_dir.display()));
    out.push_str(&format!("# AI-memory server URL: {server_url}\n"));
    if auth_token.is_some() {
        out.push_str("# Auth: set AI_MEMORY_AUTH_TOKEN in each hook's environment to the\n");
        out.push_str("#       value passed via --auth-token (omitted from this printout).\n");
    } else {
        out.push_str("# Auth: server requires no bearer token. To require one, generate a\n");
        out.push_str("#       token with `ai-memory generate-auth-token` and pass it via\n");
        out.push_str("#       --auth-token here AND set AI_MEMORY_AUTH_TOKEN on the server.\n");
    }
    out.push('\n');
    for events in event_lists {
        for (_, script) in *events {
            let script = hook_script_for_current_platform(script);
            out.push_str(&format!(
                "- {}\n",
                hooks_dir.join(script.as_ref()).display()
            ));
        }
    }
    out.push('\n');
    out.push_str("Set AI_MEMORY_HOOK_URL in each hook's environment to override the default.\n");
    if let Some(instruction) = manual_agent_project_strategy_instruction(project_strategy) {
        out.push_str(&instruction);
        out.push('\n');
    }
    if label == "antigravity-cli" {
        out.push('\n');
        out.push_str(ANTIGRAVITY_FINALIZATION_GUIDANCE);
    }
    out
}

fn manual_agent_project_strategy_instruction(project_strategy: Option<&str>) -> Option<String> {
    project_strategy.map(|strategy| {
        format!(
            "Set AI_MEMORY_PROJECT_STRATEGY={strategy} in each hook's environment to use the requested project strategy."
        )
    })
}

/// Copy the bundled hook scripts to a stable user-global location
/// and return that location. The path the agent's config file
/// references is THIS path, not the source bundle's path.
///
/// Why this matters:
///
/// - **Project-portability.** The previous behaviour wrote the
///   repo-relative path (e.g. `/mnt/data/Projects/ai-memory/hooks/
///   claude-code/session-start.sh`) into the agent's settings.
///   Any agent CLI started from a different project — or in a
///   filesystem sandbox that didn't whitelist that path — failed
///   the SessionStart hook with "No such file or directory".
///
/// - **Docker-image upgrades.** Users who installed via the docker
///   image had paths under `/usr/local/share/ai-memory/hooks/`
///   baked into their settings — paths only valid INSIDE the
///   container. Staging copies the scripts OUT to the host's
///   `~/.local/share/ai-memory/hooks/` so the host-side agent can
///   actually reach them.
///
/// - **Updates.** When a new docker image ships with updated hook
///   scripts, the user re-runs `install-hooks --apply` and the
///   stage step overwrites the previous copies. No special
///   `update-hooks` command, no version-tracking dance.
///
/// Errors propagate when source is missing, the staging dir
/// can't be created, or any file copy fails.
fn stage_hook_scripts(source_dir: &Path, agent_label: &str, data_dir: &Path) -> Result<PathBuf> {
    stage_hook_scripts_in(
        source_dir,
        agent_label,
        &hook_staging_root(data_dir, ai_memory_wiki::backup::running_in_container()),
    )
}

/// Where hook scripts are staged — which is also the path written into
/// the agent's settings, so it must be reachable by the process that
/// EXECUTES the hooks: the host-side agent CLI.
///
/// - **Native installs**: the resolved data dir (`--data-dir` /
///   `AI_MEMORY_DATA_DIR` / platform default), per #554.
/// - **Inside a container** (the docker wrapper): the server data dir is
///   a volume (`/data`) the host cannot execute from — staging there
///   wrote container-only paths into the host's settings and every hook
///   failed silently (#581). The wrapper binds `$HOME:$HOME` and reads
///   staged hooks from `~/.local/share/ai-memory/hooks` (its
///   `HOOKS_STAGE_DIR` contract), so the home-based default is the
///   host-reachable location there.
pub(crate) fn hook_staging_root(data_dir: &Path, in_container: bool) -> PathBuf {
    if in_container {
        dirs::data_local_dir()
            .map(|d| d.join("ai-memory"))
            .unwrap_or_else(|| data_dir.to_path_buf())
    } else {
        data_dir.to_path_buf()
    }
}

fn stage_hook_scripts_in(source_dir: &Path, agent_label: &str, data_dir: &Path) -> Result<PathBuf> {
    // `<data_dir>/hooks/<agent>`, not `<platform default>/ai-memory/hooks/…`.
    // Byte-identical for a default install, since `default_data_dir()` *is*
    // `data_local_dir()/ai-memory` — but it now follows `--data-dir` and
    // `AI_MEMORY_DATA_DIR` instead of populating a directory the operator is
    // not using (#554).
    let dest_root = data_dir.join("hooks").join(agent_label);

    fs::create_dir_all(&dest_root)
        .with_context(|| format!("creating staging dir {}", dest_root.display()))?;

    // When `resolve_hooks_dir` falls through to the data-local
    // candidate (e.g. docker `setup-agent` already extracted the
    // bundle into ~/.local/share/ai-memory/hooks/<agent>/, or a prior
    // install left scripts in place), the source dir IS the
    // destination dir. The wipe-then-copy flow below would delete the
    // very scripts we mean to install before reading them, leaving 0
    // copied and a settings.json pointing at an empty directory
    // (issue #52). Detect that case via canonical paths and verify
    // the existing layout in place instead of touching it.
    let same_path = same_canonical_dir(source_dir, &dest_root);

    if !same_path {
        // Wipe any previously-staged scripts that the current bundle
        // no longer ships. Idempotent re-runs against an old install
        // shouldn't leave stale entries pointed at by nothing.
        if let Ok(entries) = fs::read_dir(&dest_root) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && is_hook_script_file(&p) {
                    fs::remove_file(&p).ok();
                }
            }
        }
    }

    let mut count = 0_usize;
    for entry in fs::read_dir(source_dir)
        .with_context(|| format!("reading source bundle {}", source_dir.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || !is_hook_script_file(&from) {
            continue;
        }
        if !same_path {
            copy_hook_file(&from, &dest_root)?;
        }
        count += 1;
    }

    if !same_path {
        copy_support_hook_scripts(source_dir, &dest_root)?;

        // Stage the shared `_lib.sh` helper alongside the event scripts so
        // they can `. "$(dirname "$0")/_lib.sh"` without depending on the
        // user's PATH or repo layout. The helper lives ONCE in
        // `hooks/_lib.sh` (one parent up from the agent-specific dir) —
        // staging it here is what keeps every agent's runtime view
        // consistent with the source of truth.
        if let Some(shared) = source_dir.parent().map(|p| p.join("_lib.sh"))
            && shared.is_file()
        {
            copy_hook_file(&shared, &dest_root)?;
        }
    }

    if count == 0 {
        anyhow::bail!(
            "no hook scripts found at {}.\n\
             Refusing to install — pointing the agent's settings at an empty \
             directory would silently disable all capture. Either pass \
             `--hooks-dir <path>` to point at a populated source tree, or run \
             `ai-memory setup-agent --agent <name>` first to extract the \
             bundled scripts.",
            source_dir.display()
        );
    }

    let verb = if same_path { "verified" } else { "staged" };
    eprintln!("✓ {verb} {count} hook script(s) → {}", dest_root.display());
    Ok(dest_root)
}

/// `true` when `a` and `b` resolve to the same directory after symlink
/// canonicalization. Falls back to literal `==` if either canonicalize
/// call fails (e.g. dest hasn't been created yet on Windows, network
/// FS quirks). The caller has already `create_dir_all`'d both ends
/// in the staging flow, so the fast path almost always wins.
fn same_canonical_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Copy a single hook file (event script or shared `_lib.sh`) into the
/// staging dir, preserving the executable bit on Unix. Centralised so
/// the script bulk-copy and the `_lib.sh` companion follow the same
/// rules without duplicating permission-handling.
fn copy_hook_file(from: &Path, dest_root: &Path) -> Result<()> {
    let to = dest_root.join(from.file_name().context("bad source file name")?);
    fs::copy(from, &to)
        .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&to)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&to, perms)?;
    }
    Ok(())
}

/// Copy the optional `lib/` support directory (currently PowerShell
/// helpers for Windows hook parity) alongside the event scripts.
/// No-op when the source bundle doesn't ship it.
fn copy_support_hook_scripts(source_dir: &Path, dest_root: &Path) -> Result<()> {
    let Some(source_hooks_root) = source_dir.parent() else {
        return Ok(());
    };
    let source_lib = source_hooks_root.join("lib");
    if !source_lib.is_dir() {
        return Ok(());
    }
    let Some(dest_hooks_root) = dest_root.parent() else {
        return Ok(());
    };
    let dest_lib = dest_hooks_root.join("lib");
    fs::create_dir_all(&dest_lib)
        .with_context(|| format!("creating hook support dir {}", dest_lib.display()))?;
    for entry in fs::read_dir(&source_lib)
        .with_context(|| format!("reading hook support dir {}", source_lib.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || from.extension().and_then(|s| s.to_str()) != Some("ps1") {
            continue;
        }
        let to = dest_lib.join(from.file_name().context("bad support file name")?);
        fs::copy(&from, &to)
            .with_context(|| format!("copying {} → {}", from.display(), to.display()))?;
    }
    Ok(())
}

fn staged_command_dir(staged: &Path, agent_label: &str) -> PathBuf {
    match std::env::var("AI_MEMORY_HOOKS_HOST_ROOT") {
        Ok(root) if !root.trim().is_empty() => PathBuf::from(root).join(agent_label),
        _ => staged.to_path_buf(),
    }
}

fn is_hook_script_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("sh" | "ps1")
    )
}

fn resolve_hooks_dir(
    explicit: Option<&Path>,
    agent: AgentChoice,
    data_dir: &Path,
) -> Result<PathBuf> {
    let Some(sub) = agent.script_hook_subdir() else {
        anyhow::bail!("{agent:?} uses a generated integration, not a hook script directory")
    };
    if let Some(p) = explicit {
        let path = p.join(sub);
        if path.is_dir() {
            return Ok(path);
        }
        anyhow::bail!("hooks directory {} does not exist", path.display());
    }

    // Probe candidates in order. The first dir that exists wins.
    let candidates = hook_source_candidates(
        sub,
        repo_root_guess(),
        exe_dir_guess(),
        Some(data_dir.to_path_buf()),
    );
    for path in &candidates {
        if !path.as_os_str().is_empty() && path.is_dir() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!(
        "could not locate hooks directory. Tried: {candidates:?}. \
         Pass --hooks-dir <directory containing {sub}/> to point at the bundle explicitly.",
    );
}

fn hook_source_candidates(
    sub: &str,
    repo_root: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(5);
    // Cargo-run from the repo.
    if let Some(root) = repo_root {
        candidates.push(root.join("hooks").join(sub));
    }
    // Release tarball (macOS/Windows/Linux archive): the `hooks/` bundle
    // ships in the same directory as the binary, so it's reachable without
    // `--source` (issue #107).
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("hooks").join(sub));
    }
    // Docker image lays them out under /usr/local/share/ai-memory/.
    candidates.push(PathBuf::from(format!(
        "/usr/local/share/ai-memory/hooks/{sub}"
    )));
    // Native Linux packages install hook sources under /usr/share.
    candidates.push(PathBuf::from(format!("/usr/share/ai-memory/hooks/{sub}")));
    // Local install honourable mention: the bundle a previous `--apply`, or
    // docker `setup-agent`, staged under the data dir actually in use.
    if let Some(dir) = data_dir {
        candidates.push(dir.join("hooks").join(sub));
    }
    candidates
}

fn repo_root_guess() -> Option<PathBuf> {
    repo_root_from_exe(&current_exe_resolved()?)
}

/// When the binary lives under target/{debug,release}/<name>, the
/// workspace root is two parents up.
fn repo_root_from_exe(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.parent()?.to_path_buf())
}

/// Directory the running binary lives in. The release tarball ships the
/// `hooks/` bundle right next to the binary, so a no-`--source`
/// `install-hooks` finds it there (issue #107).
fn exe_dir_guess() -> Option<PathBuf> {
    Some(current_exe_resolved()?.parent()?.to_path_buf())
}

/// Path of the running binary with symlinks resolved.
///
/// Both guesses above are relative to where the binary *lives*, not to how
/// it was invoked. `~/.local/bin/ai-memory -> <repo>/target/release/ai-memory`
/// is the layout the quick start suggests, and without this the repo-root
/// guess walks up from the link's own directory to `$HOME` while the tarball
/// guess stops at `~/.local/bin` — leaving the bundled `hooks/` unreachable
/// from every candidate, whatever the cwd (issue #546).
fn current_exe_resolved() -> Option<PathBuf> {
    std::env::current_exe().ok().map(resolve_exe_path)
}

/// Canonicalise an executable path, keeping the original when the target
/// cannot be resolved (deleted binary, or a filesystem that refuses).
fn resolve_exe_path(exe: PathBuf) -> PathBuf {
    fs::canonicalize(&exe).unwrap_or(exe)
}

// CLAUDE_CODE_EVENTS + build_claude_code_payload now live in
// `super::render_shared`, shared with `setup-agent`.

/// Which optional capture surfaces the generated Claude Code settings should
/// include.
///
/// Grouped rather than passed as two positional bools: at the call site
/// `true, false` said nothing about which surface was which, and the pair is
/// exactly the privacy-relevant part of this install — worth naming.
#[derive(Debug, Clone, Copy)]
struct ClaudeCaptureScope {
    /// Include the assistant-message capture hook (double opt-in, off by
    /// default).
    assistant: bool,
    /// Include the `UserPromptSubmit` hook. When false the entry is omitted
    /// entirely, so the agent never emits prompt text at all.
    prompts: bool,
}

fn render_claude_code(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    settings_path: &Path,
    capture: ClaudeCaptureScope,
) -> Result<()> {
    let ClaudeCaptureScope {
        assistant: capture_assistant,
        prompts: capture_prompts,
    } = capture;
    // Soft check: warn (don't bail) if a script is missing. The user
    // may be running this command inside docker against a host path
    // that exists only on the host's filesystem — bailing would
    // sabotage the docker-only flow `setup-agent` enables.
    for (event, script) in super::render_shared::CLAUDE_CODE_EVENTS {
        if !capture_prompts && event == CLAUDE_PROMPT_EVENT {
            continue;
        }
        let script = hook_script_for_claude_code(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = configure_claude_prompt_capture(
        build_claude_code_payload_with_data_dir(
            hooks_dir,
            server_url,
            auth_token,
            Some(data_dir),
            project_strategy,
            capture_assistant,
        ),
        capture_prompts,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing claude code hook config")?;
    println!(
        "# Claude Code hook config — merge into {}",
        settings_path.display()
    );
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat {} as sensitive (chmod 600).",
            settings_path.display()
        );
    }
    println!();
    println!("{serialized}");
    Ok(())
}

fn render_grok(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in super::render_shared::CLAUDE_CODE_EVENTS {
        let script = hook_script_for_claude_code(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = build_grok_payload_with_data_dir(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing grok hook config")?;
    let config_path = grok_hooks_path()?;
    println!(
        "# Grok Build CLI hook config — write to {}",
        config_path.display()
    );
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat {} as sensitive (chmod 600).",
            config_path.display()
        );
    }
    println!("# NOTE: Grok ignores hook stdout on SessionStart — capture works,");
    println!("#       but handoff injection does not. Recover a prior session's");
    println!("#       handoff via the MCP `memory_handoff_accept` tool.");
    println!();
    println!("{serialized}");
    Ok(())
}

/// Merge ai-memory's lifecycle hooks into Zero's user-level
/// `~/.config/zero/hooks.json` (issue #156). Zero executes hook entries
/// directly (`command` + `args`, JSON payload on stdin) — no scripts to
/// stage, no shell. Only entries whose `id` carries the `ai-memory-`
/// prefix are replaced, so third-party hooks in the same file survive
/// re-installs and version upgrades. The file-level `enabled` flag is
/// preserved when the file already exists (a user who disabled hooks
/// globally keeps that choice — we warn instead of overriding).
fn apply_to_zero_hooks(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => zero_hooks_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = super::render_shared::build_zero_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_array())
        .context("internal: build_zero_hooks_config didn't return a hooks array")?
        .clone();
    let mut hooks_disabled = false;
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            hooks_disabled = root.get("enabled").and_then(|v| v.as_bool()) == Some(false);
            if !root.contains_key("enabled") {
                root.insert("enabled".into(), serde_json::Value::Bool(true));
            }
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Array(Vec::new()))
                .as_array_mut()
                .context("`hooks` is present in hooks.json but not an array")?;
            hooks.retain(|hook| {
                !hook
                    .get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| id.starts_with("ai-memory-"))
            });
            hooks.extend(our_hooks.iter().cloned());
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    if hooks_disabled {
        eprintln!(
            "# warning: this hooks.json sets \"enabled\": false at the top level, \
             so Zero will not run ANY hooks (including ai-memory's) until you \
             re-enable them."
        );
    }
    println!("# NOTE: Zero discards sessionStart hook stdout — capture works, but");
    println!("#       handoff injection does not. Recover a prior session's handoff");
    println!("#       via the MCP `memory_handoff_accept` tool.");
    Ok(())
}

/// Print Zero's hooks.json to stdout (dry-run counterpart of
/// [`apply_to_zero_hooks`]).
fn render_zero(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    let payload = super::render_shared::build_zero_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing zero hook config")?;
    println!("# Zero hook config — merge into ~/.config/zero/hooks.json");
    println!("# ($XDG_CONFIG_HOME/zero/hooks.json on non-default XDG setups), or");
    println!("# re-run with --apply to merge it in place, preserving other hooks.");
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: token embedded in each hook's args below.");
        println!("#       Treat hooks.json as sensitive (chmod 600).");
    }
    println!("# NOTE: Zero discards sessionStart hook stdout — capture works, but");
    println!("#       handoff injection does not. Recover a prior session's handoff");
    println!("#       via the MCP `memory_handoff_accept` tool.");
    println!();
    println!("{serialized}");
    Ok(())
}

/// Whether a hook entry in ZCode's `hooks.events` map is one ai-memory
/// wrote. Our entries carry a `statusMessage` starting with `ai-memory`
/// — the only schema-documented free-form key, so ownership is marked
/// without emitting keys ZCode would drop.
fn is_our_zcode_hook(hook: &serde_json::Value) -> bool {
    hook.get("statusMessage")
        .and_then(|v| v.as_str())
        .is_some_and(|msg| msg.starts_with("ai-memory"))
}

/// Withdraw ai-memory's own hook entries from every matcher group in
/// `groups`, leaving each group, and anyone else's hooks inside it, in
/// place.
fn strip_our_zcode_hook_entries(groups: &mut [serde_json::Value]) {
    for group in groups.iter_mut() {
        if let Some(hooks) = group
            .as_object_mut()
            .and_then(|group| group.get_mut("hooks"))
            .and_then(|v| v.as_array_mut())
        {
            hooks.retain(|hook| !is_our_zcode_hook(hook));
        }
    }
}

/// Same, for a `hooks.events` slot of unknown shape. Returns how many
/// entries were withdrawn so the caller can report them; a slot that is
/// not an array is left untouched and counts zero.
fn strip_our_zcode_hooks(slot: &mut serde_json::Value) -> usize {
    let Some(groups) = slot.as_array_mut() else {
        return 0;
    };
    let before = zcode_hook_count(groups);
    strip_our_zcode_hook_entries(groups);
    before.saturating_sub(zcode_hook_count(groups))
}

/// Total hook entries across every matcher group in `groups`, ignoring
/// groups whose `hooks` is absent or not an array.
fn zcode_hook_count(groups: &[serde_json::Value]) -> usize {
    groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(|v| v.as_array()))
        .map(|hooks| hooks.len())
        .sum()
}

/// True when a `hooks.events` entry cannot run anything: no matcher groups
/// at all, no group carrying a non-empty `hooks` array, or a value that is
/// not a matcher-group array in the first place. Such a key is pure
/// downside under ZCode's strict validation, since it runs nothing and can
/// cost the user every other hook in the block.
///
/// Deliberately narrower than "a key ai-memory does not write": several of
/// those are real ZCode events this tool skips on purpose (see
/// `PermissionRequest` in `render_shared`), and a key still running
/// someone's hook is their working config, not a leftover. Reporting on
/// that set would be a false-positive generator.
fn zcode_event_runs_nothing(slot: &serde_json::Value) -> bool {
    match slot.as_array() {
        Some(groups) => zcode_hook_count(groups) == 0,
        None => true,
    }
}

/// Merge ai-memory hooks into the root `hooks` block of ZCode's
/// user-scope `~/.zcode/cli/config.json` (#512). Sibling config keys
/// (`model`, `provider`, `mcp.servers`, …) always survive: the merge
/// touches only `hooks.enabled` (defaulted, never flipped),
/// `hooks.maxOutputBytes` (set only when absent), and `hooks.events`,
/// replaces entries ai-memory owns (idempotent re-apply), and never
/// removes a hook it did not write. Entries it owns are withdrawn
/// wherever they sit, including under event keys this version no longer
/// writes; event keys themselves are never deleted, only reported when
/// ai-memory withdrew entries from them, or when they are left unable to
/// run anything (#600). The one thing it does
/// discard is a matcher group left holding an empty `hooks` array under a
/// key it writes, which drops a caller's own empty group along with the
/// ones its withdrawal emptied.
fn apply_to_zcode_hooks(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => zcode_config_path()?,
    };
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    let payload = super::render_shared::build_zcode_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
    );
    let our_events = payload
        .get("events")
        .and_then(|v| v.as_object())
        .context("internal: build_zcode_hooks_config didn't return an events map")?
        .clone();
    let mut hooks_disabled = false;
    let mut stale: Vec<(String, usize, bool)> = Vec::new();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // `hooks` sits beside sibling config keys that must survive.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("`hooks` is present in the ZCode config but not an object")?;
            hooks_disabled = hooks.get("enabled").and_then(|v| v.as_bool()) == Some(false);
            if !hooks.contains_key("enabled") {
                hooks.insert("enabled".into(), serde_json::Value::Bool(true));
            }
            // Raise the stdout ceiling above ZCode's 32 KiB default so a
            // fetched handoff is not truncated mid-JSON before injection.
            // Only when the user has not chosen their own value: the key
            // is block-level and would also bound third-party hooks.
            if !hooks.contains_key("maxOutputBytes")
                && let Some(ceiling) = payload.get("maxOutputBytes")
            {
                hooks.insert("maxOutputBytes".into(), ceiling.clone());
            }
            let events = hooks
                .entry("events")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("`hooks.events` is present in the ZCode config but not an object")?;
            for (event, ours) in &our_events {
                let ours = ours
                    .as_array()
                    .context("internal: zcode event payload is not a matcher-group array")?;
                let slot = events
                    .entry(event.clone())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                let slot = slot.as_array_mut().context(
                    "`hooks.events.{event}` is present in the ZCode config but not an array",
                )?;
                strip_our_zcode_hook_entries(slot);
                slot.retain(|group| match group.get("hooks") {
                    Some(serde_json::Value::Array(hooks)) => !hooks.is_empty(),
                    _ => true,
                });
                slot.extend(ours.iter().cloned());
            }
            // ai-memory writes only the six keys in `ZCODE_EVENTS`, but its
            // own entries can sit under other keys too (a config carried
            // over from another agent, or hand-edited). Those are ours to
            // withdraw; the keys themselves are not, so every one is left
            // in place. What makes a leftover dangerous is ZCode strict-
            // validating this map: one key it does not recognize and it
            // rejects the ENTIRE hooks block, ai-memory's included, so
            // capture dies while the file still matches byte for byte and
            // apply reports it as up to date (#600). Collect the keys that
            // can no longer run anything and say so after the write.
            for (event, slot) in events.iter_mut() {
                if our_events.contains_key(event) {
                    continue;
                }
                let withdrawn = strip_our_zcode_hooks(slot);
                let runs_nothing = zcode_event_runs_nothing(slot);
                if withdrawn > 0 || runs_nothing {
                    stale.push((event.clone(), withdrawn, runs_nothing));
                }
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            // Only claim the install is current when nothing turned up
            // that can stop ZCode loading it. Saying "already up to date"
            // over a block ZCode rejects is the whole of #600, and the
            // notes explaining it go to stderr, so a redirected stdout
            // would otherwise carry the reassurance and none of the cause.
            ApplyOutcome::NoOp if stale.is_empty() => "already up to date",
            ApplyOutcome::NoOp => "unchanged; see the notes below",
        }
    );
    if hooks_disabled {
        eprintln!(
            "# warning: this config sets \"enabled\": false inside `hooks`, \
             so ZCode will not run ANY hooks (including ai-memory's) until \
             you re-enable them."
        );
    }
    if !stale.is_empty() {
        // The report above goes to stdout and these go to stderr; stdout
        // block-buffers when redirected, so without this the two arrive out
        // of order in a log or in CI.
        let _ = std::io::stdout().flush();
    }
    for (event, withdrawn, runs_nothing) in &stale {
        if *withdrawn > 0 {
            // A withdrawal is positive evidence ai-memory once lived under
            // this key, so the risk here is not hypothetical.
            eprintln!(
                "# warning: withdrew {withdrawn} ai-memory hook(s) from \
                 `hooks.events.{event}`, a key ai-memory does not write. The \
                 key itself was left as found, but ZCode validates this map \
                 strictly and rejects the ENTIRE hooks block, ai-memory's \
                 included, over one key it does not recognize, which leaves \
                 capture silently dead. If ZCode reports `hookCount: 0`, \
                 remove that key from {}.",
                path.display()
            );
        } else if *runs_nothing {
            eprintln!(
                // Only the provenance claim degrades between the two
                // tiers; the consequence is identical, so state it at full
                // strength. "If ZCode does not recognize it" keeps this
                // honest about keys ZCode does accept, such as an empty
                // `Notification`.
                "# note: `hooks.events.{event}` runs nothing, and ai-memory \
                 does not write that key. If ZCode does not recognize it, \
                 ZCode rejects the ENTIRE hooks block, ai-memory's included, \
                 and capture is dead while this file looks correct. If ZCode \
                 reports `hookCount: 0`, remove that key from {}.",
                path.display()
            );
        }
    }
    println!("# NOTE: ZCode injects SessionStart stdout as model context, so the");
    println!("#       prior session's handoff is delivered automatically.");
    println!("# NOTE: ZCode fires `Stop` at the end of every turn and has no");
    println!("#       SessionEnd — close sessions with");
    println!("#       `ai-memory finalize-session --agent zcode`.");
    Ok(())
}

/// Print ZCode's `hooks` block to stdout (dry-run counterpart of
/// [`apply_to_zcode_hooks`]).
fn render_zcode(
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    let payload = super::render_shared::build_zcode_hooks_config(
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing zcode hook config")?;
    println!("# ZCode (z.ai) hooks config — merge the `hooks` block into");
    println!("# ~/.zcode/cli/config.json (the same file `install-mcp --client");
    println!("# zcode` registers MCP servers in), or re-run with --apply to");
    println!("# merge it in place, preserving the rest of the config.");
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: this printed snippet embeds the token in each hook's args");
        println!("#       so a hand-placed config works as-is — treat it as");
        println!("#       sensitive (chmod 600). NOTE: `--apply` does NOT embed the");
        println!("#       token; it persists it to <data-dir>/auth-token (0600) and");
        println!("#       writes token-less args, so the applied config differs from");
        println!("#       this snippet by design (#552, #600).");
    }
    println!("# NOTE: ZCode injects SessionStart stdout as model context, so the");
    println!("#       prior session's handoff is delivered automatically.");
    println!("# NOTE: ZCode fires `Stop` at the end of every turn and has no");
    println!("#       SessionEnd — close sessions with");
    println!("#       `ai-memory finalize-session --agent zcode`.");
    println!();
    println!("{serialized}");
    Ok(())
}

fn render_devin(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in super::render_shared::DEVIN_EVENTS {
        let script = hook_script_for_claude_code(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = build_devin_payload_with_data_dir(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing devin hook config")?;
    println!(
        "# Devin CLI hook config — write to ~/.devin/hooks.v1.json or ~/.devin/config.json hooks key"
    );
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!(
            "#       Treat ~/.devin/hooks.v1.json or ~/.devin/config.json as sensitive (chmod 600)."
        );
    }
    println!(
        "# NOTE: Devin consumes the handoff via hookSpecificOutput.additionalContext on SessionStart."
    );
    println!();
    println!("{serialized}");
    Ok(())
}

/// Print Kimi Code's `[[hooks]]` TOML fragment to stdout (dry-run
/// counterpart of [`apply_to_kimi_code_config`]). Rendered through the
/// same upsert path the apply mode uses, so the printout is byte-for-
/// byte what `--apply` would append to config.toml.
fn render_kimi_code(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in KIMI_CODE_EVENTS {
        let script = hook_script_for_current_platform(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let commands = kimi_code_hook_commands(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let mut doc = toml_edit::DocumentMut::new();
    upsert_kimi_code_hooks(&mut doc, &commands)?;
    println!("# Kimi Code hook config — merge into $KIMI_CODE_HOME/config.toml");
    println!("# (~/.kimi-code/config.toml when KIMI_CODE_HOME is unset), or re-run");
    println!("# with --apply to merge it in place, preserving providers/model and");
    println!("# any third-party [[hooks]] entries already in the file.");
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!("#       Treat config.toml as sensitive (chmod 600).");
    }
    println!();
    print!("{doc}");
    Ok(())
}

fn render_kiro_cli(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in KIRO_CLI_V2_EVENTS {
        let script = hook_script_for_current_platform(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let hooks = build_kiro_cli_v2_hooks_value(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    println!("// Kiro CLI v2-engine hooks — merge the \"hooks\" object into each");
    println!("// agent config under $KIRO_HOME/agents (~/.kiro/agents when unset),");
    println!("// or re-run with --apply to merge into every existing agent config,");
    println!("// preserving third-party hook entries. The v2 engine fires hooks");
    println!("// only for the active agent config; the built-in default agent has");
    println!("// no file, so create one first (`kiro-cli agent create`). Do not reuse");
    println!("// this v2 registration for v3; use `--agent kiro-cli-v3` instead.");
    println!("// Hook scripts: {}", hooks_dir.display());
    println!("// AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("// Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!("//       Treat agent configs as sensitive (chmod 600).");
    }
    println!();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "hooks": hooks }))
            .expect("static hook payload serializes")
    );
    Ok(())
}

fn render_kiro_cli_v3(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    for (_, script) in KIRO_CLI_V3_EVENTS {
        let abs = hooks_dir.join(hook_script_for_current_platform(script).as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. If this command is running \
                 inside docker against a host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = build_kiro_cli_v3_hooks_value(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    println!("// Kiro CLI v3 hooks — write this document to");
    println!("// $KIRO_HOME/hooks/ai-memory.json (~/.kiro/hooks/ai-memory.json when unset),");
    println!("// or re-run with --apply for an atomic, idempotent merge.");
    println!("// Hook scripts: {}", hooks_dir.display());
    println!("// AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("// Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!("//       Treat the hook file as sensitive (chmod 600).");
    }
    println!();
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).expect("static hook payload serializes")
    );
    Ok(())
}

/// Print Pool's `.poolside/settings.yaml` `hooks:` block to stdout. Pool's
/// hook config lives at the root of each repository the agent runs in, so
/// there is no user-global file for an atomic merge — the snippet is pasted
/// per project. `--apply` still stages the scripts to the stable user-global
/// location first so the printed commands reference paths that outlive the
/// source checkout / docker image (see [`apply_to_pool`]).
fn render_pool(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> Result<()> {
    // Soft check (same rationale as render_claude_code): warn, don't bail,
    // so the docker host-path flow still works.
    for (_, script) in POOL_EVENTS {
        let script = hook_script_for_current_platform(script);
        let abs = hooks_dir.join(script.as_ref());
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    print!(
        "{}",
        render_pool_output(
            hooks_dir,
            server_url,
            auth_token,
            data_dir,
            project_strategy
        )
    );
    Ok(())
}

fn render_pool_output(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
) -> String {
    let snippet = build_pool_settings_yaml_with_data_dir(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
    );
    let mut out = String::new();
    out.push_str("# Pool (Poolside Agent CLI) hook config — merge into the repo-root\n");
    out.push_str("# .poolside/settings.yaml of each project Pool runs in. ai-memory\n");
    out.push_str("# does not write project-local files, so paste this snippet manually\n");
    out.push_str("# (re-run with --apply first to stage the scripts to a stable path).\n");
    out.push_str(&format!("# Hook scripts: {}\n", hooks_dir.display()));
    out.push_str(&format!("# AI-memory server URL: {server_url}\n"));
    if auth_token.is_some() {
        out.push_str("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.\n");
        out.push_str("#       Treat .poolside/settings.yaml as sensitive (chmod 600).\n");
    }
    out.push_str("# NOTE: Pool's SessionStart stdout injection is not demonstrated —\n");
    out.push_str("#       capture works, but handoff injection does not. Recover a prior\n");
    out.push_str("#       session's handoff via the MCP `memory_handoff_accept` tool.\n");
    out.push_str("# NOTE: Pool has no true session-end event; `Stop` is a turn boundary.\n");
    out.push_str(
        "#       Close a finished session with `ai-memory finalize-session --agent pool`.\n",
    );
    out.push('\n');
    out.push_str(&snippet);
    out
}

/// `--apply` for Pool: stage the hook scripts to the stable user-global
/// location (the same step every other script agent's apply performs), then
/// print the ready-to-paste snippet pointing at the staged copies. The
/// project-local `.poolside/settings.yaml` itself is deliberately NOT
/// written: install-hooks is a user-scoped operation and must not mutate
/// files inside the user's repositories.
fn apply_to_pool(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let staged = stage_hook_scripts(hooks_dir, "pool", data_dir)?;
    let command_dir = staged_command_dir(&staged, "pool");
    let strategy = args.project_strategy.and_then(ProjectStrategyArg::baked);
    print!(
        "{}",
        render_pool_output(&command_dir, server_url, auth_token, data_dir, strategy)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #446's binding requirement: "a protection that disappears on upgrade
    /// without saying so is worse than no protection". A bare `--apply` — what
    /// `ai-memory upgrade` runs — must leave an existing opt-in alone.
    #[test]
    fn bare_apply_preserves_an_existing_allowlist_optin() {
        let tmp = tempfile::tempdir().unwrap();
        let mode = persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
        assert_eq!(mode, "allowlist");

        // The upgrade path: no flag at all.
        let after = persist_capture_mode(tmp.path(), None).unwrap();
        assert_eq!(
            after, "allowlist",
            "a bare re-apply must not revert the opt-in"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join(crate::commands::hook::CAPTURE_MODE_FILE))
                .unwrap()
                .trim(),
            "allowlist"
        );
    }

    /// #446's opt-in is stored as a bare word, and every reader maps anything
    /// it does not recognise onto `denylist` — the *less* private mode. That
    /// fallback is deliberate and tested
    /// (`hook.rs`: `unreadable_or_unknown_mode_falls_back_to_denylist_not_allowlist`);
    /// what must never happen is the truncated file that triggers it. An
    /// in-place write truncates before it writes, so a crash, a full disk or a
    /// killed `upgrade` mid-write leaves exactly that — and the next hook run
    /// reads it as capture-by-default, silently undoing the opt-in the doc
    /// comment on `persist_capture_mode` promises to preserve.
    ///
    /// Replacement rather than truncation is the property that rules the window
    /// out, and it is observable: a rename gives the path a new inode, an
    /// in-place rewrite keeps the old one. This fails against `fs::write`.
    #[cfg(unix)]
    #[test]
    fn rewriting_the_capture_mode_replaces_the_file_rather_than_truncating_it() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(crate::commands::hook::CAPTURE_MODE_FILE);

        persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
        let before = fs::metadata(&path).unwrap().ino();

        persist_capture_mode(tmp.path(), Some(CaptureModeArg::Denylist)).unwrap();
        let after = fs::metadata(&path).unwrap().ino();

        assert_ne!(
            before, after,
            "the capture mode must be replaced by rename, not rewritten in \
             place: an in-place write is observable truncated, and a truncated \
             file reads back as denylist"
        );

        let leftovers: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".ai-memory-tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "the staging tempfile must not survive the write: {leftovers:?}"
        );
    }

    #[test]
    fn absent_file_reports_the_historical_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(persist_capture_mode(tmp.path(), None).unwrap(), "denylist");
    }

    #[test]
    fn explicit_denylist_downgrades_an_earlier_optin() {
        // The opt-in must be reversible, or operators cannot undo a mistake.
        let tmp = tempfile::tempdir().unwrap();
        persist_capture_mode(tmp.path(), Some(CaptureModeArg::Allowlist)).unwrap();
        assert_eq!(
            persist_capture_mode(tmp.path(), Some(CaptureModeArg::Denylist)).unwrap(),
            "denylist"
        );
        assert_eq!(persist_capture_mode(tmp.path(), None).unwrap(), "denylist");
    }
    use crate::cli::ProjectStrategyArg;
    use crate::commands::render_shared::KIRO_CLI_V2_SESSION_START_MAX_OUTPUT;
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(any(unix, windows))]
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn capture_assistant_allowed_only_for_claude_native() {
        use crate::cli::AgentChoice::*;
        // Every non-Claude agent is rejected regardless of platform (#196): the
        // opt-in cannot take effect for them, so the installer must bail.
        for agent in [
            Codex,
            CommandCode,
            Cursor,
            GeminiCli,
            OpenCode,
            Pi,
            Omp,
            Openclaw,
            AntigravityCli,
            Grok,
            Zero,
            Devin,
            KimiCode,
            KiroCli,
            KiroCliV3,
            Pool,
            Zcode,
        ] {
            assert!(
                !capture_assistant_allowed(agent),
                "{agent:?} must not allow --capture-assistant"
            );
        }
        // Claude Code tracks the native-platform gate exactly.
        assert_eq!(
            capture_assistant_allowed(ClaudeCode),
            local_hook_policy_v1_supported()
        );
    }

    #[test]
    fn prompt_capture_options_are_claude_code_only() {
        use crate::cli::AgentChoice::*;
        assert!(prompt_capture_options_allowed(ClaudeCode));
        for agent in [
            Codex,
            CommandCode,
            Cursor,
            GeminiCli,
            OpenCode,
            Pi,
            Omp,
            Openclaw,
            AntigravityCli,
            Grok,
            Zero,
            Devin,
            KimiCode,
            KiroCli,
            KiroCliV3,
            Pool,
            Zcode,
        ] {
            assert!(!prompt_capture_options_allowed(agent), "{agent:?}");
        }
    }

    #[test]
    fn baked_prompt_capture_reads_only_owned_claude_hooks() {
        let enabled = serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "command": "ai-memory hook --event session-start --agent claude-code --server-url http://h" }] }],
                "UserPromptSubmit": [{ "hooks": [{ "command": "ai-memory hook --event user-prompt --agent claude-code --server-url http://h" }] }]
            }
        });
        assert_eq!(
            baked_claude_prompt_capture(&enabled.to_string()),
            Some(true)
        );

        let disabled = serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{ "command": "ai-memory hook --event session-start --agent claude-code --server-url http://h" }] }],
                "UserPromptSubmit": [{ "hooks": [{ "command": "third-party prompt guard" }] }]
            }
        });
        assert_eq!(
            baked_claude_prompt_capture(&disabled.to_string()),
            Some(false)
        );

        let unrelated = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{ "hooks": [{ "command": "third-party prompt guard" }] }]
            }
        });
        assert_eq!(baked_claude_prompt_capture(&unrelated.to_string()), None);
    }

    #[cfg(unix)]
    fn bash_program_for_installer_test() -> Option<std::path::PathBuf> {
        Some(std::path::PathBuf::from("bash"))
    }

    #[cfg(windows)]
    fn bash_program_for_installer_test() -> Option<std::path::PathBuf> {
        let mut candidates = Vec::new();
        if let Some(root) = std::env::var_os("EXEPATH") {
            let root = std::path::PathBuf::from(root);
            candidates.push(root.join("bin").join("bash.exe"));
            candidates.push(root.join("usr").join("bin").join("bash.exe"));
        }
        for env_key in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(root) = std::env::var_os(env_key) {
                let root = std::path::PathBuf::from(root).join("Git");
                candidates.push(root.join("bin").join("bash.exe"));
                candidates.push(root.join("usr").join("bin").join("bash.exe"));
            }
        }
        candidates.sort();
        candidates.dedup();
        let found = candidates.into_iter().find(|candidate| candidate.is_file());
        if found.is_none() {
            eprintln!("skipping installer shell contract: Git for Windows bash.exe was not found");
        }
        found
    }

    #[test]
    fn overlay_event_hooks_preserves_third_party_and_replaces_own() {
        // Regression for issue #80: install-hooks must MERGE into the event
        // array, not replace it. A third-party SessionStart hook (e.g.
        // context-mode) must survive while our own stale entry is swapped
        // for the fresh one.
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "SessionStart".into(),
            serde_json::json!([
                { "hooks": [ { "type": "command", "command": "node context-mode-cache-heal.mjs" } ] },
                { "matcher": "", "hooks": [ { "type": "command", "command": "/old/ai-memory.exe hook --event session-start --agent claude-code --server-url http://old" } ] }
            ]),
        );
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "/new/.cargo/bin/ai-memory.exe hook --event session-start --agent claude-code --server-url http://new" } ] }
        ]);
        overlay_event_hooks(&mut hooks, "SessionStart", &ours);

        let arr = hooks["SessionStart"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            2,
            "third-party + our single fresh entry: {arr:?}"
        );
        let joined = serde_json::to_string(arr).unwrap();
        assert!(
            joined.contains("context-mode-cache-heal"),
            "third-party hook must survive"
        );
        assert!(
            !joined.contains("/old/ai-memory.exe"),
            "stale ai-memory entry must be replaced"
        );
        assert!(
            joined.contains("/new/.cargo/bin/ai-memory.exe"),
            "fresh ai-memory entry must be present"
        );
    }

    #[test]
    fn command_code_apply_uses_only_stable_events_and_preserves_user_settings() {
        let source = TempDir::new().unwrap();
        stub_scripts(
            source.path(),
            &[
                "session-start.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "stop.sh",
            ],
        );
        let staging_root = TempDir::new().unwrap();
        // A real data dir ends in `ai-memory`; the entry-matching below keys
        // off that, so inject a realistic one rather than a bare temp path.
        let staging = staging_root.path().join("ai-memory");
        let config_dir = TempDir::new().unwrap();
        let config_path = config_dir.path().join("settings.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "theme": "dark",
                "hooks": {
                    "SessionStart": [{
                        "hooks": [{"type": "command", "command": "third-party"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::CommandCode,
            config_file: Some(config_path.clone()),
            ..default_hook_args()
        };

        apply_to_command_code_settings_in(
            source.path(),
            "http://memory:49374",
            Some("token"),
            config_dir.path(),
            &staging,
            &args,
        )
        .unwrap();
        let first = fs::read_to_string(&config_path).unwrap();
        apply_to_command_code_settings_in(
            source.path(),
            "http://memory:49374",
            Some("token"),
            config_dir.path(),
            &staging,
            &args,
        )
        .unwrap();
        let second = fs::read_to_string(&config_path).unwrap();
        assert_eq!(first, second, "re-apply must be idempotent");

        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(value["theme"], "dark");
        let hooks = value["hooks"].as_object().unwrap();
        for event in ["SessionStart", "PreToolUse", "PostToolUse", "Stop"] {
            let entries = hooks[event].as_array().unwrap();
            let ours = entries
                .iter()
                .find(|entry| serde_json::to_string(entry).unwrap().contains("ai-memory"))
                .unwrap_or_else(|| panic!("missing ai-memory entry for {event}"));
            assert!(ours.get("matcher").is_none(), "event: {event}");
        }
        assert_eq!(
            hooks["SessionStart"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| serde_json::to_string(entry)
                    .unwrap()
                    .contains("third-party"))
                .count(),
            1,
            "third-party hook must survive"
        );
        assert_eq!(hooks.len(), COMMAND_CODE_PROFILE.events.len());
    }

    #[test]
    fn overlay_event_hooks_inserts_when_event_absent() {
        let mut hooks = serde_json::Map::new();
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event stop --agent claude-code --server-url http://h" } ] }
        ]);
        overlay_event_hooks(&mut hooks, "Stop", &ours);
        assert_eq!(hooks["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn overlay_event_hooks_is_idempotent_on_reapply() {
        // Re-applying must not accumulate duplicate ai-memory entries.
        let mut hooks = serde_json::Map::new();
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event pre-tool-use --agent claude-code --server-url http://h" } ] }
        ]);
        overlay_event_hooks(&mut hooks, "PreToolUse", &ours);
        overlay_event_hooks(&mut hooks, "PreToolUse", &ours);
        assert_eq!(
            hooks["PreToolUse"].as_array().unwrap().len(),
            1,
            "no duplicates on re-apply"
        );
    }

    #[test]
    fn is_ai_memory_hook_entry_detects_nested_flat_and_skips_third_party() {
        // Nested (Claude Code / Codex / Gemini)
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event session-start --agent claude-code --server-url http://h" } ] }
        )));
        // Flat (Cursor) + shell form
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "type": "command", "command": "bash -c 'AI_MEMORY_HOOK_URL=x /c/x/ai-memory/hooks/pre.sh'" }
        )));
        // Claude Code exec form
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "matcher": "", "hooks": [ { "type": "command", "command": "C:\\bin\\ai-memory.exe", "args": ["hook", "--event", "session-start", "--agent", "claude-code", "--server-url", "http://h"] } ] }
        )));
        // Third-party must NOT be flagged
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "node context-mode-cache-heal.mjs" } ] }
        )));
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "C:\\bin\\third-party.exe", "args": ["hook", "--event", "session-start", "--agent", "claude-code", "--server-url", "http://h"] } ] }
        )));
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "C:\\bin\\ai-memory-helper.exe", "args": ["--check", "project"] } ] }
        )));
        // A third-party string command may use the same executable name, but
        // without either ai-memory hook signature it must not be replaced.
        assert!(!is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "/opt/alphaone/bin/ai-memory boot --quiet --limit 10 --budget-tokens 4096" } ] }
        )));
        // Legacy script commands can use any path, so the exact env marker is
        // the ownership signal rather than the script basename.
        assert!(is_ai_memory_hook_entry(&serde_json::json!(
            { "hooks": [ { "type": "command", "command": "AI_MEMORY_HOOK_URL=http://h /custom/session-start.sh" } ] }
        )));
    }

    #[test]
    fn overlay_event_hooks_preserves_third_party_string_command_and_replaces_legacy() {
        let third_party =
            "/opt/alphaone/bin/ai-memory boot --quiet --limit 10 --budget-tokens 4096";
        let legacy = "AI_MEMORY_HOOK_URL=http://old /custom/session-start.sh";
        let mut hooks = serde_json::Map::new();
        hooks.insert(
            "SessionStart".into(),
            serde_json::json!([
                { "matcher": "", "hooks": [ { "type": "command", "command": third_party } ] },
                { "matcher": "", "hooks": [ { "type": "command", "command": legacy } ] }
            ]),
        );
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "AI_MEMORY_HOOK_URL=http://new /fresh/session-start.sh" } ] }
        ]);

        overlay_event_hooks(&mut hooks, "SessionStart", &ours);

        let commands: Vec<&str> = hooks["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| {
                entry
                    .pointer("/hooks/0/command")
                    .and_then(serde_json::Value::as_str)
            })
            .collect();
        assert_eq!(
            commands,
            [
                third_party,
                "AI_MEMORY_HOOK_URL=http://new /fresh/session-start.sh"
            ]
        );
        assert!(!commands.contains(&legacy), "legacy entry must be replaced");
    }

    fn stub_scripts(dir: &Path, names: &[&str]) {
        for name in names {
            let p = dir.join(name);
            fs::write(&p, "#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&p).unwrap().permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&p, perms).unwrap();
            }
        }
    }

    fn default_hook_args() -> InstallHooksArgs {
        InstallHooksArgs {
            profile: None,
            agent: AgentChoice::OpenCode,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: None,
            server_url: None,
            auth_token: None,
            as_user: None,
            apply: true,
            config_file: None,
            project_strategy: Some(ProjectStrategyArg::Basename),
        }
    }

    // ── #issue project-strategy preservation on re-apply ─────────────

    #[test]
    fn project_strategy_from_text_reads_every_form() {
        // Shell env prefix (Claude Code / POSIX script hooks).
        assert_eq!(
            project_strategy_from_text(
                "AI_MEMORY_HOOK_URL=http://h AI_MEMORY_PROJECT_STRATEGY=repo-root /x/s.sh"
            ),
            Some(ProjectStrategyArg::RepoRoot)
        );
        // PowerShell form.
        assert_eq!(
            project_strategy_from_text("$env:AI_MEMORY_PROJECT_STRATEGY='repo-root'; & /x/s.ps1"),
            Some(ProjectStrategyArg::RepoRoot)
        );
        // Native flag, both spellings.
        assert_eq!(
            project_strategy_from_text("ai-memory hook --project-strategy repo-root run"),
            Some(ProjectStrategyArg::RepoRoot)
        );
        assert_eq!(
            project_strategy_from_text("ai-memory hook --project-strategy=repo_root"),
            Some(ProjectStrategyArg::RepoRoot)
        );
        assert_eq!(
            project_strategy_from_text("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            Some(ProjectStrategyArg::RepoRoot)
        );
    }

    #[test]
    fn baked_project_strategy_reads_owned_json_for_every_json_agent() {
        let existing = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [{
                        "command": "third-party",
                        "args": ["--project-strategy", "repo-root"]
                    }]},
                    { "hooks": [{
                        "command": "/bin/ai-memory",
                        "args": [
                            "hook", "--event", "session-start", "--agent", "claude-code",
                            "--server-url", "http://h", "--project-strategy", "repo-root"
                        ]
                    }]}
                ]
            }
        })
        .to_string();
        for agent in [
            AgentChoice::ClaudeCode,
            AgentChoice::Codex,
            AgentChoice::Cursor,
            AgentChoice::GeminiCli,
            AgentChoice::AntigravityCli,
            AgentChoice::Grok,
            AgentChoice::Zero,
            AgentChoice::Devin,
        ] {
            assert_eq!(
                baked_project_strategy(agent, &existing),
                Some(ProjectStrategyArg::RepoRoot),
                "{agent:?}"
            );
        }
    }

    #[test]
    fn baked_project_strategy_ignores_unowned_json_entry() {
        let existing = serde_json::json!({
            "hooks": {
                "SessionStart": [{ "hooks": [{
                    "command": "third-party",
                    "args": ["--project-strategy", "repo-root"]
                }]}]
            }
        })
        .to_string();
        assert_eq!(
            baked_project_strategy(AgentChoice::ClaudeCode, &existing),
            None
        );
    }

    #[test]
    fn baked_project_strategy_reads_owned_kimi_toml_only() {
        let existing = r#"
[[hooks]]
event = "UserPromptSubmit"
command = "third-party --project-strategy repo-root"

[[hooks]]
event = "UserPromptSubmit"
command = "AI_MEMORY_HOOK_URL=http://h AI_MEMORY_PROJECT_STRATEGY=repo-root /x/ai-memory/session-start.sh"
"#;
        assert_eq!(
            baked_project_strategy(AgentChoice::KimiCode, existing),
            Some(ProjectStrategyArg::RepoRoot)
        );
    }

    #[test]
    fn baked_project_strategy_reads_every_owned_generated_integration() {
        for (agent, name) in [
            (AgentChoice::OpenCode, "opencode"),
            (AgentChoice::Pi, "pi"),
            (AgentChoice::Omp, "omp"),
            (AgentChoice::Openclaw, "openclaw"),
        ] {
            let existing = format!(
                "// Auto-generated by `ai-memory install-hooks --agent {name} --apply`.\n\
                 const DEFAULT_PROJECT_STRATEGY = \"repo-root\";\n"
            );
            assert_eq!(
                baked_project_strategy(agent, &existing),
                Some(ProjectStrategyArg::RepoRoot),
                "{agent:?}"
            );
        }

        let unowned = "// user extension\nconst DEFAULT_PROJECT_STRATEGY = \"repo-root\";\n";
        assert_eq!(baked_project_strategy(AgentChoice::OpenCode, unowned), None);
    }

    #[test]
    fn install_project_strategy_preserves_baked_when_flag_absent() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::json!({
                "hooks": { "SessionStart": [{ "hooks": [{
                    "command": "AI_MEMORY_HOOK_URL=http://h AI_MEMORY_PROJECT_STRATEGY=repo-root /x/ai-memory/session-start.sh"
                }]}] }
            })
            .to_string(),
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(cfg),
            project_strategy: None,
            ..default_hook_args()
        };
        // A bare re-apply (e.g. `ai-memory upgrade`) must keep repo-root.
        assert_eq!(
            install_project_strategy(&args),
            Some(ProjectStrategyArg::RepoRoot)
        );
    }

    #[test]
    fn install_project_strategy_explicit_basename_overrides_existing() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::json!({
                "hooks": { "SessionStart": [{ "hooks": [{
                    "command": "AI_MEMORY_HOOK_URL=http://h AI_MEMORY_PROJECT_STRATEGY=repo-root /x/ai-memory/session-start.sh"
                }]}] }
            })
            .to_string(),
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(cfg),
            project_strategy: Some(ProjectStrategyArg::Basename),
            ..default_hook_args()
        };
        // Explicit basename is honored, not overridden by the baked repo-root.
        assert_eq!(
            install_project_strategy(&args),
            Some(ProjectStrategyArg::Basename)
        );
    }

    #[test]
    fn install_project_strategy_none_when_target_absent() {
        let tmp = TempDir::new().unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(tmp.path().join("nope.json")),
            project_strategy: None,
            ..default_hook_args()
        };
        assert_eq!(install_project_strategy(&args), None);
    }

    #[test]
    fn install_project_strategy_reads_openclaw_entrypoint_below_config_dir() {
        let tmp = TempDir::new().unwrap();
        let entrypoint = tmp.path().join(openclaw_plugin::ENTRYPOINT_TS);
        std::fs::write(
            entrypoint,
            "// Auto-generated by `ai-memory install-hooks --agent openclaw --apply`.\n\
             const DEFAULT_PROJECT_STRATEGY = \"repo-root\";\n",
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Openclaw,
            config_file: Some(tmp.path().to_path_buf()),
            project_strategy: None,
            ..default_hook_args()
        };
        assert_eq!(
            install_project_strategy(&args),
            Some(ProjectStrategyArg::RepoRoot)
        );
    }

    // ── P1.8 validate_as_user ────────────────────────────────────────

    /// No `--as-user` at all → always OK.
    #[test]
    fn validate_as_user_passes_when_not_set() {
        assert!(validate_as_user(None, None).is_ok());
        assert!(validate_as_user(None, Some("tok")).is_ok());
    }

    /// Empty / whitespace-only `--as-user` is treated as not-set.
    /// Defensive: an accidental `--as-user ""` shouldn't bail.
    #[test]
    fn validate_as_user_treats_blank_as_unset() {
        assert!(validate_as_user(Some(""), None).is_ok());
        assert!(validate_as_user(Some("   "), None).is_ok());
    }

    /// `--as-user` with no `--auth-token` is the error case the v0.8
    /// docs warn about — without a token the hook scripts authenticate
    /// anonymously / as root, making the `--as-user X` label misleading.
    #[test]
    fn validate_as_user_bails_without_auth_token() {
        let err = validate_as_user(Some("alice"), None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--as-user 'alice'") && msg.contains("--auth-token"),
            "error must name both flags: {msg}"
        );
        // Empty auth token is treated the same as missing.
        assert!(validate_as_user(Some("alice"), Some("")).is_err());
        assert!(validate_as_user(Some("alice"), Some("   ")).is_err());
    }

    /// `--as-user X --auth-token <something>` passes — the install
    /// proceeds with X as metadata and the supplied token as the
    /// bearer.
    #[test]
    fn validate_as_user_passes_with_both_flags() {
        assert!(validate_as_user(Some("alice"), Some("some-token")).is_ok());
    }

    #[test]
    fn manual_agent_render_mentions_repo_root_project_strategy() {
        let temp = TempDir::new().unwrap();
        stub_scripts(temp.path(), &["session-start.sh"]);
        for agent in ["codex", "cursor", "gemini-cli", "antigravity-cli"] {
            let output = render_agent_output(
                agent,
                temp.path(),
                "http://127.0.0.1:49374",
                None,
                Some("repo-root"),
                &[CODEX_PROFILE.events],
            );
            assert!(
                output.contains("AI_MEMORY_PROJECT_STRATEGY=repo-root"),
                "{agent} manual output must tell users to set the strategy env: {output}"
            );
        }
    }

    #[test]
    fn manual_agent_render_omits_project_strategy_by_default() {
        let temp = TempDir::new().unwrap();
        stub_scripts(temp.path(), &["session-start.sh"]);
        let output = render_agent_output(
            "codex",
            temp.path(),
            "http://127.0.0.1:49374",
            None,
            None,
            &[CODEX_PROFILE.events],
        );
        assert!(!output.contains("AI_MEMORY_PROJECT_STRATEGY"));
    }

    #[test]
    fn antigravity_manual_render_explains_explicit_finalization() {
        let temp = TempDir::new().unwrap();
        stub_scripts(temp.path(), &["session-start.sh", "stop.sh"]);
        let output = render_agent_output(
            "antigravity-cli",
            temp.path(),
            "http://127.0.0.1:49374",
            None,
            None,
            &[&ANTIGRAVITY_LIFECYCLE_EVENTS],
        );

        assert!(output.contains("`Stop` ends one execution loop, not the conversation"));
        assert!(
            output.contains("ai-memory finalize-session --agent antigravity-cli"),
            "Antigravity install output must expose the supported finalizer: {output}"
        );
    }

    #[test]
    fn manual_agent_render_uses_agent_profile_not_physical_bundle_listing() {
        let temp = TempDir::new().unwrap();
        stub_scripts(
            temp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "stop.sh",
                "subagent-start.sh",
                "subagent-stop.sh",
            ],
        );

        let gemini = render_agent_output(
            "gemini-cli",
            temp.path(),
            "http://127.0.0.1:49374",
            None,
            None,
            &[GEMINI_PROFILE.events],
        );
        assert!(gemini.contains("session-start"));
        assert!(gemini.contains("session-end"));
        assert!(
            !gemini.contains("user-prompt-submit")
                && !gemini.contains("subagent-start")
                && !gemini.contains("subagent-stop"),
            "Gemini manual output must omit scripts outside Gemini's hook vocabulary: {gemini}"
        );

        let codex = render_agent_output(
            "codex",
            temp.path(),
            "http://127.0.0.1:49374",
            None,
            None,
            &[CODEX_PROFILE.events],
        );
        assert!(codex.contains("stop"));
        assert!(
            !codex.contains("session-end") && !codex.contains("subagent-start"),
            "Codex manual output must omit scripts outside Codex's hook vocabulary: {codex}"
        );
    }

    #[test]
    fn hook_server_url_defaults_to_configured_server_url() {
        let config = Config {
            server_url: "http://192.168.0.90:49374/".into(),
            ..Config::default()
        };
        let args = default_hook_args();

        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://192.168.0.90:49374"
        );
    }

    #[test]
    fn hook_server_url_explicit_flag_wins_over_config() {
        let config = Config {
            server_url: "http://homelab:49374".into(),
            ..Config::default()
        };
        let mut args = default_hook_args();
        args.server_url = Some("http://explicit:49374/".into());

        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://explicit:49374"
        );
    }

    /// Regression (found 2026-07-12 during Devin real-acceptance A/B
    /// testing): an explicit `--server-url` that happens to equal the
    /// compiled-in `DEFAULT_SERVER_URL` must still win over a configured
    /// (env/config.toml) server_url pointing somewhere else. Before the
    /// `Option<String>` fix, `args.server_url` was a plain `String` with
    /// `default_value_t = DEFAULT_SERVER_URL`, so clap couldn't
    /// distinguish "operator explicitly typed the default value" from
    /// "operator passed nothing at all" — both produced the same string,
    /// so this exact case silently fell through to `AI_MEMORY_SERVER_URL`
    /// / config.toml instead of honouring the explicit flag.
    #[test]
    fn hook_server_url_explicit_flag_matching_compiled_default_still_wins() {
        let config = Config {
            server_url: "http://127.0.0.1:49375".into(),
            ..Config::default()
        };
        let mut args = default_hook_args();
        args.server_url = Some(DEFAULT_SERVER_URL.to_string());

        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            DEFAULT_SERVER_URL,
            "an explicit --server-url matching the compiled default must not be \
             silently overridden by a differently-configured server_url"
        );
    }

    /// Post-audit P1 — the new `ai-memory hook` subcommand (#84) builds
    /// its request URL by hand, skipping `Config::load` for latency. PR
    /// #82 made thin-client commands respect `AI_MEMORY_BASE_PATH` via
    /// `ServerEndpoint::build_url`, but the hook subcommand doesn't go
    /// through there — so a deployment under `--base-path /wiki` with
    /// the base set via env (not the URL path) had `ai-memory status`
    /// working and `ai-memory hook` 404'ing. Fix: install-hooks bakes
    /// the prefix into the URL it embeds, so hook.rs uses what it's
    /// given and stays unchanged.
    #[test]
    fn hook_server_url_threads_base_path_when_url_has_no_path() {
        let config = Config {
            server_url: "http://homelab:49374".into(),
            base_path: "/wiki".into(),
            ..Config::default()
        };
        let args = default_hook_args();
        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://homelab:49374/wiki",
            "URL baked into the hook command must carry the base-path so \
             `ai-memory hook` POSTs to /wiki/hook (not /hook)"
        );
    }

    /// If the operator already put the prefix into the URL itself, do
    /// NOT append `base_path` on top — that would double the prefix to
    /// `/wiki/wiki`.
    #[test]
    fn hook_server_url_does_not_double_base_path_when_already_in_url() {
        let config = Config {
            server_url: "http://homelab:49374/wiki".into(),
            base_path: "/wiki".into(),
            ..Config::default()
        };
        let args = default_hook_args();
        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://homelab:49374/wiki"
        );
    }

    #[test]
    fn hook_server_url_falls_back_to_existing_mcp_entry() {
        let config = Config::default();
        let args = default_hook_args();
        let inferred = InferredMcpConfig {
            hook_server_url: Some("http://homelab:49374".into()),
            auth_token: Some("tok".into()),
        };

        assert_eq!(
            effective_hook_server_url(&config, &args, Some(&inferred)),
            "http://homelab:49374"
        );
    }

    #[test]
    fn resolve_hooks_dir_uses_grok_bundle_for_grok() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("grok")).unwrap();
        fs::create_dir_all(tmp.path().join("claude-code")).unwrap();

        let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Grok, tmp.path()).unwrap();
        assert_eq!(resolved, tmp.path().join("grok"));
    }

    #[test]
    fn resolve_hooks_dir_uses_pool_bundle_for_pool() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("pool")).unwrap();
        fs::create_dir_all(tmp.path().join("claude-code")).unwrap();

        let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Pool, tmp.path()).unwrap();
        assert_eq!(resolved, tmp.path().join("pool"));
    }

    #[test]
    fn pool_settings_yaml_covers_all_events_with_bounded_entries() {
        let snippet = super::super::render_shared::build_pool_settings_yaml_with_data_dir(
            Path::new("/staging/pool"),
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Some(Path::new("/data")),
            Some("repo-root"),
        );
        assert!(snippet.starts_with("hooks:\n"), "snippet: {snippet}");
        for (event, script) in POOL_EVENTS {
            assert!(
                snippet.contains(&format!("  {event}:\n")),
                "missing {event} in: {snippet}"
            );
            let stem = script.strip_suffix(".sh").unwrap();
            assert!(
                snippet.contains(&format!("- name: ai-memory-{stem}\n")),
                "missing name for {event} in: {snippet}"
            );
        }
        assert_eq!(
            snippet.matches("matcher: \"*\"").count(),
            POOL_EVENTS.len(),
            "one wildcard matcher per event"
        );
        assert_eq!(
            snippet.matches("timeout: 20").count(),
            POOL_EVENTS.len(),
            "one bounded timeout per event"
        );
        // Every command is a YAML single-quoted scalar so the embedded POSIX
        // quoting survives; the auth token rides inside the command.
        assert_eq!(
            snippet.matches("command: '").count(),
            POOL_EVENTS.len(),
            "one single-quoted command per event"
        );
        assert!(snippet.contains("tok-test"), "auth token must be embedded");
    }

    #[test]
    fn render_pool_output_states_manual_paste_and_finalize_path() {
        let out = render_pool_output(
            Path::new("/staging/pool"),
            "http://127.0.0.1:49374",
            None,
            Path::new("/data"),
            None,
        );
        assert!(out.contains(".poolside/settings.yaml"));
        assert!(out.contains("does not write project-local files"));
        assert!(out.contains("finalize-session --agent pool"));
        assert!(out.contains("memory_handoff_accept"));
        assert!(out.contains("hooks:\n"));
        // No token was passed, so none of the auth caution lines render.
        assert!(!out.contains("AI_MEMORY_AUTH_TOKEN embedded"));
    }

    // Issue #156: Zero hook install writes exec-form entries into Zero's
    // hooks.json shape and merges around third-party hooks by id prefix.
    #[test]
    fn zero_hooks_config_covers_all_events_in_exec_form() {
        let payload = super::super::render_shared::build_zero_hooks_config(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Some(Path::new("/data")),
            Some("repo-root"),
        );
        assert_eq!(payload["enabled"], serde_json::json!(true));
        let hooks = payload["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 6, "one entry per Zero lifecycle event");
        let events: Vec<&str> = hooks.iter().map(|h| h["event"].as_str().unwrap()).collect();
        for zero_event in [
            "sessionStart",
            "sessionEnd",
            "beforeTool",
            "afterTool",
            "specialistStart",
            "specialistStop",
        ] {
            assert!(events.contains(&zero_event), "missing {zero_event}");
        }
        for hook in hooks {
            let id = hook["id"].as_str().unwrap();
            assert!(id.starts_with("ai-memory-"), "ownership prefix: {id}");
            assert!(hook["enabled"].as_bool().unwrap());
            let args: Vec<&str> = hook["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            // Exec form: the native hook subcommand with agent + auth +
            // strategy — never a shell string.
            assert!(args.contains(&"hook"), "{args:?}");
            assert!(
                args.contains(&"--agent") && args.contains(&"zero"),
                "{args:?}"
            );
            assert!(args.contains(&"--auth-token") && args.contains(&"tok-test"));
            assert!(args.contains(&"--project-strategy") && args.contains(&"repo-root"));
            assert!(args.contains(&"--data-dir") && args.contains(&"/data"));
        }
    }

    #[test]
    fn zero_apply_merges_around_third_party_hooks_and_preserves_disabled_flag() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hooks.json");
        fs::write(
            &path,
            r#"{"enabled": false, "hooks": [
                {"id": "my-custom-hook", "event": "beforeTool",
                 "command": "/usr/bin/true", "args": [], "enabled": true},
                {"id": "ai-memory-session-start", "event": "sessionStart",
                 "command": "/old/ai-memory", "args": [], "enabled": true}
            ]}"#,
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zero,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zero_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            root["enabled"],
            serde_json::json!(false),
            "a user-disabled hooks file must stay disabled (we warn instead)"
        );
        let hooks = root["hooks"].as_array().unwrap();
        assert!(
            hooks
                .iter()
                .any(|h| h["id"] == serde_json::json!("my-custom-hook")),
            "third-party hooks must survive the merge"
        );
        let ours: Vec<&serde_json::Value> = hooks
            .iter()
            .filter(|h| {
                h["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("ai-memory-"))
            })
            .collect();
        assert_eq!(
            ours.len(),
            6,
            "stale ai-memory entries replaced, not duplicated"
        );
        assert!(
            ours.iter()
                .all(|h| h["command"] != serde_json::json!("/old/ai-memory")),
            "the stale command path must be replaced"
        );
    }

    #[test]
    fn zcode_hooks_config_covers_all_events_with_strict_entries() {
        let payload = super::super::render_shared::build_zcode_hooks_config(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Some(Path::new("/data")),
            Some("repo-root"),
        );
        assert_eq!(payload["enabled"], serde_json::json!(true));
        // Raised above ZCode's 32 KiB default so a fetched handoff is not
        // truncated before injection.
        assert_eq!(payload["maxOutputBytes"], serde_json::json!(65536));
        let events = payload["events"].as_object().unwrap();
        assert_eq!(
            events.len(),
            super::super::render_shared::ZCODE_EVENTS.len(),
            "one entry per documented ZCode trigger"
        );
        for (zcode_event, our_event) in super::super::render_shared::ZCODE_EVENTS {
            let groups = events[zcode_event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{zcode_event}: one matcher group");
            let hooks = groups[0]["hooks"].as_array().unwrap();
            assert_eq!(hooks.len(), 1, "{zcode_event}: one hook entry");
            let hook = &hooks[0];
            // Exec form: `type: "process"` spawns command + args with no
            // shell, so every key must be from ZCode's documented schema —
            // undocumented keys make it drop the entry.
            assert_eq!(hook["type"], serde_json::json!("process"), "{zcode_event}");
            assert_eq!(
                hook["statusMessage"].as_str().unwrap(),
                "ai-memory capture",
                "{zcode_event}: ownership marker"
            );
            assert_eq!(hook["timeoutMs"], serde_json::json!(10_000));
            for key in hook.as_object().unwrap().keys() {
                assert!(
                    matches!(
                        key.as_str(),
                        "type" | "command" | "args" | "enabled" | "timeoutMs" | "statusMessage"
                    ),
                    "{zcode_event}: undocumented hook key {key}"
                );
            }
            let args: Vec<&str> = hook["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            assert!(args.contains(&"hook"), "{args:?}");
            assert!(
                args.contains(&"--event") && args.contains(&our_event),
                "{args:?}"
            );
            assert!(
                args.contains(&"--agent") && args.contains(&"zcode"),
                "{args:?}"
            );
            assert!(args.contains(&"--server-url") && args.contains(&"http://127.0.0.1:49374"));
            assert!(args.contains(&"--auth-token") && args.contains(&"tok-test"));
            assert!(args.contains(&"--data-dir") && args.contains(&"/data"));
            assert!(args.contains(&"--project-strategy") && args.contains(&"repo-root"));
        }
        // PostToolUseFailure fires instead of PostToolUse when the tool
        // throws; the loop above already proved it forwards onto the same
        // `post-tool-use` channel as its success sibling.
        assert!(events.contains_key("PostToolUseFailure"));
    }

    #[test]
    fn zcode_apply_preserves_siblings_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{
  "theme": "dark",
  "model": {"main": "zai/glm-5.1"},
  "mcp": {"servers": {"other": {"type": "http", "url": "https://other.example/mcp"}}},
  "hooks": {
    "enabled": true,
    "events": {
      "SessionStart": [
        {"matcher": "Bash", "hooks": [
          {"type": "process", "command": "/usr/bin/true", "args": [], "enabled": true}
        ]},
        {"hooks": [
          {"type": "process", "command": "/old/ai-memory", "args": [],
           "enabled": true, "statusMessage": "ai-memory capture"}
        ]}
      ]
    }
  }
}"#,
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();
        let first = fs::read_to_string(&path).unwrap();
        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();
        let second = fs::read_to_string(&path).unwrap();

        assert_eq!(first, second, "re-apply must be a no-op");
        let root: serde_json::Value = serde_json::from_str(&second).unwrap();
        // Sibling config keys survive: hooks share the file with the model
        // and MCP registration (#511).
        assert_eq!(root["theme"], serde_json::json!("dark"));
        assert_eq!(root["model"]["main"], serde_json::json!("zai/glm-5.1"));
        assert_eq!(
            root["mcp"]["servers"]["other"]["url"],
            serde_json::json!("https://other.example/mcp")
        );
        // The handoff-injection stdout ceiling lands with the hooks block.
        assert_eq!(root["hooks"]["maxOutputBytes"], serde_json::json!(65536));
        let session_start = root["hooks"]["events"]["SessionStart"].as_array().unwrap();
        assert_eq!(
            session_start.len(),
            2,
            "the third-party matcher group must survive the merge"
        );
        let third_party = &session_start[0];
        assert_eq!(
            third_party["matcher"],
            serde_json::json!("Bash"),
            "third-party matcher groups are preserved untouched"
        );
        assert_eq!(
            third_party["hooks"][0]["command"],
            serde_json::json!("/usr/bin/true")
        );
        let ours = &session_start[1];
        assert_eq!(
            ours["hooks"][0]["command"],
            super::super::render_shared::build_zcode_hooks_config(
                "http://127.0.0.1:49374",
                Some("tok-test"),
                Some(Path::new("/data")),
                None
            )["events"]["SessionStart"][0]["hooks"][0]["command"],
            "the stale ai-memory command path must be replaced"
        );
        for (zcode_event, _) in super::super::render_shared::ZCODE_EVENTS {
            assert!(
                root["hooks"]["events"][zcode_event].is_array(),
                "missing {zcode_event}"
            );
        }
    }
    #[test]
    fn zcode_apply_withdraws_our_hooks_from_foreign_keys_and_keeps_the_rest() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        // Keys outside `ZCODE_EVENTS`. ZCode rejects the whole block over
        // ones it does not recognize, which is how #600 stayed invisible.
        // `SessionEnd` runs nothing, `PreCompact` holds only our entry, and
        // `Notification` mixes our entry with a hook we did not write.
        fs::write(
            &path,
            r#"{
  "hooks": {
    "enabled": true,
    "events": {
      "SessionEnd": [],
      "PreCompact": [
        {"hooks": [
          {"type": "process", "command": "/old/ai-memory", "args": [],
           "enabled": true, "statusMessage": "ai-memory capture"}
        ]}
      ],
      "Notification": [
        {"hooks": [
          {"type": "process", "command": "/old/ai-memory", "args": [],
           "enabled": true, "statusMessage": "ai-memory capture"},
          {"type": "process", "command": "/usr/bin/true", "args": [], "enabled": true}
        ]}
      ]
    }
  }
}"#,
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();

        let first = fs::read_to_string(&path).unwrap();
        let root: serde_json::Value = serde_json::from_str(&first).unwrap();
        let events = root["hooks"]["events"].as_object().unwrap();

        // Event keys are never deleted: they are not ai-memory's to remove.
        assert!(events.contains_key("SessionEnd"));
        assert!(events.contains_key("PreCompact"));
        // Our own stale entry is withdrawn wherever it sits.
        assert_eq!(
            events["PreCompact"][0]["hooks"].as_array().unwrap().len(),
            0,
            "our entry under a key we no longer write must be withdrawn"
        );
        // The mixed group is the only case where the ownership filter
        // decides anything: the foreign hook survives, ours does not.
        let notification = events["Notification"][0]["hooks"].as_array().unwrap();
        assert_eq!(notification.len(), 1, "the hook we did not write must stay");
        assert_eq!(
            notification[0]["command"],
            serde_json::json!("/usr/bin/true")
        );
        for (zcode_event, _) in super::super::render_shared::ZCODE_EVENTS {
            assert!(
                root["hooks"]["events"][zcode_event].is_array(),
                "missing {zcode_event}"
            );
        }

        // Re-apply is still a no-op once the withdrawal has happened.
        apply_to_zcode_hooks(
            "http://127.0.0.1:49374",
            Some("tok-test"),
            Path::new("/data"),
            &args,
        )
        .unwrap();
        assert_eq!(first, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn zcode_event_runs_nothing_covers_degenerate_matcher_groups() {
        // Each of these leaves the key present but unable to run anything,
        // which is what makes ZCode reject the block while the file still
        // looks settled (#600).
        for shape in [
            serde_json::json!([]),
            serde_json::json!([{}]),
            serde_json::json!([{"matcher": "*"}]),
            serde_json::json!([{"hooks": []}]),
            serde_json::json!([{"hooks": null}]),
        ] {
            assert!(
                zcode_event_runs_nothing(&shape),
                "expected {shape} to run nothing"
            );
        }
        assert!(!zcode_event_runs_nothing(&serde_json::json!([
            {"hooks": [{"type": "process", "command": "/usr/bin/true"}]}
        ])));
        // A value that is not a matcher-group array cannot run hooks either,
        // and is doubly invalid to ZCode. It is left unmodified, but it is
        // still reported: staying quiet about it is the #600 failure.
        assert!(zcode_event_runs_nothing(&serde_json::json!(42)));
    }

    #[test]
    fn zcode_apply_flags_a_user_disabled_hooks_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(&path, r#"{"hooks": {"enabled": false, "events": {}}}"#).unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            root["hooks"]["enabled"],
            serde_json::json!(false),
            "a user-disabled hooks block must stay disabled (we warn instead)"
        );
        assert_eq!(
            root["hooks"]["events"].as_object().unwrap().len(),
            super::super::render_shared::ZCODE_EVENTS.len()
        );
    }

    #[test]
    fn zcode_apply_respects_a_user_chosen_output_ceiling() {
        // `maxOutputBytes` is block-level and would also bound third-party
        // hooks, so a value the user chose is never overwritten.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{"hooks": {"enabled": true, "maxOutputBytes": 4096, "events": {}}}"#,
        )
        .unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Zcode,
            capture_assistant: false,
            config_file: Some(path.clone()),
            ..default_hook_args()
        };

        apply_to_zcode_hooks("http://127.0.0.1:49374", None, Path::new("/data"), &args).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["hooks"]["maxOutputBytes"], serde_json::json!(4096));
    }

    #[test]
    fn resolve_hooks_dir_uses_devin_bundle_for_devin() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("devin")).unwrap();
        fs::create_dir_all(tmp.path().join("claude-code")).unwrap();

        let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Devin, tmp.path()).unwrap();
        assert_eq!(resolved, tmp.path().join("devin"));
    }

    #[test]
    fn opencode_mcp_inference_supplies_hook_origin_and_token() {
        let inferred = infer_json_mcp_config(
            r#"{
              "mcp": {
                "ai-memory": {
                  "type": "remote",
                  "url": "http://homelab:49374/mcp",
                  "headers": { "Authorization": "Bearer secret-token" }
                }
              }
            }"#,
            &["mcp", "ai-memory"],
            "url",
        )
        .unwrap();

        assert_eq!(
            inferred.hook_server_url.as_deref(),
            Some("http://homelab:49374")
        );
        assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
    }

    /// Inferring the hook URL from Kimi Code's flavored mcp.json entry
    /// must yield the bare origin — hooks POST to `<origin>/hook`.
    #[test]
    fn kimi_code_mcp_inference_strips_flavor_query_and_mcp_path() {
        let inferred = infer_json_mcp_config(
            r#"{
              "mcpServers": {
                "ai-memory": {
                  "url": "http://homelab:49374/mcp?flavor=moonshot",
                  "headers": { "Authorization": "Bearer secret-token" }
                }
              }
            }"#,
            &["mcpServers", "ai-memory"],
            "url",
        )
        .unwrap();

        assert_eq!(
            inferred.hook_server_url.as_deref(),
            Some("http://homelab:49374")
        );
        assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn hook_server_url_from_mcp_url_strips_query_and_suffix() {
        for (input, expected) in [
            ("http://homelab:49374/mcp", Some("http://homelab:49374")),
            ("http://homelab:49374", Some("http://homelab:49374")),
            ("http://homelab:49374/", Some("http://homelab:49374")),
            (
                "http://homelab:49374/mcp?flavor=moonshot",
                Some("http://homelab:49374"),
            ),
            (
                "http://homelab:49374/mcp/?flavor=moonshot",
                Some("http://homelab:49374"),
            ),
            // Reverse-proxy prefix survives; hooks POST under it.
            (
                "http://homelab:49374/wiki/mcp",
                Some("http://homelab:49374/wiki"),
            ),
            (
                "http://homelab:49374/wiki/mcp?flavor=moonshot",
                Some("http://homelab:49374/wiki"),
            ),
            ("", None),
            ("   ", None),
        ] {
            assert_eq!(
                hook_server_url_from_mcp_url(input).as_deref(),
                expected,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn codex_mcp_inference_accepts_block_form_config() {
        let inferred = infer_toml_mcp_config(
            r#"[mcp_servers.ai-memory]
url = "http://homelab:49374/mcp"

[mcp_servers.ai-memory.http_headers]
Authorization = "Bearer secret-token"
"#,
        )
        .unwrap();

        assert_eq!(
            inferred.hook_server_url.as_deref(),
            Some("http://homelab:49374")
        );
        assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn grok_mcp_inference_accepts_headers_key() {
        let inferred = infer_toml_mcp_config(
            r#"[mcp_servers.ai-memory]
url = "http://homelab:49374/mcp"
enabled = true

[mcp_servers.ai-memory.headers]
Authorization = "Bearer secret-token"
"#,
        )
        .unwrap();

        assert_eq!(
            inferred.hook_server_url.as_deref(),
            Some("http://homelab:49374")
        );
        assert_eq!(inferred.auth_token.as_deref(), Some("secret-token"));
    }

    /// Regression for issue #53 — `install-hooks --agent codex` used to
    /// panic with "index not found" when `~/.codex/config.toml` had an
    /// `[mcp_servers]` table populated with *other* servers (context7,
    /// node_repl, …) but no ai-memory entry. A perfectly valid setup —
    /// ai-memory can live in Codex via hooks only without being an MCP
    /// server — must return None, not abort the whole install.
    #[test]
    fn codex_mcp_inference_returns_none_when_ai_memory_entry_missing() {
        let inferred = infer_toml_mcp_config(
            r#"[mcp_servers.context7]
url = "http://localhost:9000/mcp"

[mcp_servers.node_repl]
command = "npx"
args = ["node-repl"]
"#,
        );
        assert!(
            inferred.is_none(),
            "missing [mcp_servers.ai-memory] must yield None, got {inferred:?}"
        );
    }

    /// Same regression class — no `[mcp_servers]` table at all means
    /// the user is on a hooks-only / fresh config; we should return
    /// None rather than panic on the first index.
    #[test]
    fn codex_mcp_inference_returns_none_when_no_mcp_servers_table() {
        let inferred = infer_toml_mcp_config(
            r#"# fresh codex config
model = "gpt-5"
"#,
        );
        assert!(inferred.is_none());
    }

    /// And the empty-file edge case the parser still accepts.
    #[test]
    fn codex_mcp_inference_returns_none_for_empty_doc() {
        assert!(infer_toml_mcp_config("").is_none());
    }

    /// An ai-memory entry that exists but ships neither a `url` nor an
    /// `Authorization` header still falls back to None (caller infers
    /// from defaults). Distinguishes "config absent" from "config
    /// present but unhelpful" — both yield None, neither panics.
    #[test]
    fn codex_mcp_inference_returns_none_for_bare_ai_memory_entry() {
        let inferred = infer_toml_mcp_config(
            r#"[mcp_servers.ai-memory]
# intentionally empty — no url, no headers.
"#,
        );
        assert!(inferred.is_none());
    }

    #[test]
    fn bundled_posix_and_powershell_hooks_stay_in_parity() {
        let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("hooks");
        assert!(
            hooks_root.join("lib").join("ai-memory-hook.ps1").is_file(),
            "PowerShell hooks require the shared lib helper"
        );

        // Enumerate the bundles instead of listing them: a hardcoded
        // list silently skips whatever agent lands next, which is how
        // `command-code` and `kiro-cli` shipped uncovered. `lib/` holds
        // the shared PowerShell helper, not an agent bundle.
        let mut agent_dirs: Vec<String> = fs::read_dir(&hooks_root)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", hooks_root.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .filter_map(|path| path.file_name()?.to_str().map(str::to_string))
            .filter(|name| name != "lib")
            .collect();
        agent_dirs.sort();
        assert!(
            !agent_dirs.is_empty(),
            "no agent hook bundles found under {}",
            hooks_root.display()
        );

        for agent_dir in agent_dirs {
            let agent_dir = agent_dir.as_str();
            let dir = hooks_root.join(agent_dir);
            let mut sh = BTreeMap::new();
            let mut ps1 = BTreeMap::new();
            for entry in fs::read_dir(&dir).unwrap_or_else(|e| {
                panic!("failed to read bundled hook dir {}: {e}", dir.display())
            }) {
                let path = entry.unwrap().path();
                if !path.is_file() {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                match path.extension().and_then(|s| s.to_str()) {
                    Some("sh") => {
                        sh.insert(stem.to_string(), extract_sh_hook_metadata(&path));
                    }
                    Some("ps1") => {
                        ps1.insert(stem.to_string(), extract_ps1_hook_metadata(&path));
                    }
                    _ => {}
                }
            }
            assert_eq!(
                sh.keys().collect::<Vec<_>>(),
                ps1.keys().collect::<Vec<_>>(),
                "{agent_dir}: every .sh hook must have a .ps1 peer"
            );
            for (stem, sh_meta) in sh {
                assert_eq!(
                    Some(sh_meta),
                    ps1.remove(&stem),
                    "{agent_dir}/{stem}: .sh and .ps1 must post the same event/agent"
                );
            }
        }
    }

    fn extract_sh_hook_metadata(path: &Path) -> (String, String) {
        let text = fs::read_to_string(path).unwrap();
        let marker = "hook?event=";
        let start = text
            .find(marker)
            .unwrap_or_else(|| panic!("{} missing hook endpoint", path.display()))
            + marker.len();
        let rest = &text[start..];
        let event = rest
            .split('&')
            .next()
            .unwrap_or_else(|| panic!("{} missing event", path.display()))
            .to_string();
        let agent_marker = "&agent=";
        let agent_start = rest
            .find(agent_marker)
            .unwrap_or_else(|| panic!("{} missing agent", path.display()))
            + agent_marker.len();
        let agent = rest[agent_start..]
            .split(['"', '\'', ' ', '\n', '\r', '$'])
            .next()
            .unwrap_or_else(|| panic!("{} missing agent value", path.display()))
            .to_string();
        (event, agent)
    }

    #[test]
    fn devin_bundle_has_no_subagent_scripts() {
        let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("hooks");
        let devin_dir = hooks_root.join("devin");

        for entry in fs::read_dir(&devin_dir).unwrap() {
            let path = entry.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            assert!(
                !stem.contains("subagent"),
                "Devin bundle should not contain subagent scripts, found: {}",
                path.display()
            );
        }
    }

    #[test]
    fn devin_bundle_has_post_compaction_not_pre_compact() {
        let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("hooks");
        let devin_dir = hooks_root.join("devin");

        assert!(
            devin_dir.join("post-compaction.sh").is_file(),
            "Devin bundle must have post-compaction.sh"
        );
        assert!(
            devin_dir.join("post-compaction.ps1").is_file(),
            "Devin bundle must have post-compaction.ps1"
        );
        assert!(
            !devin_dir.join("pre-compact.sh").exists(),
            "Devin bundle should not have pre-compact.sh"
        );
        assert!(
            !devin_dir.join("pre-compact.ps1").exists(),
            "Devin bundle should not have pre-compact.ps1"
        );
    }

    fn extract_ps1_hook_metadata(path: &Path) -> (String, String) {
        let text = fs::read_to_string(path).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains("Invoke-AiMemoryHook"))
            .unwrap_or_else(|| panic!("{} missing Invoke-AiMemoryHook", path.display()));
        (
            extract_ps1_arg(line, "Event", path),
            extract_ps1_arg(line, "Agent", path),
        )
    }

    fn extract_ps1_arg(line: &str, name: &str, path: &Path) -> String {
        let marker = format!("-{name} \"");
        let start = line
            .find(&marker)
            .unwrap_or_else(|| panic!("{} missing {name} argument", path.display()))
            + marker.len();
        line[start..]
            .split('"')
            .next()
            .unwrap_or_else(|| panic!("{} missing {name} value", path.display()))
            .to_string()
    }

    // ----------------------------------------------------------------
    // Shared `_lib.sh` staging
    // ----------------------------------------------------------------

    /// `stage_hook_scripts` copies the parent dir's `_lib.sh` alongside
    /// the agent's event scripts so the runtime layout doesn't depend
    /// on the source-tree shape. This is the only piece of evidence we
    /// have that the marker-file walk-up helper actually ships — the
    /// scripts themselves source it with `. "$(dirname "$0")/_lib.sh"`
    /// and a missing helper would surface as a runtime "command not
    /// found" much further from the cause.
    /// #581: inside the docker wrapper the server data dir is a volume
    /// the host cannot execute from; staging must fall back to the
    /// home-based path the wrapper bind-mounts and reads. Natively, the
    /// resolved data dir keeps winning (#554/#573).
    #[test]
    fn staging_root_leaves_the_container_volume_for_home() {
        let data = Path::new("/data");
        assert_eq!(
            hook_staging_root(data, false),
            PathBuf::from("/data"),
            "native installs stage under the resolved data dir"
        );
        let in_container = hook_staging_root(data, true);
        assert_ne!(
            in_container,
            PathBuf::from("/data"),
            "container staging must not target the volume"
        );
        assert_eq!(
            in_container,
            dirs::data_local_dir().unwrap().join("ai-memory"),
            "container staging targets the wrapper's bind-mounted home contract"
        );
    }

    #[test]
    fn stage_hook_scripts_copies_shared_lib_sh() {
        // Distinct agent_label per test: `stage_hook_scripts` writes
        // under `dirs::data_local_dir()/.../hooks/<agent_label>` and
        // the test binary runs cases in parallel, so two tests using
        // the same label race on the same staging dir.
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("hooks");
        let agent_src = bundle.join("stage-shared-lib");
        fs::create_dir_all(&agent_src).unwrap();
        fs::write(bundle.join("_lib.sh"), "# shared helper\n").unwrap();
        stub_scripts(&agent_src, &["session-start.sh", "post-tool-use.sh"]);

        let data_dir = tmp.path().join("data");
        let staged = stage_hook_scripts_in(&agent_src, "stage-shared-lib", &data_dir).unwrap();
        assert!(staged.join("session-start.sh").exists());
        assert!(staged.join("post-tool-use.sh").exists());
        assert!(
            staged.join("_lib.sh").exists(),
            "_lib.sh must be staged alongside event scripts",
        );

        let lib = fs::read_to_string(staged.join("_lib.sh")).unwrap();
        assert!(
            lib.contains("shared helper"),
            "staged _lib.sh must match the source-of-truth"
        );
    }

    /// Skipping `_lib.sh` is fine — older source bundles without the
    /// marker-walk-up feature should still install cleanly.
    #[test]
    fn stage_hook_scripts_tolerates_missing_lib_sh() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("hooks");
        let agent_src = bundle.join("stage-no-lib");
        fs::create_dir_all(&agent_src).unwrap();
        // Note: no _lib.sh in `bundle`.
        stub_scripts(&agent_src, &["session-start.sh"]);

        let data_dir = tmp.path().join("data");
        let staged = stage_hook_scripts_in(&agent_src, "stage-no-lib", &data_dir).unwrap();
        assert!(staged.join("session-start.sh").exists());
        assert!(!staged.join("_lib.sh").exists());
    }

    /// Regression for issue #52 — when `resolve_hooks_dir` picks the
    /// data-local dir as the source bundle (the docker `setup-agent`
    /// flow extracts scripts there) AND the staging destination is
    /// the *same* dir, the pre-fix wipe-then-copy loop would delete
    /// every populated script and report `staged 0`. The same-path
    /// branch must verify in place without wiping, so existing scripts
    /// survive a re-run.
    #[test]
    fn stage_hook_scripts_preserves_in_place_scripts_when_source_equals_dest() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let agent_label = "stage-in-place";
        // Simulate "scripts already extracted into the data dir's hooks
        // dir by a prior `setup-agent` run".
        let in_place = data_dir.join("hooks").join(agent_label);
        fs::create_dir_all(&in_place).unwrap();
        stub_scripts(&in_place, &["session-start.sh", "post-tool-use.sh"]);

        // Source == destination (this is what resolve_hooks_dir hands
        // us when no other candidate exists).
        let staged = stage_hook_scripts_in(&in_place, agent_label, &data_dir).unwrap();

        assert_eq!(staged, in_place, "destination must canonicalize to source");
        assert!(
            staged.join("session-start.sh").is_file(),
            "in-place script must survive the same-path branch (not be wiped)"
        );
        assert!(
            staged.join("post-tool-use.sh").is_file(),
            "in-place script must survive the same-path branch (not be wiped)"
        );
    }

    /// Regression for issue #52 — the failure that the reporter actually
    /// hit: `resolve_hooks_dir` resolved to a pre-existing but empty
    /// data-local dir, so source == dest and there's nothing to verify.
    /// The pre-fix code silently returned Ok with `copied = 0` and the
    /// caller went on to rewrite `settings.json` against an empty dir,
    /// disabling capture without any error. We must bail with an
    /// actionable message instead.
    #[test]
    fn stage_hook_scripts_bails_when_source_equals_empty_dest() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let agent_label = "stage-empty-in-place";
        // Must equal what `stage_hook_scripts_in` computes, or this stops
        // being the source==dest case it exists to cover and duplicates the
        // different-paths test below (#554 moved the destination shape).
        let in_place = data_dir.join("hooks").join(agent_label);
        fs::create_dir_all(&in_place).unwrap();
        // Intentionally no scripts in `in_place`.

        let err = stage_hook_scripts_in(&in_place, agent_label, &data_dir)
            .expect_err("an empty source dir must produce a hard error, not Ok(0)");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no hook scripts"),
            "error should call out the empty source: {msg}"
        );
        assert!(
            msg.contains("--hooks-dir") || msg.contains("setup-agent"),
            "error should point at the workaround (--hooks-dir or setup-agent): {msg}"
        );
    }

    /// Regression for issue #52 — same fail-on-zero guard applies even
    /// when source and dest are different paths (e.g. user pointed
    /// `--hooks-dir` at the wrong dir). Previously this also silently
    /// returned Ok with `copied = 0`.
    #[test]
    fn stage_hook_scripts_bails_when_source_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("hooks");
        let agent_src = bundle.join("stage-empty-src");
        fs::create_dir_all(&agent_src).unwrap();
        // Source dir exists but has no scripts.

        let data_dir = tmp.path().join("data");
        let err = stage_hook_scripts_in(&agent_src, "stage-empty-src", &data_dir)
            .expect_err("zero scripts should be an error, not a silent success");
        assert!(format!("{err:#}").contains("no hook scripts"));
    }

    /// #554: with `AI_MEMORY_DATA_DIR` set, `install-hooks` probed and staged
    /// under the *platform default* instead of the data dir in use — so the
    /// server logged one path while `--apply` reported staging into another,
    /// silently populating a directory the operator was not using.
    #[test]
    fn a_configured_data_dir_is_where_the_bundle_is_probed_and_staged() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("custom-data");
        let source = tmp.path().join("bundle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("session-start.sh"), b"#!/bin/sh\n").unwrap();

        // Staged into the configured dir…
        let staged = stage_hook_scripts_in(&source, "claude-code", &data_dir).unwrap();
        assert_eq!(staged, data_dir.join("hooks").join("claude-code"));
        assert!(staged.join("session-start.sh").is_file());
        assert!(
            !tmp.path().join("ai-memory").exists(),
            "nothing may be written under a platform-default path"
        );

        // …and the probe looks in the same place, so the two halves agree.
        let candidates = hook_source_candidates("claude-code", None, None, Some(data_dir.clone()));
        assert!(
            candidates.contains(&data_dir.join("hooks").join("claude-code")),
            "the configured data dir must be probed; got {candidates:?}"
        );
    }

    /// The default install must be byte-identical to the old behaviour:
    /// `default_data_dir()` is `data_local_dir()/ai-memory`, so
    /// `<data_dir>/hooks/<agent>` is the same path the previous
    /// `<data_local>/ai-memory/hooks/<agent>` produced.
    #[test]
    fn the_default_data_dir_stages_to_the_same_path_as_before() {
        let tmp = TempDir::new().unwrap();
        let data_local = tmp.path().join(".local").join("share");
        let default_data_dir = data_local.join("ai-memory");
        let source = tmp.path().join("bundle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("session-start.sh"), b"#!/bin/sh\n").unwrap();

        let staged = stage_hook_scripts_in(&source, "claude-code", &default_data_dir).unwrap();
        assert_eq!(
            staged,
            data_local
                .join("ai-memory")
                .join("hooks")
                .join("claude-code"),
            "existing installs must keep staging exactly where they did"
        );
    }

    /// #552, the regression guard: `--apply` must persist the bearer under the
    /// data dir and leave it out of the agent config entirely.
    ///
    /// Before this, the token was written into the rendered command — as
    /// `--auth-token <token>` for native hooks — so it sat in the agent's
    /// config file *and* in `/proc/<pid>/cmdline` for the lifetime of every
    /// hook, on every tool call.
    #[test]
    fn apply_persists_the_bearer_and_keeps_it_out_of_the_agent_config() {
        let home = TempDir::new().unwrap();
        let cfg_dir = TempDir::new().unwrap();
        let settings = cfg_dir.path().join("settings.json");
        std::fs::write(&settings, "{}").unwrap();

        let config = crate::config::Config::load(None, Some(home.path().to_path_buf())).unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            apply: true,
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: Some("SEKRIT-BEARER-552".to_string()),
            config_file: Some(settings.clone()),
            // Point at the repo's bundle explicitly. Without this the test
            // leans on hook-dir discovery, which from `target/debug/deps`
            // walks to `<repo>/target` and then falls through to whatever the
            // machine happens to have staged — passing for a reason that has
            // nothing to do with what is under test.
            hooks_dir: Some(
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks"),
            ),
            ..default_hook_args()
        };

        run(&config, args).expect("apply should succeed");

        let rendered = std::fs::read_to_string(&settings).unwrap();
        assert!(
            !rendered.contains("SEKRIT-BEARER-552"),
            "the bearer must not be written into the agent config: {rendered}"
        );
        assert!(
            !rendered.contains("--auth-token"),
            "no --auth-token flag may reach a hook command line: {rendered}"
        );

        assert_eq!(
            crate::config::read_hook_auth_token(&config.data_dir).as_deref(),
            Some("SEKRIT-BEARER-552"),
            "the bearer must be persisted where the hook can read it"
        );
        assert!(
            std::fs::read_to_string(crate::config::hook_auth_header_path_in(&config.data_dir))
                .unwrap()
                .contains("SEKRIT-BEARER-552"),
            "the curl header file must carry it too"
        );
    }

    #[test]
    fn hook_source_candidates_include_native_package_dir() {
        let candidates = hook_source_candidates(
            "claude-code",
            Some(PathBuf::from("/repo")),
            Some(PathBuf::from("/opt/ai-memory")),
            // The resolved data dir, not the platform data-local root (#554).
            Some(PathBuf::from("/home/alice/.local/share/ai-memory")),
        );

        assert_eq!(candidates[0], PathBuf::from("/repo/hooks/claude-code"));
        assert_eq!(
            candidates[1],
            PathBuf::from("/opt/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[2],
            PathBuf::from("/usr/local/share/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[3],
            PathBuf::from("/usr/share/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[4],
            PathBuf::from("/home/alice/.local/share/ai-memory/hooks/claude-code")
        );
    }

    /// #546: `~/.local/bin/ai-memory -> <repo>/target/release/ai-memory` is
    /// what the quick start's `~/.local/bin` layout produces for a source
    /// build. Discovery is derived from the binary's own location, so the
    /// link has to be resolved first or every candidate misses the checkout.
    #[cfg(unix)]
    #[test]
    fn hooks_are_found_through_a_symlinked_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let exe = repo.join("target").join("release").join("ai-memory");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"").unwrap();
        let link_dir = tmp.path().join("home").join(".local").join("bin");
        fs::create_dir_all(&link_dir).unwrap();
        let link = link_dir.join("ai-memory");
        std::os::unix::fs::symlink(&exe, &link).unwrap();

        let resolved = resolve_exe_path(link);
        let candidates = hook_source_candidates(
            "claude-code",
            repo_root_from_exe(&resolved),
            resolved.parent().map(Path::to_path_buf),
            None,
        );

        assert_eq!(
            candidates[0],
            fs::canonicalize(&repo)
                .unwrap()
                .join("hooks")
                .join("claude-code"),
            "the checkout must be the first candidate, not the symlink's ancestor"
        );
    }

    #[test]
    fn resolve_exe_path_keeps_an_unresolvable_path() {
        let missing = PathBuf::from("/definitely/not/here/ai-memory");
        assert_eq!(resolve_exe_path(missing.clone()), missing);
    }

    #[test]
    fn hook_source_candidates_include_binary_sibling_for_flat_tarball() {
        // Extracted release tarball: no repo root, `hooks/` beside the binary
        // (issue #107). The sibling dir must be probed or discovery fails with
        // a bogus `/private/hooks/...` on macOS.
        let candidates = hook_source_candidates(
            "claude-code",
            None,
            Some(PathBuf::from("/private/tmp/ai-memory-macos-aarch64")),
            None,
        );
        assert!(
            candidates.contains(&PathBuf::from(
                "/private/tmp/ai-memory-macos-aarch64/hooks/claude-code"
            )),
            "binary-sibling hooks/ dir must be probed; got {candidates:?}"
        );
    }

    // ----------------------------------------------------------------
    // OpenCode tests
    // ----------------------------------------------------------------

    fn assert_generated_ts_uses_bounded_hook_queue(generated: &str) {
        assert!(generated.contains("const HOOK_QUEUE_MAX = 100;"));
        assert!(generated.contains("const HOOK_FLUSH_INTERVAL_MS = 2000;"));
        assert!(generated.contains("const HOOK_FLUSH_THRESHOLD = 20;"));
        assert!(generated.contains("const HOOK_INTER_REQUEST_DELAY_MS = 50;"));
        assert!(generated.contains("const HOOK_REQUEST_TIMEOUT_MS = 2000;"));
        assert!(generated.contains("const HOOK_IMMEDIATE_EVENTS = new Set([\"session-start\", \"stop\", \"session-end\", \"pre-compact\"]);"));
        assert!(generated.contains("const hookQueue: HookQueueItem[] = [];"));
        assert!(generated.contains(
            "function enqueueHook(event: string, url: URL, payload: Record<string, unknown>): void"
        ));
        assert!(generated.contains("if (hookQueue.length >= HOOK_QUEUE_MAX) hookQueue.shift();"));
        assert!(generated.contains(
            "HOOK_IMMEDIATE_EVENTS.has(event) || hookQueue.length >= HOOK_FLUSH_THRESHOLD"
        ));
        assert!(generated.contains("function scheduleHookFlush(): void"));
        assert!(generated.contains("hookFlushTimer.unref?.();"));
        assert!(generated.contains("async function drainHookQueue(): Promise<void>"));
        assert!(generated.contains("signal: timeoutSignal(HOOK_REQUEST_TIMEOUT_MS)"));
        assert!(generated.contains("await sleep(HOOK_INTER_REQUEST_DELAY_MS)"));
        assert!(generated.contains("const policy = capturePolicy(payload"));
        assert!(generated.contains("if (policy.disposition === \"drop\") return;"));
        assert!(generated.contains("enqueueHook(event, url, policy.payload);"));
        assert!(generated.contains("const CAPTURE_POLICY_V1 = 1;"));
        assert!(generated.contains("const CAPTURE_MARKER_MAX_BYTES = 64 * 1024;"));
        assert!(generated.contains("async function fetchHandoff"));
        assert!(generated.contains("if (!process.env.AI_MEMORY_RUN_ID) {"));
        assert!(generated.contains("const response = await fetch(url, {"));
        assert!(generated.contains("signal: timeoutSignal(1000)"));
        assert!(!generated.contains("signal: timeoutSignal(500)"));
        assert!(!generated.contains("void fetch(url, {"));
    }

    #[test]
    fn generated_ts_integrations_spool_failed_deliveries() {
        // #580: every queue-based TS integration must persist a failed
        // delivery instead of dropping it, and must self-drain the spool.
        for (name, source) in [
            (
                "opencode",
                build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None),
            ),
            (
                "omp",
                build_omp_extension("http://127.0.0.1:49374", Some("tok"), None),
            ),
            (
                "pi",
                build_pi_extension("http://127.0.0.1:49374", Some("tok"), None),
            ),
        ] {
            // The fire-and-forget delivery must be gone...
            assert!(
                !source.contains("}).catch(() => undefined);\n      } catch (_e) {"),
                "{name}: still fire-and-forgets failed deliveries"
            );
            // ...replaced by capture + spool of network failures and 5xx.
            assert!(
                source.contains(
                    "if (!resp || resp.status >= 500) spoolFailedHook(item.url, item.payload);"
                ),
                "{name}: missing spool-on-failure"
            );
            // The spool runtime is present and CLI-compatible.
            assert!(source.contains("function spoolFailedHook("), "{name}");
            assert!(source.contains("async function drainHookSpool()"), "{name}");
            assert!(
                source.contains(r#"return join(env, "hook-spool");"#),
                "{name}: spool dir must honour AI_MEMORY_DATA_DIR"
            );
            // Every queue flush also kicks a spool drain.
            assert!(
                source.contains("if (hookDraining) return;\n  requestSpoolDrain();"),
                "{name}: drainHookQueue must trigger the spool drain"
            );
            // The runtime's fs needs made it into the import line.
            for f in [
                "mkdirSync",
                "writeFileSync",
                "renameSync",
                "readdirSync",
                "unlinkSync",
            ] {
                assert!(
                    source.contains(&format!("{f},")) || source.contains(&format!("{f} }}")),
                    "{name}: node:fs import missing {f}"
                );
            }
        }
    }

    #[test]
    fn ts_spool_entries_are_readable_by_the_cli_drain() {
        // The TS runtime writes the CLI hook-spool's exact on-disk contract.
        // Mirror the bytes the generated code produces and prove the CLI
        // side parses them: entry JSON deserializes, the filename's
        // timestamp is honoured, and the body survives round-tripping.
        let created_ms: u64 = 1_756_800_000_000;
        // Filename exactly as the TS builds it:
        // `${String(createdMs).padStart(13, "0")}-${process.pid}-${seq}.json`
        let name = format!("{created_ms:013}-4242-{:016x}.json", 7u64);
        // Entry JSON exactly as the TS `JSON.stringify` emits it (key order
        // irrelevant to serde, but the shapes and enum strings are load-bearing).
        let json = format!(
            r#"{{"url":"http://127.0.0.1:49374/hook?event=stop&agent=open-code&ingest_key=ts18f3","body":"{{\"session_id\":\"s1\"}}","created_ms":{created_ms},"auth_mode":"static","token":"tok","attempts":0}}"#
        );
        let entry: crate::commands::hook_spool::SpoolEntry =
            serde_json::from_str(&json).expect("CLI drain must parse a TS-written entry");
        assert_eq!(entry.created_ms, created_ms);
        assert_eq!(entry.token.as_deref(), Some("tok"));
        assert_eq!(entry.attempts, 0);
        assert!(entry.url.contains("ingest_key=ts18f3"));
        serde_json::from_str::<serde_json::Value>(&entry.body)
            .expect("TS body must be raw JSON, not double-encoded");

        // And the anonymous variant (no token installed).
        let anon = r#"{"url":"http://h/hook?event=stop&agent=omp","body":"{}","created_ms":1,"auth_mode":"none","attempts":0}"#;
        serde_json::from_str::<crate::commands::hook_spool::SpoolEntry>(anon)
            .expect("auth_mode none must parse");

        // The timestamp-bearing filename is one the spool reader accepts:
        // spool_health derives oldest-age from names without reading files.
        let dir = tempfile::tempdir().expect("tempdir");
        let spool = dir.path().join("hook-spool");
        std::fs::create_dir_all(&spool).expect("mkdir");
        std::fs::write(spool.join(&name), json.as_bytes()).expect("write");
        assert_eq!(crate::commands::hook_spool::spool_len(&spool), 1);
        let health = crate::commands::hook_spool::spool_health(&spool);
        assert_eq!(health.pending, 1);
        assert!(
            health.oldest_age_ms.is_some(),
            "TS filename's 13-digit timestamp must be readable: {name}"
        );
    }

    #[test]
    fn opencode_plugin_uses_real_plugin_hooks() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None);

        assert!(plugin.contains("event: async (input)"));
        assert!(plugin.contains(r#""chat.message": async"#));
        assert!(plugin.contains(r#""tool.execute.before": async"#));
        assert!(plugin.contains(r#""tool.execute.after": async"#));
        assert!(plugin.contains(r#""experimental.chat.system.transform": async"#));
        assert!(plugin.contains("export default AiMemoryHooks"));
        assert!(plugin.contains("const startedSessions = new Set<string>();"));
        assert!(
            plugin
                .contains("const handoffFetches = new Map<string, Promise<string | undefined>>();")
        );
        assert!(!plugin.contains("handoffChecked"));
        assert!(plugin.contains("function startSession"));
        assert!(plugin.contains("function endSession"));
        assert!(plugin.contains("fetchHandoff"));
        assert!(plugin.contains("function applyMarkerParams"));
        assert!(plugin.contains("readFileSync(marker, \"utf8\")"));
        assert!(plugin.contains("text.split(/\\r?\\n/)"));
        assert!(plugin.contains("tomlKey(body, \"project_strategy\")"));
        assert!(plugin.contains("tomlKey(body, \"drop_subagent_captures\")"));
        assert!(plugin.contains("url.searchParams.set(\"project_strategy\", projectStrategy)"));
        assert!(plugin.contains("url.searchParams.set(\"drop_subagent\", dropSubagent)"));
        assert!(plugin.contains("function tomlFlag"));
        assert!(plugin.contains("tomlFlag(body, \"default_global\")"));
        assert!(plugin.contains("tomlFlag(body, \"inject_on_session_start\")"));
        assert!(plugin.contains("url.searchParams.set(\"briefing_budget\", briefingBudget)"));
        assert!(plugin.contains(
            "applyMarkerParams(url, typeof payload.cwd === \"string\" ? payload.cwd : undefined);"
        ));
        assert!(plugin.contains("applyMarkerParams(url, cwd);"));
        assert!(plugin.contains("postPreCompact"));
        assert!(plugin.contains("dispose: async () =>"));
        assert!(plugin.contains("const HOOK_DISPOSE_DRAIN_BUDGET_MS = 2000;"));
        assert!(plugin.contains("let hookDrainPromise: Promise<void> | undefined;"));
        assert!(plugin.contains("function requestHookDrain(): Promise<void>"));
        assert!(plugin.contains("function disposeDrainTimeout(): Promise<void>"));
        assert!(plugin.contains("timer.unref?.();"));
        assert!(plugin.contains("async function drainHookQueueForDispose(): Promise<void>"));
        assert!(plugin.contains("for (const id of Array.from(startedSessions))"));
        assert!(plugin.contains("await drainHookQueueForDispose();"));
        assert!(plugin.contains("postHook(\"session-start\""));
        assert!(plugin.contains(r#""session.deleted")"#));
        assert_eq!(
            plugin.matches("postHook(\"session-end\"").count(),
            1,
            "OpenCode generated plugin must route session closes through one idempotent helper"
        );
        assert!(plugin.contains("!startedSessions.delete(id)"));
        assert!(plugin.contains("sessionCwds.delete(id);"));
        assert!(plugin.contains("handoffFetches.delete(id);"));
        assert!(plugin.contains("preCompactLast.delete(id);"));
        assert!(plugin.contains("postHook(\"user-prompt\""));
        assert!(plugin.contains("Bearer ${TOKEN}"));
        assert!(plugin.contains("tok"));
        assert!(
            !plugin.contains(r#""session.created": async"#),
            "OpenCode bus events must be handled through the `event` hook"
        );
        assert!(plugin.contains("import { execFileSync } from \"node:child_process\";"));
        assert!(
            plugin.contains("import { basename, dirname, join, resolve, sep } from \"node:path\";")
        );
        assert!(plugin.contains("if (existsSync(join(probe, \".git\")))"));
        assert!(plugin.contains("boundary ??= dir;"));
        assert!(plugin.contains("function repoRootProject"));
        assert!(plugin.contains("--git-common-dir"));
        assert!(
            plugin
                .contains("projectStrategy === \"repo-root\" || projectStrategy === \"repo_root\"")
        );
        assert!(plugin.contains("url.searchParams.set(\"project\", repoProject)"));
    }

    #[test]
    fn opencode_plugin_normalizes_payloads_without_legacy_wrapper() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374/", None, None);

        assert!(plugin.contains("const SERVER = \"http://127.0.0.1:49374/\".replace"));
        assert!(plugin.contains("const TOKEN: string | null = null;"));
        assert!(plugin.contains("sessionID: id,"));
        assert!(plugin.contains("cwd,"));
        assert!(plugin.contains("prompt: textFromParts"));
        assert!(plugin.contains("output: (output as any).output"));
        assert!(plugin.contains("if (typeof AbortSignal === \"undefined\")"));
        assert!(
            !plugin.contains("hook_event_name"),
            "new plugin should send normalized top-level fields, not legacy wrappers"
        );
    }

    #[test]
    fn opencode_plugin_bakes_repo_root_default() {
        let plugin =
            build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), Some("repo-root"));
        assert!(
            plugin.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            "repo-root install default must bake the const: {plugin}"
        );
        assert!(
            plugin.contains("if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;"),
            "must apply the default when a marker pins no strategy: {plugin}"
        );
        assert!(
            plugin.contains("if (repoProject) project = repoProject;"),
            "{plugin}"
        );
    }

    #[test]
    fn opencode_plugin_default_omits_baked_strategy() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None);
        assert!(
            !plugin.contains("DEFAULT_PROJECT_STRATEGY"),
            "basename default must bake no strategy: {plugin}"
        );
    }

    #[test]
    fn opencode_plugin_uses_bounded_hook_queue() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"), None);

        assert_generated_ts_uses_bounded_hook_queue(&plugin);
    }

    // ----------------------------------------------------------------
    // OMP tests
    // ----------------------------------------------------------------

    #[test]
    fn omp_extension_uses_native_lifecycle_events() {
        let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None);

        assert!(extension.contains("export default function AiMemoryExtension"));
        assert!(extension.contains("const AGENT = \"omp\";"));
        assert!(extension.contains("api.on(\"session_start\""));
        assert!(extension.contains("api.on(\"before_agent_start\""));
        assert!(extension.contains("api.on(\"tool_call\""));
        assert!(extension.contains("api.on(\"tool_result\""));
        assert!(extension.contains("api.on(\"session_shutdown\""));
        assert!(extension.contains("postHook(\"session-start\""));
        assert!(extension.contains("postHook(\"user-prompt\""));
        assert!(extension.contains("fetchHandoff"));
        assert!(extension.contains("if (!process.env.AI_MEMORY_RUN_ID) {"));
        assert!(extension.contains("function applyMarkerParams"));
        assert!(extension.contains("readFileSync(marker, \"utf8\")"));
        assert!(extension.contains("text.split(/\\r?\\n/)"));
        assert!(extension.contains("tomlKey(body, \"project_strategy\")"));
        assert!(extension.contains("tomlKey(body, \"drop_subagent_captures\")"));
        assert!(extension.contains("url.searchParams.set(\"project_strategy\", projectStrategy)"));
        assert!(extension.contains("url.searchParams.set(\"drop_subagent\", dropSubagent)"));
        assert!(extension.contains("function tomlFlag"));
        assert!(extension.contains("tomlFlag(body, \"default_global\")"));
        assert!(extension.contains("tomlFlag(body, \"inject_on_session_start\")"));
        assert!(extension.contains("url.searchParams.set(\"briefing_budget\", briefingBudget)"));
        assert!(extension.contains(
            "applyMarkerParams(url, typeof payload.cwd === \"string\" ? payload.cwd : undefined);"
        ));
        assert!(extension.contains("applyMarkerParams(url, cwd);"));
        assert!(extension.contains("Bearer ${TOKEN}"));
        assert!(extension.contains("tok"));
        assert!(
            extension
                .contains("import { basename, dirname, join, resolve, sep } from \"node:path\";")
        );
        assert!(extension.contains("if (existsSync(join(probe, \".git\")))"));
        assert!(extension.contains("boundary ??= dir;"));
        assert!(extension.contains("import { execFileSync } from \"node:child_process\";"));
        assert!(extension.contains("function repoRootProject"));
        assert!(extension.contains("--git-common-dir"));
        assert!(
            extension
                .contains("projectStrategy === \"repo-root\" || projectStrategy === \"repo_root\"")
        );
        assert!(extension.contains("url.searchParams.set(\"project\", repoProject)"));
    }

    #[test]
    fn omp_extension_bakes_repo_root_default() {
        let extension =
            build_omp_extension("http://127.0.0.1:49374", Some("tok"), Some("repo-root"));
        assert!(
            extension.contains("const DEFAULT_PROJECT_STRATEGY = \"repo-root\";"),
            "repo-root install default must bake the const: {extension}"
        );
        assert!(
            extension.contains("if (!projectStrategy) projectStrategy = DEFAULT_PROJECT_STRATEGY;"),
            "{extension}"
        );
    }

    #[test]
    fn omp_extension_default_omits_baked_strategy() {
        let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None);
        assert!(
            !extension.contains("DEFAULT_PROJECT_STRATEGY"),
            "{extension}"
        );
    }

    #[test]
    fn omp_extension_uses_bounded_hook_queue() {
        let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"), None);

        assert_generated_ts_uses_bounded_hook_queue(&extension);
    }

    /// The manual (non-`--apply`) instructions must name the same path
    /// `--apply` would write to. Printing the `~/.pi/agent` default to an
    /// operator who set `PI_CODING_AGENT_DIR` would tell them to hand-copy the
    /// extension exactly where the agent never loads it — reintroducing, in
    /// the manual path, the silent no-capture this change fixes.
    #[test]
    fn extension_hints_follow_pi_coding_agent_dir() {
        let custom = if cfg!(windows) {
            r"C:\custom\pi-agent"
        } else {
            "/custom/pi-agent"
        };
        let env = || Some(std::ffi::OsString::from(custom));

        for hint in [
            omp_extension_hint_in(env(), None),
            pi_extension_hint_in(env()),
        ] {
            assert!(
                hint.contains("custom")
                    && (hint.ends_with("ai-memory-omp.ts") || hint.ends_with("ai-memory-pi.ts")),
                "hint must point at the relocated agent home, got: {hint}"
            );
            assert!(
                !hint.contains(".pi/agent") && !hint.contains(".omp/agent"),
                "hint must not advertise the default home when the var is set: {hint}"
            );
        }

        // Unset: the hints keep naming the documented defaults. Build the
        // expected tail from path components rather than hardcoding `/` —
        // `Path::display` renders `\` on Windows, so a slash-literal suffix
        // asserts a path shape that platform never produces.
        for (agent_home, filename, hint) in [
            (
                ".omp",
                "ai-memory-omp.ts",
                omp_extension_hint_in(None, None),
            ),
            (".pi", "ai-memory-pi.ts", pi_extension_hint_in(None)),
        ] {
            let tail: PathBuf = [agent_home, "agent", "extensions", filename]
                .iter()
                .collect();
            assert!(
                hint.ends_with(&tail.display().to_string()),
                "unset must keep the documented default, got: {hint}"
            );
        }
    }

    /// Precedence must not depend on what the override directory is *named*.
    /// An earlier revision re-rooted only when the last component happened to
    /// be "agent", so `/custom/agent` and `/custom/pi-agent` behaved
    /// differently for the same inputs.
    #[test]
    fn omp_profile_precedence_is_independent_of_the_override_dir_name() {
        let a = omp_extension_path_in(
            Some(std::ffi::OsString::from("/custom/agent")),
            Some("work"),
        )
        .unwrap();
        let b = omp_extension_path_in(
            Some(std::ffi::OsString::from("/custom/pi-agent")),
            Some("work"),
        )
        .unwrap();

        // The override names the agent dir in both cases; the profile does
        // not re-root it, and neither result invents a `profiles/` segment.
        assert!(
            a.ends_with("agent/extensions/ai-memory-omp.ts"),
            "got {a:?}"
        );
        assert!(
            b.ends_with("pi-agent/extensions/ai-memory-omp.ts"),
            "got {b:?}"
        );
        for p in [&a, &b] {
            assert!(
                !p.to_string_lossy().contains("profiles"),
                "PI_CODING_AGENT_DIR names the agent dir outright: {p:?}"
            );
        }
    }

    /// With no override, `--profile` relocates to the documented
    /// `~/.omp/profiles/<name>/agent/extensions`.
    #[test]
    fn omp_profile_relocates_when_no_override_is_set() {
        let path = omp_extension_path_in(None, Some("work")).unwrap();
        assert!(
            path.ends_with(".omp/profiles/work/agent/extensions/ai-memory-omp.ts"),
            "got {path:?}"
        );

        let plain = omp_extension_path_in(None, None).unwrap();
        assert!(
            plain.ends_with(".omp/agent/extensions/ai-memory-omp.ts"),
            "got {plain:?}"
        );
    }

    /// `remove_legacy_extension` must delete only what this tool generated.
    /// `uninstall` gates every delete on the same marker
    /// (`uninstall_omp_extension_deletes_only_generated_file`), so an
    /// unconditional delete on the install path would destroy exactly what
    /// uninstall refuses to touch.
    #[test]
    fn legacy_cleanup_spares_a_user_owned_file_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("ai-memory.ts");
        let user_content = "// user-owned extension that happens to use this filename\n";
        std::fs::write(&legacy, user_content).unwrap();

        remove_legacy_extension(&dir.path().join("ai-memory-omp.ts"), "omp");

        assert!(legacy.exists(), "user-owned file must survive");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), user_content);
    }

    /// The generated marker (banner + agent constant) is what authorises
    /// removal.
    #[test]
    fn legacy_cleanup_removes_our_own_superseded_file() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("ai-memory.ts");
        std::fs::write(
            &legacy,
            "// Auto-generated by `ai-memory install-hooks --agent omp --apply`\nconst AGENT = \"omp\";\n",
        )
        .unwrap();

        remove_legacy_extension(&dir.path().join("ai-memory-omp.ts"), "omp");

        assert!(
            !legacy.exists(),
            "our own superseded file should be removed"
        );
    }

    /// Pi and OMP can share one extensions directory, so a Pi install must
    /// not delete an OMP-generated legacy file — that would silently stop
    /// OMP capturing until someone reinstalled it.
    #[test]
    fn legacy_cleanup_does_not_touch_the_other_agents_file() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("ai-memory.ts");
        let omp_content = "// Auto-generated by `ai-memory install-hooks --agent omp --apply`\nconst AGENT = \"omp\";\n";
        std::fs::write(&legacy, omp_content).unwrap();

        // A *Pi* install runs its cleanup over the shared directory.
        remove_legacy_extension(&dir.path().join("ai-memory-pi.ts"), "pi");

        assert!(legacy.exists(), "OMP's file must survive a Pi install");
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), omp_content);
    }

    /// `PI_CODING_AGENT_DIR` relocates Pi's whole agent config home
    /// (`~/.pi/agent`), not just session storage, so the extension must follow
    /// the variable or Pi never loads it — the install reports success and
    /// capture silently does nothing.
    #[test]
    fn pi_extension_path_honours_pi_coding_agent_dir() {
        let custom = if cfg!(windows) {
            r"C:\custom\pi-agent"
        } else {
            "/custom/pi-agent"
        };
        let path = pi_extension_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
        assert_eq!(
            path,
            Path::new(custom).join("extensions").join("ai-memory-pi.ts")
        );

        // Unset and blank both fall back to ~/.pi/agent/extensions/ai-memory-pi.ts.
        // Blank counts as unset because an exported-but-empty variable is nearly
        // always a failed shell expansion, not a request to install into the
        // filesystem root.
        for env in [None, Some(std::ffi::OsString::new())] {
            let path = pi_extension_path_in(env).unwrap();
            assert!(
                path.ends_with(
                    Path::new(".pi")
                        .join("agent")
                        .join("extensions")
                        .join("ai-memory-pi.ts")
                ),
                "default must be ~/.pi/agent/extensions/ai-memory-pi.ts, got {}",
                path.display()
            );
        }
    }

    /// Same relocation contract as Pi: OMP honors `PI_CODING_AGENT_DIR` for
    /// the default profile and moves `~/.omp/agent` wholesale, extensions
    /// included.
    #[test]
    fn omp_extension_path_honours_pi_coding_agent_dir() {
        let custom = if cfg!(windows) {
            r"C:\custom\omp-agent"
        } else {
            "/custom/omp-agent"
        };
        let path = omp_extension_path_in(Some(std::ffi::OsString::from(custom)), None).unwrap();
        assert_eq!(
            path,
            Path::new(custom)
                .join("extensions")
                .join("ai-memory-omp.ts")
        );

        for env in [None, Some(std::ffi::OsString::new())] {
            let path = omp_extension_path_in(env, None).unwrap();
            assert!(
                path.ends_with(
                    Path::new(".omp")
                        .join("agent")
                        .join("extensions")
                        .join("ai-memory-omp.ts")
                ),
                "default must be ~/.omp/agent/extensions/ai-memory-omp.ts, got {}",
                path.display()
            );
        }
    }

    #[test]
    fn omp_extension_path_honours_profile() {
        let path = omp_extension_path_in(None, Some("my-profile")).unwrap();
        assert!(
            path.ends_with(
                Path::new(".omp")
                    .join("profiles")
                    .join("my-profile")
                    .join("agent")
                    .join("extensions")
                    .join("ai-memory-omp.ts")
            ),
            "profile path must be correct, got {}",
            path.display()
        );
    }

    #[test]
    fn omp_extension_is_directly_discoverable_by_omp() {
        let tmp = TempDir::new().unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Omp,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: None,
            server_url: Some("http://127.0.0.1:49374".into()),
            auth_token: None,
            as_user: None,
            apply: true,
            config_file: Some(tmp.path().join("extensions").join("ai-memory-omp.ts")),
            project_strategy: Some(ProjectStrategyArg::Basename),
            profile: None,
        };

        let path = resolve_omp_extension_path(&args).unwrap();
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("ai-memory-omp.ts")
        );
        assert_eq!(
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str()),
            Some("extensions")
        );
    }

    #[test]
    fn pi_extension_is_directly_discoverable_by_pi() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("extensions").join("ai-memory-pi.ts");
        let args = InstallHooksArgs {
            agent: AgentChoice::Pi,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: None,
            server_url: Some("http://127.0.0.1:49374".into()),
            auth_token: None,
            as_user: None,
            apply: true,
            config_file: Some(path.clone()),
            project_strategy: Some(ProjectStrategyArg::Basename),
            profile: None,
        };

        let resolved = resolve_pi_extension_path(&args).unwrap();

        assert_eq!(resolved, path);
        assert_eq!(
            resolved.file_name().and_then(|s| s.to_str()),
            Some("ai-memory-pi.ts")
        );
        assert_eq!(
            resolved
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str()),
            Some("extensions")
        );
    }

    #[test]
    fn pi_extension_contains_lifecycle_capture_and_mcp_bridge() {
        let extension = build_pi_extension("http://127.0.0.1:49374/base", Some("tok"), None);

        assert!(extension.contains("export default function AiMemoryExtension(pi: any): void"));
        assert!(extension.contains("const AGENT = \"pi\";"));
        assert!(extension.contains("pi.on(\"session_start\""));
        assert!(extension.contains("pi.on(\"before_agent_start\""));
        assert!(extension.contains("pi.on(\"tool_call\""));
        assert!(extension.contains("pi.on(\"tool_result\""));
        assert!(extension.contains("pi.on(\"session_before_compact\""));
        assert!(extension.contains("pi.on(\"session_compact\""));
        assert!(!extension.contains("pi.on(\"session.compacting\""));
        assert!(extension.contains("pi.on(\"agent_end\""));
        assert!(extension.contains("pi.on(\"session_shutdown\""));
        assert!(extension.contains("postHook(\"session-start\""));
        assert!(extension.contains("postHook(\"user-prompt\""));
        assert!(extension.contains("postHook(\"pre-tool-use\""));
        assert!(extension.contains("postHook(\"post-tool-use\""));
        assert!(extension.contains("postHook(\"pre-compact\""));
        assert!(extension.contains("postHook(\"stop\""));
        assert!(extension.contains("postHook(\"session-end\""));
        assert!(extension.contains("fetchHandoff"));
        assert!(extension.contains("customType: \"ai-memory-handoff\""));
        assert!(extension.contains("const MCP_SERVER = deriveMcpServer(SERVER);"));
        assert!(
            extension.contains("return trimmed.endsWith(\"/mcp\") ? trimmed : `${trimmed}/mcp`;")
        );
        assert!(extension.contains("\"Accept\": \"application/json, text/event-stream\""));
        assert!(extension.contains("...authHeaders()"));
        assert!(extension.contains("headers[\"X-Memory-Actor-Session-Id\"] = session;"));
        assert!(extension.contains("headers[\"Mcp-Session-Id\"] = session;"));
        assert!(extension.contains("function mcpSignal(signal?: AbortSignal)"));
        assert!(extension.contains("anyFactory([signal, timeout])"));
        assert!(extension.contains("mcpRpc(\"initialize\""));
        assert!(extension.contains("mcpRpc(\"notifications/initialized\""));
        assert!(extension.contains("mcpRpc(\"tools/list\""));
        assert!(extension.contains("pi.registerTool"));
        assert!(extension.contains("label: tool.name"));
        assert!(extension.contains("parameters: toolInputSchema(tool)"));
        assert!(extension.contains(
            "mcpRpc(\"tools/call\", { name: tool.name, arguments: params ?? {} }, ctx, signal)"
        ));
        assert!(extension.contains("payload?.error"));
        assert!(extension.contains("payload?.result?.isError"));
        assert!(extension.contains("response.ok"));
        assert!(extension.contains("signal: mcpSignal(signal)"));
        assert!(extension.contains("Bearer ${TOKEN}"));
        assert!(extension.contains("tok"));
        assert!(extension.contains("import { execFileSync } from \"node:child_process\";"));
        assert!(!extension.contains(".omp"));
        assert!(!extension.contains("serve --transport stdio"));
        assert!(!extension.contains("serve --stdio"));
    }

    // Windows 11 + Git Bash support matters for regulated enterprise setups
    // where Git Bash is the approved shell available from the corporate
    // repository, so this installer contract should be exercised anywhere
    // Bash is the supported execution surface.
    #[cfg(any(unix, windows))]
    #[test]
    fn curl_installer_accepts_generated_integration_agents() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("install-hooks.sh");
        let Some(bash) = bash_program_for_installer_test() else {
            return;
        };

        for alias in ["opencode", "openclaw", "omp", "oh-my-pi", "pi"] {
            let output = Command::new(&bash)
                .arg(&script)
                .arg("--agent")
                .arg(alias)
                .output()
                .unwrap_or_else(|e| {
                    panic!("failed to run {} for alias {alias}: {e}", script.display())
                });

            assert!(
                output.status.success(),
                "script rejected generated integration alias {alias}: stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let stdout = String::from_utf8_lossy(&output.stdout);
            match alias {
                "opencode" => assert!(stdout.contains("install-hooks --agent opencode --apply")),
                "openclaw" => assert!(stdout.contains("install-hooks --agent openclaw --apply")),
                "omp" | "oh-my-pi" => {
                    assert!(stdout.contains("install-hooks --agent omp --apply"));
                    assert!(stdout.contains("~/.omp/agent/extensions/ai-memory.ts"));
                }
                "pi" => {
                    assert!(stdout.contains("install-hooks --agent pi --apply"));
                    assert!(stdout.contains("~/.pi/agent/extensions/ai-memory.ts"));
                    assert!(stdout.contains("MCP tools come through the same generated bridge"));
                    assert!(!stdout.contains("~/.omp/agent/extensions/ai-memory.ts"));
                }
                _ => unreachable!(),
            }
        }
    }

    // ----------------------------------------------------------------
    // Cursor tests
    // ----------------------------------------------------------------

    #[test]
    fn cursor_preserves_existing_user_hooks_and_adds_ours() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.json");
        // Pre-existing settings with a user hook under a different event.
        fs::write(
            &config_path,
            r#"{"version":1,"hooks":{"userHook":"something"}}"#,
        )
        .unwrap();

        merge_cursor_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // User's hook survives.
        assert_eq!(parsed["hooks"]["userHook"], "something");
        // Our hooks are present.
        assert!(
            parsed["hooks"]["sessionStart"].is_array(),
            "sessionStart hook should be present"
        );
        assert!(
            parsed["hooks"]["preToolUse"].is_array(),
            "preToolUse hook should be present"
        );
        assert_eq!(
            parsed["version"], 1,
            "version: 1 must be set at the top level"
        );
    }

    #[test]
    fn cursor_apply_is_idempotent() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.json");

        let first = merge_cursor_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_ne!(
            first,
            ApplyOutcome::NoOp,
            "first apply should not be a no-op"
        );

        let second = merge_cursor_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_eq!(second, ApplyOutcome::NoOp, "second apply must be a no-op");
    }

    // ----------------------------------------------------------------
    // Codex tests
    // ----------------------------------------------------------------

    #[test]
    fn codex_preserves_unrelated_keys_and_adds_hooks() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.json");
        // Pre-existing settings with an unrelated key.
        fs::write(&config_path, r#"{"theme":"dark"}"#).unwrap();

        merge_codex_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // Unrelated key survives.
        assert_eq!(parsed["theme"], "dark");
        // Our hooks are present.
        assert!(
            parsed["hooks"]["SessionStart"].is_array(),
            "SessionStart hook should be present"
        );
        assert!(
            parsed["hooks"].get("SessionEnd").is_none(),
            "Codex has no reliable true SessionEnd hook; install must omit it"
        );
    }

    #[test]
    fn codex_removes_stale_session_end_key() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.json");
        // Simulate a file with a stale SessionEnd entry from a previous
        // install that mistakenly included the Claude-Code-only event.
        fs::write(
            &config_path,
            r#"{"hooks":{"SessionEnd":[{"matcher":"","hooks":[{"type":"command","command":"stale.sh"}]}]}}"#,
        )
        .unwrap();

        merge_codex_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // SessionEnd must be gone.
        assert!(
            parsed["hooks"].get("SessionEnd").is_none(),
            "stale SessionEnd must be removed; got: {:?}",
            parsed["hooks"]
        );
        // Our hooks are present.
        assert!(parsed["hooks"]["SessionStart"].is_array());
    }

    /// The test-only Codex wrapper must stage scripts under its injected
    /// data-local root and wire that stable path into the generated config.
    #[test]
    fn codex_apply_stages_into_injected_dir() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.json");
        let staging_tmp = TempDir::new().unwrap();

        apply_to_codex_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &InstallHooksArgs {
                profile: None,
                agent: AgentChoice::Codex,
                capture_assistant: false,
                no_capture_prompts: false,
                capture_mode: None,
                capture_prompts: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: Some(ProjectStrategyArg::Basename),
                as_user: None,
                apply: false,
            },
        )
        .unwrap();

        let staged_script = staging_tmp
            .path()
            .join("hooks")
            .join("codex")
            .join("session-start.sh");
        assert!(
            staged_script.is_file(),
            "expected hook script staged at {}, override was not honoured",
            staged_script.display()
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let command = parsed
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(serde_json::Value::as_str)
            .expect("SessionStart command should be present");
        assert!(
            command.contains(&staged_script.to_string_lossy().into_owned()),
            "generated command must reference staged script {}: {command}",
            staged_script.display()
        );
    }

    // ----------------------------------------------------------------
    // Gemini tests
    // ----------------------------------------------------------------

    #[test]
    fn gemini_preserves_mcp_servers_and_adds_hooks() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("settings.json");
        // Pre-existing settings with an mcpServers entry.
        fs::write(&config_path, r#"{"mcpServers":{"foo":{}}}"#).unwrap();

        merge_gemini_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // The pre-existing mcpServers.foo survives.
        assert!(
            parsed["mcpServers"]["foo"].is_object(),
            "mcpServers.foo must survive"
        );
        // Our hooks are present with Gemini-specific event names.
        assert!(
            parsed["hooks"]["SessionStart"].is_array(),
            "SessionStart hook should be present"
        );
        assert!(
            parsed["hooks"]["BeforeTool"].is_array(),
            "BeforeTool hook should be present"
        );
        // Claude-Code-only events must NOT appear.
        assert!(
            parsed["hooks"].get("PreToolUse").is_none(),
            "PreToolUse must not appear in Gemini config"
        );
    }

    // ----------------------------------------------------------------
    // Antigravity tests
    // ----------------------------------------------------------------

    #[test]
    fn antigravity_preserves_existing_hooks_and_adds_ours() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.json");
        // Pre-existing settings with another named hook group.
        fs::write(
            &config_path,
            r#"{"my-linter":{"PostToolUse":[{"matcher":"run_command","hooks":[{"type":"command","command":"lint.sh"}]}]}}"#,
        )
        .unwrap();

        merge_antigravity_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // The pre-existing my-linter group survives.
        assert!(
            parsed["my-linter"]["PostToolUse"].is_array(),
            "my-linter.PostToolUse must survive"
        );
        // Our named group "ai-memory" is present.
        assert!(
            parsed["ai-memory"]["PreInvocation"].is_array(),
            "PreInvocation hook should be present"
        );
        assert!(
            parsed["ai-memory"]["PreToolUse"].is_array(),
            "PreToolUse hook should be present"
        );
        assert!(
            parsed["ai-memory"]["PostToolUse"].is_array(),
            "PostToolUse hook should be present"
        );
        assert!(
            parsed["ai-memory"]["Stop"].is_array(),
            "Stop hook should be present"
        );
    }

    #[test]
    fn antigravity_apply_is_idempotent() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.json");

        let first = merge_antigravity_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_ne!(
            first,
            ApplyOutcome::NoOp,
            "first apply should not be a no-op"
        );

        let second = merge_antigravity_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_eq!(second, ApplyOutcome::NoOp, "second apply must be a no-op");
    }

    // ----------------------------------------------------------------
    // Kimi Code tests
    // ----------------------------------------------------------------

    const KIMI_CODE_STUB_SCRIPTS: [&str; 9] = [
        "session-start.sh",
        "user-prompt-submit.sh",
        "pre-tool-use.sh",
        "post-tool-use.sh",
        "pre-compact.sh",
        "stop.sh",
        "session-end.sh",
        "subagent-start.sh",
        "subagent-stop.sh",
    ];

    fn kimi_code_hooks_in(config_path: &Path) -> Vec<(String, String)> {
        let doc: toml_edit::DocumentMut = fs::read_to_string(config_path)
            .unwrap()
            .parse()
            .expect("config.toml must stay valid TOML");
        doc.get("hooks")
            .and_then(toml_edit::Item::as_array_of_tables)
            .expect("`hooks` must be an array of tables")
            .iter()
            .map(|entry| {
                let event = entry
                    .get("event")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let command = entry
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                (event, command)
            })
            .collect()
    }

    #[test]
    fn kimi_code_apply_writes_all_events_into_fresh_config() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(hooks_tmp.path(), &KIMI_CODE_STUB_SCRIPTS);

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("config.toml");

        let outcome = merge_kimi_code_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_eq!(outcome, ApplyOutcome::Created);

        let entries = kimi_code_hooks_in(&config_path);
        assert_eq!(entries.len(), KIMI_CODE_EVENTS.len());
        for (i, (event, script)) in KIMI_CODE_EVENTS.iter().enumerate() {
            assert_eq!(entries[i].0, *event, "entry order follows KIMI_CODE_EVENTS");
            let stem = script.strip_suffix(".sh").unwrap();
            assert!(
                entries[i].1.contains(stem),
                "{event}: command must reference the staged script: {}",
                entries[i].1
            );
        }
        // Only the allowed keys are written — Kimi Code rejects unknown
        // fields by failing the whole config load.
        let doc: toml_edit::DocumentMut =
            fs::read_to_string(&config_path).unwrap().parse().unwrap();
        for entry in doc
            .get("hooks")
            .and_then(toml_edit::Item::as_array_of_tables)
            .unwrap()
        {
            let keys: Vec<&str> = entry.iter().map(|(k, _)| k).collect();
            assert_eq!(keys, ["event", "command"], "only event + command allowed");
        }
    }

    #[test]
    fn kimi_code_apply_preserves_providers_and_third_party_hooks() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(hooks_tmp.path(), &KIMI_CODE_STUB_SCRIPTS);

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("config.toml");
        fs::write(
            &config_path,
            r#"# Kimi Code user config
model = "kimi-k2"

[providers.moonshot]
api_key = "sk-secret"
base_url = "https://api.moonshot.cn/v1"

[[hooks]]
event = "SessionStart"
command = "echo third-party"

[[hooks]]
event = "SessionStart"
command = "/opt/alphaone/bin/ai-memory boot --quiet --limit 10 --budget-tokens 4096"

[[hooks]]
event = "SessionStart"
command = "AI_MEMORY_HOOK_URL=http://old:1 /old/ai-memory/hooks/kimi-code/session-start.sh"
"#,
        )
        .unwrap();

        merge_kimi_code_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("# Kimi Code user config"),
            "comments survive"
        );
        assert!(
            content.contains(r#"api_key = "sk-secret""#),
            "providers table survives"
        );

        let doc: toml_edit::DocumentMut = content.parse().unwrap();
        assert_eq!(
            doc.get("model").and_then(|v| v.as_str()),
            Some("kimi-k2"),
            "model setting survives"
        );
        assert_eq!(
            doc.get("providers")
                .and_then(|p| p.get("moonshot"))
                .and_then(|m| m.get("base_url"))
                .and_then(|v| v.as_str()),
            Some("https://api.moonshot.cn/v1"),
            "providers.moonshot survives"
        );

        let entries = kimi_code_hooks_in(&config_path);
        // Two third-party entries + our 9: the stale ai-memory entry must be
        // replaced, not duplicated.
        assert_eq!(entries.len(), 2 + KIMI_CODE_EVENTS.len());
        let ours: Vec<&(String, String)> = entries
            .iter()
            .filter(|(_, command)| hook_command_is_ours(command))
            .collect();
        assert_eq!(ours.len(), KIMI_CODE_EVENTS.len(), "no duplicated entries");
        assert!(
            entries
                .iter()
                .any(|(_, command)| command == "echo third-party"),
            "third-party hook survives: {entries:?}"
        );
        assert!(
            entries.iter().any(|(_, command)| {
                command
                    == "/opt/alphaone/bin/ai-memory boot --quiet --limit 10 --budget-tokens 4096"
            }),
            "third-party string hook survives: {entries:?}"
        );
        assert!(
            !entries.iter().any(|(_, command)| command.contains("/old/")),
            "stale ai-memory entry must be replaced: {entries:?}"
        );
    }

    #[test]
    fn kimi_code_apply_is_idempotent() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(hooks_tmp.path(), &KIMI_CODE_STUB_SCRIPTS);

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("config.toml");

        let first = merge_kimi_code_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_ne!(
            first,
            ApplyOutcome::NoOp,
            "first apply should not be a no-op"
        );

        let second = merge_kimi_code_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_eq!(second, ApplyOutcome::NoOp, "second apply must be a no-op");
    }

    #[test]
    fn claude_settings_path_honours_claude_config_dir() {
        let custom = if cfg!(windows) {
            r"C:\custom\claude"
        } else {
            "/custom/claude"
        };
        let path = claude_settings_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
        assert_eq!(path, Path::new(custom).join("settings.json"));

        // Empty override and unset var both fall back to ~/.claude/settings.json.
        for env in [None, Some(std::ffi::OsString::new())] {
            let path = claude_settings_path_in(env).unwrap();
            assert!(
                path.ends_with(Path::new(".claude").join("settings.json")),
                "default must be ~/.claude/settings.json, got {}",
                path.display()
            );
        }
    }

    /// `CODEX_HOME` relocates Codex's entire config home, so hooks written to
    /// `~/.codex` are never loaded by a Codex configured that way — the install
    /// reports success and capture silently does nothing. ai-memory already
    /// honors the variable when resolving Codex transcripts, so the two halves
    /// of one install have to agree on where that home is.
    #[test]
    fn codex_hooks_path_honours_codex_home() {
        let custom = if cfg!(windows) {
            r"C:\custom\codex"
        } else {
            "/custom/codex"
        };
        let path = codex_hooks_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
        assert_eq!(path, Path::new(custom).join("hooks.json"));

        // Unset and blank both fall back to ~/.codex/hooks.json. Blank counts as
        // unset because an exported-but-empty variable is nearly always a failed
        // shell expansion, not a request to install into the filesystem root.
        for env in [None, Some(std::ffi::OsString::new())] {
            let path = codex_hooks_path_in(env).unwrap();
            assert!(
                path.ends_with(Path::new(".codex").join("hooks.json")),
                "default must be ~/.codex/hooks.json, got {}",
                path.display()
            );
        }
    }

    /// The test-only Claude Code wrapper must stage scripts under its injected
    /// data-local root and wire that stable path into the generated config.
    #[test]
    fn claude_code_apply_stages_into_injected_dir() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("settings.json");
        let staging_tmp = TempDir::new().unwrap();

        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &InstallHooksArgs {
                profile: None,
                agent: AgentChoice::ClaudeCode,
                capture_assistant: false,
                no_capture_prompts: false,
                capture_mode: None,
                capture_prompts: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: Some(ProjectStrategyArg::Basename),
                as_user: None,
                apply: false,
            },
        )
        .unwrap();

        let staged_script = staging_tmp
            .path()
            .join("hooks")
            .join("claude-code")
            .join("session-start.sh");
        assert!(
            staged_script.is_file(),
            "expected hook script staged at {}, override was not honoured",
            staged_script.display()
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let command = parsed
            .pointer("/hooks/SessionStart/0/hooks/0/command")
            .and_then(serde_json::Value::as_str)
            .expect("SessionStart command should be present");
        assert!(
            command.contains(&staged_script.to_string_lossy().into_owned()),
            "generated command must reference staged script {}: {command}",
            staged_script.display()
        );
    }

    #[test]
    fn claude_prompt_opt_out_survives_reapply_and_preserves_third_party_hook() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "pre-compact.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("settings.json");
        let staging_tmp = TempDir::new().unwrap();
        fs::write(
            &config_path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [
                        { "hooks": [{ "command": "third-party prompt guard" }] },
                        { "hooks": [{ "command": "/old/ai-memory hook --event user-prompt --agent claude-code --server-url http://old" }] }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        let disabled = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(config_path.clone()),
            no_capture_prompts: true,
            capture_mode: None,
            ..default_hook_args()
        };
        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &disabled,
        )
        .unwrap();

        let assert_disabled = || {
            let parsed: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
            let prompts = parsed["hooks"][CLAUDE_PROMPT_EVENT]
                .as_array()
                .expect("third-party prompt hook keeps the event present");
            assert_eq!(prompts.len(), 1);
            assert!(
                serde_json::to_string(prompts)
                    .unwrap()
                    .contains("third-party prompt guard")
            );
            assert!(!prompts.iter().any(is_ai_memory_hook_entry));
        };
        assert_disabled();

        let bare_reapply = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(config_path.clone()),
            ..default_hook_args()
        };
        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &bare_reapply,
        )
        .unwrap();
        assert_disabled();

        let enabled = InstallHooksArgs {
            agent: AgentChoice::ClaudeCode,
            config_file: Some(config_path.clone()),
            capture_prompts: true,
            ..default_hook_args()
        };
        apply_to_claude_code_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            staging_tmp.path(),
            &enabled,
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let prompts = parsed["hooks"][CLAUDE_PROMPT_EVENT].as_array().unwrap();
        assert_eq!(prompts.len(), 2, "third-party + one ai-memory hook");
        assert_eq!(
            prompts
                .iter()
                .filter(|entry| is_ai_memory_hook_entry(entry))
                .count(),
            1
        );
    }

    #[test]
    fn kimi_code_config_path_honours_kimi_code_home() {
        let custom = if cfg!(windows) {
            r"C:\custom\kimi"
        } else {
            "/custom/kimi"
        };
        let path = kimi_code_config_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
        assert_eq!(path, Path::new(custom).join("config.toml"));

        // Empty override and unset var both fall back to ~/.kimi-code.
        for env in [None, Some(std::ffi::OsString::new())] {
            let path = kimi_code_config_path_in(env).unwrap();
            assert!(
                path.ends_with(Path::new(".kimi-code").join("config.toml")),
                "default must be ~/.kimi-code/config.toml, got {}",
                path.display()
            );
        }
    }

    // ----------------------------------------------------------------
    // Kiro CLI v2 agent-config hook tests.
    // ----------------------------------------------------------------

    const KIRO_CLI_STUB_SCRIPTS: [&str; 5] = [
        "session-start.sh",
        "user-prompt-submit.sh",
        "pre-tool-use.sh",
        "post-tool-use.sh",
        "stop.sh",
    ];

    #[test]
    fn kiro_cli_apply_writes_all_v2_events_into_agent_config() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(hooks_tmp.path(), &KIRO_CLI_STUB_SCRIPTS);

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("dev.json");
        fs::write(
            &config_path,
            r#"{"name": "dev", "description": "my agent", "tools": ["*"]}"#,
        )
        .unwrap();

        let outcome = merge_kiro_cli_agent_hooks(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            None,
            &config_path,
        )
        .unwrap();
        assert_eq!(outcome, ApplyOutcome::Updated);

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // Non-hook agent fields survive the merge.
        assert_eq!(root["name"], "dev");
        assert_eq!(root["tools"][0], "*");
        let hooks = root["hooks"].as_object().expect("hooks object");
        assert_eq!(hooks.len(), KIRO_CLI_V2_EVENTS.len());
        for (event, script) in KIRO_CLI_V2_EVENTS {
            let entries = hooks[event].as_array().expect("event array");
            assert_eq!(entries.len(), 1, "{event}");
            let entry = entries[0].as_object().expect("entry object");
            let command = entry["command"].as_str().unwrap();
            let stem = script.strip_suffix(".sh").unwrap();
            assert!(
                command.contains(stem),
                "{event}: command must reference the staged script: {command}"
            );
            // The v2 Hook schema has no `type` field, and an empty-string
            // matcher would match no tool at all (absent = every tool), so
            // neither key may be written.
            assert!(!entry.contains_key("type"), "{event}: no type key");
            assert!(!entry.contains_key("matcher"), "{event}: no matcher key");
            if event == "agentSpawn" {
                // The handoff + brief must not be truncated mid-injection.
                assert_eq!(
                    entry["max_output_size"],
                    serde_json::json!(KIRO_CLI_V2_SESSION_START_MAX_OUTPUT)
                );
            } else {
                assert!(!entry.contains_key("max_output_size"), "{event}");
            }
        }
    }

    #[test]
    fn kiro_cli_apply_preserves_third_party_hooks_and_is_idempotent() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(hooks_tmp.path(), &KIRO_CLI_STUB_SCRIPTS);

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("dev.json");
        fs::write(
            &config_path,
            r#"{
  "name": "dev",
  "hooks": {
    "agentSpawn": [
      {"command": "git status"},
      {"command": "echo ai-memory status"},
      {"command": "AI_MEMORY_HOOK_URL=http://old:1 /old/ai-memory/hooks/kiro-cli/session-start.sh"}
    ],
    "preToolUse": [
      {"matcher": "execute_bash", "command": "echo audit >> /tmp/audit.log"}
    ]
  }
}"#,
        )
        .unwrap();

        let apply = || {
            merge_kiro_cli_agent_hooks(
                hooks_tmp.path(),
                "http://127.0.0.1:49374",
                None,
                config_tmp.path(),
                None,
                &config_path,
            )
        };
        assert_eq!(apply().unwrap(), ApplyOutcome::Updated);

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        let spawn = root["hooks"]["agentSpawn"].as_array().unwrap();
        // Third-party entry first (preserved), our refreshed entry after;
        // the stale ai-memory entry is gone.
        assert_eq!(spawn.len(), 3);
        assert_eq!(spawn[0]["command"], "git status");
        assert_eq!(spawn[1]["command"], "echo ai-memory status");
        let ours = spawn[2]["command"].as_str().unwrap();
        assert!(ours.contains("session-start"), "{ours}");
        assert!(
            !ours.contains("http://old:1"),
            "stale entry replaced: {ours}"
        );
        // A third-party hook under a trigger we also write survives with
        // its matcher intact.
        let pre = root["hooks"]["preToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["matcher"], "execute_bash");

        // Second apply with identical inputs must be a no-op.
        assert_eq!(apply().unwrap(), ApplyOutcome::NoOp);
    }

    #[test]
    fn kiro_cli_paths_honour_kiro_home() {
        let custom = if cfg!(windows) {
            r"C:\custom\kiro"
        } else {
            "/custom/kiro"
        };
        let agents = kiro_cli_home_join(Some(std::ffi::OsString::from(custom)), "agents").unwrap();
        assert_eq!(agents, Path::new(custom).join("agents"));
        let hooks = kiro_cli_home_join(Some(std::ffi::OsString::from(custom)), "hooks")
            .unwrap()
            .join("ai-memory.json");
        assert_eq!(hooks, Path::new(custom).join("hooks/ai-memory.json"));

        // Empty override and unset var both fall back to ~/.kiro.
        for env in [None, Some(std::ffi::OsString::new())] {
            let agents = kiro_cli_home_join(env.clone(), "agents").unwrap();
            assert!(
                agents.ends_with(Path::new(".kiro").join("agents")),
                "default must be ~/.kiro/agents, got {}",
                agents.display()
            );
        }
    }

    #[test]
    fn kiro_hooks_infer_the_managed_mcp_client() {
        assert_eq!(
            mcp_client_for_agent(AgentChoice::KiroCli),
            Some(McpClient::KiroCli)
        );
        assert_eq!(
            mcp_client_for_agent(AgentChoice::KiroCliV3),
            Some(McpClient::KiroCli)
        );
    }

    #[test]
    fn kiro_cli_v3_apply_writes_documented_schema_and_is_idempotent() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(hooks_tmp.path(), &KIRO_CLI_STUB_SCRIPTS);
        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks/ai-memory.json");

        let apply = || {
            merge_kiro_cli_v3_hooks(
                hooks_tmp.path(),
                "https://memory.example",
                Some("test-token"),
                config_tmp.path(),
                Some("repo-root"),
                &config_path,
            )
        };
        assert_eq!(apply().unwrap(), ApplyOutcome::Created);

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(root["version"], "v1");
        let hooks = root["hooks"].as_array().expect("hooks array");
        assert_eq!(hooks.len(), KIRO_CLI_V3_EVENTS.len());
        for ((trigger, script), entry) in KIRO_CLI_V3_EVENTS.iter().zip(hooks) {
            assert_eq!(entry["trigger"], *trigger);
            assert_eq!(entry["action"]["type"], "command");
            assert_eq!(
                entry["timeout"],
                if *trigger == "SessionStart" { 5 } else { 1 }
            );
            assert_eq!(entry["enabled"], true);
            assert_eq!(
                entry["name"],
                format!("ai-memory-{}", script.trim_end_matches(".sh"))
            );
            let command = entry["action"]["command"].as_str().unwrap();
            assert!(is_ai_memory_kiro_hook_command(command), "{command}");
            assert!(command.contains("test-token"), "{command}");
            assert!(command.contains("repo-root"), "{command}");
            assert!(entry.get("matcher").is_none());
        }
        assert_eq!(apply().unwrap(), ApplyOutcome::NoOp);
    }

    #[test]
    fn kiro_cli_v3_apply_preserves_third_party_hooks_and_replaces_only_ours() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("ai-memory.json");
        fs::write(
            &config_path,
            r#"{
  "version": "v1",
  "owner": "user",
  "hooks": [
    {"name":"audit","trigger":"PreToolUse","action":{"type":"command","command":"audit-tool"}},
    {"name":"ai-memory-session-start","trigger":"SessionStart","action":{"type":"command","command":"AI_MEMORY_HOOK_URL=http://old /old/hooks/kiro-cli/session-start.sh"}}
  ]
}"#,
        )
        .unwrap();

        assert_eq!(
            merge_kiro_cli_v3_hooks(
                tmp.path(),
                "https://memory.example",
                None,
                tmp.path(),
                None,
                &config_path,
            )
            .unwrap(),
            ApplyOutcome::Updated
        );
        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(root["owner"], "user");
        let hooks = root["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1 + KIRO_CLI_V3_EVENTS.len());
        assert_eq!(hooks[0]["name"], "audit");
        assert!(hooks[1..].iter().all(is_ai_memory_kiro_v3_hook_entry));
    }

    #[test]
    fn kiro_cli_v3_apply_rejects_incompatible_schema_and_reserved_collision() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("ai-memory.json");
        fs::write(&config_path, r#"{"version":"v2","hooks":[]}"#).unwrap();
        let error = merge_kiro_cli_v3_hooks(
            tmp.path(),
            "https://memory.example",
            None,
            tmp.path(),
            None,
            &config_path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("unsupported Kiro hook schema version"));

        fs::write(
            &config_path,
            r#"{"version":"v1","hooks":[{"name":"ai-memory-stop","trigger":"Stop","action":{"type":"command","command":"third-party"}}]}"#,
        )
        .unwrap();
        let error = merge_kiro_cli_v3_hooks(
            tmp.path(),
            "https://memory.example",
            None,
            tmp.path(),
            None,
            &config_path,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("reserved name"));
    }

    #[test]
    fn kiro_cli_agent_config_listing_is_sorted_json_only() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("b.json"), "{}").unwrap();
        fs::write(tmp.path().join("a.json"), "{}").unwrap();
        fs::write(tmp.path().join("notes.md"), "not an agent").unwrap();
        fs::create_dir(tmp.path().join("sub.json")).unwrap();

        let listed = list_kiro_cli_agent_configs(tmp.path()).unwrap();
        assert_eq!(
            listed,
            vec![tmp.path().join("a.json"), tmp.path().join("b.json")]
        );

        // A missing directory is an empty list, not an error.
        assert!(
            list_kiro_cli_agent_configs(&tmp.path().join("missing"))
                .unwrap()
                .is_empty()
        );

        let not_a_directory = tmp.path().join("agents.json");
        fs::write(&not_a_directory, "{}").unwrap();
        let error = list_kiro_cli_agent_configs(&not_a_directory).unwrap_err();
        assert!(
            format!("{error:#}").contains("reading"),
            "filesystem failures must not look like an empty agent directory: {error:#}"
        );
    }

    #[test]
    fn kiro_cli_v2_preflight_is_all_or_nothing_and_preserves_per_agent_strategy() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("a.json");
        let basename = tmp.path().join("b.json");
        // Script-form hooks always include the URL marker; the project strategy
        // is an additional setting, not the ownership signature.
        fs::write(
            &repo_root,
            r#"{"name":"a","hooks":{"agentSpawn":[{"command":"AI_MEMORY_HOOK_URL=http://old:1 AI_MEMORY_PROJECT_STRATEGY=repo-root /old/hooks/kiro-cli/session-start.sh"}]}}"#,
        )
        .unwrap();
        fs::write(&basename, r#"{"name":"b"}"#).unwrap();

        let prepared = preflight_kiro_cli_agent_configs(
            vec![repo_root.clone(), basename.clone()],
            tmp.path(),
            "http://127.0.0.1:49374",
            None,
            tmp.path(),
            None,
        )
        .unwrap();
        assert_eq!(prepared[0].1, Some(ProjectStrategyArg::RepoRoot));
        assert_eq!(prepared[1].1, None);

        let before = fs::read_to_string(&repo_root).unwrap();
        fs::write(&basename, "{not-json").unwrap();
        let error = preflight_kiro_cli_agent_configs(
            vec![repo_root.clone(), basename],
            tmp.path(),
            "http://127.0.0.1:49374",
            None,
            tmp.path(),
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("valid JSON"));
        assert_eq!(
            fs::read_to_string(repo_root).unwrap(),
            before,
            "a malformed later config must not mutate an earlier agent"
        );
    }

    // ----------------------------------------------------------------
    // Devin tests
    // ----------------------------------------------------------------

    #[test]
    fn devin_apply_writes_hooks_v1_json_by_default() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "post-compaction.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.v1.json");

        apply_to_devin_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            config_tmp.path(),
            &InstallHooksArgs {
                profile: None,
                agent: AgentChoice::Devin,
                capture_assistant: false,
                no_capture_prompts: false,
                capture_mode: None,
                capture_prompts: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
                as_user: None,
                apply: false,
            },
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // hooks.v1.json: the entire file IS the hooks object (no wrapper)
        assert!(
            parsed["SessionStart"].is_array(),
            "SessionStart hook should be present"
        );
        assert!(
            parsed["PostCompaction"].is_array(),
            "PostCompaction hook should be present"
        );
        assert!(
            parsed.get("hooks").is_none(),
            "hooks.v1.json should not have a 'hooks' wrapper"
        );
    }

    #[test]
    fn devin_apply_config_json_hooks_key_via_flag() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "post-compaction.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("config.json");

        // Pre-existing config.json with mcpServers
        fs::write(
            &config_path,
            r#"{"mcpServers":{"other-server":{"url":"http://example.com"}}}"#,
        )
        .unwrap();

        apply_to_devin_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            config_tmp.path(),
            &InstallHooksArgs {
                profile: None,
                agent: AgentChoice::Devin,
                capture_assistant: false,
                no_capture_prompts: false,
                capture_mode: None,
                capture_prompts: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
                as_user: None,
                apply: false,
            },
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // config.json: hooks are nested under the "hooks" key
        assert!(
            parsed["hooks"]["SessionStart"].is_array(),
            "SessionStart hook should be present"
        );
        assert!(
            parsed["hooks"]["PostCompaction"].is_array(),
            "PostCompaction hook should be present"
        );
        // mcpServers should be preserved
        assert!(
            parsed["mcpServers"]["other-server"].is_object(),
            "mcpServers should be preserved"
        );
    }

    #[test]
    fn devin_apply_both_targets_are_idempotent() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "session-end.sh",
                "user-prompt-submit.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "post-compaction.sh",
                "stop.sh",
            ],
        );

        // Test hooks.v1.json idempotency
        let config_tmp = TempDir::new().unwrap();
        let hooks_v1_path = config_tmp.path().join("hooks.v1.json");

        let args_v1 = InstallHooksArgs {
            profile: None,
            agent: AgentChoice::Devin,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(hooks_v1_path.clone()),
            project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
            as_user: None,
            apply: false,
        };

        apply_to_devin_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            config_tmp.path(),
            &args_v1,
        )
        .unwrap();

        apply_to_devin_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            config_tmp.path(),
            &args_v1,
        )
        .unwrap();

        // Idempotency check: second apply should be a no-op
        let v1_first_content = fs::read_to_string(&hooks_v1_path).unwrap();
        let v1_second_content = fs::read_to_string(&hooks_v1_path).unwrap();
        assert_eq!(
            v1_first_content, v1_second_content,
            "hooks.v1.json should be unchanged after second apply"
        );

        // Test config.json hooks key idempotency
        let config_tmp2 = TempDir::new().unwrap();
        let config_path = config_tmp2.path().join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let args_config = InstallHooksArgs {
            profile: None,
            agent: AgentChoice::Devin,
            capture_assistant: false,
            no_capture_prompts: false,
            capture_mode: None,
            capture_prompts: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(config_path.clone()),
            project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
            as_user: None,
            apply: false,
        };

        apply_to_devin_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp2.path(),
            config_tmp2.path(),
            &args_config,
        )
        .unwrap();

        apply_to_devin_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp2.path(),
            config_tmp2.path(),
            &args_config,
        )
        .unwrap();

        // Idempotency check: second apply should be a no-op
        let config_first_content = fs::read_to_string(&config_path).unwrap();
        let config_second_content = fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            config_first_content, config_second_content,
            "config.json should be unchanged after second apply"
        );
    }

    #[test]
    fn devin_preserves_existing_hooks_and_adds_ours() {
        let hooks_tmp = TempDir::new().unwrap();
        stub_scripts(
            hooks_tmp.path(),
            &[
                "session-start.sh",
                "pre-tool-use.sh",
                "post-tool-use.sh",
                "post-compaction.sh",
                "stop.sh",
            ],
        );

        let config_tmp = TempDir::new().unwrap();
        let config_path = config_tmp.path().join("hooks.v1.json");
        // Pre-existing settings with another named hook group.
        fs::write(
            &config_path,
            r#"{"my-linter":{"PostToolUse":[{"matcher":"run_command","hooks":[{"type":"command","command":"lint.sh"}]}]}}"#,
        )
        .unwrap();

        apply_to_devin_settings_in(
            hooks_tmp.path(),
            "http://127.0.0.1:49374",
            None,
            config_tmp.path(),
            config_tmp.path(),
            &InstallHooksArgs {
                profile: None,
                agent: AgentChoice::Devin,
                capture_assistant: false,
                no_capture_prompts: false,
                capture_mode: None,
                capture_prompts: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: Some(crate::cli::ProjectStrategyArg::Basename),
                as_user: None,
                apply: false,
            },
        )
        .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // The pre-existing my-linter group survives.
        assert!(
            parsed["my-linter"]["PostToolUse"].is_array(),
            "my-linter.PostToolUse must survive"
        );
        // Our hooks are present at the top level (hooks.v1.json format).
        assert!(
            parsed["SessionStart"].is_array(),
            "SessionStart hook should be present"
        );
        assert!(
            parsed["PostCompaction"].is_array(),
            "PostCompaction hook should be present"
        );
        assert!(
            parsed["PostToolUse"].is_array(),
            "PostToolUse hook should be present"
        );
    }

    #[test]
    fn devin_session_start_injects_handoff_additional_context() {
        let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("hooks");
        let devin_session_start = hooks_root.join("devin").join("session-start.sh");

        // Normalized because this is the only assertion below that spans a
        // line break: on a Windows checkout with core.autocrlf=true the
        // working-tree copy has CRLF even though the committed blob is
        // LF-only, which breaks an exact multi-line substring match.
        let script_content = fs::read_to_string(&devin_session_start)
            .unwrap()
            .replace("\r\n", "\n");
        // Verify the script injects handoff via hookSpecificOutput.additionalContext
        assert!(
            script_content.contains("hookSpecificOutput"),
            "Devin session-start.sh must inject handoff via hookSpecificOutput"
        );
        assert!(
            script_content.contains("additionalContext"),
            "Devin session-start.sh must use additionalContext field"
        );
        assert!(
            script_content.contains("ai_memory_get_handoff"),
            "Devin session-start.sh must fetch handoff"
        );
        assert!(
            script_content.contains("$SERVER/handoff?agent=devin${QS}${SID_QS}"),
            "Devin session-start.sh must bind the handoff claim to its generated session id"
        );
        assert!(
            !script_content.contains("/handoff/latest"),
            "Devin session-start.sh must not call the removed /handoff/latest route"
        );
        assert!(
            script_content.contains("ai_memory_json_string"),
            "Devin session-start.sh must JSON-escape handoff text before embedding it"
        );
        assert!(
            script_content.contains("else\n    printf '{}\\n'\nfi"),
            "Devin session-start.sh must print {{}} only when no handoff is available"
        );
    }

    #[test]
    fn startup_handoff_scripts_forward_native_session_ids() {
        let hooks_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("hooks");
        for agent in ["claude-code", "codex", "opencode", "cursor", "gemini-cli"] {
            let script = fs::read_to_string(hooks_root.join(agent).join("session-start.sh"))
                .unwrap()
                .replace("\r\n", "\n");
            assert!(
                script.contains("SESSION_ID=$(ai_memory_extract_session_id \"$PAYLOAD\")"),
                "{agent} must extract the native receiver session id"
            );
            assert!(
                script.contains("${SESSION_QS}"),
                "{agent} must forward the native receiver session id to /handoff"
            );
        }
    }

    #[test]
    fn grok_placeholder_expansion_resolves_values_and_defaults() {
        let expanded = expand_grok_placeholders_with("${HOST}/mcp ${TOKEN:-fallback}", |name| {
            (name == "HOST").then(|| "https://memory.example".to_string())
        })
        .unwrap();
        assert_eq!(expanded, "https://memory.example/mcp fallback");
    }

    #[test]
    fn grok_placeholder_expansion_rejects_missing_variable() {
        let err = expand_grok_placeholders_with("${MISSING}", |_| None).unwrap_err();
        assert!(
            err.to_string()
                .contains("pass explicit --server-url and --auth-token")
        );
    }

    #[test]
    fn grok_project_overlays_cover_repo_root_through_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let parent = root.join("nested");
        let cwd = parent.join("leaf");
        fs::create_dir_all(&cwd).unwrap();

        for directory in [&root, &parent, &cwd] {
            let overlay = directory.join(".grok/config.toml");
            fs::create_dir_all(overlay.parent().unwrap()).unwrap();
            fs::write(&overlay, "# override").unwrap();
            assert_eq!(find_grok_project_overlay(&cwd, Some(&root)), Some(overlay));
            fs::remove_file(directory.join(".grok/config.toml")).unwrap();
        }
        assert_eq!(find_grok_project_overlay(&cwd, Some(&root)), None);
    }

    #[test]
    fn grok_project_overlay_outside_repo_checks_only_cwd() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("outside").join("leaf");
        fs::create_dir_all(&cwd).unwrap();
        let parent_overlay = cwd.parent().unwrap().join(".grok/config.toml");
        fs::create_dir_all(parent_overlay.parent().unwrap()).unwrap();
        fs::write(&parent_overlay, "# ignored").unwrap();
        assert_eq!(find_grok_project_overlay(&cwd, None), None);

        let cwd_overlay = cwd.join(".grok/config.toml");
        fs::create_dir_all(cwd_overlay.parent().unwrap()).unwrap();
        fs::write(&cwd_overlay, "# active").unwrap();
        assert_eq!(find_grok_project_overlay(&cwd, None), Some(cwd_overlay));
    }
}
