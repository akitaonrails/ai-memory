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

/// Config-file representation of retention settings.
///
/// The breadth coefficient lives here rather than expanding the public
/// `ai_memory_store::DecayParams` struct, preserving source compatibility for
/// downstream Rust callers that construct that struct directly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct DecaySettings {
    /// Per-day decay rate.
    pub lambda: f64,
    /// Access-reinforcement magnitude.
    pub sigma: f64,
    /// Per-day decay of access reinforcement.
    pub mu: f64,
    /// Default page salience.
    pub salience_default: f64,
    /// Wiki-backed eviction threshold.
    pub cold_threshold: f64,
    /// Grace period before permanently deleting an evicted version chain.
    pub hard_delete_after_days: i64,
    /// Optional weight for the number of distinct authenticated readers.
    pub breadth_weight: f64,
    /// Age past which a consolidated session's raw observations may be pruned.
    /// `0` disables the pass; nothing is deleted until an operator opts in.
    pub observation_retention_days: i64,
    /// Observation rows deleted per prune transaction.
    pub observation_prune_batch: usize,
}

impl Default for DecaySettings {
    fn default() -> Self {
        let base = ai_memory_store::DecayParams::default();
        Self {
            lambda: base.lambda,
            sigma: base.sigma,
            mu: base.mu,
            salience_default: base.salience_default,
            cold_threshold: base.cold_threshold,
            hard_delete_after_days: base.hard_delete_after_days,
            breadth_weight: 0.0,
            observation_retention_days: 0,
            observation_prune_batch: ai_memory_consolidate::DEFAULT_OBSERVATION_PRUNE_BATCH,
        }
    }
}

impl DecaySettings {
    /// Retention coefficients consumed by existing store/consolidation APIs.
    #[must_use]
    pub fn decay_params(self) -> ai_memory_store::DecayParams {
        ai_memory_store::DecayParams {
            lambda: self.lambda,
            sigma: self.sigma,
            mu: self.mu,
            salience_default: self.salience_default,
            cold_threshold: self.cold_threshold,
            hard_delete_after_days: self.hard_delete_after_days,
        }
    }

    /// Opt-in observation prune bound consumed by the M8 sweep.
    ///
    /// Deliberately separate from [`Self::decay_params`]: keeping it out of the
    /// public `DecayParams` struct is what lets every downstream Rust caller
    /// that builds one directly keep compiling — and keep today's behaviour.
    #[must_use]
    pub fn observation_retention(self) -> ai_memory_consolidate::ObservationRetention {
        ai_memory_consolidate::ObservationRetention {
            days: self.observation_retention_days,
            batch: self.observation_prune_batch,
        }
    }
}

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
    /// Send `response_format=json_schema` (strict) to the `openai-compat`
    /// provider instead of relying on prose instructions and extracting the
    /// first balanced object. On by default because every structured call
    /// already supplies its own schema. Set
    /// `AI_MEMORY_LLM_COMPAT_STRICT=false` for an incompatible endpoint.
    pub llm_compat_strict: bool,
    /// Per-request timeout (seconds) applied to every chat
    /// completion request and to the Copilot token exchange; the
    /// openai-oauth token refresh keeps the built-in default ceiling
    /// (it is a quick grant exchange). Defaults to
    /// `ai_memory_llm::DEFAULT_REQUEST_TIMEOUT_SECS` (300s), which
    /// tolerates a local engine cold-loading a large model. Raise it
    /// for slow hosted gateways whose long completions exceed the
    /// ceiling (observed with free aggregator tiers). Set with
    /// `AI_MEMORY_LLM_TIMEOUT_SECS`.
    pub llm_timeout_secs: u64,
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
    /// Strip root-level `anyOf`/`oneOf`/`allOf` from MCP tool input
    /// schemas (e.g. `memory_read_page`'s "exactly one of path/query"
    /// contract) on every `tools/list`, regardless of client or `?flavor=`
    /// marker. Moonshot and Bedrock reject root combinators with a 400, and
    /// generic MCP clients (OpenCode, Cursor) never send the flavor marker —
    /// so operators routing through a strict upstream can opt in here.
    /// Runtime "exactly one of" validation is unchanged and remains the
    /// enforcement backstop (issue #412). Set with
    /// `AI_MEMORY_STRIP_ROOT_COMBINATORS=true` or `strip_root_combinators = true`
    /// in config.toml.
    pub strip_root_combinators: bool,
    /// Serve MCP tool input schemas in the subset Google's `Schema`
    /// (Vertex/Gemini `functionDeclaration.parameters`) accepts: the nullable
    /// unions `schemars` emits for every optional argument collapse to a single
    /// `type` plus `nullable: true`. Vertex rejects the union outright once a
    /// client forwards it verbatim — "specified other fields alongside any_of"
    /// — and fails the whole session at `tools/list`. Gemini CLI and
    /// Antigravity CLI normalize schemas client-side and need nothing; this is
    /// for pass-through clients such as OpenCode on a Gemini/Vertex model.
    /// Implies `strip_root_combinators`. Runtime validation is unchanged. Set
    /// with `AI_MEMORY_GEMINI_SAFE_SCHEMAS=true` or
    /// `gemini_safe_schemas = true` in config.toml; the per-request
    /// `?flavor=gemini` marker on the MCP URL is the equivalent opt-in for one
    /// client.
    pub gemini_safe_schemas: bool,
    /// Opt-in post-RRF reranker for `memory_query`. Only `"llm"` is
    /// supported: LLM-as-judge over the configured LLM provider, so it
    /// requires `AI_MEMORY_LLM_PROVIDER` too. Off by default — it puts
    /// an LLM call on the search hot path, trading latency for recall
    /// at the top of the ranking. On any error or timeout the query
    /// preserves the fused, authority-adjusted order. Set with
    /// `AI_MEMORY_RERANKER=llm`.
    pub reranker: Option<String>,
    /// Optional embedding provider (`openai`, `voyage`, `google` / `gemini`,
    /// or `openai-compat`).
    pub embedding_provider: Option<String>,
    /// Optional embedding model override.
    pub embedding_model: Option<String>,
    /// Optional embedding dimension override.
    pub embedding_dim: Option<u32>,
    /// Optional embedding base URL override.
    pub embedding_base_url: Option<String>,
    /// M8 retention-sweep parameters. The defaults give an ~80-day
    /// "survival floor" for unused episodic content (above the cold
    /// threshold), followed by ~180 days of tombstone grace before permanent
    /// version-chain deletion. Tune `decay.lambda` down to slow decay or
    /// `decay.cold_threshold` to evict more / less aggressively.
    pub decay: DecaySettings,
    /// Server-side scheduled maintenance. Jobs run outside hook latency.
    pub maintenance: MaintenanceSettings,
    /// Memory-slot behaviour.
    pub slots: SlotSettings,
    /// LLM consolidation prompt limits. Defaults are sized for a model with a
    /// 200k-token context window.
    pub consolidation: ConsolidationSettings,
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
    /// `[routing]` — how mid-session events whose cwd moved are attributed.
    /// Default `follow-cwd` preserves the historical per-event resolution;
    /// `sticky` keeps the session's project. See [`RoutingSettings`].
    pub routing: RoutingSettings,
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
    embedding_api_key: Option<SecretString>,
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
            // The embedding counterpart of LLM_API_KEY: it credentials the
            // embedding role alone, so the embedder can target a different
            // provider than the chat model instead of borrowing its key.
            embedding_api_key: env_secret("EMBEDDING_API_KEY"),
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
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// Shared bearer token. When set, all HTTP routes require
    /// `Authorization: Bearer <token>`. Generate one with
    /// `ai-memory generate-auth-token`.
    pub bearer_token: Option<String>,
    /// Mark the browser session cookie `Secure`. Enable this only when a
    /// trusted reverse proxy terminates HTTPS for `/web`; direct HTTP browsers
    /// deliberately will not send a Secure cookie.
    pub secure_cookie: bool,
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
    /// OIDC issuer for the root operator when a trusted proxy asserts stable
    /// identities. Configure together with [`Self::root_subject`].
    pub root_issuer: Option<String>,
    /// OIDC subject for the root operator. Configure together with
    /// [`Self::root_issuer`]; only this pair can grant a proxy-authenticated
    /// request root capability. A display username is not a stable root key.
    pub root_subject: Option<String>,
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
    /// Dedicated bearer token for a trusted authenticating proxy, allowing it
    /// to name the real end user in `X-Memory-Actor-*` headers.
    ///
    /// A proxy that terminates SSO usually cannot forward the user's own
    /// credential upstream. This token must differ from [`Self::bearer_token`]
    /// so an omitted or malformed identity cannot fall through as root.
    /// Actor headers on ordinary root and DB-user requests are ignored.
    ///
    /// Only set this when the server is reachable *only* through that proxy.
    pub actor_proxy_bearer_token: Option<String>,
}

impl std::fmt::Debug for AuthSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSettings")
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .field("secure_cookie", &self.secure_cookie)
            .field("root_username", &self.root_username)
            .field("root_issuer", &self.root_issuer)
            .field("root_subject", &self.root_subject)
            .field("root_email", &self.root_email)
            .field("root_name", &self.root_name)
            .field(
                "token_pepper",
                &self.token_pepper.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "actor_proxy_bearer_token",
                &self.actor_proxy_bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
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

/// `[routing]` section of `config.toml`.
///
/// Set under `[routing]` in `config.toml` or via the
/// `AI_MEMORY_ROUTING__MID_SESSION` env var.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingSettings {
    /// `follow-cwd` (default) or `sticky`. See
    /// [`ai_memory_core::MidSessionRouting`] for full semantics.
    pub mid_session: ai_memory_core::MidSessionRouting,
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
            llm_compat_strict: true,
            llm_timeout_secs: ai_memory_llm::DEFAULT_REQUEST_TIMEOUT_SECS,
            consolidate_on_session_end: false,
            capture_assistant: false,
            strip_root_combinators: false,
            gemini_safe_schemas: false,
            reranker: None,
            embedding_provider: None,
            embedding_model: None,
            embedding_dim: None,
            embedding_base_url: None,
            decay: DecaySettings::default(),
            maintenance: MaintenanceSettings::default(),
            slots: SlotSettings::default(),
            consolidation: ConsolidationSettings::default(),
            auto_improve: AutoImproveSettings::default(),
            sanitize: ai_memory_core::SanitizeConfig::default(),
            auth: AuthSettings::default(),
            auto_scope: AutoScopeSettings::default(),
            routing: RoutingSettings::default(),
            hook_rate_per_sec: 0.0,
            hook_rate_burst: 0.0,
            allowed_hosts: vec!["localhost".into(), "127.0.0.1".into(), "::1".into()],
            cors_allow_origins: Vec::new(),
            admission_webhooks: Vec::new(),
            runtime_env: RuntimeEnv::default(),
        }
    }
}

/// `[consolidation]` LLM consolidation prompt sizing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsolidationSettings {
    /// Approximate token target for the entire consolidation prompt:
    /// observation dump, current page body, system prompt, page conventions,
    /// slot snapshots, and the structured-output schema.
    ///
    /// The exact tokenizer is provider-specific, so leave headroom. This value
    /// plus [`Self::max_output_tokens`] must fit the model's context window.
    pub max_input_tokens: usize,
    /// Maximum tokens the provider may generate for a consolidation response.
    /// Small-context models must lower this together with `max_input_tokens`.
    pub max_output_tokens: u32,
}

impl Default for ConsolidationSettings {
    fn default() -> Self {
        Self {
            max_input_tokens: ai_memory_consolidate::DEFAULT_CONSOLIDATION_MAX_INPUT_TOKENS,
            max_output_tokens: ai_memory_consolidate::DEFAULT_CONSOLIDATION_MAX_OUTPUT_TOKENS,
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

/// `[slots]` memory-slot behaviour.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SlotSettings {
    /// Namespace engine-written slots under the operator that produced them
    /// (`_slots/u-alice/current-focus.md` instead of
    /// `_slots/current-focus.md`). The segment is the operator's
    /// `IdentityKey::path_segment()` — `u-<name>` for lowercase safe usernames
    /// and a bounded deterministic identifier for mixed-case, Windows-unsafe,
    /// or otherwise path-hostile usernames and complete OIDC issuer/subject
    /// pairs — never a raw OIDC value.
    ///
    /// Off by default, so nothing changes for an existing install: with the
    /// flag off a nested slot path carries no ownership meaning at all, and
    /// every slot goes into every brief exactly as it did before.
    ///
    /// Turning it ON changes reads and writes, in both directions:
    ///
    /// * a session brief and the consolidation prompt see the shared slots
    ///   plus the requesting operator's own — so a slot already stored under
    ///   `_slots/<segment>/…` becomes visible to that operator alone;
    /// * writing into another operator's namespace is refused (admins aside);
    /// * a write naming the SHARED slot is namespaced into the writer's own
    ///   prefix, whether it comes from the engine or from `memory_write_page`.
    ///
    /// What the flag scopes is INJECTION, not access: an exact-path read
    /// still returns anyone's slot, like any other page.
    ///
    /// Turning it back OFF restores the pre-feature rule everywhere: personal
    /// slots become visible to everyone again and nested writes stop being
    /// gated. Un-namespaced slots are shared under either setting, so nothing
    /// already stored is ever hidden or reinterpreted.
    ///
    /// Only meaningful once requests carry distinct identities; with a single
    /// shared credential every slot lands under the same namespace.
    pub per_user: bool,
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

        if !config.decay.breadth_weight.is_finite() || config.decay.breadth_weight < 0.0 {
            anyhow::bail!(
                "decay.breadth_weight must be a finite number greater than or equal to zero"
            );
        }

        // Fail closed at load rather than at 3am inside a destructive pass: a
        // negative age would be a nonsensical cutoff, and a zero batch would
        // spin the prune loop forever without deleting anything.
        if config.decay.observation_retention_days < 0 {
            anyhow::bail!(
                "decay.observation_retention_days must be greater than or equal to zero \
                 (0 disables observation pruning)"
            );
        }
        if config.decay.observation_prune_batch == 0 {
            anyhow::bail!("decay.observation_prune_batch must be greater than zero");
        }

        // Fail at startup rather than shipping a prompt that is all scaffolding
        // and no observations: below this floor the fixed system prompt and page
        // conventions consume the entire budget, so every consolidation would
        // either be evidence-free or rejected by the provider.
        let min_input_tokens = ai_memory_consolidate::MIN_CONSOLIDATION_MAX_INPUT_TOKENS;
        if config.consolidation.max_input_tokens < min_input_tokens {
            anyhow::bail!(
                "consolidation.max_input_tokens must be at least {min_input_tokens} \
                 (got {}); below that the system prompt and page conventions leave \
                 no room for observations",
                config.consolidation.max_input_tokens
            );
        }
        let min_output_tokens = ai_memory_consolidate::MIN_CONSOLIDATION_MAX_OUTPUT_TOKENS;
        if config.consolidation.max_output_tokens < min_output_tokens {
            anyhow::bail!(
                "consolidation.max_output_tokens must be at least {min_output_tokens} \
                 (got {}); below that a structured consolidation response is unlikely to fit",
                config.consolidation.max_output_tokens
            );
        }
        // Zero (or a sub-second remainder rounded down) would cut every
        // provider request off before it is sent.
        if config.llm_timeout_secs == 0 {
            anyhow::bail!(
                "llm_timeout_secs must be at least 1 second (got {}); \
                 AI_MEMORY_LLM_TIMEOUT_SECS is read in seconds",
                config.llm_timeout_secs
            );
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
                ProviderChoice::Gemini => "gemini-3.5-flash".to_string(),
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
            request_timeout_secs: self.llm_timeout_secs,
        }))
    }

    /// OpenAI-compatible embedding key. `EMBEDDING_API_KEY` is checked first
    /// so an operator can point the embedder at one provider while the LLM
    /// uses another; without it, direct OpenAI keeps requiring
    /// `OPENAI_API_KEY` and a custom embedding base URL may reuse
    /// `LLM_API_KEY` for gateways such as OpenRouter.
    fn openai_embedding_api_key(&self) -> LlmResult<SecretString> {
        if let Some(key) = self.runtime_env.embedding_api_key.clone() {
            return Ok(key);
        }
        if let Some(key) = self.runtime_env.openai_api_key.clone() {
            return Ok(key);
        }
        if non_empty(self.embedding_base_url.as_deref()).is_some() {
            if let Some(key) = self.runtime_env.llm_api_key.clone() {
                return Ok(key);
            }
            return Err(LlmError::NotConfigured(
                "EMBEDDING_API_KEY, OPENAI_API_KEY or LLM_API_KEY required for \
                 openai-compatible embeddings"
                    .into(),
            ));
        }
        Err(LlmError::NotConfigured(
            "EMBEDDING_API_KEY or OPENAI_API_KEY".into(),
        ))
    }

    /// Whether the operator opted into post-RRF reranking.
    ///
    /// Unknown values are rejected loudly at *startup* rather than
    /// silently disabling the feature — a typo'd `AI_MEMORY_RERANKER`
    /// should not look like "reranking is on" in the operator's head
    /// while eligible queries keep their normal ranking.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] for any value other than
    /// `llm` (case-insensitive) or empty.
    pub fn reranker_choice(&self) -> LlmResult<bool> {
        match non_empty(self.reranker.as_deref()).map(str::to_ascii_lowercase) {
            None => Ok(false),
            Some(v) if v == "llm" => Ok(true),
            Some(other) => Err(LlmError::NotConfigured(format!(
                "AI_MEMORY_RERANKER={other} is not supported (only `llm`)"
            ))),
        }
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
            "openai-compat" | "openai_compat" => EmbedderChoice::OpenAiCompat,
            other => {
                return Err(LlmError::NotConfigured(format!(
                    "AI_MEMORY_EMBEDDING_PROVIDER={other} not one of \
                     openai|voyage|google|gemini|openai-compat"
                )));
            }
        };
        let model = match non_empty(self.embedding_model.as_deref()) {
            Some(s) => s.to_string(),
            None => match provider {
                EmbedderChoice::OpenAi => "text-embedding-3-small".to_string(),
                EmbedderChoice::Voyage => "voyage-3".to_string(),
                EmbedderChoice::Google => ai_memory_llm::GOOGLE_DEFAULT_EMBED_MODEL.to_string(),
                EmbedderChoice::OpenAiCompat => {
                    return Err(LlmError::NotConfigured(
                        "AI_MEMORY_EMBEDDING_MODEL must be set explicitly for openai-compat \
                         (no safe default for self-hosted engines)"
                            .into(),
                    ));
                }
            },
        };
        let dim = match self.embedding_dim {
            Some(0) => {
                return Err(LlmError::NotConfigured(
                    "AI_MEMORY_EMBEDDING_DIM must be greater than zero".into(),
                ));
            }
            Some(d) => d,
            None => {
                ai_memory_llm::try_default_embedding_dim(provider, &model).ok_or_else(|| {
                    LlmError::NotConfigured(
                        "AI_MEMORY_EMBEDDING_DIM must be set explicitly for openai-compat \
                         (self-hosted model dims vary)"
                            .into(),
                    )
                })?
            }
        };
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
            // Keyless engines (Ollama, LM Studio) are the norm; a
            // gateway key rides on EMBEDDING_API_KEY, or on LLM_API_KEY
            // when the chat model shares that gateway.
            EmbedderChoice::OpenAiCompat => self
                .runtime_env
                .embedding_api_key
                .clone()
                .or_else(|| self.runtime_env.llm_api_key.clone())
                .unwrap_or_else(|| SecretString::from(String::new())),
        };
        let base_url = self.embedding_base_url.clone();
        if provider == EmbedderChoice::OpenAiCompat && non_empty(base_url.as_deref()).is_none() {
            return Err(LlmError::NotConfigured(
                "AI_MEMORY_EMBEDDING_BASE_URL required for openai-compat embeddings".into(),
            ));
        }
        Ok(Some(EmbedderConfig {
            provider,
            model,
            dim,
            api_key,
            base_url,
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
    fn auth_settings_debug_redacts_secrets_in_auth_and_config() {
        let auth = AuthSettings {
            bearer_token: Some("bearer-secret-sentinel".into()),
            root_username: Some("operator".into()),
            token_pepper: Some("pepper-secret-sentinel".into()),
            actor_proxy_bearer_token: Some("proxy-secret-sentinel".into()),
            ..AuthSettings::default()
        };
        let config = Config {
            auth: auth.clone(),
            ..Config::default()
        };

        for rendered in [format!("{auth:?}"), format!("{config:?}")] {
            assert!(rendered.contains("root_username: Some(\"operator\")"));
            assert!(rendered.contains("<redacted>"));
            for secret in [
                "bearer-secret-sentinel",
                "pepper-secret-sentinel",
                "proxy-secret-sentinel",
            ] {
                assert!(!rendered.contains(secret), "Debug output exposed {secret}");
            }
        }
    }

    #[test]
    fn defaults_have_canonical_endings() {
        let cfg = Config::default();
        assert!(cfg.data_dir.ends_with("ai-memory"));
        assert_eq!(cfg.bind, DEFAULT_BIND);
        assert_eq!(cfg.server_url, DEFAULT_SERVER_URL);
        assert_eq!(cfg.log_level, "info");
        assert_eq!(
            cfg.llm_timeout_secs,
            ai_memory_llm::DEFAULT_REQUEST_TIMEOUT_SECS
        );
        assert!(!cfg.auth.secure_cookie);
        assert!(cfg.maintenance.enabled);
        assert_eq!(cfg.maintenance.forget_sweep_interval_secs, 86_400);
        assert_eq!(cfg.maintenance.lint_interval_secs, 86_400);
        assert_eq!(cfg.maintenance.embedding_backfill_interval_secs, 0);
        assert_eq!(cfg.decay.breadth_weight, 0.0);
        // Observation pruning must stay OFF by default: an install that never
        // opts in keeps every raw observation it has today.
        assert_eq!(cfg.decay.observation_retention_days, 0);
        assert_eq!(cfg.decay.observation_prune_batch, 5_000);
        assert!(!cfg.decay.observation_retention().is_enabled());
        assert!(!cfg.slots.per_user);
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
    fn load_rejects_destructive_invalid_breadth_weights() {
        for value in ["-0.1", "nan", "inf"] {
            let tmp = TempDir::new().unwrap();
            let config_path = tmp.path().join("config.toml");
            std::fs::write(&config_path, format!("[decay]\nbreadth_weight = {value}\n")).unwrap();
            let error = Config::load(Some(&config_path), Some(tmp.path().to_path_buf()))
                .expect_err("invalid breadth weight must fail closed");
            assert!(
                error.to_string().contains("breadth_weight"),
                "unexpected error for {value}: {error:#}"
            );
        }
    }

    /// A negative retention age or a zero batch is rejected at load, beside
    /// the breadth-weight guard, so a destructive pass can never be configured
    /// into a nonsensical shape.
    #[test]
    fn load_rejects_destructive_invalid_observation_retention() {
        for (key, value) in [
            ("observation_retention_days", "-1"),
            ("observation_prune_batch", "0"),
        ] {
            let tmp = TempDir::new().unwrap();
            let config_path = tmp.path().join("config.toml");
            std::fs::write(&config_path, format!("[decay]\n{key} = {value}\n")).unwrap();
            let error = Config::load(Some(&config_path), Some(tmp.path().to_path_buf()))
                .expect_err("invalid observation retention must fail closed");
            assert!(
                error.to_string().contains(key),
                "unexpected error for {key} = {value}: {error:#}"
            );
        }
    }

    /// The consolidation budget must be big enough to leave room for
    /// observations after the fixed system prompt and page conventions.
    /// Below the floor every consolidation would be evidence-free, so it
    /// fails at startup instead of once per PreCompact.
    #[test]
    fn load_rejects_a_consolidation_budget_below_the_prompt_reserve() {
        let min = ai_memory_consolidate::MIN_CONSOLIDATION_MAX_INPUT_TOKENS;
        for value in [0, 1, min - 1] {
            let tmp = TempDir::new().unwrap();
            let config_path = tmp.path().join("config.toml");
            std::fs::write(
                &config_path,
                format!("[consolidation]\nmax_input_tokens = {value}\n"),
            )
            .unwrap();
            let error = Config::load(Some(&config_path), Some(tmp.path().to_path_buf()))
                .expect_err("an unusable consolidation budget must fail closed");
            assert!(
                error.to_string().contains("consolidation.max_input_tokens"),
                "unexpected error for {value}: {error:#}"
            );
        }
    }

    #[test]
    fn load_rejects_a_consolidation_output_limit_too_small_for_json() {
        let min = ai_memory_consolidate::MIN_CONSOLIDATION_MAX_OUTPUT_TOKENS;
        for value in [0, 1, min - 1] {
            let tmp = TempDir::new().unwrap();
            let config_path = tmp.path().join("config.toml");
            std::fs::write(
                &config_path,
                format!("[consolidation]\nmax_output_tokens = {value}\n"),
            )
            .unwrap();
            let error = Config::load(Some(&config_path), Some(tmp.path().to_path_buf()))
                .expect_err("an unusable consolidation output limit must fail closed");
            assert!(
                error
                    .to_string()
                    .contains("consolidation.max_output_tokens"),
                "unexpected error for {value}: {error:#}"
            );
        }
    }

    /// A small-context provider needs both sides of the context allocation to
    /// survive the config round-trip.
    #[test]
    fn load_accepts_a_small_context_consolidation_budget() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[consolidation]\nmax_input_tokens = 7000\nmax_output_tokens = 1000\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&config_path), Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(cfg.consolidation.max_input_tokens, 7_000);
        assert_eq!(cfg.consolidation.max_output_tokens, 1_000);
    }

    /// `AI_MEMORY_LLM_TIMEOUT_SECS` (figment maps it to this field) exists so
    /// slow hosted gateways can outlive the 300s default; the accepted floor
    /// mirrors that motivation — anything below one second severs every
    /// provider request before it is sent.
    #[test]
    fn load_round_trips_a_custom_llm_request_timeout() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "llm_timeout_secs = 900\n").unwrap();
        let cfg = Config::load(Some(&config_path), Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(cfg.llm_timeout_secs, 900);
    }

    #[test]
    fn load_rejects_a_zero_llm_request_timeout() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "llm_timeout_secs = 0\n").unwrap();
        let error = Config::load(Some(&config_path), Some(tmp.path().to_path_buf()))
            .expect_err("a sub-second timeout must fail closed");
        assert!(
            error.to_string().contains("llm_timeout_secs"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn defaults_bound_consolidation_prompts_for_a_large_context_provider() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config::load(None, Some(tmp.path().to_path_buf())).unwrap();
        assert_eq!(
            cfg.consolidation.max_input_tokens,
            ai_memory_consolidate::DEFAULT_CONSOLIDATION_MAX_INPUT_TOKENS
        );
        assert_eq!(
            cfg.consolidation.max_output_tokens,
            ai_memory_consolidate::DEFAULT_CONSOLIDATION_MAX_OUTPUT_TOKENS
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
    fn reranker_choice_is_explicit_case_insensitive_and_fail_closed() {
        for value in [None, Some(""), Some("  ")] {
            let cfg = Config {
                reranker: value.map(str::to_string),
                ..Config::default()
            };
            assert!(!cfg.reranker_choice().unwrap());
        }
        for value in ["llm", "LLM", " LlM "] {
            let cfg = Config {
                reranker: Some(value.into()),
                ..Config::default()
            };
            assert!(cfg.reranker_choice().unwrap());
        }
        let cfg = Config {
            reranker: Some("cross-encoder".into()),
            ..Config::default()
        };
        let err = cfg.reranker_choice().unwrap_err();
        assert!(err.to_string().contains("AI_MEMORY_RERANKER=cross-encoder"));
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

            [auth]
            secure_cookie = true

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
        assert!(cfg.auth.secure_cookie);
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

        // A dedicated embedding key outranks the borrowed LLM one.
        let cfg = Config {
            runtime_env: RuntimeEnv {
                embedding_api_key: Some(SecretString::from("sk-embed-key")),
                ..cfg.runtime_env.clone()
            },
            ..cfg
        };
        let embedder = cfg.embedder_config().unwrap().unwrap();
        assert_eq!(embedder.api_key.expose_secret(), "sk-embed-key");
    }

    #[test]
    fn openai_embedding_prefers_dedicated_embedding_api_key() {
        // The LLM runs on api.openai.com (OPENAI_API_KEY) while embeddings
        // run somewhere else; without a dedicated key the OpenAI one would
        // be sent to the embedding base URL and rejected.
        let cfg = Config {
            embedding_provider: Some("openai".into()),
            embedding_base_url: Some("https://api.cheap-embeddings.example/v1".into()),
            runtime_env: RuntimeEnv {
                embedding_api_key: Some(SecretString::from("sk-embed-key")),
                openai_api_key: Some(SecretString::from("sk-openai-key")),
                llm_api_key: Some(SecretString::from("sk-llm-key")),
                ..RuntimeEnv::default()
            },
            ..Config::default()
        };

        let embedder = cfg.embedder_config().unwrap().unwrap();
        assert_eq!(embedder.api_key.expose_secret(), "sk-embed-key");

        // It also works without a custom base URL, i.e. against OpenAI
        // itself, where OPENAI_API_KEY was previously the only accepted key.
        let direct = Config {
            embedding_base_url: None,
            ..cfg.clone()
        };
        assert_eq!(
            direct
                .embedder_config()
                .unwrap()
                .unwrap()
                .api_key
                .expose_secret(),
            "sk-embed-key"
        );

        // Absent, the previous precedence is untouched: OPENAI_API_KEY
        // still beats LLM_API_KEY.
        let without = Config {
            runtime_env: RuntimeEnv {
                embedding_api_key: None,
                ..cfg.runtime_env.clone()
            },
            ..cfg
        };
        assert_eq!(
            without
                .embedder_config()
                .unwrap()
                .unwrap()
                .api_key
                .expose_secret(),
            "sk-openai-key"
        );
    }

    #[test]
    fn openai_compat_embedding_is_keyless_and_requires_explicit_settings() {
        // Fully specified, no key: valid (Ollama / LM Studio).
        let cfg = Config {
            embedding_provider: Some("openai-compat".into()),
            embedding_model: Some("nomic-embed-text".into()),
            embedding_dim: Some(768),
            embedding_base_url: Some("http://localhost:11434/v1".into()),
            ..Config::default()
        };
        let embedder = cfg.embedder_config().unwrap().unwrap();
        assert_eq!(embedder.provider, EmbedderChoice::OpenAiCompat);
        assert_eq!(embedder.model, "nomic-embed-text");
        assert_eq!(embedder.dim, 768);
        assert!(embedder.api_key.expose_secret().is_empty());
        assert_eq!(
            embedder.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );

        // A gateway key rides on LLM_API_KEY when present.
        let cfg_with_key = Config {
            runtime_env: RuntimeEnv {
                llm_api_key: Some(SecretString::from("sk-or-key")),
                ..RuntimeEnv::default()
            },
            ..cfg.clone()
        };
        let embedder = cfg_with_key.embedder_config().unwrap().unwrap();
        assert_eq!(embedder.api_key.expose_secret(), "sk-or-key");

        // EMBEDDING_API_KEY takes precedence over that borrowed key, and
        // still leaves the keyless path keyless when it is unset.
        let cfg_with_embedding_key = Config {
            runtime_env: RuntimeEnv {
                embedding_api_key: Some(SecretString::from("sk-embed-key")),
                ..cfg_with_key.runtime_env.clone()
            },
            ..cfg_with_key
        };
        let embedder = cfg_with_embedding_key.embedder_config().unwrap().unwrap();
        assert_eq!(embedder.api_key.expose_secret(), "sk-embed-key");

        // Missing model / dim / base URL each fail closed.
        let missing_model = Config {
            embedding_model: None,
            ..cfg.clone()
        };
        assert!(matches!(
            missing_model.embedder_config().unwrap_err(),
            LlmError::NotConfigured(msg) if msg.contains("AI_MEMORY_EMBEDDING_MODEL")
        ));
        let missing_dim = Config {
            embedding_dim: None,
            ..cfg.clone()
        };
        assert!(matches!(
            missing_dim.embedder_config().unwrap_err(),
            LlmError::NotConfigured(msg) if msg.contains("AI_MEMORY_EMBEDDING_DIM")
        ));
        let zero_dim = Config {
            embedding_dim: Some(0),
            ..cfg.clone()
        };
        assert!(matches!(
            zero_dim.embedder_config().unwrap_err(),
            LlmError::NotConfigured(msg) if msg.contains("greater than zero")
        ));
        let missing_base = Config {
            embedding_base_url: None,
            ..cfg
        };
        assert!(matches!(
            missing_base.embedder_config().unwrap_err(),
            LlmError::NotConfigured(msg) if msg.contains("AI_MEMORY_EMBEDDING_BASE_URL")
        ));
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
        assert!(
            matches!(err, LlmError::NotConfigured(msg) if msg == "EMBEDDING_API_KEY or OPENAI_API_KEY")
        );

        // The error names the dedicated key, and setting it resolves the
        // same configuration.
        let with_embedding_key = Config {
            runtime_env: RuntimeEnv {
                embedding_api_key: Some(SecretString::from("sk-embed-key")),
                ..cfg.runtime_env.clone()
            },
            ..cfg
        };
        assert_eq!(
            with_embedding_key
                .embedder_config()
                .unwrap()
                .unwrap()
                .api_key
                .expose_secret(),
            "sk-embed-key"
        );
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
        assert!(provider.compat_strict);
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
    fn openai_compat_provider_defaults_strict_and_allows_opt_out() {
        let mut cfg = Config {
            llm_provider: Some("openai-compat".into()),
            llm_model: Some("qwen3:32b".into()),
            llm_base_url: Some("http://localhost:11434/v1".into()),
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

        cfg.llm_compat_strict = false;
        let provider = cfg.llm_provider_config().unwrap().unwrap();
        assert!(!provider.compat_strict);
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

#[test]
fn llm_provider_config_passes_base_url_to_gemini() {
    let cfg = Config {
        llm_provider: Some("gemini".into()),
        llm_base_url: Some("http://localhost:9379".into()),
        runtime_env: RuntimeEnv {
            gemini_api_key: Some(SecretString::from("dummy")),
            ..RuntimeEnv::default()
        },
        ..Config::default()
    };
    let provider = cfg.llm_provider_config().unwrap().unwrap();
    assert_eq!(provider.provider, ProviderChoice::Gemini);
    assert_eq!(provider.base_url.as_deref(), Some("http://localhost:9379"));
}

#[test]
fn llm_provider_config_gemini_uses_default_base_url_when_none_provided() {
    let cfg = Config {
        llm_provider: Some("gemini".into()),
        llm_base_url: None, // Explicitly None
        runtime_env: RuntimeEnv {
            gemini_api_key: Some(SecretString::from("dummy")),
            ..RuntimeEnv::default()
        },
        ..Config::default()
    };
    let provider = cfg.llm_provider_config().unwrap().unwrap();
    assert_eq!(provider.provider, ProviderChoice::Gemini);
    assert_eq!(provider.base_url, None);
}
