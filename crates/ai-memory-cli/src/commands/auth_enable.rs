//! `ai-memory auth enable` — require a bearer token on this install's server.
//!
//! The opt-in for installs that predate the new-install default: it does what
//! `init` does on a fresh data dir, then re-points the wiring that already
//! exists at the token. Idempotent, so a second run changes nothing:
//!
//! 1. reuse `[auth].bearer_token_file`'s token, or mint
//!    `<data_dir>/auth-tokens/default` (`0600`);
//! 2. record `bearer_token_file` in `config.toml`, keeping its comments;
//! 3. give the hooks the token through `auth-token` / `auth-header`, the files
//!    every hook front door reads, unless another credential is stored there;
//! 4. re-register every MCP client whose `ai-memory` entry points at this
//!    server, through `install-mcp`: Claude Code gets the session-aware
//!    `mcp-bridge` entry, Codex and Prime Agent a reference to
//!    `$AI_MEMORY_AUTH_TOKEN`, every other client a stdio entry for the
//!    session-less bridge, which reads the token file itself. A client that can
//!    only hold a literal header (ZCode) is listed for a manual change.
//!
//! The token is never printed and never written into a harness config.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::cli::{AuthEnableArgs, InstallMcpArgs, McpClient, SchemaFlavor};
use crate::commands::install_mcp::{self, mcp_server_url_from_base};
use crate::commands::path_util::display_path;
use crate::config::Config;
use crate::local_auth::{self, HookCredential};
use crate::server_profiles::{self, ProfileName};

/// The MCP entry name `install-mcp` writes by default.
const ENTRY_NAME: &str = "ai-memory";

/// What happened to one client's `ai-memory` MCP entry. Clients without one
/// are not listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wiring {
    /// Re-registered through `install-mcp` so it reaches the token file.
    Rewired,
    /// The entry targets another server; it is left alone.
    OtherServer(String),
    /// It targets this server but could not be re-registered; the reason
    /// says what to do by hand.
    Manual(String),
}

/// One client's line in the outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientWiring {
    pub(crate) client: McpClient,
    pub(crate) path: PathBuf,
    pub(crate) wiring: Wiring,
}

/// Where each client keeps its MCP config; injectable so tests never touch a
/// real harness config.
pub(crate) type ConfigLocator<'a> = &'a dyn Fn(McpClient) -> Option<PathBuf>;

/// The result of one `auth enable`.
#[derive(Debug)]
pub(crate) struct EnableOutcome {
    /// A bearer was already supplied inline (`AI_MEMORY_AUTH_TOKEN` or
    /// `[auth].bearer_token`); nothing was changed.
    pub(crate) already_inline: bool,
    pub(crate) token_path: PathBuf,
    pub(crate) token_created: bool,
    pub(crate) config_path: PathBuf,
    pub(crate) config_changed: bool,
    pub(crate) hooks: Option<HookCredential>,
    pub(crate) mcp: Vec<ClientWiring>,
}

/// Run `auth enable`.
///
/// # Errors
/// Fails when a server profile named `default` would share the token file,
/// when `servers.toml` or `config.toml` cannot be parsed, or on a failed write.
pub fn run(config: &Config, args: &AuthEnableArgs, config_path: Option<&Path>) -> Result<()> {
    let i18n = &config.i18n;
    let locate = |client: McpClient| install_mcp::mcp_config_path(client).ok();
    let locator: Option<ConfigLocator<'_>> = (!args.no_mcp).then_some(&locate as _);
    let outcome = enable(config, config_path, locator)?;
    print_outcome(i18n, &outcome);
    Ok(())
}

pub(crate) fn enable(
    config: &Config,
    config_path: Option<&Path>,
    locate: Option<ConfigLocator<'_>>,
) -> Result<EnableOutcome> {
    let data_dir = &config.data_dir;
    let config_path = config_path.map_or_else(|| data_dir.join("config.toml"), PathBuf::from);
    let token_path = config.auth.token_file_path().map_or_else(
        || server_profiles::root_token_path(data_dir),
        Path::to_path_buf,
    );

    if config.auth.renderable_bearer_token().is_some() {
        return Ok(EnableOutcome {
            already_inline: true,
            token_path,
            token_created: false,
            config_path,
            config_changed: false,
            hooks: None,
            mcp: Vec::new(),
        });
    }

    refuse_a_profile_sharing_the_token(data_dir, &token_path)?;
    let minted = local_auth::provision_root_token(&token_path)?;
    let config_changed = if config.auth.bearer_token_file.is_none() {
        record_token_file(&config_path)?
    } else {
        false
    };
    let hooks = local_auth::seed_hook_credential(data_dir, &minted.token)?;

    let mut enabled = config.clone();
    enabled
        .auth
        .adopt_token_file(minted.path.clone(), minted.token.clone());
    let mcp = locate.map_or_else(Vec::new, |locate| rewire_clients(&enabled, locate));

    Ok(EnableOutcome {
        already_inline: false,
        token_path: minted.path,
        token_created: minted.created,
        config_path,
        config_changed,
        hooks: Some(hooks),
        mcp,
    })
}

/// A registered server profile named `default` reads `auth-tokens/default`
/// too: the hook would then present this server's root bearer to that
/// profile's server.
fn refuse_a_profile_sharing_the_token(data_dir: &Path, token_path: &Path) -> Result<()> {
    let Some(name) = ProfileName::parse(server_profiles::ROOT_TOKEN_NAME) else {
        return Ok(());
    };
    if !server_profiles::profile_shares_root_token(data_dir, &name, Some(token_path)) {
        return Ok(());
    }
    if server_profiles::root_token_name_is_registered(data_dir)
        .context("reading servers.toml to check the `default` profile name")?
    {
        bail!(
            "a server profile named `default` is registered, and its token file is the one \
             `auth enable` would use for this server's root bearer. Re-register that profile \
             under another name (`ai-memory server add <name> ...`, then \
             `ai-memory server remove default`) and run `ai-memory auth enable` again."
        );
    }
    Ok(())
}

/// Set `[auth].bearer_token_file` in the config file, keeping every comment
/// and other key. Creates the file (owner-only) when the install never had
/// one. Returns whether the file changed.
fn record_token_file(config_path: &Path) -> Result<bool> {
    let existing = match std::fs::read_to_string(config_path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", display_path(config_path)));
        }
    };
    let mut doc: toml_edit::DocumentMut = existing
        .as_deref()
        .unwrap_or_default()
        .parse()
        .with_context(|| format!("parsing {}", display_path(config_path)))?;
    let auth = doc
        .entry("auth")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("`auth` in {} is not a table", display_path(config_path)))?;
    if auth.get("bearer_token_file").is_some() {
        return Ok(false);
    }
    auth.insert(
        "bearer_token_file",
        toml_edit::value(server_profiles::ROOT_TOKEN_RELATIVE),
    );

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(config_path)
            .ok()
            .map(|meta| meta.permissions().mode())
    };
    if let Some(parent) = config_path.parent() {
        crate::commands::path_util::create_private_dir(parent)
            .with_context(|| format!("creating {}", display_path(parent)))?;
    }
    // The atomic write's temp file is owner-only; an existing config keeps
    // the mode it had (a packaged one may be group-readable on purpose).
    ai_memory_wiki::write_atomic(config_path, doc.to_string().as_bytes())
        .with_context(|| format!("writing {}", display_path(config_path)))?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(config_path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("restoring the mode of {}", display_path(config_path)))?;
    }
    Ok(true)
}

/// Clients `install-mcp --apply` writes a config file for. Pi has no MCP
/// config (its extension reads the hook credential file), and DeepSeek
/// Harness and Hermes Agent are print-only, so none has an entry to
/// re-point. Hermes' entry is a bridge carrying no token, so a new token
/// needs no edit there either.
fn rewirable_clients() -> impl Iterator<Item = McpClient> {
    <McpClient as clap::ValueEnum>::value_variants()
        .iter()
        .copied()
        .filter(|client| !matches!(client, McpClient::Pi | McpClient::Dsh | McpClient::Hermes))
}

/// The endpoint without a `?flavor=` (or any) query, normalised to `/mcp`.
fn bare_endpoint(url: &str) -> String {
    mcp_server_url_from_base(url.split('?').next().unwrap_or(url))
}

/// The schema flavor an existing entry's URL pinned, so re-registering keeps
/// the dialect the operator chose with `--flavor`.
fn pinned_flavor(url: &str) -> Option<SchemaFlavor> {
    let query = url.split_once('?')?.1;
    <SchemaFlavor as clap::ValueEnum>::value_variants()
        .iter()
        .copied()
        .find(|flavor| {
            query
                .split('&')
                .any(|pair| pair == format!("flavor={}", flavor.marker()))
        })
}

/// Re-register every client whose `ai-memory` entry targets this server. One
/// client failing never stops the others: it is reported as manual.
fn rewire_clients(config: &Config, locate: ConfigLocator<'_>) -> Vec<ClientWiring> {
    let ours = bare_endpoint(&config.server_url);
    let mut outcome = Vec::new();
    for client in rewirable_clients() {
        let Some(path) = locate(client) else {
            continue;
        };
        let wiring = match install_mcp::existing_entry_endpoint(client, &path, ENTRY_NAME) {
            Ok(None) => continue,
            Err(e) => Wiring::Manual(format!("{e:#}")),
            Ok(Some(url)) if bare_endpoint(&url) != ours => Wiring::OtherServer(url),
            Ok(Some(url)) => match rewire(config, client, &path, &ours, pinned_flavor(&url)) {
                Ok(()) => Wiring::Rewired,
                Err(e) => Wiring::Manual(format!("{e:#}")),
            },
        };
        outcome.push(ClientWiring {
            client,
            path,
            wiring,
        });
    }
    outcome
}

fn rewire(
    config: &Config,
    client: McpClient,
    path: &Path,
    url: &str,
    flavor: Option<SchemaFlavor>,
) -> Result<()> {
    let args = InstallMcpArgs {
        client,
        server_url: Some(url.to_owned()),
        name: ENTRY_NAME.to_owned(),
        auth_token: None,
        apply: true,
        config_file: Some(path.to_path_buf()),
        session_aware: false,
        flavor,
        token_env_var: false,
        stdio_bridge: false,
        bridge_data_dir: None,
    };
    // Name the OpenCode major version outright: both share one file, and
    // probing `opencode --version` here would pick one for both locations.
    let dialect = match client {
        McpClient::OpenCode => Some(ai_memory_workstream::OpenCodeDialect::V1),
        McpClient::OpenCode2 => Some(ai_memory_workstream::OpenCodeDialect::V2),
        _ => None,
    };
    install_mcp::run_with_opencode_dialect(config, args, dialect)
}

fn print_outcome(i18n: &ai_memory_core::i18n::Localizer, outcome: &EnableOutcome) {
    if outcome.already_inline {
        println!(
            "{}", i18n.text("cli-auth-enable-auth-already-required-by-ai-memory-auth-token-or-auth-bearer-token-nothing-c"));
        return;
    }
    println!(
        "{}",
        i18n.format(
            "cli-auth-enable-auth-bearer-token-at-owner-only",
            &[
                (
                    "value0",
                    (if outcome.token_created {
                        "generated"
                    } else {
                        "kept"
                    })
                    .to_string()
                ),
                ("value1", display_path(&outcome.token_path).to_string())
            ]
        )
    );
    println!(
        "{}",
        i18n.format(
            "cli-setup-config",
            &[
                (
                    "value0",
                    (if outcome.config_changed {
                        "recorded [auth].bearer_token_file in"
                    } else {
                        "already names the token file:"
                    })
                    .to_string()
                ),
                ("value1", display_path(&outcome.config_path).to_string())
            ]
        )
    );
    match outcome.hooks {
        Some(HookCredential::Seeded) => {
            println!("{}", i18n.text("cli-auth-enable-hooks-given-the-token-through-the-hook-credential-files"));
        }
        Some(HookCredential::Current) => println!("{}", i18n.text("cli-auth-enable-hooks-already-use-the-token")),
        Some(HookCredential::KeptOther) => println!(
            "{}", i18n.text("cli-auth-enable-hooks-another-credential-is-stored-for-them-and-was-kept-re-run-ai-memory-in")),
        None => {}
    }
    for line in &outcome.mcp {
        let client = format!("{:?}", line.client);
        match &line.wiring {
            Wiring::Rewired => println!(
                "{}", i18n.format("cli-auth-enable-mcp-client-re-registered-in-so-it-reads-the-token-without-holding-it", &[("value0", client.to_string()), ("value1", display_path(&line.path).to_string())])),
            Wiring::OtherServer(url) => {
                println!("{}", i18n.format("cli-auth-enable-mcp-client-targets-url-another-server-left-unchanged", &[("value0", client.to_string()), ("value1", url.to_string())]));
            }
            Wiring::Manual(reason) => {
                println!("{}", i18n.format("cli-auth-enable-mcp-client-needs-a-manual-change-reason", &[("value0", client.to_string()), ("value1", reason.to_string())]));
            }
        }
    }
    println!(
        "{}",
        i18n.text("cli-auth-enable-restart-the-ai-memory-server-for-it-to-require-the-token")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_in(data_dir: &Path) -> Config {
        Config::load_with_runtime_env(
            None,
            Some(data_dir.to_path_buf()),
            crate::config::RuntimeEnv::default(),
        )
        .unwrap()
    }

    /// The pre-default config `init` wrote: a token pepper and comments, no
    /// bearer.
    fn pre_a1_install(data_dir: &Path) {
        std::fs::create_dir_all(data_dir).unwrap();
        std::fs::write(
            data_dir.join("config.toml"),
            "# operator comment\nlog_level = \"info\"\n\n[auth]\n# keep me\nsecure_cookie = false\ntoken_pepper = \"pp\"\n",
        )
        .unwrap();
    }

    /// Every file under `root`, read as text where it is text.
    fn files_under(root: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(text) = std::fs::read_to_string(&path) {
                    out.push((path, text));
                }
            }
        }
        out
    }

    /// Every client `rewire_clients` considers, each wired the pre-default way
    /// (`install-mcp --apply`, no token) into its own file under `home`.
    fn wire_every_client_over_http(home: &Path, url: &str) -> Vec<(McpClient, PathBuf)> {
        let open = Config::default();
        rewirable_clients()
            .map(|client| {
                let ext = if matches!(client, McpClient::Codex | McpClient::Grok) {
                    "toml"
                } else {
                    "json"
                };
                let path = home.join(format!("{client:?}.{ext}"));
                rewire(&open, client, &path, url, None)
                    .unwrap_or_else(|e| panic!("pre-default {client:?} install: {e:#}"));
                (client, path)
            })
            .collect()
    }

    fn locator(files: &[(McpClient, PathBuf)]) -> impl Fn(McpClient) -> Option<PathBuf> + '_ {
        move |client| {
            files
                .iter()
                .find(|(c, _)| *c == client)
                .map(|(_, path)| path.clone())
        }
    }

    /// `auth enable` on a pre-default install: token minted, the key recorded
    /// with the operator's comments intact, the hooks given the token, and
    /// every client whose entry points at this server re-registered (ZCode,
    /// which only takes a literal header, listed as manual). Run twice, the
    /// second run changes nothing, and the token appears in no harness config.
    #[test]
    fn enable_twice_is_idempotent_and_leaks_the_token_into_no_harness_config() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        pre_a1_install(&data_dir);
        let files = wire_every_client_over_http(&home, "http://127.0.0.1:49374/mcp");
        let locate = locator(&files);

        let first = enable(&config_in(&data_dir), None, Some(&locate)).unwrap();
        assert!(first.token_created);
        assert!(first.config_changed);
        assert_eq!(first.hooks, Some(HookCredential::Seeded));
        assert_eq!(first.mcp.len(), files.len(), "{:?}", first.mcp);
        for line in &first.mcp {
            match line.client {
                McpClient::Zcode => assert!(
                    matches!(&line.wiring, Wiring::Manual(reason) if reason.contains("ZCode")),
                    "{line:?}"
                ),
                _ => assert_eq!(line.wiring, Wiring::Rewired, "{line:?}"),
            }
        }

        let token = crate::config::read_trimmed_secret(&first.token_path).unwrap();
        let config_text = std::fs::read_to_string(data_dir.join("config.toml")).unwrap();
        assert!(config_text.contains("# operator comment"), "{config_text}");
        assert!(config_text.contains("# keep me"), "{config_text}");
        assert!(
            config_text.contains("token_pepper = \"pp\""),
            "{config_text}"
        );
        assert!(
            config_text.contains("bearer_token_file = \"auth-tokens/default\""),
            "{config_text}"
        );
        let claude = &files
            .iter()
            .find(|(c, _)| *c == McpClient::ClaudeCode)
            .unwrap()
            .1;
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(claude).unwrap()).unwrap();
        let entry = &entry["mcpServers"]["ai-memory"];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["args"][0], "mcp-bridge");
        assert!(entry.get("env").is_none(), "no token env: {entry}");
        let cursor = &files
            .iter()
            .find(|(c, _)| *c == McpClient::Cursor)
            .unwrap()
            .1;
        let entry: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(cursor).unwrap()).unwrap();
        assert_eq!(
            entry["mcpServers"]["ai-memory"]["args"][1],
            "--no-session-id"
        );
        assert_eq!(
            config_in(&data_dir).auth.bearer_token.as_deref(),
            Some(token.as_str())
        );
        let after_first: Vec<String> = files
            .iter()
            .map(|(_, path)| std::fs::read_to_string(path).unwrap())
            .collect();

        let second = enable(&config_in(&data_dir), None, Some(&locate)).unwrap();
        assert!(!second.token_created);
        assert!(!second.config_changed);
        assert_eq!(second.hooks, Some(HookCredential::Current));
        assert_eq!(second.mcp, first.mcp);
        assert_eq!(
            crate::config::read_trimmed_secret(&second.token_path),
            Some(token.clone())
        );
        assert_eq!(
            std::fs::read_to_string(data_dir.join("config.toml")).unwrap(),
            config_text
        );
        for ((client, path), before) in files.iter().zip(&after_first) {
            assert_eq!(
                &std::fs::read_to_string(path).unwrap(),
                before,
                "{client:?} changed on the second run"
            );
        }

        for (path, text) in files_under(&home) {
            assert!(
                !text.contains(&token),
                "token leaked into {}",
                path.display()
            );
        }
        // Compare canonical paths: the token path comes from the canonical
        // data dir, which differs from the walked temp path on macOS
        // (`/var` -> `/private/var`) and Windows (`\\?\` and 8.3 names).
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let allowed_paths = [
            canonical(&first.token_path),
            canonical(&crate::config::hook_auth_token_path_in(&data_dir)),
            canonical(&crate::config::hook_auth_header_path_in(&data_dir)),
        ];
        for (path, text) in files_under(&data_dir) {
            let allowed = allowed_paths.contains(&canonical(&path));
            assert!(
                allowed || !text.contains(&token),
                "token copied to an unexpected file {}",
                path.display()
            );
        }
    }

    /// Control: an entry for a remote server keeps its destination.
    #[test]
    fn enable_leaves_an_entry_for_another_server_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        pre_a1_install(&data_dir);
        let files = wire_every_client_over_http(tmp.path(), "https://team.example/mcp");
        let before: Vec<String> = files
            .iter()
            .map(|(_, path)| std::fs::read_to_string(path).unwrap())
            .collect();
        let locate = locator(&files);

        let outcome = enable(&config_in(&data_dir), None, Some(&locate)).unwrap();
        assert_eq!(outcome.mcp.len(), files.len());
        for line in &outcome.mcp {
            assert!(
                matches!(&line.wiring, Wiring::OtherServer(url) if url.starts_with("https://team.example/mcp")),
                "{line:?}"
            );
        }
        for ((client, path), before) in files.iter().zip(&before) {
            assert_eq!(
                &std::fs::read_to_string(path).unwrap(),
                before,
                "{client:?}"
            );
        }
    }

    /// A registered profile named `default` would read the same file as the
    /// root bearer and present it to its own server: refused before anything
    /// is written.
    #[test]
    fn enable_refuses_while_a_default_profile_would_share_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        pre_a1_install(&data_dir);
        let default = ProfileName::parse("default").unwrap();
        server_profiles::add(
            &data_dir,
            &default,
            "https://team.example",
            &[],
            Some("team"),
        )
        .unwrap();

        let err = enable(&config_in(&data_dir), None, None).unwrap_err();
        assert!(format!("{err:#}").contains("named `default`"), "{err:#}");
        assert_eq!(
            server_profiles::read_token(&data_dir, &default).as_deref(),
            Some("team"),
            "the profile's own token is untouched"
        );
        assert!(
            !std::fs::read_to_string(data_dir.join("config.toml"))
                .unwrap()
                .contains("bearer_token_file")
        );

        // Control: another profile name does not conflict.
        server_profiles::remove(&data_dir, &default).unwrap();
        let team = ProfileName::parse("team").unwrap();
        server_profiles::add(&data_dir, &team, "https://team.example", &[], Some("team")).unwrap();
        assert!(enable(&config_in(&data_dir), None, None).is_ok());
    }

    /// An inline bearer is already enforcement; `auth enable` adds nothing.
    #[test]
    fn enable_with_an_inline_bearer_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        pre_a1_install(&data_dir);
        let mut config = config_in(&data_dir);
        config.auth.bearer_token = Some("inline".into());

        let outcome = enable(&config, None, None).unwrap();
        assert!(outcome.already_inline);
        assert!(!server_profiles::root_token_path(&data_dir).exists());
        assert!(crate::config::read_hook_auth_token(&data_dir).is_none());
    }

    /// An install that never had a config file gets one, owner-only.
    #[test]
    fn enable_without_a_config_file_creates_one() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        enable(&config_in(&data_dir), None, None).unwrap();
        let loaded = config_in(&data_dir);
        assert!(loaded.auth.bearer_token.is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(data_dir.join("config.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
