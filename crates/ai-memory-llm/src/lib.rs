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
//! * **GitHub Copilot**: GitHub token exchange to a short-lived Copilot API
//!   token, then OpenAI-style Chat Completions with JSON schema format.
//! * **Gemini**: `generationConfig.responseMimeType = "application/json"`
//!   plus `responseSchema` (OpenAPI 3 subset; `$ref`s inlined,
//!   Draft-2020-12 keywords stripped before send).
//! * **OpenAI-compat** (Ollama, vLLM, LM Studio): we ask for
//!   `response_format: { type: "json_object" }` when supported,
//!   otherwise parse the first balanced `{…}` from the text body.
//!   No tenacity-style 8-128s backoff (cognee #2840 lesson).

pub mod anthropic;
pub mod auth;
pub mod copilot;
pub mod embedding;
pub mod error;
pub mod factory;
pub mod gemini;
pub mod google;
pub mod health;
pub mod openai;
pub mod openai_compat;
pub mod openai_oauth;
pub mod provider;
pub mod types;

mod auth_file;
mod text;

pub use anthropic::AnthropicProvider;
pub use auth::{AuthRequirement, CopilotAuth, Credential, CredentialSource, ProviderAuth};
pub use copilot::{
    COPILOT_INTEGRATION_ID, CopilotProvider, CopilotToken, DEFAULT_COPILOT_API_BASE_URL,
    GITHUB_ACCESS_TOKEN_URL, GITHUB_COPILOT_CLIENT_ID, GITHUB_COPILOT_TOKEN_URL,
    GITHUB_DEVICE_CODE_URL,
};
pub use embedding::{Embedder, OpenAiEmbedder, SyntheticEmbedder, VoyageEmbedder, cosine};
pub use error::{LlmError, LlmResult};
pub use factory::{
    EmbedderChoice, EmbedderConfig, ProviderChoice, ProviderConfig, build_embedder, build_provider,
    default_embedding_dim,
};
pub use gemini::GeminiProvider;
pub use google::{DEFAULT_MODEL as GOOGLE_DEFAULT_EMBED_MODEL, GoogleEmbedder};
pub use health::{
    ProviderHealth, ProviderHealthSnapshot, ProviderHealthStatus, ProviderRoleHealthSnapshot,
};
pub use openai::OpenAiProvider;
pub use openai_compat::OpenAiCompatProvider;
pub use openai_oauth::{
    CODEX_CLIENT_ID, CODEX_RESPONSES_URL, OPENAI_OAUTH_AUTH_URL, OPENAI_OAUTH_ISSUER,
    OPENAI_OAUTH_TOKEN_URL, OpenAiOAuthProvider, OpenAiOAuthToken, OpenAiOAuthTokenResponse,
};
pub use provider::{LlmProvider, complete_structured};
pub use types::{ChatMessage, ChatRequest, ChatResponse, Role, Usage};
