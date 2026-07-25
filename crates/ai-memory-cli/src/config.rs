//! Runtime configuration loader.
//!
//! All settings are read exactly once at startup, merged into a single
//! immutable [`Config`] value, and passed by reference everywhere. There is
//! no second read path (lesson from agentmemory #456 / #469 — the dimension
//! guard read `process.env` while the rest of the codebase used
//! `getMergedEnv()`, masking the bug for weeks).

use std::path::{Path, PathBuf};

use ai_memory_llm::{
    AuthRequirement, EmbedderChoice, EmbedderConfig, LlmError, LlmResult, OPENCODE_DEFAULT_MODEL,
    ProviderAuth, ProviderChoice, ProviderConfig,
};
use anyhow::{Context, Result};
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

/// Default HTTP bind address for the local single-user server.
pub const DEFAULT_BIND: &str = "127.0.0.1:49374";

/// Default base URL used by thin-client CLI subcommands.
pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

/// Default MCP endpoint URL rendered for client integrations.
pub const DEFAULT_MCP_URL: &str = "http://127.0.0.1:49374/mcp";

/// Default confidence floor for staged auto-improvement proposals.
pub const DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE: f32 =
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE;

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
    /// URL subpath the server is mounted under (e.g. `/wiki`). Thin-client
    /// CLI commands prepend it to every `/admin/*` request so deployments
    /// hosted behind a reverse proxy under a subpath don't 404. Settable via
    /// `AI_MEMORY_BASE_PATH`. Empty for root-mounted deployments (the
    /// default). The `serve` subcommand reads the same env var via clap and
    /// nests its router accordingly; this field is what the thin-client
    /// path needs to discover the same prefix without a second
    /// `std::env::var` call (invariant: one config-read path).
    #[serde(default)]
    pub base_path: String,
    /// Operator home directory, captured once here (the single config-read
    /// path) from `AI_MEMORY_HOME` or `$HOME`. Used to keep the cwd->project resolver and the
    /// startup heal from treating `$HOME` as a prefix-match catch-all
    /// (issue #103) without env reads scattered through the runtime. Not a
    /// config.toml key: always derived from the process environment at load.
    #[serde(skip)]
    pub home_dir: Option<String>,
    /// Per-subsystem log filter (overridable by `RUST_LOG`).
    pub log_level: String,
    /// Optional LLM provider (`anthropic`, `openai`, `gemini`, `openai-compat`, `openai-oauth`, `copilot`).
    pub llm_provider: Option<String>,
    /// Optional LLM model override.
    pub llm_model: Option<String>,
    /// Optional Gemini/Google API keys (comma-separated string or TOML
    /// array) enabling round-robin rotation on 429/5xx. Loaded from the
    /// root of `config.toml` or the `GEMINI_API_KEYS`/`GOOGLE_API_KEYS` env
    /// vars; env takes precedence when both are present.
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "deserialize_api_keys_string_or_vec_option"
    )]
    pub gemini_api_keys: Option<Vec<SecretString>>,
    /// Optional singular Gemini/Google API key at the root of `config.toml`
    /// (e.g. `gemini_api_key = "..."`). Accepted for ergonomics; when both
    /// `gemini_api_key` and `gemini_api_keys` are present in TOML, the plural
    /// `gemini_api_keys` list wins. Env-provided keys still take precedence
    /// over either TOML form.
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "deserialize_optional_secret_string"
    )]
    pub gemini_api_key: Option<SecretString>,
    /// Optional LLM base URL override.
    pub llm_base_url: Option<String>,
    /// Optional LLM API keys (comma-separated string or TOML array) at the
    /// root of `config.toml`, enabling round-robin rotation on 429/5xx for the
    /// OpenAI-family providers (`openai`, `openai-compat`, `opencode`). Loaded
    /// from `LLM_API_KEYS`, or the root `llm_api_keys` TOML field; env takes
    /// precedence when both are present. Mirrors `gemini_api_keys`.
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "deserialize_api_keys_string_or_vec_option"
    )]
    pub llm_api_keys: Option<Vec<SecretString>>,
    /// Optional LLM API key at the root of `config.toml` (e.g.
    /// `llm_api_key = "..."`). Used by `openai-compat` (and any provider whose
    /// `LLM_API_KEY` env var is the key source) so the key can live in
    /// `config.toml` instead of the environment. Env `LLM_API_KEY` wins when
    /// both are present.
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "deserialize_optional_secret_string"
    )]
    pub llm_api_key: Option<SecretString>,
    /// Opt-in: send `response_format=json_schema` (strict) to the
    /// `openai-compat` provider instead of asking for prose JSON and
    /// extracting the first balanced object. Off by default — the tolerant
    /// parser stays the default for older local engines that ignore
    /// `response_format`. Modern engines (recent OLLama, vLLM, LM Studio,
    /// llama.cpp) honour structured output; this lets the operator opt in.
    /// If the strict raw call fails, the provider falls back to the tolerant
    /// parser. Set with `AI_MEMORY_LLM_COMPAT_STRICT=true`.
    pub llm_compat_strict: bool,
    /// Optional cap on concurrent in-flight requests to the LLM gateway
    /// (OpenAI-family providers: `openai`, `openai-compat`, `opencode`).
    /// Bounds bursts of consolidation / embedding calls so they cannot trip
    /// gateway throttling ("too many calls"). `None` uses the provider
    /// default (4); set to `0` to disable the limiter. Configured via the
    /// `llm_max_concurrency` TOML field or `AI_MEMORY_LLM_MAX_CONCURRENCY`.
    pub llm_max_concurrency: Option<usize>,
    /// Per-key 429 blacklist cooldown in seconds. When a key receives a
    /// 429 rate-limit response, it is parked for this duration before the
    /// rotation pool retries it. `None` uses the provider default (3600
    /// seconds = 1 hour). Configurable via the `llm_key_cooldown_secs`
    /// TOML field or `AI_MEMORY_LLM_KEY_COOLDOWN_SECS`.
    pub llm_key_cooldown_secs: Option<u64>,
    /// Run LLM consolidation on SessionEnd (in addition to the always-written
    /// heuristic session page), when an LLM provider is configured. On by
    /// default — SessionEnd compiles observations into wiki pages; set
    /// `AI_MEMORY_CONSOLIDATE_ON_SESSION_END=false` to disable and keep
    /// SessionEnd cheap and fire-and-forget. The LLM checkpoint also runs on
    /// PreCompact and via manual `memory_consolidate`.
    pub consolidate_on_session_end: bool,
    /// Server-side opt-in for assistant/Stop capture (#196). When true, the
    /// server honors a client's sanitized `_ai_memory_assistant` marker on a
    /// `Stop` event and persists the excerpt as the Stop body. Off by default;
    /// when off the marker is stripped and the Stop stays empty. The client half
    /// of the double opt-in is baked separately by
    /// `install-hooks --capture-assistant`. Set with
    /// `AI_MEMORY_CAPTURE_ASSISTANT=true`.
    pub capture_assistant: bool,
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
    /// Auto-improvement reviewer. The scheduler launches background review for
    /// newly completed sessions; manual CLI/admin/MCP runs remain available.
    /// Both approve validated proposals by default unless `require_approval` is
    /// set. The SessionEnd trigger stays off by default.
    pub auto_improve: AutoImproveSettings,
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
    /// `[auto_scope]` — opt-in isolation of the hook-published "current
    /// project" pointer used by MCP tools that omit `workspace`/`project`.
    /// Default `single` mode preserves the legacy global slot; `per_session`
    /// and `per_actor` are for shared installs. See [`AutoScopeSettings`]
    /// and [`ai_memory_core::ActiveProjectMode`].
    pub auto_scope: AutoScopeSettings,
    /// Env-backed alias for hook ingest tokens per second per source.
    pub hook_rate_per_sec: f64,
    /// Env-backed alias for hook ingest burst tokens per source.
    pub hook_rate_burst: f64,
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
    /// Origins allowed to make cross-origin requests to /api/v1. Empty
    /// (default) means same-origin only — host your SPA via --web-ui-dir
    /// instead of using CORS if you can. When non-empty, a CorsLayer is
    /// attached ONLY to /api/v1; /mcp, /hook, /admin, and /web are NOT
    /// CORS-enabled (those aren't browser-accessible by design).
    ///
    /// Settable via AI_MEMORY_CORS_ALLOW_ORIGINS=a,b,c or one or more
    /// --cors-allow-origin flags. Each entry must include a scheme;
    /// `*` is rejected.
    #[serde(deserialize_with = "deserialize_string_or_vec", default)]
    pub cors_allow_origins: Vec<String>,
    /// Admission webhook chain — synchronous HTTP hooks invoked in
    /// [`ai_memory_wiki::Wiki::write_page`] just before page persistence.
    /// Each entry is a [`ai_memory_wiki::WebhookConfig`]. Empty by default
    /// (no chain attached → engine runs as before). Configure via TOML:
    /// ```toml
    /// [[admission_webhooks]]
    /// name = "contributors"
    /// url  = "http://contributors-webhook.memory.svc.cluster.local/enrich"
    /// timeout_ms = 2000
    /// failure_policy = "ignore"
    /// events = ["write_page", "consolidate"]
    /// ```
    /// Env override: `AI_MEMORY_ADMISSION_WEBHOOKS__0__URL=…`,
    /// `AI_MEMORY_ADMISSION_WEBHOOKS__0__NAME=…`, etc.
    /// See [`ai_memory_wiki::admission`] for the contract.
    #[serde(default)]
    pub admission_webhooks: Vec<ai_memory_wiki::WebhookConfig>,
    /// Process-only env values that should never be written to config files.
    #[serde(skip)]
    pub runtime_env: RuntimeEnv,
}

/// Environment-only values captured once by [`Config::load`].
#[derive(Debug, Clone, Default)]
pub struct RuntimeEnv {
    data_dir: Option<PathBuf>,
    home_dir: Option<String>,
    server_url: Option<String>,
    auth_token: Option<String>,
    host_cwd: Option<String>,
    anthropic_api_key: Option<SecretString>,
    anthropic_oauth_token: Option<SecretString>,
    openai_api_key: Option<SecretString>,
    openai_api_keys: Option<Vec<SecretString>>,
    gemini_api_key: Option<SecretString>,
    llm_api_keys: Option<Vec<SecretString>>,
    gemini_api_keys: Option<Vec<SecretString>>,
    llm_api_key: Option<SecretString>,
    llm_base_url: Option<String>,
    copilot_github_token: Option<SecretString>,
    github_copilot_api_token: Option<SecretString>,
    copilot_api_url: Option<String>,
    copilot_client_id: Option<String>,
    voyage_api_key: Option<SecretString>,
    opencode_api_key: Option<SecretString>,
}

impl RuntimeEnv {
    fn from_process() -> Self {
        // GEMINI_API_KEYS can be comma-separated
        let gemini_api_keys: Option<Vec<SecretString>> = env_secret("GEMINI_API_KEYS")
            .or_else(|| env_secret("GOOGLE_API_KEYS"))
            .map(|s| {
                s.expose_secret()
                    .split(',')
                    .map(|k| k.trim())
                    .filter(|k| !k.is_empty())
                    .map(SecretString::from)
                    .collect()
            });
        let gemini_single = env_secret("GEMINI_API_KEY").or_else(|| env_secret("GOOGLE_API_KEY"));
        let gemini_key = if gemini_api_keys
            .as_ref()
            .map(|k| !k.is_empty())
            .unwrap_or(false)
        {
            gemini_api_keys.as_ref().and_then(|k| k.first().cloned())
        } else {
            gemini_single
        };

        Self {
            data_dir: env_path("AI_MEMORY_DATA_DIR"),
            home_dir: env_string("AI_MEMORY_HOME").or_else(|| env_string("HOME")),
            server_url: env_string("AI_MEMORY_SERVER_URL"),
            auth_token: env_string("AI_MEMORY_AUTH_TOKEN"),
            host_cwd: env_string("AI_MEMORY_HOST_CWD"),
            anthropic_api_key: env_secret("ANTHROPIC_API_KEY"),
            // CLAUDE_CODE_OAUTH_TOKEN is what `claude setup-token` writes;
            // ANTHROPIC_OAUTH_TOKEN is our canonical name — accept both.
            anthropic_oauth_token: env_secret("ANTHROPIC_OAUTH_TOKEN")
                .or_else(|| env_secret("CLAUDE_CODE_OAUTH_TOKEN")),
            openai_api_key: env_secret("OPENAI_API_KEY"),
            openai_api_keys: env_secret("OPENAI_API_KEYS").map(|s| {
                s.expose_secret()
                    .split(',')
                    .map(|k| k.trim())
                    .filter(|k| !k.is_empty())
                    .map(SecretString::from)
                    .collect()
            }),
            gemini_api_key: gemini_key,
            llm_api_keys: env_secret("LLM_API_KEYS").map(|s| {
                s.expose_secret()
                    .split(',')
                    .map(|k| k.trim())
                    .filter(|k| !k.is_empty())
                    .map(SecretString::from)
                    .collect()
            }),
            gemini_api_keys,
            llm_api_key: env_secret("LLM_API_KEY"),
            llm_base_url: env_string("LLM_BASE_URL"),
            copilot_github_token: env_secret("COPILOT_GITHUB_TOKEN")
                .or_else(|| env_secret("GH_TOKEN"))
                .or_else(|| env_secret("GITHUB_TOKEN")),
            github_copilot_api_token: env_secret("GITHUB_COPILOT_API_TOKEN"),
            copilot_api_url: env_string("COPILOT_API_URL"),
            copilot_client_id: env_string("AI_MEMORY_COPILOT_CLIENT_ID"),
            voyage_api_key: env_secret("VOYAGE_API_KEY"),
            opencode_api_key: env_secret("OPENCODE_API_KEY"),
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

/// Accept an optional `Vec<SecretString>` from either a comma-separated
/// string (e.g. `gemini_api_keys = "sk-1,sk-2,sk-3"`) or a TOML/JSON array at the
/// config root. Used for the root-level `gemini_api_keys` key.
fn deserialize_api_keys_string_or_vec_option<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<SecretString>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Single(String),
        Many(Vec<String>),
    }
    Ok(match Option::<Either>::deserialize(deserializer)? {
        None => None,
        Some(Either::Single(s)) => Some(split_api_keys(&s)),
        Some(Either::Many(v)) => Some(
            v.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(SecretString::from)
                .collect(),
        ),
    })
}

/// Split a comma-separated API-key string into [`SecretString`]s, dropping
/// empty entries (e.g. trailing commas).
fn split_api_keys(s: &str) -> Vec<SecretString> {
    s.split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(SecretString::from)
        .collect()
}

/// Accept an optional single [`SecretString`] from a plain TOML string. Used
/// for the root-level singular `gemini_api_key` key, so operators can write
/// `gemini_api_key = "..."` instead of the plural `gemini_api_keys` list.
fn deserialize_optional_secret_string<'de, D>(
    deserializer: D,
) -> Result<Option<SecretString>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.map(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            SecretString::from("")
        } else {
            SecretString::from(trimmed)
        }
    }))
}

/// Resolve the effective Gemini keys from env vs. the root-level TOML
/// `gemini_api_key` (singular) and `gemini_api_keys` (plural) values.
///
/// Precedence (most to least specific):
/// 1. Plural env (`GEMINI_API_KEYS`/`GOOGLE_API_KEYS`) — wins over everything.
/// 2. Singular env (`GEMINI_API_KEY`/`GOOGLE_API_KEY`).
/// 3. TOML `gemini_api_keys` (plural list).
/// 4. TOML `gemini_api_key` (singular).
///
/// Within any of the above, a plural key list always overrides a singular
/// key: when both a singular and a plural value are present, the plural list
/// is used and the singular value is ignored (env and TOML alike). So if TOML
/// has both `gemini_api_key` and `gemini_api_keys`, the plural list wins.
fn resolve_gemini_keys(
    env_key: Option<SecretString>,
    env_keys: Option<Vec<SecretString>>,
    toml_key: Option<SecretString>,
    toml_keys: Vec<SecretString>,
) -> (Option<SecretString>, Option<Vec<SecretString>>) {
    // Plural env wins outright and subsumes any singular env key.
    if let Some(keys) = &env_keys
        && !keys.is_empty()
    {
        return (keys.first().cloned(), Some(keys.clone()));
    }
    // Singular env wins over any TOML key.
    if let Some(key) = env_key {
        return (Some(key), None);
    }
    // TOML plural list backs the multi-key rotation when present.
    if !toml_keys.is_empty() {
        let key = toml_keys.first().cloned();
        return (key, Some(toml_keys));
    }
    // TOML singular key is the lowest-priority fallback.
    if let Some(key) = toml_key {
        return (Some(key), None);
    }
    (None, None)
}

/// Resolve the effective OpenAI-family LLM keys from env vs. the root-level
/// TOML `llm_api_key` (singular) and `llm_api_keys` (plural) values.
///
/// Precedence (most to least specific):
/// 1. Plural env (`OPENAI_API_KEYS` / `LLM_API_KEYS`).
/// 2. Singular env (`OPENAI_API_KEY` / `LLM_API_KEY`).
/// 3. TOML `llm_api_keys` (plural list).
/// 4. TOML `llm_api_key` (singular).
///
/// A plural key list always overrides a singular value when both are present
/// (env and TOML alike), mirroring [`resolve_gemini_keys`]. Used to back
/// round-robin rotation for `openai`, `openai-compat`, and `opencode` when the
/// operator supplies multiple keys.
fn resolve_llm_keys(
    env_key: Option<SecretString>,
    env_keys: Option<Vec<SecretString>>,
    toml_key: Option<SecretString>,
    toml_keys: Vec<SecretString>,
) -> (Option<SecretString>, Option<Vec<SecretString>>) {
    if let Some(keys) = &env_keys
        && !keys.is_empty()
    {
        return (keys.first().cloned(), Some(keys.clone()));
    }
    if let Some(key) = env_key {
        return (Some(key), None);
    }
    if !toml_keys.is_empty() {
        let key = toml_keys.first().cloned();
        return (key, Some(toml_keys));
    }
    if let Some(key) = toml_key {
        return (Some(key), None);
    }
    (None, None)
}

/// `[auth]` section of `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// Shared bearer token. When set, all HTTP routes require
    /// `Authorization: Bearer <token>`. Generate one with
    /// `ai-memory generate-auth-token`.
    pub bearer_token: Option<String>,
    /// Username attributed to writes authenticated by the bearer
    /// token (rung 1: "identified single-user"). When set, the
    /// auth middleware injects an
    /// [`ai_memory_core::ActorContext`] with `user =
    /// Some(root_username)` on root-token requests, so audit_log
    /// and page frontmatter record the operator instead of
    /// staying anonymous. Omit (or leave empty) to keep the
    /// pre-multi-user behaviour — bearer authenticates but
    /// attributes anonymously.
    pub root_username: Option<String>,
    /// Optional email for the root user, surfaced alongside
    /// `root_username` in the web UI + `/api/v1` responses.
    pub root_email: Option<String>,
    /// Optional display name for the root user (e.g.
    /// `"Alice Smith"`); falls back to `root_username` in UIs.
    pub root_name: Option<String>,
    /// Per-server token pepper used by
    /// [`ai_memory_store::hash_token`] to keep stolen
    /// `users.token_hash` rows useless to an offline attacker.
    /// Auto-generated by `ai-memory init` (32 bytes of OS CSPRNG,
    /// hex-encoded). MUST NOT change after the first user is added
    /// — rotating it invalidates every existing token. It enables DB-user
    /// token resolution even during first-user bootstrap; operational admin
    /// access becomes root-only once a user row exists.
    pub token_pepper: Option<String>,
}

/// `[auto_scope]` — controls how the hook-published "currently active
/// project" pointer is shared across concurrent callers. The legacy default
/// is `single` (process-wide slot, last-write-wins). Opt-in modes isolate
/// concurrent agent runs and/or operators.
///
/// Set under `[auto_scope]` in `config.toml` or via the
/// `AI_MEMORY_AUTO_SCOPE__MODE`, `AI_MEMORY_AUTO_SCOPE__SESSION_TTL_SECS`,
/// and `AI_MEMORY_AUTO_SCOPE__MAX_ENTRIES` env vars.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoScopeSettings {
    /// `single` (default), `per_session`, or `per_actor`. See
    /// [`ai_memory_core::ActiveProjectMode`] for full semantics.
    pub mode: ai_memory_core::ActiveProjectMode,
    /// TTL (seconds) for per-key entries in `per_session`/`per_actor`
    /// modes. Default is 1 hour. Set to 0 to fall back to the default.
    pub session_ttl_secs: u64,
    /// Hard upper bound on the per-key map size, evicting the oldest
    /// insertions first. Default 4096; lower for very small installs,
    /// raise for shared engines with many concurrent agents.
    pub max_entries: usize,
}

impl Default for AutoScopeSettings {
    fn default() -> Self {
        Self {
            mode: ai_memory_core::ActiveProjectMode::default(),
            session_ttl_secs: ai_memory_core::DEFAULT_PER_KEY_TTL.as_secs(),
            max_entries: ai_memory_core::DEFAULT_MAX_ENTRIES,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bind: DEFAULT_BIND.into(),
            server_url: DEFAULT_SERVER_URL.into(),
            base_path: String::new(),
            home_dir: None,
            log_level: "info".into(),
            llm_provider: Some("gemini".into()),
            llm_model: None,
            gemini_api_keys: None,
            gemini_api_key: None,
            llm_base_url: None,
            llm_api_keys: None,
            llm_max_concurrency: None,
            llm_key_cooldown_secs: None,
            llm_api_key: None,
            llm_compat_strict: false,
            consolidate_on_session_end: false,
            capture_assistant: false,
            embedding_provider: None,
            embedding_model: None,
            embedding_dim: None,
            embedding_base_url: None,
            decay: ai_memory_store::DecayParams::default(),
            maintenance: MaintenanceSettings::default(),
            auto_improve: AutoImproveSettings::default(),
            sanitize: ai_memory_core::SanitizeConfig::default(),
            auth: AuthSettings::default(),
            auto_scope: AutoScopeSettings::default(),
            hook_rate_per_sec: 0.0,
            hook_rate_burst: 0.0,
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
            cors_allow_origins: Vec::new(),
            admission_webhooks: Vec::new(),
            runtime_env: RuntimeEnv::default(),
        }
    }
}

/// `[auto_improve]` optional post-session reviewer settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoImproveSettings {
    /// Background scheduler settings. This controls whether reviews are launched
    /// automatically; it does not control whether accepted proposals are applied.
    pub scheduler: AutoImproveSchedulerSettings,
    /// Optional executable evaluation gate for selected proposal targets.
    pub eval: AutoImproveEvalSettings,
    /// Require manual pending-writes approval. Defaults false so validated
    /// proposals are staged for audit and immediately approved through the
    /// normal wiki write path.
    pub require_approval: bool,
    /// Whether SessionEnd should schedule a reviewer run. Defaults off so hooks
    /// stay cheap and fire-and-forget.
    pub on_session_end: bool,
    /// Minimum observations before a session is worth reviewing.
    pub min_observations: usize,
    /// Minimum span between first and last observation before review.
    pub min_session_duration_secs: u64,
    /// Minimum model confidence accepted by validation.
    pub min_confidence: f32,
    /// Approximate chars/4 prompt budget for review input.
    pub max_input_tokens: usize,
    /// Maximum validated proposals returned from one run.
    pub max_proposals_per_run: usize,
    /// Maximum existing _rules/ and procedures/ pages included for patch proposals.
    pub max_patchable_pages: usize,
    /// Maximum body chars rendered per patchable target page.
    pub max_patchable_body_chars: usize,
    /// Maximum patch edits per proposal.
    pub max_edits_per_proposal: usize,
    /// Maximum content chars in one patch edit.
    pub max_edit_content_chars: usize,
    /// Maximum aggregate changed chars in one patch proposal.
    pub max_changed_chars_per_proposal: usize,
    /// Maximum patch edits accepted across one review run.
    pub max_patch_edits_per_run: usize,
    /// Maximum recent rejection-buffer entries rendered into prompt context.
    pub max_rejection_context: usize,
    /// Maximum age in days for rejection-buffer prompt context.
    pub rejection_context_days: u32,
    /// Maximum materialized final body size.
    pub max_final_body_chars: usize,
    /// Maximum approximate tokens allowed in one _rules/ page.
    pub max_rule_page_tokens: usize,
    /// Maximum approximate tokens allowed in one procedures/ page.
    pub max_procedure_page_tokens: usize,
    /// Whether future reviewers may include raw observation fallback details.
    pub include_raw_fallback: bool,
    /// Synthetic actor used for autonomous proposal provenance.
    pub proposal_actor: String,
    /// Wiki-relative folder for non-indexed pending proposal sidecars.
    pub pending_path: String,
}

/// `[auto_improve.eval]` optional executable proposal gate settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoImproveEvalSettings {
    /// Whether the eval command gate is enabled.
    pub enabled: bool,
    /// Executable command plus whitespace-separated args. Executed directly, not through a shell.
    pub command: String,
    /// Timeout per proposal eval command.
    pub timeout_secs: u64,
    /// Wiki path prefixes that require eval when enabled.
    pub targets: Vec<String>,
    /// Required score_after - score_before when scores are present.
    pub min_delta: f64,
}

impl Default for AutoImproveEvalSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            timeout_secs: 120,
            targets: ai_memory_consolidate::default_auto_improve_eval_targets(),
            min_delta: 0.0,
        }
    }
}

/// `[auto_improve.scheduler]` background learning loop settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoImproveSchedulerSettings {
    /// Whether the server should periodically review newly-completed sessions.
    pub enabled: bool,
    /// Scheduler cadence. `0` disables the scheduler while keeping manual runs.
    pub interval_secs: u64,
    /// Maximum sessions reviewed per project in one scheduler tick. `0` disables the scheduler.
    pub max_sessions_per_tick: usize,
    /// Minimum age after SessionEnd before a session becomes eligible.
    pub min_session_age_secs: u64,
}

impl Default for AutoImproveSchedulerSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 3_600,
            max_sessions_per_tick: 1,
            min_session_age_secs: 600,
        }
    }
}

impl Default for AutoImproveSettings {
    fn default() -> Self {
        Self {
            scheduler: AutoImproveSchedulerSettings::default(),
            eval: AutoImproveEvalSettings::default(),
            require_approval: false,
            on_session_end: false,
            min_observations: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS,
            min_session_duration_secs:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS,
            min_confidence: DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE,
            max_input_tokens: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS,
            max_proposals_per_run: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS,
            max_patchable_pages: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES,
            max_patchable_body_chars:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS,
            max_edits_per_proposal:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL,
            max_edit_content_chars:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS,
            max_changed_chars_per_proposal:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL,
            max_patch_edits_per_run:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN,
            max_rejection_context:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT,
            rejection_context_days:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS,
            max_final_body_chars: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS,
            max_rule_page_tokens: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS,
            max_procedure_page_tokens:
                ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS,
            include_raw_fallback: false,
            proposal_actor: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR.into(),
            pending_path: ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_PENDING_PATH.into(),
        }
    }
}

/// `[maintenance]` scheduled server jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaintenanceSettings {
    /// Master switch for scheduled jobs.
    pub enabled: bool,
    /// Interval for the retention forget sweep. `0` disables this job. Successful
    /// runs persist cadence across restarts; overdue work starts after a bounded
    /// startup delay.
    pub forget_sweep_interval_secs: u64,
    /// Interval for rule-based wiki lint. `0` disables this job. Successful runs
    /// persist cadence across restarts; overdue work starts after a bounded
    /// startup delay.
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
        // Convenience env override for the admission webhook list. Figment
        // can't reliably round-trip `Vec<Struct>` from `AI_MEMORY_X__0__Y`
        // (env split builds a Map, not a Vec), so we accept a single
        // JSON-encoded env var instead — perfect for charts that
        // `toJson` a values.yaml list. Overrides anything figment loaded
        // from file/other env layers.
        if let Ok(raw) = std::env::var("AI_MEMORY_ADMISSION_WEBHOOKS_JSON")
            && !raw.trim().is_empty()
        {
            let parsed: Vec<ai_memory_wiki::WebhookConfig> = serde_json::from_str(&raw)
                .with_context(|| {
                    "parsing AI_MEMORY_ADMISSION_WEBHOOKS_JSON (must be a JSON array of \
                     {name,url,timeout_ms?,failure_policy?,events})"
                })?;
            config.admission_webhooks = parsed;
        }

        // Home is captured once in RuntimeEnv (config-read-path invariant);
        // threaded to the resolver guard and startup heal so neither reads the
        // env directly. AI_MEMORY_HOME is accepted for tests/wrappers that need
        // to emulate a host home distinct from the process HOME.
        config.home_dir = runtime_env.home_dir.as_deref().and_then(normalize_home_dir);

        // CLI override always wins (figment doesn't see it because clap has
        // already parsed the flag into `cli_data_dir`).
        if let Some(dir) = cli_data_dir {
            config.data_dir = dir;
        } else if let Some(dir) = runtime_env.data_dir.clone() {
            config.data_dir = dir;
        }

        config.data_dir = canonicalise_or_keep(&config.data_dir);
        config.runtime_env = runtime_env;

        // Root-level `gemini_api_key` (singular) and `gemini_api_keys`
        // (plural) in config.toml back the multi-key rotation only when no
        // Gemini env key (singular or plural) is set. Env wins; when present
        // it already populated both the plural list and the singular key in
        // `RuntimeEnv::from_process`. When both TOML forms are present, the
        // plural list wins over the singular key.
        let toml_key = config.gemini_api_key.clone();
        let toml_keys = config.gemini_api_keys.clone().unwrap_or_default();
        let (resolved_key, resolved_keys) = resolve_gemini_keys(
            config.runtime_env.gemini_api_key.clone(),
            config.runtime_env.gemini_api_keys.clone(),
            toml_key,
            toml_keys,
        );
        config.runtime_env.gemini_api_key = resolved_key;
        config.runtime_env.gemini_api_keys = resolved_keys;

        // Root-level `llm_api_key` / `llm_api_keys` in config.toml back the
        // OpenAI-family key sources (openai, openai-compat, opencode) only
        // when no LLM env key (singular or plural) is set. Env wins; when
        // present it already populated both the plural list and the singular
        // key in `RuntimeEnv::from_process`. When both TOML forms are present,
        // the plural list wins over the singular key.
        let toml_key = config.llm_api_key.clone();
        let toml_keys = config.llm_api_keys.clone().unwrap_or_default();
        let (resolved_key, resolved_keys) = resolve_llm_keys(
            config.runtime_env.llm_api_key.clone(),
            config.runtime_env.llm_api_keys.clone(),
            toml_key,
            toml_keys,
        );
        config.runtime_env.llm_api_key = resolved_key;
        config.runtime_env.llm_api_keys = resolved_keys.clone();

        // `opencode` falls back to the resolved LLM key/keys when its own
        // provider-specific env var is unset. Env wins; `RuntimeEnv::from_process`
        // already populated `opencode_api_key` when present.
        if config.runtime_env.opencode_api_key.is_none() {
            config.runtime_env.opencode_api_key = config.runtime_env.llm_api_key.clone();
        }

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
            "opencode" | "opencode-zen" | "opencode_zen" => ProviderChoice::OpenCode,
            other => {
                return Err(LlmError::NotConfigured(format!(
                    "AI_MEMORY_LLM_PROVIDER={other} is not one of \
                     anthropic|openai|gemini|openai-compat|openai-oauth|copilot|anthropic-oauth|opencode"
                )));
            }
        };
        let model = match non_empty(self.llm_model.as_deref()) {
            Some(s) => s.to_string(),
            None => match provider {
                ProviderChoice::Anthropic => "claude-haiku-4-5".to_string(),
                ProviderChoice::AnthropicOAuth => "claude-sonnet-4-6".to_string(),
                ProviderChoice::OpenAi => "gpt-5.4-mini".to_string(),
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
                ProviderChoice::OpenCode => OPENCODE_DEFAULT_MODEL.to_string(),
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
            compat_strict: self.llm_compat_strict,
            api_keys: Vec::new(),
            max_concurrency: self.llm_max_concurrency,
            key_cooldown_secs: self.llm_key_cooldown_secs,
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
        let api_keys = self.runtime_env.gemini_api_keys.clone().unwrap_or_default();
        Ok(Some(EmbedderConfig {
            provider,
            model,
            dim,
            api_key,
            base_url: self.embedding_base_url.clone(),
            api_keys,
            key_cooldown_secs: self.llm_key_cooldown_secs,
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
            ProviderChoice::OpenCode => self
                .runtime_env
                .opencode_api_key
                .clone()
                .or_else(|| self.runtime_env.llm_api_key.clone()),
        }
    }

    /// Resolve the configured multi-key list for a provider, if the operator
    /// set a comma-separated `KEYS` env var or a `llm_api_keys` /
    /// `gemini_api_keys` TOML list. OpenAI-family providers (OpenAI,
    /// OpenCode, openai-compat) read `OPENAI_API_KEYS` / `LLM_API_KEYS`;
    /// Gemini reads `GEMINI_API_KEYS`/`GOOGLE_API_KEYS`. Returns `None` when no
    /// list is configured so single-key resolution remains the default path.
    fn multi_api_keys_for(&self, provider: ProviderChoice) -> Option<Vec<SecretString>> {
        match provider {
            ProviderChoice::Gemini => self.runtime_env.gemini_api_keys.clone(),
            ProviderChoice::OpenAi | ProviderChoice::OpenCode => self
                .runtime_env
                .openai_api_keys
                .clone()
                .or_else(|| self.runtime_env.llm_api_keys.clone()),
            ProviderChoice::OpenAiCompat => self.runtime_env.llm_api_keys.clone(),
            // These providers have no multi-key rotation surface.
            ProviderChoice::Anthropic
            | ProviderChoice::OpenAiOAuth
            | ProviderChoice::Copilot
            | ProviderChoice::AnthropicOAuth => None,
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

    /// Shared OIDC device-grant token file path.
    #[must_use]
    pub fn oidc_device_token_path(&self) -> PathBuf {
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
                // For Gemini and OpenAI-family providers, use multi-key auth
                // if a comma-separated key list is configured (e.g.
                // GEMINI_API_KEYS / OPENAI_API_KEYS), enabling round-robin
                // rotation on 429/5xx like the Gemini provider.
                if let Some(ref keys) = self.multi_api_keys_for(provider)
                    && !keys.is_empty()
                {
                    return ProviderAuth::required_api_keys_from_env(
                        env_var,
                        if let Some(override_key) = api_key_override {
                            Some(vec![override_key])
                        } else {
                            Some(keys.clone())
                        },
                    );
                }
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

/// Normalize home for prefix-match comparisons: accept either slash spelling,
/// strip trailing separators,
/// so a stored `repo_path` of `/home/u` still equals a `$HOME` of `/home/u/`
/// (the cwd side is trimmed the same way in `find_project_by_cwd_prefix`).
/// All-separator or empty input yields `None` (no usable home).
fn normalize_home_dir(home: &str) -> Option<String> {
    let normalized = home.replace('\\', "/");
    let trimmed = normalized.trim_end_matches('/');
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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
        assert!(cfg.auto_improve.scheduler.enabled);
        assert_eq!(cfg.auto_improve.scheduler.interval_secs, 3_600);
        assert_eq!(cfg.auto_improve.scheduler.max_sessions_per_tick, 1);
        assert_eq!(cfg.auto_improve.scheduler.min_session_age_secs, 600);
        assert!(!cfg.auto_improve.on_session_end);
        assert!(!cfg.auto_improve.require_approval);
        assert_eq!(cfg.auto_improve.min_observations, 8);
        assert_eq!(cfg.auto_improve.min_session_duration_secs, 120);
        assert_eq!(
            cfg.auto_improve.min_confidence,
            DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE
        );
        assert_eq!(cfg.auto_improve.max_input_tokens, 24_000);
        assert_eq!(cfg.auto_improve.max_proposals_per_run, 5);
        assert_eq!(cfg.auto_improve.max_patchable_pages, 8);
        assert_eq!(cfg.auto_improve.max_patchable_body_chars, 8_000);
        assert_eq!(cfg.auto_improve.max_edits_per_proposal, 5);
        assert_eq!(cfg.auto_improve.max_edit_content_chars, 4_000);
        assert_eq!(cfg.auto_improve.max_changed_chars_per_proposal, 12_000);
        assert_eq!(cfg.auto_improve.max_patch_edits_per_run, 8);
        assert_eq!(cfg.auto_improve.max_rejection_context, 50);
        assert_eq!(cfg.auto_improve.rejection_context_days, 180);
        assert_eq!(cfg.auto_improve.max_final_body_chars, 32_000);
        assert_eq!(cfg.auto_improve.max_rule_page_tokens, 2_000);
        assert_eq!(cfg.auto_improve.max_procedure_page_tokens, 2_000);
        assert!(!cfg.auto_improve.eval.enabled);
        assert_eq!(cfg.auto_improve.eval.command, "");
        assert_eq!(cfg.auto_improve.eval.timeout_secs, 120);
        assert_eq!(cfg.auto_improve.eval.targets, vec!["_rules", "procedures"]);
        assert_eq!(cfg.auto_improve.eval.min_delta, 0.0);
        assert!(!cfg.auto_improve.include_raw_fallback);
        assert_eq!(cfg.auto_improve.proposal_actor, "auto_improve");
        assert_eq!(cfg.auto_improve.pending_path, "_pending/auto-improve");
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
    fn load_populates_home_dir_from_env() {
        let tmp = TempDir::new().unwrap();
        let cli_dir = tmp.path().join("override");
        let cfg = Config::load(None, Some(cli_dir)).unwrap();
        // `home_dir` is derived from AI_MEMORY_HOME or `$HOME` at load (the
        // single config-read path), normalized so a trailing slash can't bypass
        // the catch-all guards. Reading the env in a test is allowed; this
        // fails if the load-time assignment is dropped while either env var is
        // set.
        assert_eq!(
            cfg.home_dir,
            std::env::var("AI_MEMORY_HOME")
                .or_else(|_| std::env::var("HOME"))
                .ok()
                .and_then(|h| normalize_home_dir(&h))
        );
    }

    /// True when any Gemini/Google key env var is set. Used to guard tests
    /// that assert the TOML->runtime_env merge, since env wins and the crate
    /// forbids `unsafe_code` (so process env can't be cleared here).
    fn gemini_env_present() -> bool {
        [
            "GEMINI_API_KEYS",
            "GOOGLE_API_KEYS",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
        ]
        .iter()
        .any(|v| std::env::var(v).is_ok())
    }

    #[test]
    fn load_reads_root_gemini_api_keys_string() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "llm_model = \"gemini-3.1-flash-lite\"\ngemini_api_keys = \"sk-1,sk-2,sk-3\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        // The raw root field is sourced only from config.toml (no AI_MEMORY_
        // prefix), so this assertion is env-independent.
        let raw = cfg
            .gemini_api_keys
            .expect("root-level gemini_api_keys parsed from TOML");
        assert_eq!(raw.len(), 3);
        assert_eq!(raw[0].expose_secret(), "sk-1");

        if gemini_env_present() {
            return;
        }
        let keys = cfg
            .runtime_env
            .gemini_api_keys
            .expect("merged into runtime_env");
        assert_eq!(keys.len(), 3);
        assert_eq!(
            cfg.runtime_env
                .gemini_api_key
                .as_ref()
                .unwrap()
                .expose_secret(),
            "sk-1"
        );
    }

    #[test]
    fn load_reads_root_gemini_api_keys_array() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(&cfg_path, "gemini_api_keys = [\"a\", \"b\"]\n").unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        let raw = cfg
            .gemini_api_keys
            .expect("root-level gemini_api_keys array parsed from TOML");
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[1].expose_secret(), "b");

        if gemini_env_present() {
            return;
        }
        let keys = cfg
            .runtime_env
            .gemini_api_keys
            .expect("merged into runtime_env");
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn load_reads_root_gemini_api_key_singular() {
        // Single `gemini_api_key` at the TOML root is now accepted and merged
        // into the singular runtime_env key (no rotation list).
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "llm_model = \"gemini-3.1-flash-lite\"\ngemini_api_key = \"sk-solo\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(
            cfg.gemini_api_key.as_ref().unwrap().expose_secret(),
            "sk-solo"
        );

        if gemini_env_present() {
            return;
        }
        assert_eq!(
            cfg.runtime_env
                .gemini_api_key
                .as_ref()
                .unwrap()
                .expose_secret(),
            "sk-solo",
            "singular TOML key must populate runtime_env"
        );
        assert!(
            cfg.runtime_env.gemini_api_keys.is_none(),
            "singular TOML key must not synthesize a rotation list"
        );
    }

    #[test]
    fn load_root_gemini_api_keys_plural_overrides_singular() {
        // When both `gemini_api_key` and `gemini_api_keys` are in TOML, the
        // plural list wins (and supplies the singular runtime_env key too).
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "gemini_api_key = \"sk-solo\"\ngemini_api_keys = \"p1,p2\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();

        if gemini_env_present() {
            return;
        }
        let keys = cfg
            .runtime_env
            .gemini_api_keys
            .expect("plural TOML list wins");
        assert_eq!(keys.len(), 2);
        assert_eq!(
            cfg.runtime_env
                .gemini_api_key
                .as_ref()
                .unwrap()
                .expose_secret(),
            "p1",
            "plural list supplies the singular runtime key"
        );
    }

    #[test]
    fn load_root_gemini_api_keys_array_drops_blanks() {
        // Regression (M2): array form must trim and drop blank entries, just
        // like the comma-separated string form.
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(&cfg_path, "gemini_api_keys = [\"\", \"a\", \" \", \"b\"]\n").unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        let raw = cfg
            .gemini_api_keys
            .expect("root-level gemini_api_keys array parsed from TOML");
        assert_eq!(raw.len(), 2, "blank entries must be dropped");
        assert_eq!(raw[0].expose_secret(), "a");
        assert_eq!(raw[1].expose_secret(), "b");
    }

    /// `LLM_API_KEYS` env (or a TOML `llm_api_keys` list) backs round-robin
    /// rotation for the OpenAI-family providers, mirroring Gemini's handling.
    #[test]
    fn load_root_llm_api_keys_string_parsed() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "llm_provider = \"openai\"\nllm_api_keys = \"sk-1,sk-2,sk-3\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();

        if std::env::var("LLM_API_KEYS").is_ok() || std::env::var("OPENAI_API_KEYS").is_ok() {
            return;
        }
        let keys = cfg
            .runtime_env
            .llm_api_keys
            .expect("root-level llm_api_keys parsed from TOML");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].expose_secret(), "sk-1");
    }

    #[test]
    fn load_root_llm_api_keys_plural_overrides_singular() {
        // When both `llm_api_key` and `llm_api_keys` are in TOML, the plural
        // list wins (and supplies the singular runtime_env key too).
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "llm_provider = \"openai\"\nllm_api_key = \"sk-solo\"\nllm_api_keys = \"p1,p2\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();

        if std::env::var("LLM_API_KEYS").is_ok()
            || std::env::var("OPENAI_API_KEYS").is_ok()
            || std::env::var("LLM_API_KEY").is_ok()
            || std::env::var("OPENAI_API_KEY").is_ok()
        {
            return;
        }
        let keys = cfg.runtime_env.llm_api_keys.expect("plural TOML list wins");
        assert_eq!(keys.len(), 2);
        assert_eq!(
            cfg.runtime_env
                .llm_api_key
                .as_ref()
                .unwrap()
                .expose_secret(),
            "p1",
            "plural list supplies the singular runtime key"
        );
    }

    #[test]
    fn load_env_llm_api_keys_takes_precedence_over_toml() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(&cfg_path, "llm_api_keys = \"toml1,toml2,toml3\"\n").unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();

        if let Some(env_raw) = std::env::var("LLM_API_KEYS")
            .ok()
            .or_else(|| std::env::var("OPENAI_API_KEYS").ok())
        {
            let env_keys: Vec<String> = env_raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let keys = cfg
                .runtime_env
                .llm_api_keys
                .expect("env plural keys loaded");
            assert_eq!(
                keys.len(),
                env_keys.len(),
                "env plural keys override toml list"
            );
            for (got, want) in keys.iter().zip(&env_keys) {
                assert_eq!(got.expose_secret(), want);
            }
        } else if std::env::var("LLM_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok() {
            // Only a singular env key is set: the TOML list must be ignored
            // (the singular env key wins).
            assert!(
                cfg.runtime_env.llm_api_keys.is_none(),
                "toml must be ignored when only a singular env key is set"
            );
        }
    }

    #[test]
    fn llm_api_keys_reach_openai_provider_auth() {
        // A TOML `llm_api_keys` list must back the OpenAI provider's
        // multi-key rotation (and surface through `api_keys()`).
        let cfg = Config {
            llm_provider: Some("openai".into()),
            runtime_env: RuntimeEnv {
                llm_api_keys: Some(vec![SecretString::from("sk-a"), SecretString::from("sk-b")]),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };
        let auth = cfg.provider_auth(ProviderChoice::OpenAi, None);
        let keys = auth.api_keys();
        assert_eq!(keys.len(), 2, "llm_api_keys must reach openai auth");
        assert_eq!(keys[0].expose_secret(), "sk-a");
    }

    #[test]
    fn load_env_gemini_api_keys_takes_precedence_over_toml() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(&cfg_path, "gemini_api_keys = \"toml1,toml2,toml3\"\n").unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();

        if gemini_env_present() {
            // Env must win over the TOML value, in whichever form it appears.
            let env_plural = std::env::var("GEMINI_API_KEYS")
                .or_else(|_| std::env::var("GOOGLE_API_KEYS"))
                .ok();
            match env_plural {
                Some(env_raw) => {
                    let env_keys: Vec<String> = env_raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let keys = cfg
                        .runtime_env
                        .gemini_api_keys
                        .expect("env plural keys loaded");
                    assert_eq!(
                        keys.len(),
                        env_keys.len(),
                        "env plural keys must override the toml list"
                    );
                    for (got, want) in keys.iter().zip(&env_keys) {
                        assert_eq!(got.expose_secret(), want);
                    }
                }
                None => {
                    // Only a singular env key is set: the TOML list must be
                    // ignored entirely (the singular env key wins).
                    assert!(
                        cfg.runtime_env.gemini_api_keys.is_none(),
                        "toml must be ignored when only a singular env key is set"
                    );
                    assert!(
                        cfg.runtime_env.gemini_api_key.is_some(),
                        "singular env key must be resolved"
                    );
                }
            }
        } else {
            // No env: the TOML value must back the multi-key rotation.
            let keys = cfg
                .runtime_env
                .gemini_api_keys
                .expect("toml gemini_api_keys loaded");
            assert_eq!(keys.len(), 3);
            assert_eq!(
                cfg.runtime_env
                    .gemini_api_key
                    .as_ref()
                    .unwrap()
                    .expose_secret(),
                "toml1"
            );
        }
    }

    #[test]
    fn resolve_gemini_keys_toml_used_when_no_env() {
        let toml = vec![SecretString::from("t1"), SecretString::from("t2")];
        let (key, keys) = resolve_gemini_keys(None, None, None, toml);
        assert_eq!(key.as_ref().map(|s| s.expose_secret()), Some("t1"));
        assert_eq!(keys.unwrap().len(), 2);
    }

    #[test]
    fn resolve_gemini_keys_toml_singular_used_when_no_env() {
        // Single `gemini_api_key` at the TOML root must be accepted now.
        let (key, keys) = resolve_gemini_keys(None, None, Some(SecretString::from("solo")), vec![]);
        assert_eq!(key.as_ref().map(|s| s.expose_secret()), Some("solo"));
        assert!(
            keys.is_none(),
            "singular TOML key must not synthesize a list"
        );
    }

    #[test]
    fn resolve_gemini_keys_toml_plural_overrides_singular() {
        // When both `gemini_api_key` and `gemini_api_keys` are in TOML, the
        // plural list wins (and supplies the singular key too).
        let toml_keys = vec![SecretString::from("p1"), SecretString::from("p2")];
        let (key, keys) =
            resolve_gemini_keys(None, None, Some(SecretString::from("solo")), toml_keys);
        assert_eq!(key.as_ref().map(|s| s.expose_secret()), Some("p1"));
        let keys = keys.expect("plural TOML keys retained");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].expose_secret(), "p1");
        assert_eq!(keys[1].expose_secret(), "p2");
    }

    #[test]
    fn resolve_gemini_keys_singular_env_wins_over_toml() {
        // Regression for H2: a singular GEMINI_API_KEY env must not be
        // overridden by a TOML gemini_api_keys value.
        let toml = vec![SecretString::from("toml1"), SecretString::from("toml2")];
        let (key, keys) = resolve_gemini_keys(Some(SecretString::from("env1")), None, None, toml);
        assert_eq!(key.as_ref().map(|s| s.expose_secret()), Some("env1"));
        assert!(
            keys.is_none(),
            "TOML must be ignored when a singular env key is present"
        );
    }

    #[test]
    fn resolve_gemini_keys_plural_env_wins_over_toml() {
        // Mirror `RuntimeEnv::from_process`: when GEMINI_API_KEYS is set, the
        // singular key is derived as its first entry.
        let toml = vec![SecretString::from("toml1")];
        let env = vec![SecretString::from("env1"), SecretString::from("env2")];
        let (key, keys) = resolve_gemini_keys(
            Some(SecretString::from("env1")),
            Some(env.clone()),
            None,
            toml,
        );
        assert_eq!(key.as_ref().map(|s| s.expose_secret()), Some("env1"));
        let keys = keys.expect("env keys retained");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].expose_secret(), "env1");
        assert_eq!(keys[1].expose_secret(), "env2");
    }

    #[test]
    fn resolve_gemini_keys_empty_toml_yields_none() {
        let (key, keys) = resolve_gemini_keys(None, None, None, vec![]);
        assert!(key.is_none());
        assert!(keys.is_none());
    }

    #[test]
    fn resolve_gemini_keys_plural_env_overrides_singular_env() {
        // Regression: when both the singular (GEMINI_API_KEY) and plural
        // (GEMINI_API_KEYS) env vars are present, the plural list must win and
        // the singular value must be ignored outright.
        let env = vec![SecretString::from("env1"), SecretString::from("env2")];
        let (key, keys) = resolve_gemini_keys(
            Some(SecretString::from("singular")),
            Some(env.clone()),
            None,
            vec![],
        );
        assert_eq!(
            key.as_ref().map(|s| s.expose_secret()),
            Some("env1"),
            "plural env must supply the singular key too"
        );
        let keys = keys.expect("plural env keys retained");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].expose_secret(), "env1");
        assert_eq!(keys[1].expose_secret(), "env2");
    }

    #[test]
    fn normalize_home_dir_trims_trailing_separators() {
        assert_eq!(normalize_home_dir("/home/u/"), Some("/home/u".to_string()));
        assert_eq!(normalize_home_dir("/home/u"), Some("/home/u".to_string()));
        assert_eq!(
            normalize_home_dir("/home/u///"),
            Some("/home/u".to_string())
        );
        assert_eq!(
            normalize_home_dir(r"C:\Users\tester\"),
            Some("C:/Users/tester".to_string())
        );
        // Degenerate inputs yield no usable home rather than an empty or
        // root-collapsing prefix key.
        assert_eq!(normalize_home_dir("/"), None);
        assert_eq!(normalize_home_dir(""), None);
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
            hook_rate_per_sec = 7.5
            hook_rate_burst = 12.0

            [maintenance]
            enabled = false
            lint_interval_secs = 3600

            [auto_improve]
            mode = "dry_run"
            require_approval = true
            on_session_end = true
            min_observations = 3
            min_session_duration_secs = 45
            min_confidence = 0.9
            max_input_tokens = 12000
            max_proposals_per_run = 2
            max_patchable_pages = 3
            max_patchable_body_chars = 4096
            max_edits_per_proposal = 4
            max_edit_content_chars = 1024
            max_changed_chars_per_proposal = 2048
            max_patch_edits_per_run = 6
            max_rejection_context = 7
            rejection_context_days = 14
            max_final_body_chars = 8192
            max_rule_page_tokens = 1000
            max_procedure_page_tokens = 1500
            include_raw_fallback = true
            proposal_actor = "review_bot"
            pending_path = "_pending/review-bot"

            [auto_improve.scheduler]
            enabled = true
            interval_secs = 1800
            max_sessions_per_tick = 4
            min_session_age_secs = 30

            [auto_improve.eval]
            enabled = true
            command = "/usr/local/bin/auto-improve-eval --json"
            timeout_secs = 9
            targets = ["_rules"]
            min_delta = 0.05
            "#,
        )
        .unwrap();
        // Use the tmp dir as the data dir so the resolved config path
        // matches what `load` derives. Passing it explicitly keeps the test
        // free of any global env.
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(cfg.bind, "0.0.0.0:9999");
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.hook_rate_per_sec, 7.5);
        assert_eq!(cfg.hook_rate_burst, 12.0);
        assert!(!cfg.maintenance.enabled);
        assert_eq!(cfg.maintenance.lint_interval_secs, 3600);
        assert!(cfg.auto_improve.scheduler.enabled);
        assert_eq!(cfg.auto_improve.scheduler.interval_secs, 1_800);
        assert_eq!(cfg.auto_improve.scheduler.max_sessions_per_tick, 4);
        assert_eq!(cfg.auto_improve.scheduler.min_session_age_secs, 30);
        assert!(cfg.auto_improve.on_session_end);
        assert!(cfg.auto_improve.require_approval);
        assert_eq!(cfg.auto_improve.min_observations, 3);
        assert_eq!(cfg.auto_improve.min_session_duration_secs, 45);
        assert_eq!(cfg.auto_improve.min_confidence, 0.9);
        assert_eq!(cfg.auto_improve.max_input_tokens, 12_000);
        assert_eq!(cfg.auto_improve.max_proposals_per_run, 2);
        assert_eq!(cfg.auto_improve.max_patchable_pages, 3);
        assert_eq!(cfg.auto_improve.max_patchable_body_chars, 4_096);
        assert_eq!(cfg.auto_improve.max_edits_per_proposal, 4);
        assert_eq!(cfg.auto_improve.max_edit_content_chars, 1_024);
        assert_eq!(cfg.auto_improve.max_changed_chars_per_proposal, 2_048);
        assert_eq!(cfg.auto_improve.max_patch_edits_per_run, 6);
        assert_eq!(cfg.auto_improve.max_rejection_context, 7);
        assert_eq!(cfg.auto_improve.rejection_context_days, 14);
        assert_eq!(cfg.auto_improve.max_final_body_chars, 8_192);
        assert_eq!(cfg.auto_improve.max_rule_page_tokens, 1_000);
        assert_eq!(cfg.auto_improve.max_procedure_page_tokens, 1_500);
        assert!(cfg.auto_improve.eval.enabled);
        assert_eq!(
            cfg.auto_improve.eval.command,
            "/usr/local/bin/auto-improve-eval --json"
        );
        assert_eq!(cfg.auto_improve.eval.timeout_secs, 9);
        assert_eq!(cfg.auto_improve.eval.targets, vec!["_rules"]);
        assert_eq!(cfg.auto_improve.eval.min_delta, 0.05);
        assert!(cfg.auto_improve.include_raw_fallback);
        assert_eq!(cfg.auto_improve.proposal_actor, "review_bot");
        assert_eq!(cfg.auto_improve.pending_path, "_pending/review-bot");
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
        assert_eq!(provider.model, "gpt-5.4-mini");
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
        assert!(!provider.compat_strict);
    }

    #[test]
    fn anthropic_provider_uses_documented_default_model() {
        let cfg = Config {
            llm_provider: Some("anthropic".into()),
            runtime_env: RuntimeEnv {
                anthropic_api_key: Some(SecretString::from("sk-ant-test-key")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let provider = cfg.llm_provider_config().unwrap().unwrap();
        assert_eq!(provider.provider, ProviderChoice::Anthropic);
        assert_eq!(provider.model, "claude-haiku-4-5");
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
    fn openai_compat_toml_llm_api_key_reaches_auth() {
        // A `llm_api_key` at the TOML root must back the `openai-compat`
        // `LLM_API_KEY` source (so the key can live in config.toml, not just
        // the environment). This is what unblocks `openai-compat` against a
        // remote endpoint that 401s on the silent `dummy` fallback key.
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "llm_provider = \"openai-compat\"\n\
             llm_model = \"hy3-free\"\n\
             llm_base_url = \"https://opencode.ai/zen/v1\"\n\
             llm_api_key = \"sk-toml-key\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();

        let auth = cfg.provider_auth(ProviderChoice::OpenAiCompat, None);
        assert_eq!(
            auth.optional_api_key().unwrap().expose_secret(),
            "sk-toml-key",
            "root llm_api_key must reach openai-compat auth"
        );
    }

    #[test]
    fn toml_llm_max_concurrency_reaches_provider_config() {
        // `llm_max_concurrency` in config.toml must thread into
        // `ProviderConfig::max_concurrency` so the gateway concurrency cap
        // can be set without an env var.
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "llm_provider = \"openai-compat\"\n\
             llm_model = \"hy3-free\"\n\
             llm_base_url = \"https://example.test/v1\"\n\
             llm_max_concurrency = 3\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(
            cfg.llm_max_concurrency,
            Some(3),
            "toml llm_max_concurrency must load"
        );
        let provider = cfg.llm_provider_config().unwrap().unwrap();
        assert_eq!(
            provider.max_concurrency,
            Some(3),
            "llm_max_concurrency must reach ProviderConfig"
        );
    }

    #[test]
    fn opencode_toml_llm_api_key_reaches_auth() {
        // The `opencode` provider's key normally comes from `OPENCODE_API_KEY`,
        // but a root `llm_api_key` in config.toml must be accepted as a
        // fallback so operators can keep a single key field.
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "llm_provider = \"opencode\"\nllm_api_key = \"sk-toml-key\"\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();

        let auth = cfg.provider_auth(ProviderChoice::OpenCode, None);
        assert_eq!(
            auth.require_api_key().unwrap().expose_secret(),
            "sk-toml-key",
            "root llm_api_key must reach opencode auth when OPENCODE_API_KEY is unset"
        );
    }

    #[test]
    fn openai_compat_provider_threads_strict_flag() {
        let cfg = Config {
            llm_provider: Some("openai-compat".into()),
            llm_model: Some("qwen3:32b".into()),
            llm_base_url: Some("http://localhost:11434/v1".into()),
            llm_compat_strict: true,
            ..Config::default()
        };

        let provider = cfg.llm_provider_config().unwrap().unwrap();

        assert_eq!(provider.provider, ProviderChoice::OpenAiCompat);
        assert_eq!(provider.model, "qwen3:32b");
        assert_eq!(
            provider.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert!(provider.compat_strict);
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
        assert_eq!(provider.model, "gpt-5.4");
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
        assert_eq!(provider.model, "gpt-5.4");
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
        assert_eq!(provider.model, "claude-sonnet-5-0");
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
    fn opencode_provider_resolves_choice_default_model_and_api_key() {
        for spelling in ["opencode", "opencode-zen", "opencode_zen"] {
            let cfg = Config {
                llm_provider: Some(spelling.into()),
                runtime_env: RuntimeEnv {
                    opencode_api_key: Some(SecretString::from("sk-opencode-test")),
                    ..RuntimeEnv::default()
                },
                ..Config::default()
            };

            let provider = cfg.llm_provider_config().unwrap().unwrap();
            assert_eq!(provider.provider, ProviderChoice::OpenCode, "{spelling}");
            assert_eq!(provider.model, "claude-sonnet-4-6", "{spelling}");
            assert_eq!(
                provider.auth.requirement(),
                AuthRequirement::RequiredApiKey {
                    env_var: "OPENCODE_API_KEY"
                },
                "{spelling}"
            );
            assert_eq!(
                provider.auth.require_api_key().unwrap().expose_secret(),
                "sk-opencode-test",
                "{spelling}"
            );
        }
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
