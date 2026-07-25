//! `ai-memory install-mcp` — print the MCP server registration
//! snippet for any supported client.
//!
//! The snippet format and the config-file location differ across
//! clients. We render the *content* the user needs to paste; we
//! deliberately do not auto-edit their config (formats are evolving
//! upstream and a bad merge is very user-visible).
//!
//! For clients that don't support remote MCP servers in their JSON
//! config (Claude Desktop today), the rendered snippet uses the
//! community-standard `npx mcp-remote` stdio shim so the same HTTP
//! endpoint still works.
//!
//! OMP uses a native `~/.omp/agent/mcp.json` file with the same
//! `mcpServers` root as several other clients.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::cli::{InstallMcpArgs, McpClient};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json, mutate_toml};
use crate::commands::path_util::{claude_config_dir, home_dir};
use crate::commands::render_shared::bearer_header_value;
use crate::config::{Config, DEFAULT_MCP_URL};

const GEMINI_MCP_TIMEOUT_MS: u64 = 5000;

#[derive(Clone, Copy)]
enum JsonMcpLocation {
    RootMcpServers,
    RootMcp,
    NestedMcpServers,
    /// Top-level `servers` key — what VS Code's MCP framework expects
    /// in `.vscode/mcp.json` (workspace) or the user-level mcp.json.
    /// Distinct from `RootMcpServers` despite the similar shape: VS
    /// Code documents `servers`, not `mcpServers`, and writing the
    /// wrong key produces a silent no-op rather than an error.
    RootServers,
}

/// Run the `install-mcp` subcommand.
///
/// # Errors
/// Returns an error if JSON serialisation fails (should never happen
/// for our handcrafted values).
pub fn run(config: &Config, args: InstallMcpArgs) -> Result<()> {
    let server_url = effective_mcp_server_url(config, &args);
    let args = InstallMcpArgs {
        server_url: Some(server_url),
        auth_token: args.auth_token.or_else(|| config.auth.bearer_token.clone()),
        ..args
    };
    if args.apply {
        return apply_to_config_file(&args);
    }
    let snippet = match args.client {
        McpClient::ClaudeCode => render_claude_code(&args, &resolve_config_file(&args)?)?,
        McpClient::Codex => render_codex(&args),
        McpClient::Grok => render_grok(&args)?,
        McpClient::OpenCode => render_opencode(&args)?,
        McpClient::Cursor => render_cursor(&args)?,
        McpClient::ClaudeDesktop => render_claude_desktop(&args)?,
        McpClient::GeminiCli => render_gemini_cli(&args)?,
        McpClient::Openclaw => render_openclaw(&args)?,
        McpClient::Pi => render_pi(&args)?,
        McpClient::Omp => render_omp(&args)?,
        McpClient::AntigravityCli => render_antigravity_cli(&args)?,
        McpClient::Zero => render_zero(&args)?,
        McpClient::Devin => render_devin(&args)?,
        McpClient::KimiCode => render_kimi_code(&args)?,
        McpClient::VsCodeCopilot => render_vscode_copilot(&args)?,
    };
    println!("{snippet}");
    Ok(())
}

fn effective_mcp_server_url(config: &Config, args: &InstallMcpArgs) -> String {
    if let Some(url) = &args.server_url {
        // Normalize an explicit --server-url exactly like the config/env
        // branch below: users habitually pass the BASE url (the same value
        // `install-hooks --server-url` takes), and returning it verbatim
        // rendered a config pointing at the server root, which 404s (#185).
        // `mcp_server_url_from_base` is idempotent for full `/mcp` endpoints,
        // so callers who already pass the endpoint are unchanged.
        return mcp_server_url_from_base(url);
    }
    if config.server_url_configured() {
        return mcp_server_url_from_base(&config.server_url);
    }
    DEFAULT_MCP_URL.to_string()
}

fn mcp_server_url_from_base(server_url: &str) -> String {
    let trimmed = server_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/mcp") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/mcp")
    }
}

/// Default MCP config-file path for a client (ignores any
/// `--config-file` override). Shared by install and uninstall.
///
/// # Errors
/// Returns an error for `Pi` (no MCP config), for Claude Desktop on
/// unsupported OSes, or when `$HOME` can't be resolved.
pub(crate) fn mcp_config_path(client: crate::cli::McpClient) -> Result<PathBuf> {
    use crate::cli::McpClient;
    let home = || home_dir().context("could not locate $HOME for config-file auto-detect");
    Ok(match client {
        McpClient::ClaudeCode => claude_code_config_path_in(std::env::var_os("CLAUDE_CONFIG_DIR"))?,
        McpClient::Codex => home()?.join(".codex").join("config.toml"),
        // Project scope is `.grok/config.toml` under cwd/repo; pass
        // --config-file for that case rather than inventing a second default.
        McpClient::Grok => grok_home()?.join("config.toml"),
        McpClient::OpenCode => home()?
            .join(".config")
            .join("opencode")
            .join("opencode.json"),
        McpClient::Cursor => home()?.join(".cursor").join("mcp.json"),
        McpClient::ClaudeDesktop => {
            #[cfg(target_os = "macos")]
            {
                home()?
                    .join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            }
            #[cfg(target_os = "windows")]
            {
                // %APPDATA% is roughly ~/AppData/Roaming.
                home()?
                    .join("AppData")
                    .join("Roaming")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                bail!(
                    "Claude Desktop is not officially distributed for this OS. \
                     Pass --config-file explicitly if you know where it lives."
                );
            }
        }
        McpClient::GeminiCli => home()?.join(".gemini").join("settings.json"),
        McpClient::Openclaw => home()?.join(".openclaw").join("config.json"),
        McpClient::Pi => bail!(
            "Pi has no native mcp.json; use `ai-memory install-hooks --agent pi --apply` to install the generated MCP bridge extension."
        ),
        McpClient::Omp => home()?.join(".omp").join("agent").join("mcp.json"),
        McpClient::AntigravityCli => home()?
            .join(".gemini")
            .join("antigravity-cli")
            .join("mcp_config.json"),
        // Zero resolves its user config under $XDG_CONFIG_HOME falling back
        // to ~/.config; we target the default and --config-file covers
        // non-default XDG setups (same policy as OpenCode above).
        McpClient::Zero => home()?.join(".config").join("zero").join("config.json"),
        McpClient::Devin => home()?.join(".devin").join("config.json"),
        // Kimi Code keeps its data dir at $KIMI_CODE_HOME when set,
        // falling back to ~/.kimi-code; MCP servers live in mcp.json at
        // that root.
        McpClient::KimiCode => kimi_code_home(std::env::var_os("KIMI_CODE_HOME"))?.join("mcp.json"),
        // VS Code MCP is workspace-scoped by default: `.vscode/mcp.json`
        // at the current workspace root. The user-profile alternative
        // lives under VS Code's profile-specific data dir; use VS
        // Code's `MCP: Open User Configuration` command to open it,
        // then pass that concrete path via `--config-file`.
        McpClient::VsCodeCopilot => std::env::current_dir()
            .context("could not resolve current dir for .vscode/mcp.json default")?
            .join(".vscode")
            .join("mcp.json"),
    })
}

/// Claude Code reads MCP-server registrations from `.claude.json`
/// (the same file `claude mcp add`/`claude mcp list` operate on) —
/// `$CLAUDE_CONFIG_DIR/.claude.json` when the var is set, else
/// `~/.claude.json`. `settings.json` is a separate file for hooks /
/// permissions / etc. — putting `mcpServers` there does NOT make
/// Claude Code load the server. (Confirmed against CC 1.x by
/// observing that `mcpServers` in settings.json is silently ignored
/// while the same entry under `~/.claude.json` shows up in
/// `claude mcp list`.) The env value comes in as a parameter so tests
/// can exercise both branches without mutating process env.
fn claude_code_config_path_in(env_override: Option<std::ffi::OsString>) -> Result<PathBuf> {
    if let Some(dir) = claude_config_dir(env_override) {
        return Ok(dir.join(".claude.json"));
    }
    Ok(home_dir()
        .context("could not locate $HOME for ~/.claude.json")?
        .join(".claude.json"))
}

/// Kimi Code's data dir: `$KIMI_CODE_HOME` when set (non-empty), else
/// `~/.kimi-code`. The env value comes in as a parameter so tests can
/// exercise both branches without mutating process env.
fn kimi_code_home(env_override: Option<std::ffi::OsString>) -> Result<PathBuf> {
    if let Some(dir) = env_override.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(home_dir()
        .context("could not locate $HOME for config-file auto-detect")?
        .join(".kimi-code"))
}

/// Resolve Grok Build CLI's user configuration root. Grok honours
/// `GROK_HOME`; otherwise it uses `~/.grok`.
pub(crate) fn grok_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("GROK_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()
        .context("could not locate $HOME for Grok configuration")?
        .join(".grok"))
}

/// Resolve the user-config file for this client. Honours
/// `--config-file` when provided, else uses the canonical default
/// per client.
fn resolve_config_file(args: &InstallMcpArgs) -> Result<PathBuf> {
    if let Some(p) = &args.config_file {
        return Ok(p.clone());
    }
    mcp_config_path(args.client)
}

/// Mutate the resolved client config file in place. Idempotent —
/// re-runs that produce the same content are reported as no-op.
fn apply_to_config_file(args: &InstallMcpArgs) -> Result<()> {
    if matches!(args.client, McpClient::Pi) {
        bail!(pi_mcp_apply_guidance(args));
    }
    let path = resolve_config_file(args)?;
    let outcome = match args.client {
        McpClient::Codex => apply_atomic(&path, |existing| {
            mutate_toml(existing, |doc| codex_upsert_mcp_server(doc, args))
        })?,
        McpClient::Grok => apply_atomic(&path, |existing| {
            mutate_toml(existing, |doc| grok_upsert_mcp_server(doc, args))
        })?,
        _ => apply_atomic(&path, |existing| {
            mutate_json(existing, |root| upsert_json_mcp_entry(root, args))
        })?,
    };
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

fn json_mcp_location(client: McpClient) -> Option<JsonMcpLocation> {
    match client {
        McpClient::ClaudeCode
        | McpClient::ClaudeDesktop
        | McpClient::Cursor
        | McpClient::GeminiCli
        | McpClient::Omp
        | McpClient::AntigravityCli
        | McpClient::Devin
        | McpClient::KimiCode => Some(JsonMcpLocation::RootMcpServers),
        McpClient::OpenCode => Some(JsonMcpLocation::RootMcp),
        // Zero's config.json nests servers under `mcp.servers`, the same
        // shape OpenClaw uses.
        McpClient::Openclaw | McpClient::Zero => Some(JsonMcpLocation::NestedMcpServers),
        McpClient::VsCodeCopilot => Some(JsonMcpLocation::RootServers),
        McpClient::Codex | McpClient::Grok | McpClient::Pi => None,
    }
}

fn build_json_mcp_entry(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    match args.client {
        McpClient::OpenCode => build_mcp_entry_opencode(args),
        McpClient::Openclaw => build_mcp_entry_openclaw(args),
        McpClient::Zero => build_mcp_entry_zero(args),
        McpClient::Codex | McpClient::Grok => {
            bail!("internal: Codex/Grok MCP config is TOML, not JSON")
        }
        _ => build_mcp_entry(args),
    }
}

fn upsert_json_mcp_entry(
    root: &mut serde_json::Map<String, serde_json::Value>,
    args: &InstallMcpArgs,
) -> Result<()> {
    let entry = build_json_mcp_entry(args)?;
    match json_mcp_location(args.client).context("internal: unsupported JSON MCP client")? {
        JsonMcpLocation::RootMcpServers => {
            let servers = root
                .entry("mcpServers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcpServers` is present but not an object")?;
            servers.insert(args.name.clone(), entry);
        }
        JsonMcpLocation::RootMcp => {
            let mcp = root
                .entry("mcp")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcp` is present but not an object")?;
            mcp.insert(args.name.clone(), entry);
        }
        JsonMcpLocation::NestedMcpServers => {
            let mcp = root
                .entry("mcp")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcp` is present but not an object")?;
            let servers = mcp
                .entry("servers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`mcp.servers` is present but not an object")?;
            servers.insert(args.name.clone(), entry);
        }
        JsonMcpLocation::RootServers => {
            let servers = root
                .entry("servers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`servers` is present but not an object")?;
            servers.insert(args.name.clone(), entry);
        }
    }
    Ok(())
}

fn render_json_mcp_fragment(args: &InstallMcpArgs) -> Result<String> {
    let entry = build_json_mcp_entry(args)?;
    let fragment =
        match json_mcp_location(args.client).context("internal: unsupported JSON MCP client")? {
            JsonMcpLocation::RootMcpServers => json!({
                "mcpServers": { args.name.as_str(): entry }
            }),
            JsonMcpLocation::RootMcp => json!({
                "mcp": { args.name.as_str(): entry }
            }),
            JsonMcpLocation::NestedMcpServers => json!({
                "mcp": { "servers": { args.name.as_str(): entry } }
            }),
            JsonMcpLocation::RootServers => json!({
                "servers": { args.name.as_str(): entry }
            }),
        };
    Ok(serde_json::to_string_pretty(&fragment)?)
}

/// Append the `flavor=moonshot` marker to the MCP URL written into Kimi
/// Code's mcp.json: Moonshot's API 400s root-level `anyOf`/`oneOf`/`allOf`
/// in tool parameter schemas (issue #155's `anyOf` on `memory_read_page`),
/// and the server answers flavored requests with flat schemas. Idempotent
/// so re-runs never stack duplicate query pairs.
pub(crate) fn moonshot_flavored_mcp_url(server_url: &str) -> String {
    const FLAVOR: &str = "flavor=moonshot";
    let url = server_url.trim();
    let already_marked = url
        .split_once('?')
        .is_some_and(|(_, query)| query.split('&').any(|pair| pair == FLAVOR));
    if already_marked {
        return url.to_string();
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{FLAVOR}")
}

/// JSON entry shape used by Claude Code, Claude Desktop, Cursor, and
/// Gemini CLI — they all accept `mcpServers.<name>` with `url` or
/// `httpUrl` plus optional `headers`. Returns the per-client variant.
fn build_mcp_entry(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    // `run()` resolves the URL before dispatch; the fallback only fires for
    // direct callers (tests, uninstall re-render) that skip that step.
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    match args.client {
        McpClient::ClaudeCode => {
            entry.insert("type".into(), json!("http"));
            entry.insert("url".into(), json!(server_url));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::ClaudeDesktop => {
            // Stdio shim via mcp-remote — Claude Desktop's JSON
            // doesn't accept HTTP transport directly.
            let mut cmd_args = vec![json!("-y"), json!("mcp-remote"), json!(server_url)];
            if let Some(b) = &bearer {
                cmd_args.push(json!("--header"));
                cmd_args.push(json!("Authorization:${AI_MEMORY_AUTH_HEADER}"));
                entry.insert("env".into(), json!({"AI_MEMORY_AUTH_HEADER": b}));
            }
            entry.insert("command".into(), json!("npx"));
            entry.insert("args".into(), serde_json::Value::Array(cmd_args));
        }
        McpClient::Cursor => {
            entry.insert("url".into(), json!(server_url));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::GeminiCli => {
            entry.insert("httpUrl".into(), json!(server_url));
            entry.insert("timeout".into(), json!(GEMINI_MCP_TIMEOUT_MS));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::Omp => {
            entry.insert("type".into(), json!("http"));
            entry.insert("url".into(), json!(server_url));
            entry.insert("enabled".into(), json!(true));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::AntigravityCli => {
            entry.insert("serverUrl".into(), json!(server_url));
            entry.insert("timeout".into(), json!(GEMINI_MCP_TIMEOUT_MS));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::Devin => {
            entry.insert("url".into(), json!(server_url));
            entry.insert("transport".into(), json!("http"));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::KimiCode => {
            // Kimi Code treats an entry with `url` and no `transport`
            // field as streamable-HTTP; `transport` is only for legacy
            // SSE endpoints.
            entry.insert("url".into(), json!(moonshot_flavored_mcp_url(server_url)));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::VsCodeCopilot => {
            // VS Code MCP framework schema: `type: "http"` + `url`,
            // headers map for auth. Verified against
            // https://code.visualstudio.com/docs/agents/reference/mcp-configuration.
            // The `mcpServers` key (used by Claude Code/Cursor/Gemini)
            // is silently ignored here — VS Code reads `servers`.
            entry.insert("type".into(), json!("http"));
            entry.insert("url".into(), json!(server_url));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        _ => bail!("internal: build_mcp_entry called for unsupported client"),
    }
    Ok(serde_json::Value::Object(entry))
}

fn build_mcp_entry_opencode(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    entry.insert("type".into(), json!("remote"));
    entry.insert("url".into(), json!(server_url));
    entry.insert("enabled".into(), json!(true));
    if let Some(b) = bearer {
        entry.insert("headers".into(), json!({"Authorization": b}));
    }
    Ok(serde_json::Value::Object(entry))
}

fn build_mcp_entry_openclaw(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    entry.insert("url".into(), json!(server_url));
    entry.insert("transport".into(), json!("streamable-http"));
    if let Some(b) = bearer {
        entry.insert("headers".into(), json!({"Authorization": b}));
    }
    Ok(serde_json::Value::Object(entry))
}

/// Zero (Gitlawb/zero) MCP entry: native HTTP transport with optional
/// bearer headers — `internal/config/types.go`'s `MCPServerConfig` accepts
/// `type: "http"` + `url` + a `headers` map (issue #156).
fn build_mcp_entry_zero(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let server_url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL);
    let mut entry = serde_json::Map::new();
    entry.insert("type".into(), json!("http"));
    entry.insert("url".into(), json!(server_url));
    if let Some(b) = bearer {
        entry.insert("headers".into(), json!({"Authorization": b}));
    }
    Ok(serde_json::Value::Object(entry))
}

/// Insert / replace `[mcp_servers.<name>]` in a Codex `config.toml`.
///
/// Codex parses both forms (block-style `[mcp_servers.foo]` and the
/// dotted-inline `mcp_servers = { foo = { ... } }`), but its docs show
/// the block form and that's the only one humans want to read. This
/// helper canonicalises to the block form even when the file currently
/// stores `mcp_servers` as an inline table — siblings are preserved.
fn codex_upsert_mcp_server(
    doc: &mut toml_edit::DocumentMut,
    args: &InstallMcpArgs,
) -> anyhow::Result<()> {
    use toml_edit::{Item, Table, value};

    // Build our `[mcp_servers.<name>]` as a block-style table.
    //
    // IMPORTANT: Codex's MCP schema (verified against
    // `openai/codex/codex-rs/config/src/mcp_types.rs`) draws a hard
    // line between transports. For STREAMABLE_HTTP (which ai-memory
    // uses — `url = "...mcp"` triggers this transport), the
    // allowed auth-related keys are:
    //
    //   bearer_token_env_var  string  env-var NAME holding the token
    //   http_headers          table   static headers map
    //   env_http_headers      table   header_name → env_var_name
    //
    // `bearer_token` (literal) is rejected with
    //   "bearer_token is not supported for streamable_http"
    // — it's a stdio-transport-only key. Confusingly the field
    // sits in the same struct, but throw_if_set guards it for
    // streamable_http.
    //
    // We use [mcp_servers.<name>.http_headers] with a literal
    // Authorization header. Static, no env-var dance required.
    //
    // History note (so the next maintainer doesn't repeat this):
    //   - v1: emitted `[mcp_servers.X.headers]` — wrong key name
    //     entirely, Codex silently ignored it and fell back to
    //     OAuth ("Run `codex mcp login <name>`").
    //   - v2: switched to top-level `bearer_token = "..."` — also
    //     wrong; Codex rejects this for streamable_http with the
    //     "bearer_token is not supported" error.
    //   - v3 (this): `[mcp_servers.X.http_headers]` with
    //     `Authorization = "Bearer ..."`. Codex schema-validates
    //     and uses it as a static auth header.
    let mut server = Table::new();
    server["url"] = value(args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL));
    // Auto-approve ai-memory's tool calls. Without this, Codex
    // prompts on EVERY tool invocation ("approve memory_query?"
    // "approve memory_briefing?" …) which makes the MCP unusable
    // for an auto-capture workflow. The valid TOML values per
    // Codex's `AppToolApproval` enum are "auto" / "prompt" /
    // "approve" — `approve` means "no prompt, just run it". ai-
    // memory's surface is dominantly read-only (query, recent,
    // status, briefing, explore); the few writes (consolidate,
    // forget_sweep) are tagged `destructiveHint: true` upstream
    // so any agent that wants to gate THOSE specifically can
    // override per-tool — see Codex's `[mcp_servers.X.tools]`
    // map.
    server["default_tools_approval_mode"] = value("approve");
    if let Some(b) = bearer_header_value(args.auth_token.as_deref()) {
        let mut headers = Table::new();
        headers["Authorization"] = value(b);
        server["http_headers"] = Item::Table(headers);
    }

    upsert_toml_mcp_server(doc, &args.name, server);
    Ok(())
}

/// Insert / replace `[mcp_servers.<name>]` in a Grok `config.toml`.
///
/// Grok's schema (user-guide § MCP Servers) uses:
/// - `url` for native HTTP/SSE
/// - optional `enabled = true`
/// - `[mcp_servers.<name>.headers]` for static headers (NOT Codex's
///   `http_headers` key — wrong key is silently ignored by Grok)
///
/// Sibling servers are preserved; storage is canonicalised to block form
/// even when the file currently holds an inline `mcp_servers` table.
fn grok_upsert_mcp_server(
    doc: &mut toml_edit::DocumentMut,
    args: &InstallMcpArgs,
) -> anyhow::Result<()> {
    use toml_edit::{Item, Table, value};

    let mut server = Table::new();
    server["url"] = value(args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL));
    server["enabled"] = value(true);
    if let Some(b) = bearer_header_value(args.auth_token.as_deref()) {
        let mut headers = Table::new();
        headers["Authorization"] = value(b);
        server["headers"] = Item::Table(headers);
    }

    upsert_toml_mcp_server(doc, &args.name, server);
    Ok(())
}

/// Replace one server while preserving siblings and canonicalising an inline
/// `mcp_servers` map to block-form TOML tables.
fn upsert_toml_mcp_server(doc: &mut toml_edit::DocumentMut, name: &str, server: toml_edit::Table) {
    use toml_edit::{Item, Table, Value};

    let preserved: Vec<(String, Item)> = match doc.get("mcp_servers") {
        Some(Item::Table(table)) => table
            .iter()
            .filter(|(key, _)| *key != name)
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect(),
        Some(Item::Value(Value::InlineTable(table))) => table
            .iter()
            .filter(|(key, _)| *key != name)
            .map(|(key, value)| (key.to_string(), Item::Value(value.clone())))
            .collect(),
        _ => Vec::new(),
    };

    let mut parent = Table::new();
    parent.set_implicit(true);
    for (k, v) in preserved {
        parent.insert(&k, v);
    }
    parent.insert(name, Item::Table(server));

    doc.insert("mcp_servers", Item::Table(parent));
}

fn render_claude_code(args: &InstallMcpArgs, config_path: &Path) -> Result<String> {
    let bearer = bearer_header_value(args.auth_token.as_deref());
    let cli_line = if let Some(b) = &bearer {
        format!(
            "claude mcp add --transport http {name} {url} \\\n    --header \"Authorization: {b}\"",
            name = args.name,
            url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
            b = b,
        )
    } else {
        format!(
            "claude mcp add --transport http {name} {url}",
            name = args.name,
            url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
        )
    };
    let snippet = render_json_mcp_fragment(args)?;
    Ok(format!(
        "# Claude Code — register the MCP server\n\
         #\n\
         # Recommended (one-shot CLI):\n\
         {cli_line}\n\
         #\n\
         # Equivalent JSON if you'd rather edit {config_path} directly:\n\
         {snippet}\n",
        config_path = config_path.display(),
    ))
}

fn render_codex(args: &InstallMcpArgs) -> String {
    // Codex uses TOML, not JSON. Hand-render the snippet so the
    // table headers stay deterministic.
    //
    // Schema: Codex's MCP `streamable_http` transport accepts
    //   - `bearer_token_env_var = "NAME"` (env-var indirection)
    //   - `[mcp_servers.<name>.http_headers]` (static headers)
    //   - `[mcp_servers.<name>.env_http_headers]` (env-var-sourced headers)
    // — NOT a literal `bearer_token = "..."` (that's stdio-only)
    // and NOT a `[mcp_servers.<name>.headers]` sub-table (the key
    // is `http_headers`, with the `http_` prefix).
    let mut out = format!(
        "# Codex CLI — append to ~/.codex/config.toml\n\
         #\n\
         [mcp_servers.{name}]\n\
         url = \"{url}\"\n\
         # Skip per-call approval prompts on ai-memory's tools.\n\
         # ai-memory is read-mostly + writes are auto-capture; the\n\
         # approval friction makes it unusable otherwise.\n\
         default_tools_approval_mode = \"approve\"\n",
        name = args.name,
        url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
    );
    if let Some(b) = bearer_header_value(args.auth_token.as_deref()) {
        out.push_str(&format!(
            "\n[mcp_servers.{name}.http_headers]\n\
             Authorization = \"{b}\"\n\
             # Alternative (avoids embedding the literal token):\n\
             # bearer_token_env_var = \"AI_MEMORY_AUTH_TOKEN\"\n\
             # — and export AI_MEMORY_AUTH_TOKEN in your shell init.\n",
            name = args.name,
            b = b,
        ));
    }
    out
}

fn render_grok(args: &InstallMcpArgs) -> Result<String> {
    // Grok Build CLI uses TOML under ~/.grok/config.toml. Schema differs
    // from Codex: static auth lives under `.headers` (not `http_headers`),
    // and `enabled = true` is the documented toggle.
    let mut doc = toml_edit::DocumentMut::new();
    grok_upsert_mcp_server(&mut doc, args)?;
    let config_path = grok_home()?.join("config.toml");
    let mut out = format!(
        "# Grok Build CLI — append to {config_path}\n\
         #\n\
         # Native HTTP transport. Pair with:\n\
         #   ai-memory install-hooks --agent grok --apply\n\
         # for lifecycle capture. Grok ignores SessionStart stdout, so\n\
         # handoffs are recovered via MCP memory_handoff_accept.\n\
         #\n\
         # CLI alternative:\n\
         #   grok mcp add --transport http {name} {url}\n\
         #\n\
         [mcp_servers.{name}]\n\
         url = \"{url}\"\n\
         enabled = true\n",
        name = args.name,
        url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
        config_path = config_path.display(),
    );
    let config_start = out
        .find("[mcp_servers.")
        .context("internal: generated Grok TOML table missing")?;
    out.truncate(config_start);
    out.push_str(&doc.to_string());
    Ok(out)
}

fn render_opencode(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# OpenCode — add to ~/.config/opencode/opencode.json under \"mcp\":\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_cursor(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Cursor — write to one of:\n\
         #   - ~/.cursor/mcp.json   (global, all projects)\n\
         #   - .cursor/mcp.json     (per-project, in the workspace root)\n\
         #\n\
         # Cursor supports HTTP MCP servers via the `url` field. Restart\n\
         # Cursor (or toggle the server off+on in Settings → MCP) after\n\
         # adding a new entry; live reload landed in recent builds but\n\
         # is still flaky.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_claude_desktop(args: &InstallMcpArgs) -> Result<String> {
    // mcp-remote's --header flag is how we plumb the Authorization
    // through Claude Desktop's stdio-only config. Put the Bearer value
    // in env so Windows subprocess parsing never has to split a value
    // containing a space.
    Ok(format!(
        "# Claude Desktop — write to claude_desktop_config.json:\n\
         #   - macOS:    ~/Library/Application Support/Claude/claude_desktop_config.json\n\
         #   - Windows:  %APPDATA%\\Claude\\claude_desktop_config.json\n\
         #   - Linux:    Claude Desktop is not officially distributed for Linux;\n\
         #               use Claude Code or another HTTP client instead.\n\
         #\n\
         # Claude Desktop's JSON config does not support HTTP MCP servers\n\
         # directly. We bridge through the community `mcp-remote` stdio shim\n\
         # (https://www.npmjs.com/package/mcp-remote). Requires Node.js.\n\
         # After editing, fully quit + relaunch Claude Desktop; \"Check for\n\
         # Updates\" is not enough.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_gemini_cli(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Gemini CLI — merge into ~/.gemini/settings.json:\n\
         #\n\
         # Gemini CLI uses `httpUrl` (not `url`) for streamable-HTTP\n\
         # endpoints. The `timeout` is in milliseconds.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_openclaw(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# OpenClaw — merge into ~/.openclaw/config.json:\n\
         #\n\
         # OpenClaw distinguishes transports explicitly. Use\n\
         # \"transport\": \"streamable-http\" for ai-memory's HTTP endpoint.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_zero(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Zero (Gitlawb/zero) — merge into ~/.config/zero/config.json\n\
         # ($XDG_CONFIG_HOME/zero/config.json on non-default XDG setups),\n\
         # or run `zero mcp add` / re-run this command with --apply.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_pi(args: &InstallMcpArgs) -> Result<String> {
    Ok(pi_mcp_render_guidance(args))
}

fn pi_mcp_render_guidance(args: &InstallMcpArgs) -> String {
    format!(
        "# Pi has no native mcp.json. Do not write ~/.pi/agent/mcp.json.\n\
         # Install ai-memory's generated Pi extension instead; it includes\n\
         # lifecycle capture and an HTTP MCP bridge that registers tools in Pi.\n\
         ai-memory install-hooks --agent pi --apply --server-url {}{}\n\
         # Restart Pi after installing ~/.pi/agent/extensions/ai-memory.ts.\n",
        hook_server_url_from_mcp_url(args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL)),
        if args.auth_token.is_some() {
            " --auth-token <token>"
        } else {
            ""
        }
    )
}

fn pi_mcp_apply_guidance(args: &InstallMcpArgs) -> String {
    format!(
        "Pi has no native mcp.json; refusing to write MCP config. Install the generated bridge instead: ai-memory install-hooks --agent pi --apply --server-url {}{}",
        hook_server_url_from_mcp_url(args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL)),
        if args.auth_token.is_some() {
            " --auth-token <token>"
        } else {
            ""
        }
    )
}

fn hook_server_url_from_mcp_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    trimmed.strip_suffix("/mcp").unwrap_or(trimmed).to_string()
}

fn render_omp(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Oh My Pi / OMP — merge into ~/.omp/agent/mcp.json:\n\
         #\n\
         # The current Oh My Pi package exposes the `omp` binary and native\n\
         # `.omp` config directories. Restart `omp` after changing MCP config.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_antigravity_cli(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Antigravity CLI (`agy`) — merge into ~/.gemini/antigravity-cli/mcp_config.json:\n\
         #\n\
         # Antigravity CLI uses `serverUrl` (not `url` or `httpUrl`) for\n\
         # streamable-HTTP endpoints. The `timeout` is in milliseconds.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_devin(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Devin CLI — merge into ~/.devin/config.json:\n\
         #\n\
         # Devin uses `mcpServers` with HTTP transport and optional Bearer auth.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_kimi_code(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Kimi Code — merge into ~/.kimi-code/mcp.json\n\
         # ($KIMI_CODE_HOME/mcp.json when KIMI_CODE_HOME is set):\n\
         #\n\
         # An entry with `url` and no `transport` field is a streamable-HTTP\n\
         # server; `transport` is only needed for legacy SSE endpoints.\n\
         # `?flavor=moonshot`: Moonshot's API rejects root-level schema\n\
         # combinators; ai-memory serves flat schemas to flavored requests.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_vscode_copilot(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# VS Code GitHub Copilot (agent mode) — write to one of:\n\
         #   - .vscode/mcp.json   (workspace, recommended — matches\n\
         #                         ai-memory's per-cwd auto-scoping)\n\
         #   - the user-profile mcp.json opened by VS Code's\n\
         #     `MCP: Open User Configuration` command\n\
         #\n\
         # VS Code's MCP framework uses `servers` (NOT `mcpServers`) as the\n\
         # top-level key, `type: \"http\"` for streamable-HTTP endpoints, and\n\
         # an inline `headers` map for Authorization. Copilot's agent mode\n\
         # reads this config along with any other MCP-capable VS Code\n\
         # extension. Toggle the server from the MCP view in the\n\
         # Extensions sidebar after editing.\n\
         #\n\
         # NOTE: VS Code Copilot does not yet expose lifecycle hooks\n\
         # (PreToolUse / PostToolUse / SessionStart), so ai-memory's\n\
         # automatic capture is NOT active here — call `memory_query`,\n\
         # `memory_write_page`, etc. from chat when you need them.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    use tempfile;

    fn args_for(client: McpClient) -> InstallMcpArgs {
        InstallMcpArgs {
            client,
            server_url: None,
            name: "ai-memory".into(),
            auth_token: None,
            apply: false,
            config_file: None,
        }
    }

    fn args_with_token(client: McpClient) -> InstallMcpArgs {
        InstallMcpArgs {
            client,
            server_url: None,
            name: "ai-memory".into(),
            auth_token: Some("test-token-deadbeef".into()),
            apply: false,
            config_file: None,
        }
    }

    #[test]
    fn claude_code_config_path_honours_claude_config_dir() {
        let custom = if cfg!(windows) {
            r"C:\custom\claude"
        } else {
            "/custom/claude"
        };
        let path = claude_code_config_path_in(Some(std::ffi::OsString::from(custom))).unwrap();
        assert_eq!(path, std::path::Path::new(custom).join(".claude.json"));

        // Empty override and unset var both fall back to ~/.claude.json.
        for env in [None, Some(std::ffi::OsString::new())] {
            let path = claude_code_config_path_in(env).unwrap();
            assert!(
                path.ends_with(".claude.json") && !path.starts_with(custom),
                "default must be ~/.claude.json, got {}",
                path.display()
            );
        }
    }

    #[test]
    fn claude_code_render_shows_resolved_config_path() {
        let args = args_for(McpClient::ClaudeCode);
        let config_path = std::path::Path::new("/stores/claude/.claude.json");
        let out = render_claude_code(&args, config_path).unwrap();
        assert!(
            out.contains("/stores/claude/.claude.json"),
            "render must mention the resolved config path:\n{out}"
        );
        assert!(
            !out.contains("~/.claude.json"),
            "render must not hardcode ~/.claude.json when the resolved path differs:\n{out}"
        );
    }

    fn render_with_token(client: McpClient) -> String {
        let args = args_with_token(client);
        match args.client {
            McpClient::ClaudeCode => {
                render_claude_code(&args, Path::new("/home/alice/.claude.json")).unwrap()
            }
            McpClient::Codex => render_codex(&args),
            McpClient::Grok => render_grok(&args).unwrap(),
            McpClient::OpenCode => render_opencode(&args).unwrap(),
            McpClient::Cursor => render_cursor(&args).unwrap(),
            McpClient::ClaudeDesktop => render_claude_desktop(&args).unwrap(),
            McpClient::GeminiCli => render_gemini_cli(&args).unwrap(),
            McpClient::Openclaw => render_openclaw(&args).unwrap(),
            McpClient::Pi => render_pi(&args).unwrap(),
            McpClient::Omp => render_omp(&args).unwrap(),
            McpClient::AntigravityCli => render_antigravity_cli(&args).unwrap(),
            McpClient::Zero => render_zero(&args).unwrap(),
            McpClient::Devin => render_devin(&args).unwrap(),
            McpClient::KimiCode => render_kimi_code(&args).unwrap(),
            McpClient::VsCodeCopilot => render_vscode_copilot(&args).unwrap(),
        }
    }

    /// With `--auth-token` set, every renderer must embed the Bearer
    /// header in its output.
    #[test]
    fn auth_token_threaded_into_every_client() {
        for client in [
            McpClient::ClaudeCode,
            McpClient::Codex,
            McpClient::Grok,
            McpClient::OpenCode,
            McpClient::Cursor,
            McpClient::ClaudeDesktop,
            McpClient::GeminiCli,
            McpClient::Openclaw,
            McpClient::Omp,
            McpClient::AntigravityCli,
            McpClient::Zero,
            McpClient::Devin,
            McpClient::KimiCode,
            McpClient::VsCodeCopilot,
        ] {
            let out = render_with_token(client);
            // Every client embeds the token as `Authorization:
            // Bearer <token>` in some flavour of headers map — the
            // exact key path differs (Codex uses `http_headers`,
            // OpenCode uses `headers`, Cursor / Gemini / Claude
            // Desktop / Claude Code use `headers` inside their
            // server entry, etc.), but the literal `Bearer
            // <token>` substring shows up in all of them. Keep
            // the assertion uniform.
            assert!(
                out.contains("Bearer test-token-deadbeef"),
                "client {client:?} did not embed the bearer token:\n{out}"
            );
        }
    }

    /// Sanity: every supported client renders without error and the
    /// output mentions the configured server URL.
    #[test]
    fn every_client_renders() {
        for client in [
            McpClient::ClaudeCode,
            McpClient::Codex,
            McpClient::Grok,
            McpClient::OpenCode,
            McpClient::Cursor,
            McpClient::ClaudeDesktop,
            McpClient::GeminiCli,
            McpClient::Openclaw,
            McpClient::Omp,
            McpClient::AntigravityCli,
            McpClient::Zero,
            McpClient::Devin,
            McpClient::KimiCode,
            McpClient::VsCodeCopilot,
        ] {
            let out = render_for_test(client);
            assert!(
                out.contains("http://127.0.0.1:49374/mcp"),
                "client {client:?} did not include the server URL in output:\n{out}"
            );
        }
    }

    fn render_for_test(client: McpClient) -> String {
        let args = args_for(client);
        match args.client {
            McpClient::ClaudeCode => {
                render_claude_code(&args, Path::new("/home/alice/.claude.json")).unwrap()
            }
            McpClient::Codex => render_codex(&args),
            McpClient::Grok => render_grok(&args).unwrap(),
            McpClient::OpenCode => render_opencode(&args).unwrap(),
            McpClient::Cursor => render_cursor(&args).unwrap(),
            McpClient::ClaudeDesktop => render_claude_desktop(&args).unwrap(),
            McpClient::GeminiCli => render_gemini_cli(&args).unwrap(),
            McpClient::Openclaw => render_openclaw(&args).unwrap(),
            McpClient::Pi => render_pi(&args).unwrap(),
            McpClient::Omp => render_omp(&args).unwrap(),
            McpClient::AntigravityCli => render_antigravity_cli(&args).unwrap(),
            McpClient::Zero => render_zero(&args).unwrap(),
            McpClient::Devin => render_devin(&args).unwrap(),
            McpClient::KimiCode => render_kimi_code(&args).unwrap(),
            McpClient::VsCodeCopilot => render_vscode_copilot(&args).unwrap(),
        }
    }

    #[test]
    fn mcp_server_url_defaults_to_configured_server_url() {
        let config = Config {
            server_url: "http://192.168.0.90:49374/".into(),
            ..Config::default()
        };
        let args = args_for(McpClient::OpenCode);

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "http://192.168.0.90:49374/mcp"
        );
    }

    #[test]
    fn mcp_server_url_does_not_duplicate_mcp_suffix() {
        let config = Config {
            server_url: "http://192.168.0.90:49374/mcp".into(),
            ..Config::default()
        };
        let args = args_for(McpClient::OpenCode);

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "http://192.168.0.90:49374/mcp"
        );
    }

    /// Regression for #185: an explicit `--server-url` passed as a BASE url
    /// (the same value `install-hooks --server-url` takes) must gain the
    /// `/mcp` suffix, or every client renderer emits a config pointing at
    /// the server root, which 404s. Trailing slashes are trimmed first.
    #[test]
    fn mcp_server_url_explicit_base_url_gains_mcp_suffix() {
        let config = Config::default();
        let mut args = args_for(McpClient::ClaudeCode);
        args.server_url = Some("https://memory.example.com".into());
        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "https://memory.example.com/mcp"
        );

        args.server_url = Some("https://memory.example.com/".into());
        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "https://memory.example.com/mcp"
        );

        // A reverse-proxy base path keeps its prefix.
        args.server_url = Some("https://host/prefix".into());
        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "https://host/prefix/mcp"
        );
    }

    #[test]
    fn mcp_server_url_explicit_flag_wins_over_config() {
        let config = Config {
            server_url: "http://homelab:49374".into(),
            ..Config::default()
        };
        let mut args = args_for(McpClient::OpenCode);
        args.server_url = Some("http://explicit:49374/mcp".into());

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            "http://explicit:49374/mcp"
        );
    }

    /// Regression (found 2026-07-12 during Devin real-acceptance A/B
    /// testing): an explicit `--server-url` that happens to equal the
    /// compiled-in `DEFAULT_MCP_URL` must still win over a configured
    /// (env/config.toml) server_url pointing somewhere else. Mirrors
    /// `hook_server_url_explicit_flag_matching_compiled_default_still_wins`
    /// in install_hooks.rs -- same bug class, same fix, both commands.
    #[test]
    fn mcp_server_url_explicit_flag_matching_compiled_default_still_wins() {
        let config = Config {
            server_url: "http://127.0.0.1:49375".into(),
            ..Config::default()
        };
        let mut args = args_for(McpClient::OpenCode);
        args.server_url = Some(DEFAULT_MCP_URL.to_string());

        assert_eq!(
            effective_mcp_server_url(&config, &args),
            DEFAULT_MCP_URL,
            "an explicit --server-url matching the compiled default must not be \
             silently overridden by a differently-configured server_url"
        );
    }

    /// Specific shape checks — each client has a distinguishing key
    /// in its JSON snippet. This catches accidental cross-pollination
    /// between renderers (e.g. Gemini's `httpUrl` showing up under
    /// Cursor's `mcpServers`).
    #[test]
    fn client_specific_shape_keys() {
        assert!(render_for_test(McpClient::Cursor).contains("\"url\""));
        assert!(render_for_test(McpClient::GeminiCli).contains("\"httpUrl\""));
        assert!(render_for_test(McpClient::ClaudeDesktop).contains("mcp-remote"));
        assert!(render_for_test(McpClient::Openclaw).contains("\"streamable-http\""));
        assert!(render_for_test(McpClient::Codex).contains("[mcp_servers.ai-memory]"));
        let grok = render_for_test(McpClient::Grok);
        assert!(grok.contains("[mcp_servers.ai-memory]"));
        assert!(grok.contains("enabled = true"));
        assert!(
            grok.contains(
                &grok_home()
                    .unwrap()
                    .join("config.toml")
                    .display()
                    .to_string()
            )
        );
        // Grok uses `headers`, never Codex's `http_headers`.
        let grok_token = render_with_token(McpClient::Grok);
        assert!(grok_token.contains("[mcp_servers.ai-memory.headers]"));
        assert!(!grok_token.contains("http_headers"));
        assert!(render_for_test(McpClient::Omp).contains("~/.omp/agent/mcp.json"));
        let pi = render_pi(&args_for(McpClient::Pi)).unwrap();
        assert!(pi.contains("Pi has no native mcp.json"));
        assert!(pi.contains("install-hooks --agent pi --apply"));
        assert!(pi.contains("~/.pi/agent/extensions/ai-memory.ts"));
        assert!(!pi.contains("~/.omp"));
        assert!(render_for_test(McpClient::AntigravityCli).contains("\"serverUrl\""));
        let devin = render_for_test(McpClient::Devin);
        assert!(devin.contains("\"mcpServers\""));
        assert!(devin.contains("\"url\""));
        assert!(devin.contains("\"transport\": \"http\""));
        assert!(!devin.contains("\"httpUrl\""));
        let devin_with_token = render_with_token(McpClient::Devin);
        assert!(devin_with_token.contains("\"headers\""));
        assert!(devin_with_token.contains("\"Authorization\": \"Bearer test-token-deadbeef\""));
        // Kimi Code: `url` with NO `transport` field means streamable-HTTP
        // (`transport` is legacy-SSE-only there), and the URL key is plain
        // `url` — not Gemini's `httpUrl` or Antigravity's `serverUrl`.
        let kimi = render_for_test(McpClient::KimiCode);
        assert!(kimi.contains("\"mcpServers\""));
        assert!(kimi.contains("\"url\""));
        assert!(kimi.contains("http://127.0.0.1:49374/mcp?flavor=moonshot"));
        assert!(!kimi.contains("\"transport\""));
        assert!(!kimi.contains("\"httpUrl\""));
        assert!(!kimi.contains("\"serverUrl\""));
        let kimi_with_token = render_with_token(McpClient::KimiCode);
        assert!(kimi_with_token.contains("\"headers\""));
        assert!(kimi_with_token.contains("\"Authorization\": \"Bearer test-token-deadbeef\""));
        // VS Code Copilot must use the `servers` top-level key — the
        // `mcpServers` form is silently ignored by VS Code's MCP
        // framework. Regression guard against a future copy-paste
        // from the Cursor / Claude Code renderer.
        let vsc = render_for_test(McpClient::VsCodeCopilot);
        assert!(vsc.contains("\"servers\""));
        assert!(!vsc.contains("\"mcpServers\""));
        assert!(vsc.contains("\"type\": \"http\""));
    }

    /// Kimi Code resolves its mcp.json under $KIMI_CODE_HOME when the env
    /// var is set (non-empty), else under ~/.kimi-code. Tested through the
    /// helper's parameter so no process-env mutation (unsafe in edition
    /// 2024, and racy under parallel tests) is needed.
    #[test]
    fn kimi_code_home_honours_env_override() {
        assert_eq!(
            kimi_code_home(Some("/tmp/custom-kimi-home".into())).unwrap(),
            PathBuf::from("/tmp/custom-kimi-home")
        );
        let default = home_dir().unwrap().join(".kimi-code");
        // An empty override falls back to the default home-based dir.
        assert_eq!(kimi_code_home(Some("".into())).unwrap(), default);
        assert_eq!(kimi_code_home(None).unwrap(), default);
    }

    /// Pin the append rules: `?` on a bare endpoint, `&` with an existing
    /// query, never duplicate an existing marker.
    #[test]
    fn moonshot_flavored_mcp_url_appends_marker_idempotently() {
        for (input, expected) in [
            (
                "http://127.0.0.1:49374/mcp",
                "http://127.0.0.1:49374/mcp?flavor=moonshot",
            ),
            (
                "http://homelab:49374/mcp?token=abc",
                "http://homelab:49374/mcp?token=abc&flavor=moonshot",
            ),
            (
                "http://127.0.0.1:49374/mcp?flavor=moonshot",
                "http://127.0.0.1:49374/mcp?flavor=moonshot",
            ),
            (
                "http://homelab:49374/mcp?token=abc&flavor=moonshot",
                "http://homelab:49374/mcp?token=abc&flavor=moonshot",
            ),
            // Whole-pair match: a marker inside another pair's VALUE doesn't count.
            (
                "http://homelab:49374/mcp?note=flavor=moonshot",
                "http://homelab:49374/mcp?note=flavor=moonshot&flavor=moonshot",
            ),
        ] {
            assert_eq!(moonshot_flavored_mcp_url(input), expected, "input: {input}");
        }
    }

    #[test]
    fn pi_apply_fails_closed_without_writing_even_with_config_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("mcp.json");
        let mut args = args_for(McpClient::Pi);
        args.apply = true;
        args.config_file = Some(path.clone());

        let err = apply_to_config_file(&args).unwrap_err().to_string();

        assert!(
            err.contains("has no native mcp.json"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("install-hooks --agent pi --apply"),
            "unexpected error: {err}"
        );
        assert!(!path.exists(), "Pi install must not write ignored config");
    }

    #[test]
    fn pi_guidance_derives_hook_url_from_mcp_url() {
        let mut args = args_for(McpClient::Pi);
        args.server_url = Some("http://host:49374/base/mcp".into());
        args.auth_token = Some("tok".into());

        let guidance = render_pi(&args).unwrap();

        assert!(guidance.contains("--server-url http://host:49374/base --auth-token <token>"));
        assert!(!guidance.contains("--server-url http://host:49374/base/mcp"));
    }

    /// The Codex apply path must emit block-form `[mcp_servers.<name>]`
    /// headers, NOT a dotted inline-table on one line. Regression
    /// guard: M22 originally created `mcp_servers = { ai-memory = {...} }`
    /// because toml_edit auto-vivifies inline tables when you assign
    /// through `doc["foo"]["bar"]`.
    #[test]
    fn codex_apply_writes_block_form_tables() {
        let args = args_with_token(McpClient::Codex);
        let mut doc: toml_edit::DocumentMut = "".parse().unwrap();
        codex_upsert_mcp_server(&mut doc, &args).unwrap();
        let out = doc.to_string();
        assert!(
            out.contains("[mcp_servers.ai-memory]"),
            "expected block-form table header, got:\n{out}"
        );
        // Auth lives on the [mcp_servers.X.http_headers] sub-table
        // with an Authorization: Bearer <token> value. The key is
        // `http_headers` (with the `http_` prefix) per Codex's
        // streamable_http schema. Two related regressions guarded
        // here:
        //   - the legacy `headers` key (no `http_` prefix) made
        //     Codex silently fall back to OAuth login;
        //   - a top-level `bearer_token = "..."` was rejected with
        //     "bearer_token is not supported for streamable_http"
        //     (that key is stdio-transport-only).
        assert!(
            out.contains("[mcp_servers.ai-memory.http_headers]"),
            "expected `[mcp_servers.X.http_headers]` sub-table, got:\n{out}"
        );
        assert!(
            out.contains("Authorization = \"Bearer test-token-deadbeef\""),
            "expected the Authorization header in the http_headers sub-table, got:\n{out}"
        );
        assert!(
            !out.contains("[mcp_servers.ai-memory.headers]"),
            "legacy `headers` key (no `http_` prefix) must not be emitted; got:\n{out}"
        );
        assert!(
            !out.contains("\nbearer_token ="),
            "top-level `bearer_token` is rejected for streamable_http; must not be emitted; got:\n{out}"
        );
        assert!(
            !out.contains("mcp_servers = {"),
            "found inline-table form (regression):\n{out}"
        );
    }

    /// Migrating from the old M22 inline-table form to block form must
    /// be idempotent — the second apply produces identical output.
    #[test]
    fn codex_apply_migrates_inline_form_and_is_idempotent() {
        let args = args_with_token(McpClient::Codex);

        // Simulate a config.toml in the *old* inline form.
        let original = "approval_policy = \"on-request\"\n\
                        mcp_servers = { ai-memory = { url = \"http://old\", \
                        headers = { Authorization = \"Bearer old\" } } }\n\
                        \n\
                        [other]\n\
                        keep = \"this\"\n";
        let mut doc: toml_edit::DocumentMut = original.parse().unwrap();
        codex_upsert_mcp_server(&mut doc, &args).unwrap();
        let first = doc.to_string();

        // After migration the inline-table form is gone.
        assert!(!first.contains("mcp_servers = {"));
        assert!(first.contains("[mcp_servers.ai-memory]"));
        // Unrelated content survives.
        assert!(first.contains("approval_policy"));
        assert!(first.contains("[other]"));
        assert!(first.contains("keep = \"this\""));

        // Re-applying produces the same bytes (idempotency contract).
        let mut doc2: toml_edit::DocumentMut = first.parse().unwrap();
        codex_upsert_mcp_server(&mut doc2, &args).unwrap();
        let second = doc2.to_string();
        assert_eq!(
            first, second,
            "second apply must produce identical bytes; diff:\n--- first\n{first}\n--- second\n{second}"
        );
    }

    /// Sibling `[mcp_servers.<other>]` entries the user has configured
    /// (e.g. a different MCP server) must survive an --apply.
    #[test]
    fn codex_apply_preserves_sibling_mcp_servers() {
        let args = args_for(McpClient::Codex);
        let original = "[mcp_servers.other-server]\n\
                        url = \"http://other\"\n";
        let mut doc: toml_edit::DocumentMut = original.parse().unwrap();
        codex_upsert_mcp_server(&mut doc, &args).unwrap();
        let out = doc.to_string();
        assert!(out.contains("[mcp_servers.other-server]"));
        assert!(out.contains("http://other"));
        assert!(out.contains("[mcp_servers.ai-memory]"));
    }

    #[test]
    fn grok_apply_writes_block_form_with_headers_not_http_headers() {
        let args = args_with_token(McpClient::Grok);
        let mut doc: toml_edit::DocumentMut = "".parse().unwrap();
        grok_upsert_mcp_server(&mut doc, &args).unwrap();
        let out = doc.to_string();
        assert!(
            out.contains("[mcp_servers.ai-memory]"),
            "expected block-form table header, got:\n{out}"
        );
        assert!(
            out.contains("enabled = true"),
            "expected enabled = true, got:\n{out}"
        );
        assert!(
            out.contains("[mcp_servers.ai-memory.headers]"),
            "expected `[mcp_servers.X.headers]` sub-table, got:\n{out}"
        );
        assert!(
            out.contains("Authorization = \"Bearer test-token-deadbeef\""),
            "expected Authorization header, got:\n{out}"
        );
        assert!(
            !out.contains("http_headers"),
            "Codex's http_headers key must not be emitted for Grok; got:\n{out}"
        );
        assert!(
            !out.contains("mcp_servers = {"),
            "found inline-table form (regression):\n{out}"
        );
    }

    #[test]
    fn grok_apply_preserves_sibling_mcp_servers_and_is_idempotent() {
        let args = args_with_token(McpClient::Grok);
        let original = "[mcp_servers.other-server]\n\
                        url = \"http://other\"\n\
                        enabled = true\n";
        let mut doc: toml_edit::DocumentMut = original.parse().unwrap();
        grok_upsert_mcp_server(&mut doc, &args).unwrap();
        let first = doc.to_string();
        assert!(first.contains("[mcp_servers.other-server]"));
        assert!(first.contains("http://other"));
        assert!(first.contains("[mcp_servers.ai-memory]"));
        assert!(first.contains("[mcp_servers.ai-memory.headers]"));

        let mut doc2: toml_edit::DocumentMut = first.parse().unwrap();
        grok_upsert_mcp_server(&mut doc2, &args).unwrap();
        assert_eq!(first, doc2.to_string());
    }

    #[test]
    fn grok_mcp_client_parses() {
        let cli = crate::cli::Cli::parse_from([
            "ai-memory",
            "install-mcp",
            "--client",
            "grok",
            "--server-url",
            "http://example.test:49374",
        ]);
        let crate::cli::Command::InstallMcp(args) = cli.command else {
            panic!("expected install-mcp");
        };
        assert!(matches!(args.client, McpClient::Grok));
    }

    #[test]
    fn devin_mcp_client_parses() {
        let cli = crate::cli::Cli::parse_from([
            "ai-memory",
            "install-mcp",
            "--client",
            "devin",
            "--server-url",
            "http://example.test:49374",
        ]);
        let crate::cli::Command::InstallMcp(mcp_args) = cli.command else {
            panic!("expected install-mcp command for devin");
        };
        assert!(matches!(mcp_args.client, crate::cli::McpClient::Devin));
    }

    #[test]
    fn devin_apply_writes_mcp_servers() {
        let args = args_with_token(McpClient::Devin);
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");

        let entry = build_json_mcp_entry(&args).unwrap();
        let root = serde_json::json!({
            "mcpServers": {
                "ai-memory": entry
            }
        });

        fs::write(&config_path, serde_json::to_string_pretty(&root).unwrap()).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            parsed["mcpServers"]["ai-memory"].is_object(),
            "Devin config must have mcpServers.ai-memory"
        );
        assert_eq!(
            parsed["mcpServers"]["ai-memory"]["url"],
            "http://127.0.0.1:49374/mcp"
        );
        assert_eq!(parsed["mcpServers"]["ai-memory"]["transport"], "http");
    }

    #[test]
    fn devin_apply_preserves_sibling_mcp_servers() {
        let args = args_with_token(McpClient::Devin);
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");

        // Pre-existing config with sibling MCP server
        fs::write(
            &config_path,
            r#"{"mcpServers":{"other-server":{"url":"http://example.com","transport":"http"}}}"#,
        )
        .unwrap();

        let mut args_with_path = args.clone();
        args_with_path.config_file = Some(config_path.clone());

        apply_to_config_file(&args_with_path).unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
        // Sibling must be preserved
        assert!(
            parsed["mcpServers"]["other-server"].is_object(),
            "other-server must be preserved"
        );
        // ai-memory must be added
        assert!(
            parsed["mcpServers"]["ai-memory"].is_object(),
            "ai-memory must be added"
        );
    }

    #[test]
    fn devin_apply_mcp_is_idempotent() {
        let args = args_with_token(McpClient::Devin);
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");

        let mut args_with_path = args.clone();
        args_with_path.config_file = Some(config_path.clone());

        apply_to_config_file(&args_with_path).unwrap();

        let first_content = fs::read_to_string(&config_path).unwrap();

        apply_to_config_file(&args_with_path).unwrap();

        let second_content = fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            first_content, second_content,
            "second apply must produce identical bytes"
        );
    }

    #[test]
    fn grok_print_uses_apply_toml_builder_for_dotted_names_and_quotes() {
        let mut args = args_with_token(McpClient::Grok);
        args.name = "ai.memory".into();
        args.server_url = Some("https://memory.example/mcp?note=\"quoted\"".into());

        let printed = render_grok(&args).unwrap();
        let mut applied = toml_edit::DocumentMut::new();
        grok_upsert_mcp_server(&mut applied, &args).unwrap();
        let expected = applied.to_string();

        assert!(printed.ends_with(&expected), "print output:\n{printed}");
        let parsed: toml_edit::DocumentMut = expected.parse().unwrap();
        assert!(
            parsed
                .get("mcp_servers")
                .unwrap()
                .get("ai.memory")
                .is_some()
        );
    }
}
