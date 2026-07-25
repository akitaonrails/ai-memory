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
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{AgentChoice, InstallHooksArgs, McpClient};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json, mutate_toml};
use crate::commands::install_mcp;
use crate::commands::openclaw_plugin;
use crate::commands::path_util::home_dir;
use crate::commands::render_shared::{
    ANTIGRAVITY_LIFECYCLE_EVENTS, ANTIGRAVITY_TOOL_EVENTS, CODEX_PROFILE, CURSOR_PROFILE,
    GEMINI_PROFILE, KIMI_CODE_EVENTS, build_antigravity_payload_with_data_dir,
    build_claude_code_payload_with_data_dir, build_devin_payload_with_data_dir,
    build_grok_payload_with_data_dir, build_profile_payload_for_agent, hook_script_for_claude_code,
    hook_script_for_current_platform, kimi_code_hook_commands, local_hook_policy_v1_supported,
    ts_capture_policy_v1, ts_string_literal,
};
use crate::config::{Config, DEFAULT_SERVER_URL};

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

/// `~/.codex/hooks.json`.
pub(crate) fn codex_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.codex/hooks.json")?
        .join(".codex")
        .join("hooks.json"))
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

/// `~/.omp/agent/extensions/ai-memory.ts` — OMP lifecycle extension.
pub(crate) fn omp_extension_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.omp/agent/extensions")?
        .join(".omp")
        .join("agent")
        .join("extensions")
        .join("ai-memory.ts"))
}

/// `~/.pi/agent/extensions/ai-memory.ts` — Pi lifecycle + MCP bridge extension.
pub(crate) fn pi_extension_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(home_dir()
        .context("could not locate $HOME for ~/.pi/agent/extensions")?
        .join(".pi")
        .join("agent")
        .join("extensions")
        .join("ai-memory.ts"))
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

/// Run the `install-hooks` subcommand.
///
/// # Errors
/// Returns an error if the hook script directory cannot be located.
pub fn run(config: &Config, args: InstallHooksArgs) -> Result<()> {
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
    let auth = auth_token_owned.as_deref();
    // P1.8 multi-user attribution: `--as-user` is metadata only — the
    // token stamped into the hook env block is whatever the operator
    // passed via `--auth-token` (typically the per-user token from
    // `ai-memory user add`). We surface the username to stderr so the
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
    if args.apply {
        return match args.agent {
            AgentChoice::OpenCode => apply_to_opencode_plugin(&server_url, auth, &args),
            AgentChoice::Pi => apply_to_pi_extension(&server_url, auth, &args),
            AgentChoice::Omp => apply_to_omp_extension(&server_url, auth, &args),
            AgentChoice::ClaudeCode => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_claude_code_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Codex => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_codex_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Cursor => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_cursor_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::GeminiCli => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_gemini_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::AntigravityCli => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_antigravity_settings(
                    &hooks_dir,
                    &server_url,
                    auth,
                    &config.data_dir,
                    &args,
                )
            }
            AgentChoice::Grok => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_grok_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Zero => apply_to_zero_hooks(&server_url, auth, &config.data_dir, &args),
            AgentChoice::Devin => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_devin_settings(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
            AgentChoice::Openclaw => openclaw_plugin::apply(&server_url, auth, &args),
            AgentChoice::KimiCode => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_kimi_code_config(&hooks_dir, &server_url, auth, &config.data_dir, &args)
            }
        };
    }
    let strategy = args.project_strategy.baked();
    match args.agent {
        AgentChoice::OpenCode => render_opencode_plugin(&server_url, auth, strategy),
        AgentChoice::Pi => render_pi_extension(&server_url, auth, strategy),
        AgentChoice::Omp => render_omp_extension(&server_url, auth, strategy),
        AgentChoice::ClaudeCode => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
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
                args.capture_assistant,
            )
        }
        AgentChoice::Codex => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_agent(
                "codex",
                &hooks_dir,
                &server_url,
                auth,
                strategy,
                &[CODEX_PROFILE.events],
            )
        }
        AgentChoice::Cursor => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
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
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
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
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
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
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_grok(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Zero => render_zero(&server_url, auth, &config.data_dir, strategy),
        AgentChoice::Devin => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_devin(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
        AgentChoice::Openclaw => {
            openclaw_plugin::render(&server_url, auth, strategy);
            Ok(())
        }
        AgentChoice::KimiCode => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_kimi_code(&hooks_dir, &server_url, auth, &config.data_dir, strategy)
        }
    }
}

#[derive(Debug, Clone, Default)]
struct InferredMcpConfig {
    hook_server_url: Option<String>,
    auth_token: Option<String>,
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
             (the token printed by `ai-memory user add --username {user}`)"
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
        McpClient::KimiCode => Ok(infer_json_mcp_config(
            &content,
            &["mcpServers", "ai-memory"],
            "url",
        )),
        McpClient::ClaudeDesktop => Ok(None),
        // MCP-only client: no AgentChoice counterpart routes here.
        // Reachable only if a future install_hooks flow targets VS
        // Code Copilot directly.
        McpClient::VsCodeCopilot => Ok(infer_json_mcp_config(
            &content,
            &["servers", "ai-memory"],
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
            // Legacy shell/string form: broad matching is intentional because
            // old installs may identify us by the binary/script path or by the
            // inlined AI_MEMORY_* env vars.
            return lower.contains("ai-memory") || lower.contains("ai_memory");
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

fn apply_to_claude_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => claude_settings_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "claude-code")?;
    let command_dir = staged_command_dir(&staged, "claude-code");
    let strategy = args.project_strategy.baked();
    let payload = build_claude_code_payload_with_data_dir(
        &command_dir,
        server_url,
        auth_token,
        Some(data_dir),
        strategy,
        args.capture_assistant,
    );
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
    let staged = stage_hook_scripts(hooks_dir, "grok")?;
    let command_dir = staged_command_dir(&staged, "grok");
    let strategy = args.project_strategy.baked();
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
    let staged = stage_hook_scripts(hooks_dir, "devin")?;
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
    let strategy = args.project_strategy.baked();
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
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => codex_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "codex")?;
    let command_dir = staged_command_dir(&staged, "codex");
    let strategy = args.project_strategy.baked();
    let outcome = merge_codex_hooks(
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
    let staged = stage_hook_scripts(hooks_dir, "cursor")?;
    let command_dir = staged_command_dir(&staged, "cursor");
    let strategy = args.project_strategy.baked();
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
    let staged = stage_hook_scripts(hooks_dir, "gemini-cli")?;
    let command_dir = staged_command_dir(&staged, "gemini-cli");
    let strategy = args.project_strategy.baked();
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
    let staged = stage_hook_scripts(hooks_dir, "antigravity-cli")?;
    let command_dir = staged_command_dir(&staged, "antigravity-cli");
    let strategy = args.project_strategy.baked();
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
    Ok(())
}

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
    let staged = stage_hook_scripts(hooks_dir, "kimi-code")?;
    let command_dir = staged_command_dir(&staged, "kimi-code");
    let strategy = args.project_strategy.baked();
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

/// True if a `[[hooks]]` table belongs to ai-memory — i.e. its command
/// references our staged script path or the inlined `AI_MEMORY_*` env
/// vars. Broad matching is intentional (mirrors the shell-form branch
/// of `is_ai_memory_hook_entry`): old installs may identify us by the
/// script path or by the env prefix alone.
fn is_ai_memory_toml_hook_entry(entry: &toml_edit::Table) -> bool {
    let Some(command) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let lower = command.to_ascii_lowercase();
    lower.contains("ai-memory") || lower.contains("ai_memory")
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
    let strategy = args.project_strategy.baked();
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
    format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.
// Edit by re-running the command, not by hand — install-hooks
// will overwrite this file (with a `.bak-<ts>` backup) on each
// re-run.

import type {{ Plugin }} from "@opencode-ai/plugin";
import {{ execFileSync }} from "node:child_process";
import {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync }} from "node:fs";
import {{ basename, dirname, join, resolve }} from "node:path";
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
  while (dir && dir !== dirname(dir)) {{
    const marker = join(dir, ".ai-memory.toml");
    if (existsSync(marker)) return marker;
    if (home && dir === home) return undefined;
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
    )
}

/// Generate an Oh My Pi extension at `~/.omp/agent/extensions/ai-memory.ts`.
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
    let strategy = args.project_strategy.baked();
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

fn render_omp_extension(
    server_url: &str,
    auth_token: Option<&str>,
    project_strategy: Option<&str>,
) -> Result<()> {
    println!("// Oh My Pi / OMP extension — write to ~/.omp/agent/extensions/ai-memory.ts");
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
    omp_extension_path()
}

fn apply_to_pi_extension(
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = resolve_pi_extension_path(args)?;
    let strategy = args.project_strategy.baked();
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
    println!("// Pi extension — write to ~/.pi/agent/extensions/ai-memory.ts");
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
    format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent omp --apply`.
// Edit by re-running the command, not by hand — install-hooks
// will overwrite this file (with a `.bak-<ts>` backup) on each
// re-run.

import {{ execFileSync }} from "node:child_process";
import {{ closeSync, existsSync, openSync, readFileSync as readMarkerText, readSync }} from "node:fs";
import {{ basename, dirname, join, resolve }} from "node:path";
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
  while (dir && dir !== dirname(dir)) {{
    const marker = join(dir, ".ai-memory.toml");
    if (existsSync(marker)) return marker;
    if (home && dir === home) return undefined;
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
    )
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
fn stage_hook_scripts(source_dir: &Path, agent_label: &str) -> Result<PathBuf> {
    let data_dir = dirs::data_local_dir()
        .context("could not locate the user data-local directory (e.g. ~/.local/share)")?;
    stage_hook_scripts_in(source_dir, agent_label, &data_dir)
}

fn stage_hook_scripts_in(
    source_dir: &Path,
    agent_label: &str,
    data_local_dir: &Path,
) -> Result<PathBuf> {
    let dest_root = data_local_dir
        .join("ai-memory")
        .join("hooks")
        .join(agent_label);

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

fn resolve_hooks_dir(explicit: Option<&Path>, agent: AgentChoice) -> Result<PathBuf> {
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
        dirs::data_local_dir(),
    );
    for path in &candidates {
        if !path.as_os_str().is_empty() && path.is_dir() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!("could not locate hooks directory. Tried: {:?}", candidates,);
}

fn hook_source_candidates(
    sub: &str,
    repo_root: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    data_local_dir: Option<PathBuf>,
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
    // Local install honourable mention.
    if let Some(dir) = data_local_dir {
        candidates.push(dir.join("ai-memory/hooks").join(sub));
    }
    candidates
}

fn repo_root_guess() -> Option<PathBuf> {
    // When the binary lives under target/{debug,release}/<name>, the
    // workspace root is two parents up.
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent()?.parent().map(Path::to_path_buf))
}

/// Directory the running binary lives in. The release tarball ships the
/// `hooks/` bundle right next to the binary, so a no-`--source`
/// `install-hooks` finds it there (issue #107).
fn exe_dir_guess() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

// CLAUDE_CODE_EVENTS + build_claude_code_payload now live in
// `super::render_shared`, shared with `setup-agent`.

fn render_claude_code(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    data_dir: &Path,
    project_strategy: Option<&str>,
    settings_path: &Path,
    capture_assistant: bool,
) -> Result<()> {
    // Soft check: warn (don't bail) if a script is missing. The user
    // may be running this command inside docker against a host path
    // that exists only on the host's filesystem — bailing would
    // sabotage the docker-only flow `setup-agent` enables.
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
    let payload = build_claude_code_payload_with_data_dir(
        hooks_dir,
        server_url,
        auth_token,
        Some(data_dir),
        project_strategy,
        capture_assistant,
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
    let strategy = args.project_strategy.baked();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ProjectStrategyArg;
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
                { "matcher": "", "hooks": [ { "type": "command", "command": "/old/ai-memory.exe hook --event session-start" } ] }
            ]),
        );
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "/new/.cargo/bin/ai-memory.exe hook --event session-start" } ] }
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
    fn overlay_event_hooks_inserts_when_event_absent() {
        let mut hooks = serde_json::Map::new();
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event stop" } ] }
        ]);
        overlay_event_hooks(&mut hooks, "Stop", &ours);
        assert_eq!(hooks["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn overlay_event_hooks_is_idempotent_on_reapply() {
        // Re-applying must not accumulate duplicate ai-memory entries.
        let mut hooks = serde_json::Map::new();
        let ours = serde_json::json!([
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook --event pre-tool-use" } ] }
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
            { "matcher": "", "hooks": [ { "type": "command", "command": "ai-memory.exe hook" } ] }
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
            agent: AgentChoice::OpenCode,
            capture_assistant: false,
            hooks_dir: None,
            server_url: None,
            auth_token: None,
            as_user: None,
            apply: true,
            config_file: None,
            project_strategy: ProjectStrategyArg::Basename,
        }
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

        let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Grok).unwrap();
        assert_eq!(resolved, tmp.path().join("grok"));
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
    fn resolve_hooks_dir_uses_devin_bundle_for_devin() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("devin")).unwrap();
        fs::create_dir_all(tmp.path().join("claude-code")).unwrap();

        let resolved = resolve_hooks_dir(Some(tmp.path()), AgentChoice::Devin).unwrap();
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

        for agent_dir in [
            "claude-code",
            "codex",
            "cursor",
            "gemini-cli",
            "grok",
            "devin",
            "opencode",
            "antigravity-cli",
            "kimi-code",
        ] {
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
        // Simulate "scripts already extracted into the data-local
        // hooks dir by a prior `setup-agent` run".
        let in_place = data_dir.join("ai-memory/hooks").join(agent_label);
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
        let in_place = data_dir.join("ai-memory/hooks").join(agent_label);
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

    #[test]
    fn hook_source_candidates_include_native_package_dir() {
        let candidates = hook_source_candidates(
            "claude-code",
            Some(PathBuf::from("/repo")),
            Some(PathBuf::from("/opt/ai-memory")),
            Some(PathBuf::from("/home/alice/.local/share")),
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
        assert!(plugin.contains("import { basename, dirname, join, resolve } from \"node:path\";"));
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
            extension.contains("import { basename, dirname, join, resolve } from \"node:path\";")
        );
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

    #[test]
    fn omp_extension_is_directly_discoverable_by_omp() {
        let tmp = TempDir::new().unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Omp,
            capture_assistant: false,
            hooks_dir: None,
            server_url: Some("http://127.0.0.1:49374".into()),
            auth_token: None,
            as_user: None,
            apply: true,
            config_file: Some(tmp.path().join("extensions").join("ai-memory.ts")),
            project_strategy: ProjectStrategyArg::Basename,
        };

        let path = resolve_omp_extension_path(&args).unwrap();
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("ai-memory.ts")
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
        let path = tmp.path().join("extensions").join("ai-memory.ts");
        let args = InstallHooksArgs {
            agent: AgentChoice::Pi,
            capture_assistant: false,
            hooks_dir: None,
            server_url: Some("http://127.0.0.1:49374".into()),
            auth_token: None,
            as_user: None,
            apply: true,
            config_file: Some(path.clone()),
            project_strategy: ProjectStrategyArg::Basename,
        };

        let resolved = resolve_pi_extension_path(&args).unwrap();

        assert_eq!(resolved, path);
        assert_eq!(
            resolved.file_name().and_then(|s| s.to_str()),
            Some("ai-memory.ts")
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
        // Third-party entry + our 9: the stale ai-memory entry must be
        // replaced, not duplicated.
        assert_eq!(entries.len(), 1 + KIMI_CODE_EVENTS.len());
        let ours: Vec<&(String, String)> = entries
            .iter()
            .filter(|(_, command)| {
                let lower = command.to_ascii_lowercase();
                lower.contains("ai-memory") || lower.contains("ai_memory")
            })
            .collect();
        assert_eq!(ours.len(), KIMI_CODE_EVENTS.len(), "no duplicated entries");
        assert!(
            entries
                .iter()
                .any(|(_, command)| command == "echo third-party"),
            "third-party hook survives: {entries:?}"
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
                agent: AgentChoice::Devin,
                capture_assistant: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: crate::cli::ProjectStrategyArg::Basename,
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
                agent: AgentChoice::Devin,
                capture_assistant: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: crate::cli::ProjectStrategyArg::Basename,
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
            agent: AgentChoice::Devin,
            capture_assistant: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(hooks_v1_path.clone()),
            project_strategy: crate::cli::ProjectStrategyArg::Basename,
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
            agent: AgentChoice::Devin,
            capture_assistant: false,
            hooks_dir: Some(hooks_tmp.path().to_path_buf()),
            server_url: Some("http://127.0.0.1:49374".to_string()),
            auth_token: None,
            config_file: Some(config_path.clone()),
            project_strategy: crate::cli::ProjectStrategyArg::Basename,
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
                agent: AgentChoice::Devin,
                capture_assistant: false,
                hooks_dir: Some(hooks_tmp.path().to_path_buf()),
                server_url: Some("http://127.0.0.1:49374".to_string()),
                auth_token: None,
                config_file: Some(config_path.clone()),
                project_strategy: crate::cli::ProjectStrategyArg::Basename,
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
            script_content.contains("$SERVER/handoff?agent=devin${QS}"),
            "Devin session-start.sh must use the registered /handoff endpoint with agent=devin"
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
