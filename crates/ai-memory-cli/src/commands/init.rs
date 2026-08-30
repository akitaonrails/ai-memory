//! `ai-memory init` — create the data directory layout.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::InitArgs;
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

/// Run the `init` subcommand.
///
/// Creates `<data_dir>/{wiki,raw,db,models}` (idempotent) and writes a default
/// config file unless one already exists (use `--force` to overwrite). With no
/// explicit `--config`, the config remains at `<data_dir>/config.toml`; packaged
/// systemd units can pass `--config /etc/ai-memory/config.toml` or
/// `--config ~/.config/ai-memory/config.toml` without changing the data root.
///
/// # Errors
/// Returns an error if directories cannot be created or the config file
/// cannot be written.
pub fn run(config: &Config, args: InitArgs, config_path: Option<&Path>) -> Result<()> {
    let root = &config.data_dir;
    create_private_dir_all(root)
        .with_context(|| format!("creating data root {}", root.display()))?;

    for sub in SUBDIRS {
        let path = root.join(sub);
        create_private_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        tracing::info!(path = %path.display(), "ensured directory");
    }

    let cfg_path = init_config_path(root, config_path);
    if cfg_path.exists() && !args.force {
        tracing::info!(
            path = %cfg_path.display(),
            "config already exists; leaving untouched (pass --force to overwrite)",
        );
    } else {
        if let Some(parent) = cfg_path.parent() {
            create_private_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        // Pepper is generated NOW (not at first server start) so it's
        // stable from install onwards: see comment in render_default_
        // auth_block. getrandom::Error doesn't impl `std::error::Error`
        // (anyhow's `with_context` would conflict), so map manually.
        let pepper = generate_token_hex(TOKEN_PEPPER_BYTES)
            .map_err(|e| anyhow::anyhow!("generating auth token_pepper: {e}"))?;
        let body = format!(
            "{}{}",
            DEFAULT_CONFIG_TOML,
            render_default_auth_block(&pepper)
        );
        let mut f = private_config_file(&cfg_path, args.force)
            .with_context(|| format!("creating {}", cfg_path.display()))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("writing {}", cfg_path.display()))?;
        tracing::info!(path = %cfg_path.display(), "wrote default config");
    }

    tracing::info!("init complete");
    Ok(())
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
