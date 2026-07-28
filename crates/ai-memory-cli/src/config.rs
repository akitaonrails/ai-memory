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
use secrecy::SecretString;
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
    /// Optional LLM base URL override.
    pub llm_base_url: Option<String>,
    /// Opt-in: send `response_format=json_schema` (strict) to the
    /// `openai-compat` provider instead of asking for prose JSON and
    /// extracting the first balanced object. Off by default — the tolerant
    /// parser stays the default for older local engines that ignore
    /// `response_format`. Modern engines (recent Ollama, vLLM, LM Studio,
    /// llama.cpp) honour structured output; this lets the operator opt in.
    /// If the strict raw call fails, the provider falls back to the tolerant
    /// parser. Set with `AI_MEMORY_LLM_COMPAT_STRICT=true`.
    pub llm_compat_strict: bool,
    /// Opt-in: run LLM consolidation on SessionEnd (in addition to the
    /// always-written heuristic session page), when an LLM provider is
    /// configured. Off by default. Provider work is durably queued after the
    /// deterministic session page and handoff, then handled outside the hook
    /// response by one bounded retrying worker. The LLM checkpoint otherwise
    /// happens on PreCompact and via manual `memory_consolidate`. Set with
    /// `AI_MEMORY_CONSOLIDATE_ON_SESSION_END=true`.
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
    /// `[storage]` — which content source-of-truth backend persists page
    /// content. `sqlite` (default) keeps the historical markdown-under-
    /// `wiki/` + SQLite-index pipeline; `outl` stores page content in an
    /// embedded Outl workspace (op-log CRDT) while the SQLite index keeps
    /// serving search/recency/graph in both modes. See [`StorageSettings`].
    pub storage: StorageSettings,
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
    scope_cwd: Option<String>,
    ignore_marker: bool,
    project_strategy: Option<String>,
    claude_code_session_id: Option<String>,
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
    opencode_api_key: Option<SecretString>,
}

impl RuntimeEnv {
    fn from_process() -> Self {
        Self {
            data_dir: env_path("AI_MEMORY_DATA_DIR"),
            home_dir: env_string("AI_MEMORY_HOME").or_else(|| env_string("HOME")),
            server_url: env_string("AI_MEMORY_SERVER_URL"),
            auth_token: env_string("AI_MEMORY_AUTH_TOKEN"),
            host_cwd: env_string("AI_MEMORY_HOST_CWD"),
            scope_cwd: env_string("AI_MEMORY_SCOPE_CWD"),
            // One-invocation escape hatch: run a command against the fallback
            // scope without editing (or leaving) the marker's tree.
            ignore_marker: env_string("AI_MEMORY_IGNORE_MARKER")
                .is_some_and(|value| crate::marker::is_truthy(&value)),
            // Install-wide project strategy, matching what `install-hooks
            // --project-strategy` bakes into the generated hook commands.
            // Consulted only when a marker does not pin one.
            project_strategy: env_string("AI_MEMORY_PROJECT_STRATEGY"),
            claude_code_session_id: env_string("CLAUDE_CODE_SESSION_ID"),
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
            opencode_api_key: env_secret("OPENCODE_API_KEY"),
        }
    }

    /// Host cwd forwarded by the docker wrapper, if present.
    #[must_use]
    pub fn host_cwd(&self) -> Option<&str> {
        self.host_cwd.as_deref()
    }

    /// Container-visible cwd used only for marker discovery.
    #[must_use]
    pub fn scope_cwd(&self) -> Option<&str> {
        self.scope_cwd.as_deref()
    }

    /// Operator home captured by the single config-read path.
    #[must_use]
    pub fn home_dir(&self) -> Option<&str> {
        self.home_dir.as_deref()
    }

    /// Whether `AI_MEMORY_IGNORE_MARKER` asked this invocation to resolve its
    /// scope as if no `.ai-memory.toml` existed.
    #[must_use]
    pub fn ignore_marker(&self) -> bool {
        self.ignore_marker
    }

    /// Install-wide `--project-strategy` default baked into the environment.
    #[must_use]
    pub fn project_strategy(&self) -> Option<&str> {
        self.project_strategy.as_deref()
    }

    /// Claude Code lifecycle session id inherited by an stdio MCP subprocess.
    #[must_use]
    pub fn claude_code_session_id(&self) -> Option<&str> {
        self.claude_code_session_id.as_deref()
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

/// `[storage]` — content backend selection.
///
/// `backend` names either the built-in `"sqlite"` default (markdown
/// wiki + SQLite index) or a registered content-backend adapter (see
/// `ai_memory_store::adapters`). Each adapter reads its own
/// `[storage.<name>]` table — the config layer treats those sections as
/// opaque so new adapters never require edits here. Example:
///
/// ```toml
/// [storage]
/// backend = "outl"
///
/// [storage.outl]
/// workspace_dir = "~/notes"
/// slug_prefix = "ai-memory"
/// mode = "primary"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    /// Which backend persists page content. `"sqlite"` (default) or the
    /// name of a registered adapter.
    pub backend: String,
    /// Per-adapter config tables, keyed by adapter name, passed to the
    /// selected adapter verbatim at boot.
    #[serde(flatten)]
    pub adapters: std::collections::BTreeMap<String, serde_json::Value>,
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            backend: DEFAULT_STORAGE_BACKEND.to_string(),
            adapters: std::collections::BTreeMap::new(),
        }
    }
}

/// Name of the built-in content backend.
pub const DEFAULT_STORAGE_BACKEND: &str = "sqlite";

impl StorageSettings {
    /// Whether the built-in sqlite/wiki pipeline is selected.
    #[must_use]
    pub fn is_default_backend(&self) -> bool {
        self.backend == DEFAULT_STORAGE_BACKEND
    }

    /// Fail fast on inconsistent `[storage]` settings before any
    /// subsystem starts. Adapter-specific validation happens in the
    /// adapter's factory at boot (the config layer doesn't know which
    /// adapters are compiled in).
    pub fn validate(&self) -> Result<(), String> {
        if self.backend.trim().is_empty() {
            return Err("[storage] backend must not be empty".into());
        }
        Ok(())
    }

    /// The selected adapter's raw `[storage.<name>]` section (`Null`
    /// when the operator wrote none).
    #[must_use]
    pub fn adapter_settings(&self) -> serde_json::Value {
        self.adapters
            .get(&self.backend)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
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
            llm_provider: None,
            llm_model: None,
            llm_base_url: None,
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
            storage: StorageSettings::default(),
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

        // Adapter-specific validation (paths, required keys) happens in
        // the selected adapter's factory at boot; here we only reject
        // structurally broken `[storage]` settings.
        config
            .storage
            .validate()
            .map_err(|msg| anyhow::anyhow!(msg))?;

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
            ProviderChoice::OpenCode => self.runtime_env.opencode_api_key.clone(),
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
    fn storage_defaults_to_sqlite() {
        let cfg = Config::default();
        assert!(cfg.storage.is_default_backend());
        assert_eq!(cfg.storage.backend, DEFAULT_STORAGE_BACKEND);
        assert_eq!(cfg.storage.adapter_settings(), serde_json::Value::Null);
        cfg.storage.validate().unwrap();
    }

    #[test]
    fn storage_adapter_section_is_passed_through_opaquely() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
            [storage]
            backend = "outl"

            [storage.outl]
            workspace_dir = "~/notes"
            slug_prefix = "mem"
            mode = "shadow"

            [storage.some-future-adapter]
            anything = ["goes", 42]
            "#,
        )
        .unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        assert!(!cfg.storage.is_default_backend());
        assert_eq!(cfg.storage.backend, "outl");
        let section = cfg.storage.adapter_settings();
        assert_eq!(section["workspace_dir"], "~/notes");
        assert_eq!(section["slug_prefix"], "mem");
        assert_eq!(section["mode"], "shadow");
        // Sections for other (even unknown) adapters load without error
        // — the config layer never interprets them.
        assert!(cfg.storage.adapters.contains_key("some-future-adapter"));
    }

    #[test]
    fn storage_unknown_backend_loads_and_defers_to_registry() {
        // The config layer can't know which adapters are compiled in;
        // an unknown name loads fine and fails at boot with the list of
        // registered adapters.
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(&cfg_path, "[storage]\nbackend = \"nope\"\n").unwrap();
        let cfg = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(cfg.storage.backend, "nope");
        assert_eq!(cfg.storage.adapter_settings(), serde_json::Value::Null);
    }

    #[test]
    fn storage_empty_backend_fails_to_load() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(&cfg_path, "[storage]\nbackend = \"\"\n").unwrap();
        let err = Config::load(Some(&cfg_path), Some(tmp.path().to_path_buf())).unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
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
