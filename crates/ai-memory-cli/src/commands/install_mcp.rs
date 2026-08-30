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
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
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
    /// Top-level `context_servers` key used by Zed's settings.json.
    RootContextServers,
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
    validate_args(&args)?;
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
        McpClient::Zcode => render_zcode(&args)?,
        McpClient::Devin => render_devin(&args)?,
        McpClient::KimiCode => render_kimi_code(&args)?,
        McpClient::KiroCli => render_kiro_cli(&args)?,
        McpClient::CommandCode => render_command_code(&args)?,
        McpClient::Swival => render_swival(&args)?,
        McpClient::VsCodeCopilot => render_vscode_copilot(&args)?,
        McpClient::Zed => render_zed(&args)?,
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

pub(crate) fn mcp_server_url_from_base(server_url: &str) -> String {
    let trimmed = server_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/mcp") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/mcp")
    }
}

fn validate_args(args: &InstallMcpArgs) -> Result<()> {
    if args.session_aware && !matches!(args.client, McpClient::ClaudeCode) {
        bail!("--session-aware is supported only for --client claude-code");
    }
    if matches!(args.client, McpClient::KiroCli) {
        validate_kiro_remote_url(args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL))?;
    }
    Ok(())
}

/// Kiro accepts HTTPS remote MCP endpoints and plain HTTP only on loopback.
/// Reject an unusable registration before `--apply` mutates user config.
fn validate_kiro_remote_url(server_url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(server_url).context("Kiro MCP URL is not a valid URL")?;
    if parsed.scheme() == "https" {
        return Ok(());
    }
    let loopback = parsed.host_str().is_some_and(|host| {
        let normalized = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        normalized.eq_ignore_ascii_case("localhost")
            || normalized
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if parsed.scheme() == "http" && loopback {
        return Ok(());
    }
    bail!(
        "Kiro CLI requires HTTPS for remote MCP servers (plain HTTP is accepted only on localhost); configure an HTTPS reverse proxy or pass a loopback URL"
    )
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
                let local_data_dir = dirs::data_local_dir()
                    .context("could not locate %LOCALAPPDATA% for Claude Desktop config")?;
                let roaming_config_dir = dirs::config_dir()
                    .context("could not locate %APPDATA% for Claude Desktop config")?;
                claude_desktop_config_path_in(&local_data_dir, &roaming_config_dir)?
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
            .join("config")
            .join("mcp_config.json"),
        // Zero resolves its user config under $XDG_CONFIG_HOME falling back
        // to ~/.config; we target the default and --config-file covers
        // non-default XDG setups (same policy as OpenCode above).
        McpClient::Zero => home()?.join(".config").join("zero").join("config.json"),
        // ZCode keeps its user-scope config at ~/.zcode/cli/config.json
        // (workspace scopes like .zcode/config.json exist, but user scope
        // is the install default for every other client too).
        McpClient::Zcode => home()?.join(".zcode").join("cli").join("config.json"),
        McpClient::Devin => home()?.join(".devin").join("config.json"),
        // Kimi Code keeps its data dir at $KIMI_CODE_HOME when set,
        // falling back to ~/.kimi-code; MCP servers live in mcp.json at
        // that root.
        McpClient::KimiCode => kimi_code_home(std::env::var_os("KIMI_CODE_HOME"))?.join("mcp.json"),
        McpClient::KiroCli => kiro_home(std::env::var_os("KIRO_HOME"))?
            .join("settings")
            .join("mcp.json"),
        McpClient::CommandCode => home()?.join(".commandcode").join("mcp.json"),
        McpClient::Swival => {
            let cwd = std::env::current_dir()
                .context("could not resolve current dir for .swival/mcp.json default")?;
            swival_project_root(&cwd).join(".swival").join("mcp.json")
        }
        // VS Code MCP is workspace-scoped by default: `.vscode/mcp.json`
        // at the current workspace root. The user-profile alternative
        // lives under VS Code's profile-specific data dir; use VS
        // Code's `MCP: Open User Configuration` command to open it,
        // then pass that concrete path via `--config-file`.
        McpClient::VsCodeCopilot => std::env::current_dir()
            .context("could not resolve current dir for .vscode/mcp.json default")?
            .join(".vscode")
            .join("mcp.json"),
        McpClient::Zed => {
            let config_dir = if cfg!(target_os = "macos") {
                home()?.join(".config")
            } else {
                dirs::config_dir().context("could not locate user config directory for Zed")?
            };
            zed_config_path_in(&config_dir, std::env::consts::OS)
        }
    })
}

/// Match Swival's own base-dir discovery: the nearest ancestor containing
/// `.git` or `swival.toml`, falling back to the invocation directory.
fn swival_project_root(start: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let mut current = resolved.as_path();
    loop {
        if current.join(".git").exists() || current.join("swival.toml").exists() {
            return current.to_path_buf();
        }
        let Some(parent) = current.parent() else {
            return resolved;
        };
        current = parent;
    }
}

/// Resolve Zed's user settings from the platform config root. The root is
/// injected so path behavior can be tested without consulting the real home.
fn zed_config_path_in(config_dir: &Path, target_os: &str) -> PathBuf {
    let app_dir = if target_os == "windows" { "Zed" } else { "zed" };
    config_dir.join(app_dir).join("settings.json")
}

#[cfg(any(target_os = "windows", test))]
fn claude_desktop_config_path_in(
    local_data_dir: &Path,
    roaming_config_dir: &Path,
) -> Result<PathBuf> {
    let local_packages_dir = local_data_dir.join("Packages");
    Ok(
        packaged_claude_desktop_config_path(&local_packages_dir)?.unwrap_or_else(|| {
            roaming_config_dir
                .join("Claude")
                .join("claude_desktop_config.json")
        }),
    )
}

/// Detect an MSIX/AppX-packaged Claude Desktop under
/// `%LOCALAPPDATA%\Packages`. Prefer the package that already contains a
/// config file; otherwise a single package directory is enough because the
/// atomic apply path creates its lazy `LocalCache` descendants.
#[cfg(any(target_os = "windows", test))]
fn packaged_claude_desktop_config_path(local_packages_dir: &Path) -> Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(local_packages_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "could not inspect Claude Desktop packages under {}",
                    local_packages_dir.display()
                )
            });
        }
    };
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.get(.."Claude_".len()))
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Claude_"))
        })
        .collect();
    candidates.sort();

    let config_path = |package_dir: &Path| {
        package_dir
            .join("LocalCache")
            .join("Roaming")
            .join("Claude")
            .join("claude_desktop_config.json")
    };
    let existing: Vec<PathBuf> = candidates
        .iter()
        .map(|package_dir| config_path(package_dir))
        .filter(|path| path.is_file())
        .collect();

    match (existing.as_slice(), candidates.as_slice()) {
        ([path], _) => Ok(Some(path.clone())),
        ([], []) => Ok(None),
        ([], [package_dir]) => Ok(Some(config_path(package_dir))),
        _ => {
            let package_names = candidates
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "multiple Claude Desktop packages were found under {} ({package_names}); \
                 pass --config-file with the active package's \
                 LocalCache\\Roaming\\Claude\\claude_desktop_config.json path",
                local_packages_dir.display()
            )
        }
    }
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

/// Kiro CLI's global configuration root: `$KIRO_HOME` when set, otherwise
/// `~/.kiro`. The override is injected to keep path tests process-local.
fn kiro_home(env_override: Option<std::ffi::OsString>) -> Result<PathBuf> {
    if let Some(dir) = env_override.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(home_dir()
        .context("could not locate $HOME for Kiro configuration")?
        .join(".kiro"))
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
        McpClient::Zed => apply_atomic(&path, |existing| zed_upsert_mcp_server(existing, args))?,
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

/// Merge ai-memory into Zed's JSONC settings without discarding comments,
/// trailing commas, or unrelated formatting.
fn zed_upsert_mcp_server(existing: &str, args: &InstallMcpArgs) -> Result<String> {
    let root = CstRootNode::parse(existing, &ParseOptions::default())
        .context("parsing Zed settings.json as JSONC")?;
    let settings = root
        .object_value_or_create()
        .context("Zed settings.json root is present but not an object")?;
    let servers = settings
        .object_value_or_create("context_servers")
        .context("`context_servers` is present but not an object")?;
    let entry = serde_json_to_cst(build_json_mcp_entry(args)?);
    if let Some(existing_entry) = servers.get(&args.name) {
        existing_entry.set_value(entry);
    } else {
        servers.append(&args.name, entry);
    }
    Ok(root.to_string())
}

fn serde_json_to_cst(value: serde_json::Value) -> CstInputValue {
    match value {
        serde_json::Value::Null => CstInputValue::Null,
        serde_json::Value::Bool(value) => CstInputValue::Bool(value),
        serde_json::Value::Number(value) => CstInputValue::Number(value.to_string()),
        serde_json::Value::String(value) => CstInputValue::String(value),
        serde_json::Value::Array(values) => {
            CstInputValue::Array(values.into_iter().map(serde_json_to_cst).collect())
        }
        serde_json::Value::Object(values) => CstInputValue::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, serde_json_to_cst(value)))
                .collect(),
        ),
    }
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
        | McpClient::KimiCode
        | McpClient::KiroCli
        | McpClient::CommandCode
        | McpClient::Swival => Some(JsonMcpLocation::RootMcpServers),
        McpClient::OpenCode => Some(JsonMcpLocation::RootMcp),
        // Zero's config.json nests servers under `mcp.servers`, the same
        // shape OpenClaw uses. ZCode nests its servers the same way.
        McpClient::Openclaw | McpClient::Zero | McpClient::Zcode => {
            Some(JsonMcpLocation::NestedMcpServers)
        }
        McpClient::VsCodeCopilot => Some(JsonMcpLocation::RootServers),
        McpClient::Zed => Some(JsonMcpLocation::RootContextServers),
        McpClient::Codex | McpClient::Grok | McpClient::Pi => None,
    }
}

fn build_json_mcp_entry(args: &InstallMcpArgs) -> Result<serde_json::Value> {
    validate_args(args)?;
    match args.client {
        McpClient::OpenCode => build_mcp_entry_opencode(args),
        McpClient::Openclaw => build_mcp_entry_openclaw(args),
        McpClient::Zero => build_mcp_entry_zero(args),
        McpClient::Zcode => build_mcp_entry_zcode(args),
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
        JsonMcpLocation::RootContextServers => {
            let servers = root
                .entry("context_servers")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`context_servers` is present but not an object")?;
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
            JsonMcpLocation::RootContextServers => json!({
                "context_servers": { args.name.as_str(): entry }
            }),
        };
    Ok(serde_json::to_string_pretty(&fragment)?)
}

/// Append the `flavor=moonshot` marker to the MCP URL written into Kimi
/// Code's mcp.json: Moonshot's API 400s root-level `anyOf`/`oneOf`/`allOf`
/// in tool parameter schemas (issue #155's `anyOf` on `memory_read_page`),
/// and the server answers flavored requests with flat schemas. Idempotent
/// so re-runs never stack duplicate query pairs.
fn flavored_mcp_url(server_url: &str, flavor: &str) -> String {
    let marker = format!("flavor={flavor}");
    let url = server_url.trim();
    let already_marked = url
        .split_once('?')
        .is_some_and(|(_, query)| query.split('&').any(|pair| pair == marker));
    if already_marked {
        return url.to_string();
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{marker}")
}

pub(crate) fn moonshot_flavored_mcp_url(server_url: &str) -> String {
    flavored_mcp_url(server_url, "moonshot")
}

pub(crate) fn bedrock_flavored_mcp_url(server_url: &str) -> String {
    flavored_mcp_url(server_url, "bedrock")
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
            if args.session_aware {
                entry.insert("type".into(), json!("stdio"));
                entry.insert("command".into(), json!("ai-memory"));
                entry.insert(
                    "args".into(),
                    json!(["mcp-bridge", "--server-url", server_url]),
                );
                if let Some(token) = &args.auth_token {
                    entry.insert("env".into(), json!({"AI_MEMORY_AUTH_TOKEN": token}));
                }
            } else {
                entry.insert("type".into(), json!("http"));
                entry.insert("url".into(), json!(server_url));
                if let Some(b) = &bearer {
                    entry.insert("headers".into(), json!({"Authorization": b}));
                }
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
        McpClient::Cursor | McpClient::Zed => {
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
        McpClient::KiroCli => {
            entry.insert("url".into(), json!(bedrock_flavored_mcp_url(server_url)));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::CommandCode => {
            entry.insert("transport".into(), json!("http"));
            entry.insert("enabled".into(), json!(true));
            entry.insert("url".into(), json!(server_url));
            if let Some(b) = &bearer {
                entry.insert("headers".into(), json!({"Authorization": b}));
            }
        }
        McpClient::Swival => {
            // Swival .swival/mcp.json entry: `type: "http"` + `url` +
            // optional headers (documented format).
            entry.insert("type".into(), json!("http"));
            entry.insert("url".into(), json!(server_url));
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

/// ZCode MCP entry: `type: "http"` + `url` + optional `headers` under
/// `~/.zcode/cli/config.json`'s `mcp.servers` map. ZCode's entry schema
/// is strict — an entry carrying any key outside `type`/`url`/`headers`/
/// `enabled`/`timeoutMs` is dropped silently — so this deliberately
/// emits nothing else (issue #511).
fn build_mcp_entry_zcode(args: &InstallMcpArgs) -> Result<serde_json::Value> {
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
    let cli_line = if args.session_aware {
        let env = args
            .auth_token
            .as_deref()
            .map(|token| format!(" --env \"AI_MEMORY_AUTH_TOKEN={token}\""))
            .unwrap_or_default();
        format!(
            "claude mcp add --transport stdio{env} {name} -- \\\n    ai-memory mcp-bridge --server-url {url}",
            name = args.name,
            url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
        )
    } else if let Some(b) = &bearer {
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
         #   - Windows (unpackaged): %APPDATA%\\Claude\\claude_desktop_config.json\n\
         #   - Windows (MSIX):       %LOCALAPPDATA%\\Packages\\Claude_<id>\\LocalCache\\Roaming\\Claude\\claude_desktop_config.json\n\
         #     --apply detects the installed form; if multiple MSIX packages\n\
         #     are present, select the active one with --config-file.\n\
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

fn render_zcode(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# ZCode (z.ai) — merge into ~/.zcode/cli/config.json\n\
         # (or re-run this command with --apply), then restart ZCode.\n\
         #\n\
         # The entry schema is strict: keys outside type/url/headers/\n\
         # enabled/timeoutMs make ZCode drop the server silently.\n\
         # ai-memory's default stateless /mcp endpoint needs no flavor\n\
         # marker; auth goes in the headers map.\n\
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
        "# Antigravity CLI (`agy`) — merge into ~/.gemini/config/mcp_config.json:\n\
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

fn render_kiro_cli(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Kiro CLI - merge into $KIRO_HOME/settings/mcp.json\n\
         # (defaults to ~/.kiro/settings/mcp.json):\n\
         #\n\
         # Kiro accepts HTTPS remote endpoints and plain HTTP only on\n\
         # localhost. `?flavor=bedrock` removes unsupported root-level\n\
         # schema combinators while handler validation remains unchanged.\n\
         # Lifecycle capture is installed separately: use\n\
         # `install-hooks --agent kiro-cli` for v2 or the explicit\n\
         # `install-hooks --agent kiro-cli-v3` target for v3.\n\
         # Managed workstreams are not installed by this command.\n\
         {snippet}\n",
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_command_code(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Command Code — merge into ~/.commandcode/mcp.json:\n\
         #\n\
         # The equivalent CLI registration is:\n\
         #   cmd mcp add --transport http --scope user {name} {url}\n\
         # (`cmdc` is the native Windows executable name.)\n\
         {snippet}\n",
        name = args.name,
        url = args.server_url.as_deref().unwrap_or(DEFAULT_MCP_URL),
        snippet = render_json_mcp_fragment(args)?,
    ))
}

fn render_swival(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Swival CLI — merge into .swival/mcp.json in the project root
         # (Swival's documented default lookup; project-scoped by design), or
         # re-run with --apply to merge it in place preserving other servers.
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

fn render_zed(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Zed - merge into settings.json:\n\
         #   - macOS:   ~/.config/zed/settings.json\n\
         #   - Linux:   $XDG_CONFIG_HOME/zed/settings.json\n\
         #              (defaults to ~/.config/zed/settings.json)\n\
         #   - Windows: %APPDATA%\\Zed\\settings.json\n\
         #\n\
         # Zed reads remote MCP servers from the top-level\n\
         # `context_servers` map. This is MCP-only: Zed does not expose\n\
         # ai-memory-compatible lifecycle hooks, so automatic capture and\n\
         # managed-workstream continuity are not active.\n\
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
            session_aware: false,
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
            session_aware: false,
        }
    }

    #[test]
    fn claude_desktop_render_lists_packaged_and_unpacked_windows_paths() {
        let rendered = render_claude_desktop(&args_for(McpClient::ClaudeDesktop)).unwrap();
        assert!(rendered.contains(r"%APPDATA%\Claude\claude_desktop_config.json"));
        assert!(rendered.contains(
            r"%LOCALAPPDATA%\Packages\Claude_<id>\LocalCache\Roaming\Claude\claude_desktop_config.json"
        ));
        assert!(rendered.contains("--apply detects the installed form"));
        assert!(rendered.contains("--config-file"));
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
    fn zed_config_path_uses_platform_conventions() {
        for (target_os, root, expected) in [
            (
                "linux",
                "/home/alice/.config",
                "/home/alice/.config/zed/settings.json",
            ),
            (
                "macos",
                "/Users/alice/.config",
                "/Users/alice/.config/zed/settings.json",
            ),
            (
                "windows",
                "C:/Users/alice/AppData/Roaming",
                "C:/Users/alice/AppData/Roaming/Zed/settings.json",
            ),
        ] {
            assert_eq!(
                zed_config_path_in(Path::new(root), target_os),
                PathBuf::from(expected),
                "target OS: {target_os}"
            );
        }
    }

    #[test]
    fn zed_renderer_uses_context_servers_and_native_remote_http() {
        let fragment = render_json_mcp_fragment(&args_with_token(McpClient::Zed)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();

        assert_eq!(
            value,
            json!({
                "context_servers": {
                    "ai-memory": {
                        "url": "http://127.0.0.1:49374/mcp",
                        "headers": {
                            "Authorization": "Bearer test-token-deadbeef"
                        }
                    }
                }
            })
        );
        assert!(
            render_zed(&args_for(McpClient::Zed))
                .unwrap()
                .contains("MCP-only")
        );
    }

    #[test]
    fn command_code_renderer_uses_documented_user_scope_http_schema() {
        let fragment = render_json_mcp_fragment(&args_with_token(McpClient::CommandCode)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();

        assert_eq!(
            value,
            json!({
                "mcpServers": {
                    "ai-memory": {
                        "transport": "http",
                        "enabled": true,
                        "url": "http://127.0.0.1:49374/mcp",
                        "headers": {
                            "Authorization": "Bearer test-token-deadbeef"
                        }
                    }
                }
            })
        );
        let rendered = render_command_code(&args_for(McpClient::CommandCode)).unwrap();
        assert!(rendered.contains("~/.commandcode/mcp.json"));
        assert!(rendered.contains("cmd mcp add --transport http --scope user"));
        assert!(rendered.contains("`cmdc` is the native Windows executable"));
    }

    #[test]
    fn zed_apply_preserves_settings_and_siblings_and_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("settings.json");
        fs::write(
            &config_path,
            r#"{
  // Keep this user comment.
  "theme": "One Dark",
  "context_servers": {
    // Keep this sibling comment.
    "other": { "url": "https://other.example/mcp" },
  },
}
"#,
        )
        .unwrap();
        let mut args = args_with_token(McpClient::Zed);
        args.config_file = Some(config_path.clone());

        apply_to_config_file(&args).unwrap();
        let first = fs::read_to_string(&config_path).unwrap();
        apply_to_config_file(&args).unwrap();
        let second = fs::read_to_string(&config_path).unwrap();

        assert_eq!(first, second);
        assert!(second.contains("// Keep this user comment."));
        assert!(second.contains("// Keep this sibling comment."));
        let root = CstRootNode::parse(&second, &ParseOptions::default()).unwrap();
        let value = root.to_serde_value().unwrap();
        assert_eq!(value["theme"], "One Dark");
        assert_eq!(
            value["context_servers"]["other"]["url"],
            "https://other.example/mcp"
        );
        assert_eq!(
            value["context_servers"]["ai-memory"]["headers"]["Authorization"],
            "Bearer test-token-deadbeef"
        );
    }

    #[test]
    fn zed_apply_rejects_non_object_context_servers_without_writing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("settings.json");
        let original = "{\n  // User-owned invalid shape.\n  \"context_servers\": false,\n}\n";
        fs::write(&config_path, original).unwrap();
        let mut args = args_for(McpClient::Zed);
        args.config_file = Some(config_path.clone());

        let error = apply_to_config_file(&args).unwrap_err();

        assert!(error.to_string().contains("not an object"));
        assert_eq!(fs::read_to_string(config_path).unwrap(), original);
    }

    #[test]
    fn packaged_claude_desktop_path_prefers_localcache_when_package_dir_exists() {
        let packages = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(packages.path().join("Claude_pzs8sxrjxfjjc")).unwrap();

        let found = packaged_claude_desktop_config_path(packages.path())
            .unwrap()
            .unwrap();
        assert_eq!(
            found,
            packages
                .path()
                .join("Claude_pzs8sxrjxfjjc")
                .join("LocalCache")
                .join("Roaming")
                .join("Claude")
                .join("claude_desktop_config.json")
        );
    }

    #[test]
    fn packaged_claude_desktop_path_ignores_unrelated_and_non_dir_entries() {
        let packages = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(packages.path().join("Microsoft.WindowsTerminal_abc123")).unwrap();
        fs::write(packages.path().join("Claude_notadir"), b"").unwrap();

        assert!(
            packaged_claude_desktop_config_path(packages.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn packaged_claude_desktop_path_none_when_packages_dir_absent_or_empty() {
        let missing = tempfile::TempDir::new().unwrap();
        assert!(
            packaged_claude_desktop_config_path(&missing.path().join("does-not-exist"))
                .unwrap()
                .is_none()
        );

        let empty = tempfile::TempDir::new().unwrap();
        assert!(
            packaged_claude_desktop_config_path(empty.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn claude_desktop_path_uses_resolved_app_data_roots() {
        let root = tempfile::TempDir::new().unwrap();
        let local = root.path().join("redirected-local");
        let roaming = root.path().join("redirected-roaming");

        assert_eq!(
            claude_desktop_config_path_in(&local, &roaming).unwrap(),
            roaming.join("Claude").join("claude_desktop_config.json")
        );
    }

    #[test]
    fn packaged_claude_desktop_path_prefers_existing_config_among_packages() {
        let packages = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(packages.path().join("Claude_old")).unwrap();
        let active_config = packages
            .path()
            .join("Claude_current")
            .join("LocalCache")
            .join("Roaming")
            .join("Claude")
            .join("claude_desktop_config.json");
        fs::create_dir_all(active_config.parent().unwrap()).unwrap();
        fs::write(&active_config, b"{}").unwrap();

        assert_eq!(
            packaged_claude_desktop_config_path(packages.path())
                .unwrap()
                .unwrap(),
            active_config
        );
    }

    #[test]
    fn packaged_claude_desktop_path_rejects_ambiguous_packages() {
        let packages = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(packages.path().join("Claude_first")).unwrap();
        fs::create_dir_all(packages.path().join("Claude_second")).unwrap();

        let error = packaged_claude_desktop_config_path(packages.path()).unwrap_err();
        assert!(error.to_string().contains("--config-file"));
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

    #[test]
    fn claude_code_session_aware_entry_uses_owned_stdio_bridge() {
        let mut args = args_with_token(McpClient::ClaudeCode);
        args.server_url = Some("https://memory.example/mcp".into());
        args.session_aware = true;

        let entry = build_mcp_entry(&args).unwrap();

        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "ai-memory");
        assert_eq!(
            entry["args"],
            json!(["mcp-bridge", "--server-url", "https://memory.example/mcp"])
        );
        assert_eq!(entry["env"]["AI_MEMORY_AUTH_TOKEN"], "test-token-deadbeef");
        assert!(entry.get("url").is_none());
        assert!(entry.get("headers").is_none());

        let rendered = render_claude_code(&args, Path::new("/home/alice/.claude.json")).unwrap();
        assert!(rendered.contains("claude mcp add --transport stdio"));
        assert!(rendered.contains("ai-memory mcp-bridge --server-url"));
    }

    #[test]
    fn claude_code_session_aware_apply_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_file = tmp.path().join(".claude.json");
        let mut args = args_with_token(McpClient::ClaudeCode);
        args.server_url = Some("http://192.168.0.90:49374/mcp".into());
        args.config_file = Some(config_file.clone());
        args.session_aware = true;
        args.apply = true;

        apply_to_config_file(&args).unwrap();
        let first = fs::read_to_string(&config_file).unwrap();
        apply_to_config_file(&args).unwrap();
        let second = fs::read_to_string(&config_file).unwrap();

        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(
            value["mcpServers"]["ai-memory"]["args"],
            json!([
                "mcp-bridge",
                "--server-url",
                "http://192.168.0.90:49374/mcp"
            ])
        );
    }

    #[test]
    fn session_aware_rejects_non_claude_clients() {
        let mut args = args_for(McpClient::Codex);
        args.session_aware = true;

        let error = build_json_mcp_entry(&args).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("supported only for --client claude-code"),
            "{error:#}"
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
            McpClient::Zcode => render_zcode(&args).unwrap(),
            McpClient::Devin => render_devin(&args).unwrap(),
            McpClient::KimiCode => render_kimi_code(&args).unwrap(),
            McpClient::KiroCli => render_kiro_cli(&args).unwrap(),
            McpClient::CommandCode => render_command_code(&args).unwrap(),
            McpClient::Swival => render_swival(&args).unwrap(),
            McpClient::VsCodeCopilot => render_vscode_copilot(&args).unwrap(),
            McpClient::Zed => render_zed(&args).unwrap(),
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
            McpClient::Zcode,
            McpClient::Devin,
            McpClient::KimiCode,
            McpClient::KiroCli,
            McpClient::CommandCode,
            McpClient::Swival,
            McpClient::VsCodeCopilot,
            McpClient::Zed,
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
            McpClient::Zcode,
            McpClient::Devin,
            McpClient::KimiCode,
            McpClient::KiroCli,
            McpClient::CommandCode,
            McpClient::Swival,
            McpClient::VsCodeCopilot,
            McpClient::Zed,
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
            McpClient::Zcode => render_zcode(&args).unwrap(),
            McpClient::Devin => render_devin(&args).unwrap(),
            McpClient::KimiCode => render_kimi_code(&args).unwrap(),
            McpClient::KiroCli => render_kiro_cli(&args).unwrap(),
            McpClient::CommandCode => render_command_code(&args).unwrap(),
            McpClient::Swival => render_swival(&args).unwrap(),
            McpClient::VsCodeCopilot => render_vscode_copilot(&args).unwrap(),
            McpClient::Zed => render_zed(&args).unwrap(),
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
        // The snippet must point at the documented global config, not the
        // internal ~/.gemini/antigravity-cli/ data dir (#510).
        assert!(
            render_for_test(McpClient::AntigravityCli).contains("~/.gemini/config/mcp_config.json")
        );
        assert!(!render_for_test(McpClient::AntigravityCli).contains("~/.gemini/antigravity-cli/"));
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
        let kiro = render_for_test(McpClient::KiroCli);
        assert!(kiro.contains("\"mcpServers\""));
        assert!(kiro.contains("http://127.0.0.1:49374/mcp?flavor=bedrock"));
        assert!(!kiro.contains("\"transport\""));
        assert!(kiro.contains("install-hooks --agent kiro-cli"));
        assert!(kiro.contains("install-hooks --agent kiro-cli-v3"));
        let kiro_with_token = render_with_token(McpClient::KiroCli);
        assert!(kiro_with_token.contains("\"Authorization\": \"Bearer test-token-deadbeef\""));
        // VS Code Copilot must use the `servers` top-level key — the
        // `mcpServers` form is silently ignored by VS Code's MCP
        // framework. Regression guard against a future copy-paste
        // from the Cursor / Claude Code renderer.
        let vsc = render_for_test(McpClient::VsCodeCopilot);
        assert!(vsc.contains("\"servers\""));
        assert!(!vsc.contains("\"mcpServers\""));
        assert!(vsc.contains("\"type\": \"http\""));
        let zed = render_for_test(McpClient::Zed);
        assert!(zed.contains("\"context_servers\""));
        assert!(!zed.contains("\"mcpServers\""));
        assert!(!zed.contains("\"type\": \"http\""));
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

    #[test]
    fn kiro_home_honours_env_override() {
        assert_eq!(
            kiro_home(Some("/tmp/custom-kiro-home".into())).unwrap(),
            PathBuf::from("/tmp/custom-kiro-home")
        );
        let default = home_dir().unwrap().join(".kiro");
        assert_eq!(kiro_home(Some("".into())).unwrap(), default);
        assert_eq!(kiro_home(None).unwrap(), default);
    }

    /// Pin the Antigravity CLI config destination to the documented global
    /// config at ~/.gemini/config/mcp_config.json. ~/.gemini/antigravity-cli/
    /// is Antigravity's internal data dir and only holds a copy Antigravity
    /// itself writes, so a registration there works by coincidence on an
    /// established install and is silently ignored on a fresh one (#510).
    #[test]
    fn antigravity_cli_mcp_config_path_pins_documented_location() {
        assert_eq!(
            mcp_config_path(McpClient::AntigravityCli).unwrap(),
            home_dir()
                .unwrap()
                .join(".gemini")
                .join("config")
                .join("mcp_config.json")
        );
    }

    /// Pin ZCode's user-scope config destination: ~/.zcode/cli/config.json.
    /// Workspace scopes (.zcode/config.json, zcode.json, .agents/mcp.json)
    /// exist, but user scope is the install default every other client
    /// uses too (#511).
    #[test]
    fn zcode_mcp_config_path_pins_user_scope_location() {
        assert_eq!(
            mcp_config_path(McpClient::Zcode).unwrap(),
            home_dir()
                .unwrap()
                .join(".zcode")
                .join("cli")
                .join("config.json")
        );
    }

    /// ZCode's entry schema is strict — any key outside
    /// type/url/headers/enabled/timeoutMs makes it drop the server
    /// silently — so the fragment must carry exactly the documented
    /// keys, nested under `mcp.servers` (#511).
    #[test]
    fn zcode_renderer_nests_strict_http_entry_under_mcp_servers() {
        let fragment = render_json_mcp_fragment(&args_with_token(McpClient::Zcode)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();

        assert_eq!(
            value,
            json!({
                "mcp": {
                    "servers": {
                        "ai-memory": {
                            "type": "http",
                            "url": "http://127.0.0.1:49374/mcp",
                            "headers": {
                                "Authorization": "Bearer test-token-deadbeef"
                            }
                        }
                    }
                }
            })
        );
        let rendered = render_zcode(&args_for(McpClient::Zcode)).unwrap();
        assert!(rendered.contains("~/.zcode/cli/config.json"));
        assert!(rendered.contains("strict"));
    }

    /// `--apply` merges under `mcp.servers` keeping the rest of the
    /// config and sibling servers intact, and re-runs are a no-op.
    #[test]
    fn zcode_apply_preserves_siblings_and_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
  "theme": "dark",
  "mcp": {
    "servers": {
      "other": {"type": "http", "url": "https://other.example/mcp"}
    }
  }
}"#,
        )
        .unwrap();
        let mut args = args_with_token(McpClient::Zcode);
        args.config_file = Some(config_path.clone());

        apply_to_config_file(&args).unwrap();
        let first = fs::read_to_string(&config_path).unwrap();
        apply_to_config_file(&args).unwrap();
        let second = fs::read_to_string(&config_path).unwrap();

        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(value["theme"], "dark");
        assert_eq!(
            value["mcp"]["servers"]["other"]["url"], "https://other.example/mcp",
            "install must preserve sibling servers"
        );
        assert_eq!(value["mcp"]["servers"]["ai-memory"]["type"], "http");
        assert_eq!(
            value["mcp"]["servers"]["ai-memory"]["headers"]["Authorization"],
            "Bearer test-token-deadbeef"
        );
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
    fn bedrock_flavored_mcp_url_appends_marker_idempotently() {
        assert_eq!(
            bedrock_flavored_mcp_url("https://memory.example/mcp"),
            "https://memory.example/mcp?flavor=bedrock"
        );
        assert_eq!(
            bedrock_flavored_mcp_url("https://memory.example/mcp?token=x"),
            "https://memory.example/mcp?token=x&flavor=bedrock"
        );
        assert_eq!(
            bedrock_flavored_mcp_url("https://memory.example/mcp?flavor=bedrock"),
            "https://memory.example/mcp?flavor=bedrock"
        );
    }

    #[test]
    fn kiro_rejects_plain_http_for_non_loopback_servers() {
        for allowed in [
            "http://localhost:49374/mcp",
            "http://127.0.0.1:49374/mcp",
            "http://[::1]:49374/mcp",
            "https://memory.example/mcp",
        ] {
            let mut args = args_for(McpClient::KiroCli);
            args.server_url = Some(allowed.into());
            validate_args(&args).unwrap_or_else(|error| panic!("{allowed}: {error:#}"));
        }

        let mut args = args_for(McpClient::KiroCli);
        args.server_url = Some("http://192.168.0.90:49374/mcp".into());
        let error = validate_args(&args).unwrap_err();
        assert!(error.to_string().contains("requires HTTPS"), "{error:#}");
    }

    #[test]
    fn kiro_apply_preserves_siblings_and_is_idempotent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("mcp.json");
        fs::write(
            &config_path,
            r#"{"mcpServers":{"other":{"url":"https://other.example/mcp"}},"userSetting":true}"#,
        )
        .unwrap();
        let mut args = args_with_token(McpClient::KiroCli);
        args.server_url = Some("https://memory.example/mcp".into());
        args.config_file = Some(config_path.clone());

        apply_to_config_file(&args).unwrap();
        let first = fs::read_to_string(&config_path).unwrap();
        apply_to_config_file(&args).unwrap();
        let second = fs::read_to_string(&config_path).unwrap();

        assert_eq!(first, second);
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(value["userSetting"], true);
        assert_eq!(
            value["mcpServers"]["other"]["url"],
            "https://other.example/mcp"
        );
        assert_eq!(
            value["mcpServers"]["ai-memory"]["url"],
            "https://memory.example/mcp?flavor=bedrock"
        );
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
    fn swival_entry_and_apply_match_upstream_json_schema() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join(".swival").join("mcp.json");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(
            &config_path,
            r#"{"mcpServers":{"other":{"command":"other-mcp"}}}"#,
        )
        .unwrap();

        let mut args = args_with_token(McpClient::Swival);
        args.config_file = Some(config_path.clone());
        apply_to_config_file(&args).unwrap();
        let first = fs::read_to_string(&config_path).unwrap();
        apply_to_config_file(&args).unwrap();
        let second = fs::read_to_string(&config_path).unwrap();

        assert_eq!(first, second, "Swival MCP apply must be idempotent");
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(value["mcpServers"]["ai-memory"]["type"], "http");
        assert_eq!(
            value["mcpServers"]["ai-memory"]["url"],
            "http://127.0.0.1:49374/mcp"
        );
        assert_eq!(
            value["mcpServers"]["ai-memory"]["headers"]["Authorization"],
            "Bearer test-token-deadbeef"
        );
        assert_eq!(
            value["mcpServers"]["other"]["command"], "other-mcp",
            "apply must preserve sibling servers"
        );
    }

    #[test]
    fn swival_project_root_matches_git_toml_and_fallback_rules() {
        let tmp = tempfile::TempDir::new().unwrap();
        let git_root = tmp.path().join("git-project");
        let git_nested = git_root.join("a").join("b");
        fs::create_dir_all(git_root.join(".git")).unwrap();
        fs::create_dir_all(&git_nested).unwrap();
        assert_eq!(
            swival_project_root(&git_nested),
            fs::canonicalize(&git_root).unwrap()
        );

        let toml_root = tmp.path().join("toml-project");
        let toml_nested = toml_root.join("src");
        fs::create_dir_all(&toml_nested).unwrap();
        fs::write(toml_root.join("swival.toml"), "").unwrap();
        assert_eq!(
            swival_project_root(&toml_nested),
            fs::canonicalize(&toml_root).unwrap()
        );

        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            swival_project_root(&plain),
            fs::canonicalize(&plain).unwrap()
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
