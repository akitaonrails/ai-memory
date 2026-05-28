//! Runtime configuration loader.
//!
//! All settings are read exactly once at startup, merged into a single
//! immutable [`Config`] value, and passed by reference everywhere. There is
//! no second read path (lesson from agentmemory #456 / #469 — the dimension
//! guard read `process.env` while the rest of the codebase used
//! `getMergedEnv()`, masking the bug for weeks).

use std::path::{Path, PathBuf};

use ai_memory_llm::{
    AuthRequirement, EmbedderChoice, EmbedderConfig, LlmError, LlmResult, ProviderAuth,
    ProviderChoice, ProviderConfig,
};
use anyhow::{Context, Result};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// Default HTTP bind address for the local single-user server.
pub const DEFAULT_BIND: &str = "127.0.0.1:49374";

/// Default base URL used by thin-client CLI subcommands.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

/// Default MCP endpoint URL rendered for client integrations.
pub const DEFAULT_MCP_URL: &str = "http://127.0.0.1:49374/mcp";

/// Default workspace name used by the single-workspace v1 flow.
pub const DEFAULT_WORKSPACE: &str = ai_memory_core::DEFAULT_WORKSPACE_NAME;

/// Defensive project fallback used only when no cwd/project is available.
pub const DEFAULT_PROJECT: &str = ai_memory_core::DEFAULT_PROJECT_NAME;

/// Top-level runtime configuration.
///
/// `deny_unknown_fields` is intentionally NOT set: figment's
/// `Env::prefixed("AI_MEMORY_")` pulls every env var with that prefix
/// (including future keys not represented here yet). Strict rejection
/// here would crash on harmless deploy-specific env vars before the
/// rest of the config has a chance to validate what it actually uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Root data directory holding `wiki/`, `raw/`, `db/`, `models/`, `logs/`.
    pub data_dir: PathBuf,
    /// HTTP bind address used by `ai-memory serve`.
    pub bind: String,
    /// Base URL used by thin-client CLI commands to contact the running server.
    pub server_url: String,
    /// Per-subsystem log filter (overridable by `RUST_LOG`).
    pub log_level: String,
    /// Optional LLM provider (`anthropic`, `openai`, `gemini`, `openai-compat`, `openai-oauth`, `copilot`).
    pub llm_provider: Option<String>,
    /// Optional LLM model override.
    pub llm_model: Option<String>,
    /// Optional LLM base URL override.
    pub llm_base_url: Option<String>,
    /// Opt-in: run LLM consolidation on SessionEnd (in addition to the
    /// always-written heuristic session page), when an LLM provider is
    /// configured. Off by default — SessionEnd stays cheap and
    /// fire-and-forget; the LLM checkpoint otherwise happens on PreCompact
    /// and via manual `memory_consolidate`. Set with
    /// `AI_MEMORY_CONSOLIDATE_ON_SESSION_END=true`.
    pub consolidate_on_session_end: bool,
    /// Optional embedding provider (`openai`, `voyage`, `google` / `gemini`).
    pub embedding_provider: Option<String>,
    /// Optional embedding model override.
    pub embedding_model: Option<String>,
    /// Optional embedding dimension override.
    pub embedding_dim: Option<u32>,
    /// Optional embedding base URL override.
    pub embedding_base_url: Option<String>,
    /// M8 retention-sweep parameters. The defaults give an ~80-day
    /// "survival floor" for unused episodic content (above the cold
    /// threshold), followed by ~180 days of soft-delete buffer before
    /// hard-deletion. Tune `decay.lambda` down to slow decay or
    /// `decay.cold_threshold` to evict more / less aggressively.
    pub decay: ai_memory_store::DecayParams,
    /// Server-side scheduled maintenance. Jobs run outside hook latency.
    pub maintenance: MaintenanceSettings,
    /// Privacy-strip tuning. Built-in patterns always run; this section
    /// lets the operator extend or punch holes in them.
    pub sanitize: ai_memory_core::SanitizeConfig,
    /// Bearer token required on every HTTP request. When `None`/unset,
    /// the server runs open (zero-config local-dev behaviour). When set,
    /// requests to /mcp + /hook + /handoff must carry
    /// `Authorization: Bearer <token>`. Settable via the
    /// `AI_MEMORY_AUTH_TOKEN` env var or `[auth].bearer_token` in
    /// config.toml.
    pub auth: AuthSettings,
    /// `Host`-header allowlist for the HTTP server. Requests whose
    /// `Host` header doesn't match this list are rejected before they
    /// reach MCP, hook, admin, or web routes (DNS-rebinding defence).
    /// Default is loopback only; to expose ai-memory on a LAN
    /// IP / `home.lan` / etc., add that authority here or pass it via
    /// `AI_MEMORY_ALLOWED_HOSTS=host1,host2,…` at startup.
    ///
    /// Accepts either a TOML/JSON sequence (`["a","b"]`) or a
    /// comma-separated string (`"a,b"`) for ergonomics — env vars
    /// can't be sequences without ugly escaping.
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub allowed_hosts: Vec<String>,
    /// Process-only env values that should never be written to config files.
    #[serde(skip)]
    pub runtime_env: RuntimeEnv,
}

/// Environment-only values captured once by [`Config::load`].
#[derive(Debug, Clone, Default)]
pub struct RuntimeEnv {
    data_dir: Option<PathBuf>,
    server_url: Option<String>,
    auth_token: Option<String>,
    host_cwd: Option<String>,
    anthropic_api_key: Option<SecretString>,
    anthropic_oauth_token: Option<SecretString>,
    openai_api_key: Option<SecretString>,
    gemini_api_key: Option<SecretString>,
    llm_api_key: Option<SecretString>,
    llm_base_url: Option<String>,
    copilot_github_token: Option<SecretString>,
    github_copilot_api_token: Option<SecretString>,
    copilot_api_url: Option<String>,
    copilot_client_id: Option<String>,
    voyage_api_key: Option<SecretString>,
}

impl RuntimeEnv {
    fn from_process() -> Self {
        Self {
            data_dir: env_path("AI_MEMORY_DATA_DIR"),
            server_url: env_string("AI_MEMORY_SERVER_URL"),
            auth_token: env_string("AI_MEMORY_AUTH_TOKEN"),
            host_cwd: env_string("AI_MEMORY_HOST_CWD"),
            anthropic_api_key: env_secret("ANTHROPIC_API_KEY"),
            // CLAUDE_CODE_OAUTH_TOKEN is what `claude setup-token` writes;
            // ANTHROPIC_OAUTH_TOKEN is our canonical name — accept both.
            anthropic_oauth_token: env_secret("ANTHROPIC_OAUTH_TOKEN")
                .or_else(|| env_secret("CLAUDE_CODE_OAUTH_TOKEN")),
            openai_api_key: env_secret("OPENAI_API_KEY"),
            // GOOGLE_API_KEY is the older alias many Google docs still
            // mention; accept either so users don't get tripped up.
            gemini_api_key: env_secret("GEMINI_API_KEY").or_else(|| env_secret("GOOGLE_API_KEY")),
            llm_api_key: env_secret("LLM_API_KEY"),
            llm_base_url: env_string("LLM_BASE_URL"),
            copilot_github_token: env_secret("COPILOT_GITHUB_TOKEN")
                .or_else(|| env_secret("GH_TOKEN"))
                .or_else(|| env_secret("GITHUB_TOKEN")),
            github_copilot_api_token: env_secret("GITHUB_COPILOT_API_TOKEN"),
            copilot_api_url: env_string("COPILOT_API_URL"),
            copilot_client_id: env_string("AI_MEMORY_COPILOT_CLIENT_ID"),
            voyage_api_key: env_secret("VOYAGE_API_KEY"),
        }
    }

    /// Host cwd forwarded by the docker wrapper, if present.
    #[must_use]
    pub fn host_cwd(&self) -> Option<&str> {
        self.host_cwd.as_deref()
    }

    #[cfg(test)]
    pub fn with_host_cwd_for_tests(host_cwd: impl Into<String>) -> Self {
        Self {
            host_cwd: Some(host_cwd.into()),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub fn with_openai_api_key_for_tests(api_key: impl Into<String>) -> Self {
        Self {
            openai_api_key: Some(SecretString::from(api_key.into())),
            ..Self::default()
        }
    }
}

/// Accept `Vec<String>` either as a real sequence (config.toml /
/// JSON array) or as a comma-separated single string (env var).
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Single(String),
        Many(Vec<String>),
    }
    Ok(match Either::deserialize(deserializer)? {
        Either::Single(s) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        Either::Many(v) => v,
    })
}

/// `[auth]` section of `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// Shared bearer token. When set, all HTTP routes require
    /// `Authorization: Bearer <token>`. Generate one with
    /// `ai-memory generate-auth-token`.
    pub bearer_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bind: DEFAULT_BIND.into(),
            server_url: DEFAULT_SERVER_URL.into(),
            log_level: "info".into(),
            llm_provider: None,
            llm_model: None,
            llm_base_url: None,
            consolidate_on_session_end: false,
            embedding_provider: None,
            embedding_model: None,
            embedding_dim: None,
            embedding_base_url: None,
            decay: ai_memory_store::DecayParams::default(),
            maintenance: MaintenanceSettings::default(),
            sanitize: ai_memory_core::SanitizeConfig::default(),
            auth: AuthSettings::default(),
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
            runtime_env: RuntimeEnv::default(),
        }
    }
}

/// `[maintenance]` scheduled server jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaintenanceSettings {
    /// Master switch for scheduled jobs.
    pub enabled: bool,
    /// Interval for the retention forget sweep. `0` disables this job.
    pub forget_sweep_interval_secs: u64,
    /// Interval for rule-based wiki lint. `0` disables this job.
    pub lint_interval_secs: u64,
    /// Interval for embedding backfill. `0` disables this job.
    /// Defaults to off because it may call a paid provider.
    pub embedding_backfill_interval_secs: u64,
}

impl Default for MaintenanceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            forget_sweep_interval_secs: 86_400,
            lint_interval_secs: 86_400,
            embedding_backfill_interval_secs: 0,
        }
    }
}

impl Config {
    /// Load the merged configuration: defaults → file → env → CLI.
    ///
    /// # Errors
    /// Returns an error if the config file is malformed or any required
    /// field is missing.
    pub fn load(config_path: Option<&Path>, cli_data_dir: Option<PathBuf>) -> Result<Self> {
        let runtime_env = RuntimeEnv::from_process();

        // Figure out where the config file *would* live so we can read it
        // before knowing the final data dir. CLI > env > default.
        let probe_data_dir = cli_data_dir
            .clone()
            .or_else(|| runtime_env.data_dir.clone())
            .unwrap_or_else(default_data_dir);
        let resolved_config_path = config_path
            .map(PathBuf::from)
            .unwrap_or_else(|| probe_data_dir.join("config.toml"));

        let mut figment = Figment::from(Serialized::defaults(Self::default()));
        if resolved_config_path.exists() {
            figment = figment.merge(Toml::file(&resolved_config_path));
        }
        figment = figment.merge(Env::prefixed("AI_MEMORY_").split("__"));

        let mut config: Config = figment.extract().with_context(|| {
            format!(
                "loading configuration (config file = {})",
                resolved_config_path.display()
            )
        })?;

        if let Some(token) = runtime_env.auth_token.clone() {
            config.auth.bearer_token = Some(token);
        }
        if let Some(server_url) = runtime_env.server_url.clone() {
            config.server_url = server_url;
        }

        // CLI override always wins (figment doesn't see it because clap has
        // already parsed the flag into `cli_data_dir`).
        if let Some(dir) = cli_data_dir {
            config.data_dir = dir;
        } else if let Some(dir) = runtime_env.data_dir.clone() {
            config.data_dir = dir;
        }

        config.data_dir = canonicalise_or_keep(&config.data_dir);
        config.runtime_env = runtime_env;

        Ok(config)
    }

    /// Whether the server URL came from config/env instead of the default.
    #[must_use]
    pub fn server_url_configured(&self) -> bool {
        self.server_url != DEFAULT_SERVER_URL || self.runtime_env.server_url.is_some()
    }

    /// Build the configured LLM provider settings, if LLM support is enabled.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] for unknown providers or missing
    /// provider-specific required values.
    pub fn llm_provider_config(&self) -> LlmResult<Option<ProviderConfig>> {
        let Some(provider_raw) = non_empty(self.llm_provider.as_deref()) else {
            return Ok(None);
        };
        let provider = match provider_raw {
            "anthropic" => ProviderChoice::Anthropic,
            "openai" => ProviderChoice::OpenAi,
            "gemini" | "google" => ProviderChoice::Gemini,
            "openai-compat" | "openai_compat" => ProviderChoice::OpenAiCompat,
            "openai-oauth" | "openai_oauth" => ProviderChoice::OpenAiOAuth,
            "copilot" | "github-copilot" | "github_copilot" => ProviderChoice::Copilot,
            "anthropic-oauth" | "anthropic_oauth" => ProviderChoice::AnthropicOAuth,
            other => {
                return Err(LlmError::NotConfigured(format!(
                    "AI_MEMORY_LLM_PROVIDER={other} is not one of \
                     anthropic|openai|gemini|openai-compat|openai-oauth|copilot|anthropic-oauth"
                )));
            }
        };
        let model = match non_empty(self.llm_model.as_deref()) {
            Some(s) => s.to_string(),
            None => match provider {
                ProviderChoice::Anthropic => "claude-sonnet-4-6".to_string(),
                ProviderChoice::AnthropicOAuth => "claude-sonnet-4-6".to_string(),
                ProviderChoice::OpenAi => "gpt-4o-mini".to_string(),
                ProviderChoice::Gemini => "gemini-2.5-flash".to_string(),
                ProviderChoice::OpenAiOAuth => "gpt-5.5".to_string(),
                ProviderChoice::Copilot => "gpt-5.5".to_string(),
                ProviderChoice::OpenAiCompat => {
                    return Err(LlmError::NotConfigured(
                        "AI_MEMORY_LLM_MODEL must be set explicitly for openai-compat \
                         (no safe default for self-hosted / aggregator endpoints)"
                            .into(),
                    ));
                }
            },
        };
        Ok(Some(ProviderConfig {
            provider,
            model,
            auth: self.provider_auth(provider, None),
            // base_url falls back to the runtime env (LLM_BASE_URL), mirroring
            // how auth is sourced — otherwise openai-compat is only
            // configurable via config.toml even though the key comes from env.
            base_url: self
                .llm_base_url
                .clone()
                .or_else(|| self.runtime_env.llm_base_url.clone()),
        }))
    }

    /// OpenAI-compatible embedding key. Direct OpenAI keeps requiring
    /// `OPENAI_API_KEY`; a custom embedding base URL may reuse `LLM_API_KEY`
    /// for gateways such as OpenRouter.
    fn openai_embedding_api_key(&self) -> LlmResult<SecretString> {
        if let Some(key) = self.runtime_env.openai_api_key.clone() {
            return Ok(key);
        }
        if non_empty(self.embedding_base_url.as_deref()).is_some() {
            if let Some(key) = self.runtime_env.llm_api_key.clone() {
                return Ok(key);
            }
            return Err(LlmError::NotConfigured(
                "OPENAI_API_KEY or LLM_API_KEY required for openai-compatible embeddings".into(),
            ));
        }
        Err(LlmError::NotConfigured("OPENAI_API_KEY".into()))
    }

    /// Build the configured embedder settings, if hybrid search is enabled.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] for unknown providers, missing API
    /// keys, or invalid dimensions.
    pub fn embedder_config(&self) -> LlmResult<Option<EmbedderConfig>> {
        let Some(provider_raw) = non_empty(self.embedding_provider.as_deref()) else {
            return Ok(None);
        };
        let provider = match provider_raw {
            "openai" => EmbedderChoice::OpenAi,
            "voyage" => EmbedderChoice::Voyage,
            "google" | "gemini" => EmbedderChoice::Google,
            other => {
                return Err(LlmError::NotConfigured(format!(
                    "AI_MEMORY_EMBEDDING_PROVIDER={other} not one of openai|voyage|google|gemini"
                )));
            }
        };
        let model = match non_empty(self.embedding_model.as_deref()) {
            Some(s) => s.to_string(),
            None => match provider {
                EmbedderChoice::OpenAi => "text-embedding-3-small".to_string(),
                EmbedderChoice::Voyage => "voyage-3".to_string(),
                EmbedderChoice::Google => ai_memory_llm::GOOGLE_DEFAULT_EMBED_MODEL.to_string(),
            },
        };
        let dim = self
            .embedding_dim
            .unwrap_or_else(|| ai_memory_llm::default_embedding_dim(provider, &model));
        let api_key = match provider {
            EmbedderChoice::OpenAi => self.openai_embedding_api_key()?,
            EmbedderChoice::Voyage => self
                .runtime_env
                .voyage_api_key
                .clone()
                .ok_or_else(|| LlmError::NotConfigured("VOYAGE_API_KEY".into()))?,
            EmbedderChoice::Google => self.runtime_env.gemini_api_key.clone().ok_or_else(|| {
                LlmError::NotConfigured("GEMINI_API_KEY or GOOGLE_API_KEY".into())
            })?,
        };
        Ok(Some(EmbedderConfig {
            provider,
            model,
            dim,
            api_key,
            base_url: self.embedding_base_url.clone(),
        }))
    }

    /// Resolve an API key for an explicit `llm-test` provider choice.
    #[must_use]
    pub fn provider_api_key(&self, provider: ProviderChoice) -> Option<SecretString> {
        match provider {
            ProviderChoice::Anthropic => self.runtime_env.anthropic_api_key.clone(),
            ProviderChoice::OpenAi => self.runtime_env.openai_api_key.clone(),
            ProviderChoice::Gemini => self.runtime_env.gemini_api_key.clone(),
            ProviderChoice::OpenAiCompat => self.runtime_env.llm_api_key.clone(),
            ProviderChoice::OpenAiOAuth => None,
            ProviderChoice::Copilot => None,
            ProviderChoice::AnthropicOAuth => None,
        }
    }

    /// Shared provider auth token file path.
    #[must_use]
    pub fn auth_token_path(&self) -> PathBuf {
        self.data_dir.join("auth.json")
    }

    /// Shared OpenAI OAuth token file path.
    #[must_use]
    pub fn openai_oauth_token_path(&self) -> PathBuf {
        self.auth_token_path()
    }

    /// Shared Copilot auth token file path.
    #[must_use]
    pub fn copilot_token_path(&self) -> PathBuf {
        self.auth_token_path()
    }

    /// GitHub token resolved for Copilot auth login/provider use.
    #[must_use]
    pub fn copilot_github_token(&self) -> Option<SecretString> {
        self.runtime_env.copilot_github_token.clone()
    }

    /// Copilot OAuth client id override for `auth login copilot`.
    #[must_use]
    pub fn copilot_client_id(&self) -> Option<&str> {
        self.runtime_env.copilot_client_id.as_deref()
    }

    /// Resolve typed auth material for a provider.
    ///
    /// `api_key_override` is used by `llm-test --api-key`; normal server
    /// startup passes `None` so env/config resolution remains the single path.
    #[must_use]
    pub fn provider_auth(
        &self,
        provider: ProviderChoice,
        api_key_override: Option<SecretString>,
    ) -> ProviderAuth {
        match provider.auth_requirement() {
            AuthRequirement::RequiredApiKey { env_var } => {
                ProviderAuth::required_api_key_from_env(env_var, self.provider_api_key(provider))
                    .with_cli_api_key_override(api_key_override)
            }
            AuthRequirement::OptionalApiKey { env_var } => {
                ProviderAuth::optional_api_key_from_env(env_var, self.provider_api_key(provider))
                    .with_cli_api_key_override(api_key_override)
            }
            AuthRequirement::OpenAiOAuthToken => {
                ProviderAuth::openai_oauth_token_file(self.openai_oauth_token_path())
            }
            AuthRequirement::CopilotToken => ProviderAuth::copilot(
                self.copilot_token_path(),
                self.runtime_env.copilot_github_token.clone(),
                self.runtime_env.github_copilot_api_token.clone(),
                self.runtime_env
                    .copilot_api_url
                    .clone()
                    .or_else(|| self.llm_base_url.clone()),
            ),
            AuthRequirement::AnthropicOAuthToken => {
                ProviderAuth::anthropic_oauth_token(self.runtime_env.anthropic_oauth_token.clone())
            }
        }
    }

    /// Base URL fallback for `llm-test --provider openai-compat`.
    #[must_use]
    pub fn llm_test_base_url(&self) -> Option<String> {
        self.llm_base_url
            .clone()
            .or_else(|| self.runtime_env.llm_base_url.clone())
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    env_string(name).map(PathBuf::from)
}

fn env_secret(name: &str) -> Option<SecretString> {
    env_string(name).map(SecretString::from)
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ai-memory")
}

fn canonicalise_or_keep(p: &Path) -> PathBuf {
    if let Ok(canon) = p.canonicalize() {
        return canon;
    }
    // Path may not exist yet (init hasn't run). Canonicalise the parent
    // and rejoin so logs and downstream comparisons still see the truth.
    if let (Some(parent), Some(name)) = (p.parent(), p.file_name())
        && let Ok(canon_parent) = parent.canonicalize()
    {
        return canon_parent.join(name);
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use tempfile::TempDir;

    #[test]
    fn defaults_have_canonical_endings() {
        let cfg = Config::default();
        assert!(cfg.data_dir.ends_with("ai-memory"));
        assert_eq!(cfg.bind, DEFAULT_BIND);
        assert_eq!(cfg.server_url, DEFAULT_SERVER_URL);
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.maintenance.enabled);
        assert_eq!(cfg.maintenance.forget_sweep_interval_secs, 86_400);
        assert_eq!(cfg.maintenance.lint_interval_secs, 86_400);
        assert_eq!(cfg.maintenance.embedding_backfill_interval_secs, 0);
    }

    #[test]
    fn cli_override_wins() {
        let tmp = TempDir::new().unwrap();
        let cli_dir = tmp.path().join("override");
        let cfg = Config::load(None, Some(cli_dir.clone())).unwrap();
        assert_eq!(
            cfg.data_dir,
            // We don't expect the directory to exist yet, so the
            // canonicalise-parent fallback will return parent + name.
            cli_dir
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .map(|c| c.join(cli_dir.file_name().unwrap()))
                .unwrap_or(cli_dir)
        );
    }

    #[test]
    fn config_file_overrides_defaults() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
            bind = "0.0.0.0:9999"
            log_level = "debug"

            [maintenance]
            enabled = false
            lint_interval_secs = 3600
            "#,
        )
        .unwrap();
        // Use the tmp dir as the data dir so the resolved config path
        // matches what `load` derives. Passing it explicitly keeps the test
        // free of any global env.
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:9999");
        assert_eq!(cfg.log_level, "debug");
        assert!(!cfg.maintenance.enabled);
        assert_eq!(cfg.maintenance.lint_interval_secs, 3600);
    }

    #[test]
    fn gemini_embedding_provider_uses_google_defaults() {
        let mut cfg = Config {
            embedding_provider: Some("gemini".into()),
            runtime_env: RuntimeEnv {
                gemini_api_key: Some(SecretString::from("test-key")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let embedder = cfg.embedder_config().unwrap().unwrap();
        assert_eq!(embedder.provider, EmbedderChoice::Google);
        assert_eq!(embedder.model, ai_memory_llm::GOOGLE_DEFAULT_EMBED_MODEL);
        assert_eq!(embedder.dim, 768);

        cfg.embedding_provider = Some("google".into());
        assert_eq!(
            cfg.embedder_config().unwrap().unwrap().provider,
            EmbedderChoice::Google
        );
    }

    #[test]
    fn openai_embedding_falls_back_to_llm_api_key_for_openrouter() {
        let cfg = Config {
            embedding_provider: Some("openai".into()),
            embedding_model: Some("text-embedding-3-small".into()),
            embedding_base_url: Some("https://openrouter.ai/api/v1".into()),
            runtime_env: RuntimeEnv {
                llm_api_key: Some(SecretString::from("sk-or-test-key")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let embedder = cfg.embedder_config().unwrap().unwrap();
        assert_eq!(embedder.provider, EmbedderChoice::OpenAi);
        assert_eq!(embedder.model, "text-embedding-3-small");
        assert_eq!(embedder.api_key.expose_secret(), "sk-or-test-key");
        assert_eq!(
            embedder.base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn openai_embedding_does_not_use_llm_api_key_without_custom_base_url() {
        let cfg = Config {
            embedding_provider: Some("openai".into()),
            runtime_env: RuntimeEnv {
                llm_api_key: Some(SecretString::from("sk-or-test-key")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let err = cfg.embedder_config().unwrap_err();
        assert!(matches!(err, LlmError::NotConfigured(msg) if msg == "OPENAI_API_KEY"));
    }

    #[test]
    fn llm_provider_config_uses_typed_provider_auth() {
        let cfg = Config {
            llm_provider: Some("openai".into()),
            runtime_env: RuntimeEnv {
                openai_api_key: Some(SecretString::from("sk-test-key")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let provider = cfg.llm_provider_config().unwrap().unwrap();
        assert_eq!(provider.provider, ProviderChoice::OpenAi);
        assert_eq!(provider.model, "gpt-4o-mini");
        assert_eq!(
            provider.auth.requirement(),
            AuthRequirement::RequiredApiKey {
                env_var: "OPENAI_API_KEY"
            }
        );
        assert_eq!(
            provider.auth.source(),
            ai_memory_llm::CredentialSource::Environment {
                name: "OPENAI_API_KEY"
            }
        );
        assert_eq!(
            provider.auth.require_api_key().unwrap().expose_secret(),
            "sk-test-key"
        );
    }

    #[test]
    fn llm_test_api_key_override_wins_over_env_auth() {
        let cfg = Config {
            runtime_env: RuntimeEnv {
                openai_api_key: Some(SecretString::from("env-key")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let auth = cfg.provider_auth(
            ProviderChoice::OpenAi,
            Some(SecretString::from("override-key")),
        );

        assert_eq!(auth.source(), ai_memory_llm::CredentialSource::CliOverride);
        assert_eq!(
            auth.require_api_key().unwrap().expose_secret(),
            "override-key"
        );
    }

    #[test]
    fn openai_compat_auth_remains_optional() {
        let cfg = Config::default();

        let auth = cfg.provider_auth(ProviderChoice::OpenAiCompat, None);

        assert_eq!(
            auth.requirement(),
            AuthRequirement::OptionalApiKey {
                env_var: "LLM_API_KEY"
            }
        );
        assert!(auth.optional_api_key().is_none());
    }

    #[test]
    fn openai_oauth_provider_uses_data_dir_token_file() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config {
            data_dir: tmp.path().to_path_buf(),
            llm_provider: Some("openai-oauth".into()),
            ..Config::default()
        };

        let provider = cfg.llm_provider_config().unwrap().unwrap();

        assert_eq!(provider.provider, ProviderChoice::OpenAiOAuth);
        assert_eq!(provider.model, "gpt-5.5");
        assert_eq!(
            provider.auth.requirement(),
            AuthRequirement::OpenAiOAuthToken
        );
        assert_eq!(
            provider.auth.require_openai_oauth_token_file().unwrap(),
            tmp.path().join("auth.json")
        );
    }

    #[test]
    fn copilot_provider_uses_data_dir_token_file_and_env_token() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config {
            data_dir: tmp.path().to_path_buf(),
            llm_provider: Some("copilot".into()),
            runtime_env: RuntimeEnv {
                copilot_github_token: Some(SecretString::from("ghu-test")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let provider = cfg.llm_provider_config().unwrap().unwrap();
        let auth = provider.auth.require_copilot_auth().unwrap();

        assert_eq!(provider.provider, ProviderChoice::Copilot);
        assert_eq!(provider.model, "gpt-5.5");
        assert_eq!(auth.token_file, tmp.path().join("auth.json"));
        assert_eq!(auth.github_token.unwrap().expose_secret(), "ghu-test");
    }

    #[test]
    fn anthropic_oauth_provider_resolves_choice_default_model_and_credential() {
        let cfg = Config {
            llm_provider: Some("anthropic-oauth".into()),
            runtime_env: RuntimeEnv {
                anthropic_oauth_token: Some(SecretString::from("tok-oauth-test")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let provider = cfg.llm_provider_config().unwrap().unwrap();
        assert_eq!(provider.provider, ProviderChoice::AnthropicOAuth);
        assert_eq!(provider.model, "claude-sonnet-4-6");
        assert_eq!(
            provider.auth.requirement(),
            AuthRequirement::AnthropicOAuthToken
        );
        assert_eq!(
            provider
                .auth
                .require_anthropic_oauth_token()
                .unwrap()
                .expose_secret(),
            "tok-oauth-test"
        );
    }

    #[test]
    fn anthropic_oauth_provider_underscore_alias_also_resolves() {
        let cfg = Config {
            llm_provider: Some("anthropic_oauth".into()),
            runtime_env: RuntimeEnv {
                anthropic_oauth_token: Some(SecretString::from("tok-alias")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };
        let provider = cfg.llm_provider_config().unwrap().unwrap();
        assert_eq!(provider.provider, ProviderChoice::AnthropicOAuth);
    }
}
