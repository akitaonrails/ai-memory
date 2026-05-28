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
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json};
use crate::commands::install_mcp;
use crate::commands::openclaw_plugin;
use crate::commands::render_shared::{
    CURSOR_PROFILE, GEMINI_PROFILE, build_antigravity_payload, build_claude_code_payload,
    build_codex_payload, build_profile_payload, hook_script_for_claude_code,
    hook_script_for_current_platform, ts_string_literal,
};
use crate::config::{Config, DEFAULT_SERVER_URL};

/// `~/.claude/settings.json` — Claude Code hooks live under `hooks`.
pub(crate) fn claude_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.claude/settings.json")?
        .join(".claude")
        .join("settings.json"))
}

/// `~/.codex/hooks.json`.
pub(crate) fn codex_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.codex/hooks.json")?
        .join(".codex")
        .join("hooks.json"))
}

/// `~/.cursor/hooks.json`.
pub(crate) fn cursor_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.cursor/hooks.json")?
        .join(".cursor")
        .join("hooks.json"))
}

/// `~/.gemini/settings.json`.
pub(crate) fn gemini_settings_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.gemini/settings.json")?
        .join(".gemini")
        .join("settings.json"))
}

/// `~/.gemini/config/hooks.json` — Antigravity CLI lifecycle hooks.
pub(crate) fn antigravity_hooks_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.gemini/config/hooks.json")?
        .join(".gemini")
        .join("config")
        .join("hooks.json"))
}

/// `~/.config/opencode/plugins/ai-memory.ts` — OpenCode's plugin file.
pub(crate) fn opencode_plugin_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.config/opencode")?
        .join(".config")
        .join("opencode")
        .join("plugins")
        .join("ai-memory.ts"))
}

/// `~/.omp/agent/extensions/ai-memory.ts` — OMP lifecycle extension.
pub(crate) fn omp_extension_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(dirs::home_dir()
        .context("could not locate $HOME for ~/.omp/agent/extensions")?
        .join(".omp")
        .join("agent")
        .join("extensions")
        .join("ai-memory.ts"))
}

/// Run the `install-hooks` subcommand.
///
/// # Errors
/// Returns an error if the hook script directory cannot be located.
pub fn run(config: &Config, args: InstallHooksArgs) -> Result<()> {
    let inferred = if args.server_url == DEFAULT_SERVER_URL {
        infer_installed_mcp_config(args.agent)
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
    if args.apply {
        return match args.agent {
            AgentChoice::OpenCode => apply_to_opencode_plugin(&server_url, auth, &args),
            AgentChoice::Omp => apply_to_omp_extension(&server_url, auth, &args),
            AgentChoice::ClaudeCode => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_claude_code_settings(&hooks_dir, &server_url, auth, &args)
            }
            AgentChoice::Codex => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_codex_settings(&hooks_dir, &server_url, auth, &args)
            }
            AgentChoice::Cursor => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_cursor_settings(&hooks_dir, &server_url, auth, &args)
            }
            AgentChoice::GeminiCli => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_gemini_settings(&hooks_dir, &server_url, auth, &args)
            }
            AgentChoice::AntigravityCli => {
                let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
                apply_to_antigravity_settings(&hooks_dir, &server_url, auth, &args)
            }
            AgentChoice::Openclaw => openclaw_plugin::apply(&server_url, auth, &args),
        };
    }
    match args.agent {
        AgentChoice::OpenCode => render_opencode_plugin(&server_url, auth),
        AgentChoice::Omp => render_omp_extension(&server_url, auth),
        AgentChoice::ClaudeCode => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_claude_code(&hooks_dir, &server_url, auth)
        }
        AgentChoice::Codex => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_agent("codex", &hooks_dir, &server_url, auth)
        }
        AgentChoice::Cursor => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_agent("cursor", &hooks_dir, &server_url, auth)
        }
        AgentChoice::GeminiCli => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_agent("gemini-cli", &hooks_dir, &server_url, auth)
        }
        AgentChoice::AntigravityCli => {
            let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
            render_agent("antigravity-cli", &hooks_dir, &server_url, auth)
        }
        AgentChoice::Openclaw => {
            openclaw_plugin::render(&server_url, auth);
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Default)]
struct InferredMcpConfig {
    hook_server_url: Option<String>,
    auth_token: Option<String>,
}

fn effective_hook_server_url(
    config: &Config,
    args: &InstallHooksArgs,
    inferred: Option<&InferredMcpConfig>,
) -> String {
    if args.server_url != DEFAULT_SERVER_URL {
        return normalise_hook_server_url(&args.server_url);
    }
    if config.server_url_configured() {
        return normalise_hook_server_url(&config.server_url);
    }
    if let Some(url) = inferred.and_then(|mcp| mcp.hook_server_url.clone()) {
        return normalise_hook_server_url(&url);
    }
    args.server_url.clone()
}

fn normalise_hook_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn infer_installed_mcp_config(agent: AgentChoice) -> Option<InferredMcpConfig> {
    let client = mcp_client_for_agent(agent)?;
    let path = install_mcp::mcp_config_path(client).ok()?;
    let content = fs::read_to_string(path).ok()?;
    match client {
        McpClient::ClaudeCode => {
            infer_json_mcp_config(&content, &["mcpServers", "ai-memory"], "url")
        }
        McpClient::Codex => infer_codex_mcp_config(&content),
        McpClient::OpenCode => infer_json_mcp_config(&content, &["mcp", "ai-memory"], "url"),
        McpClient::Cursor => infer_json_mcp_config(&content, &["mcpServers", "ai-memory"], "url"),
        McpClient::GeminiCli => {
            infer_json_mcp_config(&content, &["mcpServers", "ai-memory"], "httpUrl")
        }
        McpClient::Openclaw => {
            infer_json_mcp_config(&content, &["mcp", "servers", "ai-memory"], "url")
        }
        McpClient::Pi => infer_json_mcp_config(&content, &["mcpServers", "ai-memory"], "url"),
        McpClient::AntigravityCli => {
            infer_json_mcp_config(&content, &["mcpServers", "ai-memory"], "serverUrl")
        }
        McpClient::ClaudeDesktop => None,
    }
}

fn mcp_client_for_agent(agent: AgentChoice) -> Option<McpClient> {
    match agent {
        AgentChoice::ClaudeCode => Some(McpClient::ClaudeCode),
        AgentChoice::Codex => Some(McpClient::Codex),
        AgentChoice::Cursor => Some(McpClient::Cursor),
        AgentChoice::GeminiCli => Some(McpClient::GeminiCli),
        AgentChoice::OpenCode => Some(McpClient::OpenCode),
        AgentChoice::Omp => Some(McpClient::Pi),
        AgentChoice::Openclaw => Some(McpClient::Openclaw),
        AgentChoice::AntigravityCli => Some(McpClient::AntigravityCli),
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

fn infer_codex_mcp_config(content: &str) -> Option<InferredMcpConfig> {
    let doc: toml_edit::DocumentMut = content.parse().ok()?;
    let server = &doc["mcp_servers"]["ai-memory"];
    let hook_server_url = server["url"]
        .as_str()
        .and_then(hook_server_url_from_mcp_url);
    let auth_token = server["http_headers"]["Authorization"]
        .as_str()
        .or_else(|| server["headers"]["Authorization"].as_str())
        .and_then(bearer_token_from_header);
    if hook_server_url.is_none() && auth_token.is_none() {
        return None;
    }
    Some(InferredMcpConfig {
        hook_server_url,
        auth_token,
    })
}

fn hook_server_url_from_mcp_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.strip_suffix("/mcp").unwrap_or(trimmed).to_string())
}

fn bearer_token_from_header(header: &str) -> Option<String> {
    header
        .trim()
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Mutate `~/.claude/settings.json` in place: replace the seven hook
/// entries ai-memory cares about; preserve every other hook the user
/// has wired up to other tools.
fn apply_to_claude_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => claude_settings_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "claude-code")?;
    let command_dir = staged_command_dir(&staged, "claude-code");
    let payload = build_claude_code_payload(&command_dir, server_url, auth_token);
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_claude_code_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // Get-or-create the top-level `hooks` table, then OVERLAY
            // our seven event keys onto the user's table. Anything
            // they had under a non-overlapping event name (e.g. a
            // hand-written "Notification" hook) survives.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                hooks.insert(event.clone(), value.clone());
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
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => codex_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "codex")?;
    let command_dir = staged_command_dir(&staged, "codex");
    let outcome = merge_codex_hooks(&command_dir, server_url, auth_token, &path)?;
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
    config_path: &Path,
) -> Result<ApplyOutcome> {
    // Build the Codex-flavoured payload. The JSON shape is identical
    // to Claude Code's matcher + nested hooks form — only the event
    // list differs (no `SessionEnd`, which Codex doesn't recognise).
    let payload = build_codex_payload(staged, server_url, auth_token);
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
                hooks.insert(event.clone(), value.clone());
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
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => cursor_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "cursor")?;
    let command_dir = staged_command_dir(&staged, "cursor");
    let outcome = merge_cursor_hooks(&command_dir, server_url, auth_token, &path)?;
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
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_profile_payload(&CURSOR_PROFILE, staged, server_url, auth_token);
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
                hooks.insert(event.clone(), value.clone());
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
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => gemini_settings_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "gemini-cli")?;
    let command_dir = staged_command_dir(&staged, "gemini-cli");
    let outcome = merge_gemini_hooks(&command_dir, server_url, auth_token, &path)?;
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
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_profile_payload(&GEMINI_PROFILE, staged, server_url, auth_token);
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
                hooks.insert(event.clone(), value.clone());
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
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => antigravity_hooks_path()?,
    };
    let staged = stage_hook_scripts(hooks_dir, "antigravity-cli")?;
    let command_dir = staged_command_dir(&staged, "antigravity-cli");
    let outcome = merge_antigravity_hooks(&command_dir, server_url, auth_token, &path)?;
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
    config_path: &Path,
) -> Result<ApplyOutcome> {
    let payload = build_antigravity_payload(staged, server_url, auth_token);
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
                group.insert(event.clone(), value.clone());
            }
            Ok(())
        })
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
    let body = build_opencode_plugin(server_url, auth_token);

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

fn render_opencode_plugin(server_url: &str, auth_token: Option<&str>) -> Result<()> {
    println!("// OpenCode plugin — write to ~/.config/opencode/plugins/ai-memory.ts");
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OpenCode after changing plugins; config is loaded at startup.");
    println!();
    println!("{}", build_opencode_plugin(server_url, auth_token));
    Ok(())
}

fn build_opencode_plugin(server_url: &str, auth_token: Option<&str>) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.
// Edit by re-running the command, not by hand — install-hooks
// will overwrite this file (with a `.bak-<ts>` backup) on each
// re-run.

import type {{ Plugin }} from "@opencode-ai/plugin";
import {{ existsSync, readFileSync }} from "node:fs";
import {{ dirname, join, resolve }} from "node:path";
import {{ homedir }} from "node:os";

const SERVER = {server_literal}.replace(/\/+$/, "");
const AGENT = "open-code";
{token_line}

function timeoutSignal(ms: number): AbortSignal | undefined {{
  if (typeof AbortSignal === "undefined") return undefined;
  const factory = (AbortSignal as unknown as {{ timeout?: (ms: number) => AbortSignal }}).timeout;
  return factory ? factory(ms) : undefined;
}}

function authHeaders(): Record<string, string> {{
  return TOKEN ? {{ Authorization: `Bearer ${{TOKEN}}` }} : {{}};
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

function applyMarkerParams(url: URL, cwd: string | undefined): void {{
  const marker = findMarker(cwd);
  if (!marker || !cwd) return;
  url.searchParams.set("cwd", cwd);
  try {{
    const body = readFileSync(marker, "utf8");
    const workspace = tomlKey(body, "workspace");
    const project = tomlKey(body, "project");
    const projectStrategy = tomlKey(body, "project_strategy");
    if (workspace) url.searchParams.set("workspace", workspace);
    if (project) url.searchParams.set("project", project);
    if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
  }} catch (_e) {{
  }}
}}

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
const handoffChecked = new Set<string>();
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
  postHook("session-start", {{ sessionID: id, cwd, ...extra }});
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
  try {{
    void fetch(url, {{
      method: "POST",
      headers: {{ "Content-Type": "application/json", ...authHeaders() }},
      body: JSON.stringify(payload),
      signal: timeoutSignal(500),
    }}).catch(() => undefined);
  }} catch (_e) {{
    // Fire-and-forget. Hooks must never block the agent.
  }}
}}

async function fetchHandoff(cwd: string): Promise<string | undefined> {{
  const url = new URL(`${{SERVER}}/handoff`);
  url.searchParams.set("agent", AGENT);
  url.searchParams.set("cwd", cwd);
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
      if (!id || handoffChecked.has(id)) return;
      handoffChecked.add(id);
      startSession(id, cwdFor(id, directory));
      const handoff = await fetchHandoff(cwdFor(id, directory));
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
    let body = build_omp_extension(server_url, auth_token);

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

fn render_omp_extension(server_url: &str, auth_token: Option<&str>) -> Result<()> {
    println!("// Oh My Pi / OMP extension — write to ~/.omp/agent/extensions/ai-memory.ts");
    println!("// Or re-run with `--apply` to install it automatically.");
    println!("// Restart OMP after changing extensions; config is loaded at startup.");
    println!();
    println!("{}", build_omp_extension(server_url, auth_token));
    Ok(())
}

fn resolve_omp_extension_path(args: &InstallHooksArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    omp_extension_path()
}

fn build_omp_extension(server_url: &str, auth_token: Option<&str>) -> String {
    let token_line = auth_token
        .map(|t| format!("const TOKEN: string | null = {};\n", ts_string_literal(t)))
        .unwrap_or_else(|| "const TOKEN: string | null = null;\n".to_string());
    format!(
        r#"// Auto-generated by `ai-memory install-hooks --agent omp --apply`.
// Edit by re-running the command, not by hand — install-hooks
// will overwrite this file (with a `.bak-<ts>` backup) on each
// re-run.

import {{ existsSync, readFileSync }} from "node:fs";
import {{ dirname, join, resolve }} from "node:path";
import {{ homedir }} from "node:os";

const SERVER = {server_literal}.replace(/\/+$/, "");
const AGENT = "omp";
{token_line}

function timeoutSignal(ms: number): AbortSignal | undefined {{
  if (typeof AbortSignal === "undefined") return undefined;
  const factory = (AbortSignal as unknown as {{ timeout?: (ms: number) => AbortSignal }}).timeout;
  return factory ? factory(ms) : undefined;
}}

function authHeaders(): Record<string, string> {{
  return TOKEN ? {{ Authorization: `Bearer ${{TOKEN}}` }} : {{}};
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

function applyMarkerParams(url: URL, cwd: string | undefined): void {{
  const marker = findMarker(cwd);
  if (!marker || !cwd) return;
  url.searchParams.set("cwd", cwd);
  try {{
    const body = readFileSync(marker, "utf8");
    const workspace = tomlKey(body, "workspace");
    const project = tomlKey(body, "project");
    const projectStrategy = tomlKey(body, "project_strategy");
    if (workspace) url.searchParams.set("workspace", workspace);
    if (project) url.searchParams.set("project", project);
    if (projectStrategy) url.searchParams.set("project_strategy", projectStrategy);
  }} catch (_e) {{
  }}
}}

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
  postHook("session-start", {{ ...sessionPayload(ctx), ...extra }});
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
  try {{
    void fetch(url, {{
      method: "POST",
      headers: {{ "Content-Type": "application/json", ...authHeaders() }},
      body: JSON.stringify(payload),
      signal: timeoutSignal(500),
    }}).catch(() => undefined);
  }} catch (_e) {{
    // Fire-and-forget. Hooks must never block the agent.
  }}
}}

async function fetchHandoff(cwd: string): Promise<string | undefined> {{
  const url = new URL(`${{SERVER}}/handoff`);
  url.searchParams.set("agent", AGENT);
  url.searchParams.set("cwd", cwd);
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
    const handoff = await fetchHandoff(ctx?.cwd ?? "");
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
) -> Result<()> {
    println!("# {label} hook scripts (manual install — wire each to the matching event)");
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: set AI_MEMORY_AUTH_TOKEN in each hook's environment to the");
        println!("#       value passed via --auth-token (omitted from this printout).");
    } else {
        println!("# Auth: server requires no bearer token. To require one, generate a");
        println!("#       token with `ai-memory generate-auth-token` and pass it via");
        println!("#       --auth-token here AND set AI_MEMORY_AUTH_TOKEN on the server.");
    }
    println!();
    for entry in std::fs::read_dir(hooks_dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_file() && p.extension().is_some_and(|e| e == hook_script_extension()) {
            println!("- {}", p.display());
        }
    }
    println!();
    println!("Set AI_MEMORY_HOOK_URL in each hook's environment to override the default.");
    Ok(())
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

    let mut copied = 0_usize;
    for entry in fs::read_dir(source_dir)
        .with_context(|| format!("reading source bundle {}", source_dir.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        if !from.is_file() || !is_hook_script_file(&from) {
            continue;
        }
        copy_hook_file(&from, &dest_root)?;
        copied += 1;
    }

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

    eprintln!("✓ staged {copied} hook script(s) → {}", dest_root.display());
    Ok(dest_root)
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

fn hook_script_extension() -> &'static str {
    if hook_script_for_current_platform("x.sh").ends_with(".ps1") {
        "ps1"
    } else {
        "sh"
    }
}

fn is_hook_script_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("sh" | "ps1")
    )
}

fn resolve_hooks_dir(explicit: Option<&Path>, agent: AgentChoice) -> Result<PathBuf> {
    let sub = match agent {
        AgentChoice::ClaudeCode => "claude-code",
        AgentChoice::Codex => "codex",
        AgentChoice::Cursor => "cursor",
        AgentChoice::GeminiCli => "gemini-cli",
        AgentChoice::AntigravityCli => "antigravity-cli",
        AgentChoice::OpenCode | AgentChoice::Omp | AgentChoice::Openclaw => {
            anyhow::bail!("{agent:?} uses a generated integration, not a hook script directory")
        }
    };
    if let Some(p) = explicit {
        let path = p.join(sub);
        if path.is_dir() {
            return Ok(path);
        }
        anyhow::bail!("hooks directory {} does not exist", path.display());
    }

    // Probe candidates in order. The first dir that exists wins.
    let candidates = hook_source_candidates(sub, repo_root_guess(), dirs::data_local_dir());
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
    data_local_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(4);
    // Cargo-run from the repo.
    if let Some(root) = repo_root {
        candidates.push(root.join("hooks").join(sub));
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

// CLAUDE_CODE_EVENTS + build_claude_code_payload now live in
// `super::render_shared`, shared with `setup-agent`.

fn render_claude_code(hooks_dir: &Path, server_url: &str, auth_token: Option<&str>) -> Result<()> {
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
    let payload = build_claude_code_payload(hooks_dir, server_url, auth_token);
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing claude code hook config")?;
    println!("# Claude Code hook config — merge into ~/.claude/settings.json");
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook command below.");
        println!("#       Treat ~/.claude/settings.json as sensitive (chmod 600).");
    }
    println!();
    println!("{serialized}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::process::Command;
    use tempfile::TempDir;

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
            hooks_dir: None,
            server_url: DEFAULT_SERVER_URL.into(),
            auth_token: None,
            apply: true,
            config_file: None,
        }
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
        args.server_url = "http://explicit:49374/".into();

        assert_eq!(
            effective_hook_server_url(&config, &args, None),
            "http://explicit:49374"
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

    #[test]
    fn codex_mcp_inference_accepts_block_form_config() {
        let inferred = infer_codex_mcp_config(
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
            "opencode",
            "antigravity-cli",
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

    #[test]
    fn hook_source_candidates_include_native_package_dir() {
        let candidates = hook_source_candidates(
            "claude-code",
            Some(PathBuf::from("/repo")),
            Some(PathBuf::from("/home/alice/.local/share")),
        );

        assert_eq!(candidates[0], PathBuf::from("/repo/hooks/claude-code"));
        assert_eq!(
            candidates[1],
            PathBuf::from("/usr/local/share/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[2],
            PathBuf::from("/usr/share/ai-memory/hooks/claude-code")
        );
        assert_eq!(
            candidates[3],
            PathBuf::from("/home/alice/.local/share/ai-memory/hooks/claude-code")
        );
    }

    // ----------------------------------------------------------------
    // OpenCode tests
    // ----------------------------------------------------------------

    #[test]
    fn opencode_plugin_uses_real_plugin_hooks() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374", Some("tok"));

        assert!(plugin.contains("event: async (input)"));
        assert!(plugin.contains(r#""chat.message": async"#));
        assert!(plugin.contains(r#""tool.execute.before": async"#));
        assert!(plugin.contains(r#""tool.execute.after": async"#));
        assert!(plugin.contains(r#""experimental.chat.system.transform": async"#));
        assert!(plugin.contains("export default AiMemoryHooks"));
        assert!(plugin.contains("const startedSessions = new Set<string>();"));
        assert!(plugin.contains("function startSession"));
        assert!(plugin.contains("fetchHandoff"));
        assert!(plugin.contains("function applyMarkerParams"));
        assert!(plugin.contains("readFileSync(marker, \"utf8\")"));
        assert!(plugin.contains("text.split(/\\r?\\n/)"));
        assert!(plugin.contains("tomlKey(body, \"project_strategy\")"));
        assert!(plugin.contains("url.searchParams.set(\"project_strategy\", projectStrategy)"));
        assert!(plugin.contains(
            "applyMarkerParams(url, typeof payload.cwd === \"string\" ? payload.cwd : undefined);"
        ));
        assert!(plugin.contains("applyMarkerParams(url, cwd);"));
        assert!(plugin.contains("postPreCompact"));
        assert!(plugin.contains("postHook(\"session-start\""));
        assert!(plugin.contains("postHook(\"user-prompt\""));
        assert!(plugin.contains("Bearer ${TOKEN}"));
        assert!(plugin.contains("tok"));
        assert!(
            !plugin.contains(r#""session.created": async"#),
            "OpenCode bus events must be handled through the `event` hook"
        );
    }

    #[test]
    fn opencode_plugin_normalizes_payloads_without_legacy_wrapper() {
        let plugin = build_opencode_plugin("http://127.0.0.1:49374/", None);

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

    // ----------------------------------------------------------------
    // OMP tests
    // ----------------------------------------------------------------

    #[test]
    fn omp_extension_uses_native_lifecycle_events() {
        let extension = build_omp_extension("http://127.0.0.1:49374", Some("tok"));

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
        assert!(extension.contains("function applyMarkerParams"));
        assert!(extension.contains("readFileSync(marker, \"utf8\")"));
        assert!(extension.contains("text.split(/\\r?\\n/)"));
        assert!(extension.contains("tomlKey(body, \"project_strategy\")"));
        assert!(extension.contains("url.searchParams.set(\"project_strategy\", projectStrategy)"));
        assert!(extension.contains(
            "applyMarkerParams(url, typeof payload.cwd === \"string\" ? payload.cwd : undefined);"
        ));
        assert!(extension.contains("applyMarkerParams(url, cwd);"));
        assert!(extension.contains("Bearer ${TOKEN}"));
        assert!(extension.contains("tok"));
    }

    #[test]
    fn omp_extension_is_directly_discoverable_by_omp() {
        let tmp = TempDir::new().unwrap();
        let args = InstallHooksArgs {
            agent: AgentChoice::Omp,
            hooks_dir: None,
            server_url: "http://127.0.0.1:49374".into(),
            auth_token: None,
            apply: true,
            config_file: Some(tmp.path().join("extensions").join("ai-memory.ts")),
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

    #[cfg(unix)]
    #[test]
    fn curl_installer_accepts_generated_integration_agents() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("scripts")
            .join("install-hooks.sh");

        for alias in ["opencode", "openclaw", "pi", "oh-my-pi"] {
            let output = Command::new("bash")
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
                "pi" | "oh-my-pi" => {
                    assert!(stdout.contains("install-hooks --agent omp --apply"));
                    assert!(stdout.contains("~/.omp/agent/extensions/ai-memory.ts"));
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
            &config_path,
        )
        .unwrap();
        assert_eq!(second, ApplyOutcome::NoOp, "second apply must be a no-op");
    }
}
