//! `ai-memory init` — create the data directory layout.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::InitArgs;
use crate::commands::path_util::display_path;
use crate::config::Config;
use ai_memory_mcp::auth::generate_token_hex;

const DEFAULT_CONFIG_TOML: &str = include_str!("../../templates/config.default.toml");

const SUBDIRS: &[&str] = &["wiki", "raw", "db", "models"];

/// Bytes of OS CSPRNG entropy used for the auto-generated
/// `[auth].token_pepper`. 32 bytes → 64 hex chars.
const TOKEN_PEPPER_BYTES: usize = 32;

/// Build the `[auth]` block appended to the freshly-rendered default
/// config on `ai-memory init`. The pepper is auto-generated so it's
/// stable from install onwards (rotating it invalidates every existing
/// native API key — see `ai-memory-store::api_credentials`).
fn render_default_auth_block(pepper: &str) -> String {
    format!(
        "
# Per-server token pepper. Keeps stolen `api_credentials.token_hash` rows useless
# to an offline attacker by mixing each token with this secret before
# hashing. Auto-generated on `ai-memory init`. **Do NOT change after
# the first native API key is added** — rotating invalidates every key.
# Human password login does not use this pepper.
token_pepper = \"{pepper}\"

# Multi-user attribution (all optional).
#
# Without these, the bearer token authenticates the request but
# leaves it anonymous. Set them to label root-token writes in the
# audit log + page frontmatter.
#
# Add human users with `ai-memory user add-human --username <name>`, then issue
# programmatic access separately with `ai-memory api-key add`.
#
# root_username = \"boss\"
# root_email    = \"boss@example.com\"
# root_name     = \"Boss\"
#
# Trusted authenticating proxy (optional). Give the proxy a bearer token that
# differs from bearer_token. It must replace client-supplied X-Memory-Actor-*
# headers and assert either User or the Issuer+Sub pair on every request.
# actor_proxy_bearer_token = \"<distinct-random-token>\"
# root_issuer  = \"https://idp.example\"
# root_subject = \"<stable-root-subject>\"
"
    )
}

/// The `bearer_token_file` line appended to a rendered config.
fn render_bearer_token_file_line(raw: &str) -> String {
    format!(
        "
# Root bearer token, one line in an owner-only file. A relative path resolves
# against the data dir. Written by `ai-memory init` on a fresh data dir; the
# server requires it on every route but /healthz, and hooks, the CLI and
# `ai-memory mcp-bridge` read it from there. Harness configs never hold it.
bearer_token_file = {}
",
        toml_edit::Value::from(raw)
    )
}

/// Which `bearer_token_file` a config being written should record, minting the
/// token when this is a fresh install.
///
/// - A config being rewritten with `--force` that already named a token file
///   keeps naming it, so the rewrite does not silently turn auth off.
/// - A fresh install (no config and no database yet) with no bearer supplied
///   another way gets a new token at `auth-tokens/default`.
/// - Anything else, an existing install above all, records nothing and keeps
///   the loopback no-auth behaviour it had (`ai-memory auth enable` opts in).
fn root_bearer_for_new_config(config: &Config, fresh: bool) -> Result<Option<RecordedBearer>> {
    let i18n = &config.i18n;
    if let Some(raw) = config
        .auth
        .bearer_token_file
        .as_ref()
        .filter(|raw| !raw.as_os_str().is_empty())
    {
        let token = config
            .auth
            .token_file_path()
            .map(crate::local_auth::provision_root_token)
            .transpose()?;
        return Ok(Some(RecordedBearer {
            raw: raw.to_string_lossy().into_owned(),
            token,
        }));
    }
    if !fresh || config.auth.bearer_token.is_some() {
        return Ok(None);
    }
    let root = &config.data_dir;
    let minted =
        crate::local_auth::provision_root_token(&crate::server_profiles::root_token_path(root))?;
    crate::local_auth::seed_hook_credential(root, &minted.token)?;
    println!(
        "{}",
        i18n.format(
            "cli-init-generated-a-root-bearer-token-in-owner-only-the-server-requires-it-on-every",
            &[("value0", display_path(&minted.path).to_string())]
        )
    );
    Ok(Some(RecordedBearer {
        raw: crate::server_profiles::ROOT_TOKEN_RELATIVE.to_owned(),
        token: Some(minted),
    }))
}

/// The `bearer_token_file` a written config records, and the token read from
/// (or minted into) that file.
struct RecordedBearer {
    raw: String,
    token: Option<crate::local_auth::RootToken>,
}

/// What one `init` did, for a caller (`setup`) that keeps going in the same
/// process after it.
pub(crate) struct InitOutcome {
    /// The config file `init` wrote or found.
    pub(crate) config_path: PathBuf,
    /// False when a config file already existed and was left untouched.
    pub(crate) config_written: bool,
    /// The root bearer the written config names in `bearer_token_file`, so the
    /// caller can adopt it the way `Config::load` would have.
    pub(crate) root_token: Option<crate::local_auth::RootToken>,
}

/// Run the `init` subcommand.
///
/// Creates `<data_dir>/{wiki,raw,db,models}` (idempotent) and writes a default
/// config file unless one already exists (use `--force` to overwrite). With no
/// explicit `--config`, the config remains at `<data_dir>/config.toml`; packaged
/// systemd units can pass `--config /etc/ai-memory/config.toml` or
/// `--config ~/.config/ai-memory/config.toml` without changing the data root.
///
/// On a fresh data dir it also mints the root bearer token (see
/// [`root_bearer_for_new_config`]).
///
/// # Errors
/// Returns an error if directories cannot be created or the config file
/// cannot be written.
pub fn run(config: &Config, args: InitArgs, config_path: Option<&Path>) -> Result<()> {
    initialize(config, &args, config_path).map(|_| ())
}

/// [`run`], reporting what it did.
///
/// # Errors
/// As [`run`].
pub(crate) fn initialize(
    config: &Config,
    args: &InitArgs,
    config_path: Option<&Path>,
) -> Result<InitOutcome> {
    let i18n = &config.i18n;
    let root = &config.data_dir;
    let cfg_path = init_config_path(root, config_path);
    // Decided before anything is created: a data dir that already has a
    // database is an existing install even when it never had a config file
    // (env-configured and container installs), and it must keep running the
    // way it did.
    let fresh = !cfg_path.exists() && !root.join("db").join(ai_memory_store::DB_FILENAME).exists();
    create_private_dir_all(root)
        .with_context(|| format!("creating data root {}", display_path(root)))?;

    for sub in SUBDIRS {
        let path = root.join(sub);
        create_private_dir_all(&path)
            .with_context(|| format!("creating {}", display_path(&path)))?;
        tracing::info!(path = %display_path(&path), "ensured directory");
    }

    let mut outcome = InitOutcome {
        config_path: cfg_path.clone(),
        config_written: false,
        root_token: None,
    };
    if cfg_path.exists() && !args.force {
        tracing::info!(
            path = %display_path(&cfg_path),
            "config already exists; leaving untouched (pass --force to overwrite)",
        );
    } else {
        if let Some(parent) = cfg_path.parent() {
            create_private_dir_all(parent)
                .with_context(|| format!("creating config dir {}", display_path(parent)))?;
        }
        // Pepper is generated NOW (not at first server start) so it's
        // stable from install onwards: see comment in render_default_
        // auth_block. getrandom::Error doesn't impl `std::error::Error`
        // (anyhow's `with_context` would conflict), so map manually.
        let pepper = generate_token_hex(TOKEN_PEPPER_BYTES).map_err(|e| {
            anyhow::anyhow!(
                "{}",
                i18n.format(
                    "cli-init-generating-auth-token-pepper-e",
                    &[("value0", format!("{}", e))]
                )
            )
        })?;
        let recorded = root_bearer_for_new_config(config, fresh)?;
        let bearer_line = recorded
            .as_ref()
            .map(|bearer| render_bearer_token_file_line(&bearer.raw))
            .unwrap_or_default();
        outcome.root_token = recorded.and_then(|bearer| bearer.token);
        let body = format!(
            "{}{}{}",
            DEFAULT_CONFIG_TOML,
            render_default_auth_block(&pepper),
            bearer_line
        );
        let mut f = private_config_file(&cfg_path, args.force)
            .with_context(|| format!("creating {}", display_path(&cfg_path)))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("writing {}", display_path(&cfg_path)))?;
        tracing::info!(path = %display_path(&cfg_path), "wrote default config");
        outcome.config_written = true;
    }

    tracing::info!("init complete");
    Ok(outcome)
}

/// Create missing directory components with owner-only access without changing
/// the permissions of an existing installation.
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

/// Open a config file so a newly created file is private before its secret
/// token pepper is written. Forced rewrites preserve existing permissions.
fn private_config_file(path: &Path, force: bool) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).truncate(true).create(true);
    if !force {
        options.create_new(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn init_config_path(data_dir: &Path, explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn cfg_in(dir: &std::path::Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        }
    }

    #[test]
    fn init_creates_subdirs_and_config() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_in(tmp.path());
        run(&config, InitArgs { force: false }, None).unwrap();
        for sub in SUBDIRS {
            assert!(tmp.path().join(sub).is_dir(), "missing {sub}");
        }
        assert!(tmp.path().join("config.toml").exists());
    }

    #[test]
    fn init_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_in(tmp.path());
        run(&config, InitArgs { force: false }, None).unwrap();
        // Touch the config to detect a clobber.
        let stamp = std::fs::metadata(tmp.path().join("config.toml"))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        run(&config, InitArgs { force: false }, None).unwrap();
        let stamp2 = std::fs::metadata(tmp.path().join("config.toml"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(stamp, stamp2, "second init clobbered the config");
    }

    #[test]
    fn init_writes_token_pepper_into_auth_block() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_in(tmp.path());
        run(&config, InitArgs { force: false }, None).unwrap();

        let body = std::fs::read_to_string(tmp.path().join("config.toml")).unwrap();
        assert!(
            body.contains("[auth]"),
            "rendered config must include the [auth] section"
        );
        assert!(
            body.contains("token_pepper = "),
            "rendered config must include the auto-generated token_pepper"
        );

        // Sanity: pepper is 64 hex chars (32 bytes).
        let pepper_line = body
            .lines()
            .find(|l| l.trim_start().starts_with("token_pepper"))
            .expect("token_pepper line present");
        let hex_value = pepper_line
            .split('"')
            .nth(1)
            .expect("quoted token_pepper value");
        assert_eq!(hex_value.len(), 64, "token_pepper must be 64 hex chars");
        assert!(hex_value.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn init_renders_config_that_parses_back_into_auth_settings() {
        // Regression guard: the appended [auth] block must be valid TOML
        // *and* round-trip into the same AuthSettings the server uses.
        // Catches a future drift between the template and the schema.
        // Uses the same figment pipeline `Config::load` runs in production
        // (Serialized defaults + Toml file merge) so the test exercises
        // the actual loader, not a parallel parser.
        use figment::Figment;
        use figment::providers::{Format as _, Serialized, Toml};

        let tmp = TempDir::new().unwrap();
        let cfg = cfg_in(tmp.path());
        run(&cfg, InitArgs { force: false }, None).unwrap();

        let cfg_path = tmp.path().join("config.toml");
        let loaded: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(&cfg_path))
            .extract()
            .expect("rendered config must round-trip via figment (full Config extract)");

        assert!(
            !loaded.auth.secure_cookie,
            "generated config must preserve direct loopback HTTP browser compatibility"
        );
        assert!(
            loaded
                .auth
                .token_pepper
                .as_deref()
                .is_some_and(|p| p.len() == 64),
            "parsed token_pepper must round-trip from the rendered template"
        );
        // `[auth]` must stay the LAST section of config.default.toml: the
        // block appended here emits bare keys, so any section added below it
        // silently swallows them into the wrong table. Assert the structural
        // rule directly rather than leaving it to whichever `[auth]` key
        // happens to be checked above.
        let rendered = std::fs::read_to_string(&cfg_path).unwrap();
        let last_section = rendered
            .lines()
            .rfind(|line| line.trim_start().starts_with('['))
            .expect("rendered config declares sections");
        assert_eq!(
            last_section.trim(),
            "[auth]",
            "[auth] must remain the last section; init appends bare keys after it"
        );
        assert!(loaded.auth.bearer_token.is_none());
        assert!(!loaded.slots.per_user);
        assert_eq!(
            loaded.routing.mid_session,
            ai_memory_core::MidSessionRouting::FollowCwd,
            "generated config must preserve historical per-event attribution"
        );
        assert_eq!(
            loaded.consolidation.max_input_tokens,
            ai_memory_consolidate::DEFAULT_CONSOLIDATION_MAX_INPUT_TOKENS
        );
        assert_eq!(
            loaded.consolidation.max_output_tokens,
            ai_memory_consolidate::DEFAULT_CONSOLIDATION_MAX_OUTPUT_TOKENS
        );
        assert!(loaded.auth.root_username.is_none());
    }

    #[test]
    fn init_generates_a_unique_pepper_per_run() {
        // Two fresh installs must not collide on the pepper. Anyone
        // hand-patching this to a constant for testing would break the
        // security model on every new server.
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        run(&cfg_in(tmp_a.path()), InitArgs { force: false }, None).unwrap();
        run(&cfg_in(tmp_b.path()), InitArgs { force: false }, None).unwrap();
        let a = std::fs::read_to_string(tmp_a.path().join("config.toml")).unwrap();
        let b = std::fs::read_to_string(tmp_b.path().join("config.toml")).unwrap();
        let pa = a
            .lines()
            .find(|l| l.trim_start().starts_with("token_pepper"))
            .unwrap();
        let pb = b
            .lines()
            .find(|l| l.trim_start().starts_with("token_pepper"))
            .unwrap();
        assert_ne!(
            pa, pb,
            "two fresh inits must generate distinct peppers (got {pa})"
        );
    }

    #[test]
    fn init_honors_explicit_config_path_outside_data_dir() {
        let tmp = TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let config_dir = tmp.path().join("config").join("ai-memory");
        let config_path = config_dir.join("config.toml");
        let config = cfg_in(&data_dir);

        run(
            &config,
            InitArgs { force: false },
            Some(config_path.as_path()),
        )
        .unwrap();

        assert!(data_dir.join("wiki").is_dir());
        assert!(data_dir.join("raw").is_dir());
        assert!(config_path.is_file());
        assert!(
            !data_dir.join("config.toml").exists(),
            "explicit --config must not also write data-dir config"
        );
    }

    /// Load the rendered config through the production loader, with no
    /// process environment, the way the server and every client will.
    fn load(data_dir: &Path, config_path: Option<&Path>) -> Config {
        Config::load_with_runtime_env(
            config_path,
            Some(data_dir.to_path_buf()),
            crate::config::RuntimeEnv::default(),
        )
        .unwrap()
    }

    /// New-install default: a fresh data dir gets a root bearer, recorded as a relative
    /// `bearer_token_file` the loader resolves, and the hook credential files
    /// carry the same token. The token is in no rendered config.
    #[test]
    fn a_fresh_init_mints_a_root_bearer_the_loader_reads() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("data");
        run(&cfg_in(&root), InitArgs { force: false }, None).unwrap();

        let token_path = crate::server_profiles::root_token_path(&root);
        let token = crate::config::read_trimmed_secret(&token_path).expect("token minted");
        let body = std::fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(
            body.contains("bearer_token_file = \"auth-tokens/default\""),
            "{body}"
        );
        assert!(
            !body.contains(&token),
            "the token never goes into config.toml"
        );

        let loaded = load(&root, None);
        assert_eq!(loaded.auth.bearer_token.as_deref(), Some(token.as_str()));
        assert_eq!(
            loaded.auth.renderable_bearer_token(),
            None,
            "a file-sourced token is never handed to a harness renderer"
        );
        assert_eq!(
            crate::config::read_hook_auth_token(&root).as_deref(),
            Some(token.as_str()),
            "hooks read the same token from the files they already know"
        );
    }

    /// Re-running init leaves the token alone; `--force` rewrites the config
    /// but keeps naming (and reusing) the same token file, so a rewrite never
    /// silently turns auth off or rotates a credential clients hold.
    #[test]
    fn re_running_init_keeps_the_same_root_bearer() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("data");
        run(&cfg_in(&root), InitArgs { force: false }, None).unwrap();
        let token_path = crate::server_profiles::root_token_path(&root);
        let first = crate::config::read_trimmed_secret(&token_path).unwrap();

        run(&load(&root, None), InitArgs { force: false }, None).unwrap();
        run(&load(&root, None), InitArgs { force: true }, None).unwrap();

        assert_eq!(
            crate::config::read_trimmed_secret(&token_path),
            Some(first.clone())
        );
        assert_eq!(load(&root, None).auth.bearer_token, Some(first));
    }

    /// Control: a data dir that already holds a database is an existing
    /// install, even with no config file. It gets no token and keeps the
    /// loopback no-auth behaviour.
    #[test]
    fn init_on_an_existing_install_does_not_turn_auth_on() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("data");
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::write(root.join("db").join(ai_memory_store::DB_FILENAME), b"").unwrap();

        run(&cfg_in(&root), InitArgs { force: false }, None).unwrap();

        let body = std::fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(!body.contains("bearer_token_file"), "{body}");
        assert!(!crate::server_profiles::root_token_path(&root).exists());
        assert!(crate::config::read_hook_auth_token(&root).is_none());
        assert!(load(&root, None).auth.bearer_token.is_none());
    }

    /// An operator who already supplies a bearer (`AI_MEMORY_AUTH_TOKEN` or
    /// an inline key) chose their credential; a second one would leave the
    /// hooks holding a token the server does not enforce.
    #[test]
    fn init_with_a_supplied_bearer_mints_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("data");
        let mut config = cfg_in(&root);
        config.auth.bearer_token = Some("operator-token".into());

        run(&config, InitArgs { force: false }, None).unwrap();

        assert!(!crate::server_profiles::root_token_path(&root).exists());
        let body = std::fs::read_to_string(root.join("config.toml")).unwrap();
        assert!(!body.contains("bearer_token_file"), "{body}");
    }

    /// With `--config` outside the data dir the token still lives in the data
    /// dir, and the relative path resolves there.
    #[test]
    fn a_fresh_init_with_an_external_config_resolves_the_token_in_the_data_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("data");
        let config_path = tmp.path().join("etc").join("config.toml");
        run(
            &cfg_in(&root),
            InitArgs { force: false },
            Some(&config_path),
        )
        .unwrap();

        let token =
            crate::config::read_trimmed_secret(&crate::server_profiles::root_token_path(&root))
                .unwrap();
        assert_eq!(
            load(&root, Some(&config_path)).auth.bearer_token,
            Some(token)
        );
    }

    #[cfg(unix)]
    #[test]
    fn init_creates_private_directories_and_config() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("data");
        run(&cfg_in(&root), InitArgs { force: false }, None).unwrap();

        for path in std::iter::once(root.clone()).chain(SUBDIRS.iter().map(|sub| root.join(sub))) {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                path.display()
            );
        }
        assert_eq!(
            fs::metadata(root.join("config.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
