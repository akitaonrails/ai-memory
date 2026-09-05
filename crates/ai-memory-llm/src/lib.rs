//! LLM provider abstraction for ai-memory.
//!
//! Each provider ships with a *native, typed*
//! `reqwest`-based client — never a generic gateway. The cognee
//! issue tracker showed that LiteLLM + Instructor silently drop
//! unknown kwargs, which makes the wrapper layer drift away from
//! the provider's wire protocol over time (#2840, #2608, #2782).
//! Our clients deserialise into named structs that `serde` rejects
//! on unknown fields, surfacing breakage immediately.
//!
//! Structured-output strategies:
//!
//! * **Anthropic**: `tools[0]` is set to a single tool whose input
//!   schema we want filled, with `tool_choice = "tool"`. The
//!   model's `tool_use` content block is the structured payload.
//! * **OpenAI**: `response_format = { type: "json_schema", strict: true }`.
//! * **OpenAI OAuth/Codex**: ChatGPT/Codex Responses API with
//!   `text.format = { type: "json_schema", strict: true }`.
//! * **OpenCode Go**: Responses API for GPT-5.6 Luna; OpenAI-compatible Chat
//!   Completions for the rest of the catalogue.
//! * **GitHub Copilot**: GitHub token exchange to a short-lived Copilot API
//!   token, then OpenAI-style Chat Completions with JSON schema format.
//! * **Gemini**: `generationConfig.responseMimeType = "application/json"`
//!   plus `responseSchema` (OpenAPI 3 subset; `$ref`s inlined,
//!   Draft-2020-12 keywords stripped before send).
//! * **OpenAI-compat** (Ollama, vLLM, LM Studio): we ask for
//!   `response_format: { type: "json_object" }` when supported,
//!   otherwise parse the first balanced `{…}` from the text body.
//!   No tenacity-style 8-128s backoff (cognee #2840 lesson).

/// Default per-request timeout applied by every chat provider.
///
/// 300s tolerates Ollama / llama-swap cold-loading a 30B+ model from disk
/// on first request. Once `OLLAMA_KEEP_ALIVE` keeps it warm, subsequent
/// requests return in seconds — but the first one after the model unloaded
/// needs the headroom. Slow hosted gateways that stream long completions
/// may need more; operators override it with `AI_MEMORY_LLM_TIMEOUT_SECS`
/// (read once by `Config::load`, applied to every chat provider).
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 300;

/// `User-Agent` ai-memory sends on provider requests.
///
/// `reqwest` sends no `User-Agent` at all unless one is configured, so every
/// ai-memory provider request used to arrive anonymous. Gateways increasingly
/// require callers to identify themselves — OpenCode asks for a
/// specific agent ("no broad user agents") and reports traffic without one as
/// an unknown client — and an unattributable request is the one a rate
/// limiter throttles first.
///
/// Layered *under* `AI_MEMORY_LLM_HEADERS` by
/// [`factory::build_provider`], so an operator can still override it. The
/// Copilot provider keeps [`copilot::COPILOT_USER_AGENT`] instead: GitHub's
/// Copilot API expects the editor-plugin agent and rejects requests without
/// it.
pub const DEFAULT_USER_AGENT: &str = concat!("ai-memory/", env!("CARGO_PKG_VERSION"));

pub mod anthropic;
pub mod auth;
pub mod copilot;
pub mod embedding;
pub mod error;
pub mod factory;
pub mod fallback;
pub mod gemini;
pub mod google;
pub mod health;
#[cfg(feature = "local-embeddings")]
pub mod local;
pub mod oidc;
pub mod openai;
pub mod openai_compat;
pub mod openai_oauth;
pub mod opencode;
pub mod provider;
pub mod reranker;
pub mod types;

mod auth_file;
mod response;
mod stored_token;
mod text;

pub use anthropic::AnthropicProvider;
pub use auth::{AuthRequirement, CopilotAuth, Credential, CredentialSource, ProviderAuth};
pub use copilot::{
    COPILOT_INTEGRATION_ID, CopilotProvider, CopilotToken, DEFAULT_COPILOT_API_BASE_URL,
    GITHUB_ACCESS_TOKEN_URL, GITHUB_COPILOT_CLIENT_ID, GITHUB_COPILOT_TOKEN_URL,
    GITHUB_DEVICE_CODE_URL,
};
pub use embedding::{
    Embedder, OpenAiCompatEmbedder, OpenAiEmbedder, SyntheticEmbedder, VoyageEmbedder, cosine,
};
pub use error::{LlmError, LlmResult};
pub use factory::{
    EmbedderChoice, EmbedderConfig, ProviderChoice, ProviderConfig, build_embedder, build_provider,
    default_embedding_dim, try_default_embedding_dim,
};
pub use fallback::{CIRCUIT_COOLDOWN, Candidate, FallbackLlmProvider};
pub use gemini::GeminiProvider;
pub use google::{DEFAULT_MODEL as GOOGLE_DEFAULT_EMBED_MODEL, GoogleEmbedder};
pub use health::{
    CandidateHealth, ProviderHealth, ProviderHealthSnapshot, ProviderHealthStatus,
    ProviderRoleHealthSnapshot,
};
#[cfg(feature = "local-embeddings")]
pub use local::{LOCAL_DIM, LOCAL_MODEL, LocalEmbedder, fetch_model, model_present};
pub use oidc::{
    DeviceAuthorizationResponse, OIDC_DEFAULT_SCOPE, OidcDiscovery, OidcExtras, OidcToken,
    OidcTokenResponse, PollOutcome, discover, poll_token_once, refresh_access_token,
    request_device_code,
};
pub use openai::OpenAiProvider;
pub use openai_compat::OpenAiCompatProvider;
pub use openai_oauth::{
    CODEX_CLIENT_ID, CODEX_RESPONSES_URL, OPENAI_OAUTH_AUTH_URL, OPENAI_OAUTH_ISSUER,
    OPENAI_OAUTH_TOKEN_URL, OpenAiExtras, OpenAiOAuthProvider, OpenAiOAuthToken,
    OpenAiOAuthTokenResponse,
};
#[allow(deprecated)]
pub use opencode::OPENCODE_ZEN_BASE_URL;
pub use opencode::{
    OPENCODE_DEFAULT_MODEL, OPENCODE_GO_BASE_URL, OPENCODE_SESSION_HEADER, OpenCodeProvider,
};
pub use provider::{LlmProvider, complete_structured, complete_structured_with_operation_id};
pub use reranker::{LlmReranker, RerankCandidate, RerankScore, Reranker};
pub use stored_token::StoredOAuthToken;
pub use types::{
    ChatMessage, ChatRequest, ChatResponse, ExtraHeaders, LlmOperationId, ReasoningEffort, Role,
    Usage,
};

// Integration tests compile into this crate's test harness instead of a
// separate binary: every test binary is another link and, on macOS and
// Windows, another first-run malware scan. They still exercise only the
// public API; `extern crate self` lets them keep addressing it by crate name.
#[cfg(test)]
extern crate self as ai_memory_llm;
#[cfg(test)]
#[path = "../tests/suite/mod.rs"]
mod integration;
