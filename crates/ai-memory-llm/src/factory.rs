//! Provider factory.
//!
//! Maps the user-visible `ProviderChoice` + env config into a
//! concrete `Arc<dyn LlmProvider>`.

use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};

use crate::AnthropicProvider;
use crate::CopilotProvider;
use crate::GeminiProvider;
use crate::OpenAiCompatProvider;
use crate::OpenAiOAuthProvider;
use crate::OpenAiProvider;
use crate::OpenCodeProvider;
use crate::auth::{AuthRequirement, ProviderAuth};
use crate::embedding::{Embedder, OpenAiCompatEmbedder, OpenAiEmbedder, VoyageEmbedder};
use crate::error::{LlmError, LlmResult};
use crate::google::GoogleEmbedder;
use crate::provider::LlmProvider;

/// LLM providers available to ai-memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderChoice {
    /// Anthropic Messages API.
    Anthropic,
    /// OpenAI Chat Completions.
    OpenAi,
    /// Google Gemini (Generative Language API).
    Gemini,
    /// OpenAI-compatible (Ollama / vLLM / LM Studio).
    OpenAiCompat,
    /// OpenAI ChatGPT/Codex OAuth backend.
    OpenAiOAuth,
    /// GitHub Copilot Chat backend.
    Copilot,
    /// Anthropic Messages API via a Claude-subscription OAuth token.
    AnthropicOAuth,
    /// OpenCode Zen/Go cloud API (OpenAI-compatible endpoint).
    OpenCode,
}

impl ProviderChoice {
    /// Wire-format provider name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::OpenAiCompat => "openai-compat",
            Self::OpenAiOAuth => "openai-oauth",
            Self::Copilot => "copilot",
            Self::AnthropicOAuth => "anthropic-oauth",
            Self::OpenCode => "opencode",
        }
    }

    /// Auth requirement for this provider.
    #[must_use]
    pub const fn auth_requirement(self) -> AuthRequirement {
        match self {
            Self::Anthropic => AuthRequirement::RequiredApiKey {
                env_var: "ANTHROPIC_API_KEY",
            },
            Self::OpenAi => AuthRequirement::RequiredApiKey {
                env_var: "OPENAI_API_KEY",
            },
            Self::Gemini => AuthRequirement::RequiredApiKey {
                env_var: "GEMINI_API_KEY",
            },
            Self::OpenAiCompat => AuthRequirement::OptionalApiKey {
                env_var: "LLM_API_KEY",
            },
            Self::OpenAiOAuth => AuthRequirement::OpenAiOAuthToken,
            Self::Copilot => AuthRequirement::CopilotToken,
            Self::AnthropicOAuth => AuthRequirement::AnthropicOAuthToken,
            Self::OpenCode => AuthRequirement::RequiredApiKey {
                env_var: "OPENCODE_API_KEY",
            },
        }
    }
}

/// All settings needed to construct one LLM provider instance.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Provider selection.
    pub provider: ProviderChoice,
    /// Model id (`claude-opus-4-7`, `gpt-4o-mini`, `llama3.1:8b`, …).
    pub model: String,
    /// Resolved provider authentication material.
    pub auth: ProviderAuth,
    /// Base URL override (required for OpenAI-compat).
    pub base_url: Option<String>,
    /// Strict mode for the `openai-compat` provider: send
    /// `response_format=json_schema` instead of the tolerant prose-JSON
    /// parser. Enabled by default and ignored by every other provider.
    /// Sourced once from `AI_MEMORY_LLM_COMPAT_STRICT` by `Config::load`.
    pub compat_strict: bool,
    /// Per-request timeout for every chat provider, in seconds.
    /// Sourced once from `AI_MEMORY_LLM_TIMEOUT_SECS` by `Config::load`;
    /// defaults to [`crate::DEFAULT_REQUEST_TIMEOUT_SECS`].
    pub request_timeout_secs: u64,
    /// Optional reasoning / thinking effort. Each provider maps this to
    /// its native request field (OpenAI `reasoning_effort`, OpenRouter
    /// `reasoning`, xAI Grok `reasoning_effort`, Anthropic
    /// `output_config.effort`, Codex `reasoning.effort`). `None` omits
    /// the field so the model default applies. Gemini and Copilot ignore it.
    pub reasoning_effort: Option<crate::ReasoningEffort>,
    /// Operator-supplied HTTP headers sent on every chat request. Gateways
    /// that require a caller-identifying header (OpenCode Zen/Go asks for
    /// `x-opencode-session` and a specific `User-Agent`) are configured
    /// through this rather than per-provider special cases. Parsed once from
    /// `AI_MEMORY_LLM_HEADERS` by `Config::load`; empty by default.
    pub extra_headers: crate::ExtraHeaders,
}

/// Embedding providers available to ai-memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderChoice {
    /// OpenAI Embeddings API.
    OpenAi,
    /// Voyage Embeddings API.
    Voyage,
    /// Google Gemini Embeddings API (`embedContent`).
    Google,
    /// OpenAI-compatible embeddings endpoint (Ollama / LM Studio /
    /// vLLM). Keyless-capable; base URL, model, and dim are required.
    OpenAiCompat,
    /// In-process pure-Rust embeddings (all-MiniLM-L6-v2, 384-dim) —
    /// no API key, no server. Requires the model files under
    /// `<data_dir>/models/` (fetched at serve startup or dropped in
    /// manually; docs/local-embeddings.md).
    #[cfg(feature = "local-embeddings")]
    Local,
}

impl EmbedderChoice {
    /// Wire-format provider name; matches what the `Embedder::provider`
    /// implementations return so the refuse-on-mismatch query lines up.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Voyage => "voyage",
            Self::Google => "google",
            Self::OpenAiCompat => "openai-compat",
            #[cfg(feature = "local-embeddings")]
            Self::Local => "local",
        }
    }
}

/// Settings to build an embedder.
#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    /// Provider selection.
    pub provider: EmbedderChoice,
    /// Model id (e.g. `text-embedding-3-small`).
    pub model: String,
    /// Vector dimensionality. Refused on mismatch with the stored
    /// pages' dim.
    pub dim: u32,
    /// API key. An empty value selects keyless `openai-compat`; hosted
    /// providers require a non-empty key from the CLI configuration boundary.
    pub api_key: SecretString,
    /// Optional base URL override. Required for openai-compat.
    pub base_url: Option<String>,
    /// `<data_dir>/models/` root, required by the `local` provider.
    pub models_dir: Option<std::path::PathBuf>,
    /// True when no provider was configured and `local` was chosen as
    /// the 2.0 default. Best-effort semantics: a defaulted embedder
    /// that cannot fetch or load its model degrades to no-embedder with
    /// a warning instead of refusing to start; an explicitly configured
    /// one still fails hard.
    pub defaulted: bool,
}

/// Construct an `Arc<dyn Embedder>` from the config.
///
/// # Errors
/// Returns [`LlmError::NotConfigured`] for a zero dimension or missing required
/// base URL; propagates HTTP-client construction errors.
pub fn build_embedder(config: EmbedderConfig) -> LlmResult<Arc<dyn Embedder>> {
    if config.dim == 0 {
        return Err(LlmError::NotConfigured(
            "AI_MEMORY_EMBEDDING_DIM must be greater than zero".into(),
        ));
    }
    let arc: Arc<dyn Embedder> = match config.provider {
        EmbedderChoice::OpenAi => {
            let mut e = OpenAiEmbedder::new(config.api_key, config.model, config.dim)?;
            if let Some(url) = config.base_url {
                e = e.with_base_url(url);
            }
            Arc::new(e)
        }
        EmbedderChoice::Voyage => {
            let mut e = VoyageEmbedder::new(config.api_key, config.model, config.dim)?;
            if let Some(url) = config.base_url {
                e = e.with_base_url(url);
            }
            Arc::new(e)
        }
        EmbedderChoice::Google => {
            let mut e = GoogleEmbedder::new(config.api_key, config.model, config.dim)?;
            if let Some(url) = config.base_url {
                e = e.with_base_url(url);
            }
            Arc::new(e)
        }
        EmbedderChoice::OpenAiCompat => {
            let base = config
                .base_url
                .ok_or_else(|| LlmError::NotConfigured("AI_MEMORY_EMBEDDING_BASE_URL".into()))?;
            let api_key = (!config.api_key.expose_secret().is_empty()).then_some(config.api_key);
            Arc::new(OpenAiCompatEmbedder::new(
                base,
                api_key,
                config.model,
                config.dim,
            )?)
        }
        #[cfg(feature = "local-embeddings")]
        EmbedderChoice::Local => {
            let models_dir = config.models_dir.ok_or_else(|| {
                LlmError::NotConfigured("local embeddings need the data dir's models/ root".into())
            })?;
            Arc::new(crate::local::LocalEmbedder::load(&models_dir)?)
        }
    };
    Ok(arc)
}

/// Default dim for known embedding models. Used when the operator omits
/// `AI_MEMORY_EMBEDDING_DIM`. `openai-compat` has no safe default and returns
/// zero; new callers should use [`try_default_embedding_dim`] when accepting
/// that provider.
#[must_use]
pub fn default_embedding_dim(provider: EmbedderChoice, model: &str) -> u32 {
    try_default_embedding_dim(provider, model).unwrap_or(0)
}

/// Return the model-family embedding dimension when ai-memory has a safe
/// default. Self-hosted OpenAI-compatible models require an explicit value.
#[must_use]
pub fn try_default_embedding_dim(provider: EmbedderChoice, model: &str) -> Option<u32> {
    match (provider, model) {
        (EmbedderChoice::OpenAi, "text-embedding-3-small") => Some(1536),
        (EmbedderChoice::OpenAi, "text-embedding-3-large") => Some(3072),
        (EmbedderChoice::OpenAi, _) => Some(1536),
        (EmbedderChoice::Voyage, "voyage-3-large") => Some(1024),
        (EmbedderChoice::Voyage, _) => Some(1024),
        #[cfg(feature = "local-embeddings")]
        (EmbedderChoice::Local, _) => Some(crate::local::LOCAL_DIM),
        (EmbedderChoice::Google, "gemini-embedding-2") => Some(768),
        (EmbedderChoice::Google, "gemini-embedding-001") => Some(768),
        (EmbedderChoice::Google, _) => Some(768),
        (EmbedderChoice::OpenAiCompat, _) => None,
    }
}

/// Layer [`crate::DEFAULT_USER_AGENT`] under the operator's headers so every
/// provider request identifies ai-memory, while an explicit
/// `AI_MEMORY_LLM_HEADERS` entry still wins.
///
/// Copilot is excluded: its client and per-request headers both carry
/// [`crate::COPILOT_USER_AGENT`], the editor-plugin agent GitHub's Copilot
/// API expects, and `ExtraHeaders` replaces rather than appends — so a
/// default here would silently break that provider.
fn with_default_user_agent(
    provider: ProviderChoice,
    mut headers: crate::ExtraHeaders,
) -> crate::ExtraHeaders {
    if provider != ProviderChoice::Copilot {
        headers.set_default(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(crate::DEFAULT_USER_AGENT),
        );
    }
    headers
}

/// Construct an `Arc<dyn LlmProvider>` matching the config.
///
/// # Errors
/// Returns [`LlmError::NotConfigured`] if a required env value (API
/// key, base URL) is missing.
pub fn build_provider(config: ProviderConfig) -> LlmResult<Arc<dyn LlmProvider>> {
    let timeout = config.request_timeout_secs;
    let extra_headers = with_default_user_agent(config.provider, config.extra_headers);
    match config.provider {
        ProviderChoice::Anthropic => {
            let key = config.auth.require_api_key()?;
            Ok(Arc::new(
                AnthropicProvider::new(key, config.model)?
                    .with_timeout_secs(timeout)
                    .with_reasoning_effort(config.reasoning_effort)
                    .with_extra_headers(extra_headers),
            ))
        }
        ProviderChoice::OpenAi => {
            let key = config.auth.require_api_key()?;
            Ok(Arc::new(
                OpenAiProvider::new(key, config.model)?
                    .with_timeout_secs(timeout)
                    .with_reasoning_effort(config.reasoning_effort)
                    .with_extra_headers(extra_headers),
            ))
        }
        ProviderChoice::Gemini => {
            let key = config.auth.require_api_key()?;
            let mut provider = GeminiProvider::new(key, config.model)?;
            if let Some(url) = config.base_url {
                provider = provider.with_base_url(url);
            }
            Ok(Arc::new(
                provider
                    .with_timeout_secs(timeout)
                    .with_extra_headers(extra_headers),
            ))
        }
        ProviderChoice::OpenAiCompat => {
            let base = config
                .base_url
                .ok_or_else(|| LlmError::NotConfigured("LLM_BASE_URL".into()))?;
            Ok(Arc::new(
                OpenAiCompatProvider::new(base, config.auth.optional_api_key(), config.model)?
                    .with_strict(config.compat_strict)
                    .with_timeout_secs(timeout)
                    .with_reasoning_effort(config.reasoning_effort)
                    .with_extra_headers(extra_headers),
            ))
        }
        ProviderChoice::OpenAiOAuth => {
            let path = config.auth.require_openai_oauth_token_file()?.to_path_buf();
            Ok(Arc::new(
                OpenAiOAuthProvider::new(path, config.model)?
                    .with_timeout_secs(timeout)
                    .with_reasoning_effort(config.reasoning_effort)
                    .with_extra_headers(extra_headers),
            ))
        }
        ProviderChoice::Copilot => {
            let auth = config.auth.require_copilot_auth()?;
            Ok(Arc::new(
                CopilotProvider::new(auth, config.model)?
                    .with_timeout_secs(timeout)
                    .with_extra_headers(extra_headers),
            ))
        }
        ProviderChoice::AnthropicOAuth => {
            let token = config.auth.require_anthropic_oauth_token()?;
            let mut provider = AnthropicProvider::new_oauth(token, config.model)?;
            if let Some(url) = config.base_url {
                provider = provider.with_base_url(url);
            }
            Ok(Arc::new(
                provider
                    .with_timeout_secs(timeout)
                    .with_reasoning_effort(config.reasoning_effort)
                    .with_extra_headers(extra_headers),
            ))
        }
        ProviderChoice::OpenCode => {
            let key = config.auth.require_api_key()?;
            Ok(Arc::new(
                OpenCodeProvider::new(key, config.model)?
                    .with_timeout_secs(timeout)
                    .with_reasoning_effort(config.reasoning_effort)
                    .with_extra_headers(extra_headers),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_choices_declare_current_auth_requirements() {
        assert_eq!(
            ProviderChoice::Anthropic.auth_requirement(),
            AuthRequirement::RequiredApiKey {
                env_var: "ANTHROPIC_API_KEY"
            }
        );
        assert_eq!(
            ProviderChoice::OpenAi.auth_requirement(),
            AuthRequirement::RequiredApiKey {
                env_var: "OPENAI_API_KEY"
            }
        );
        assert_eq!(
            ProviderChoice::Gemini.auth_requirement(),
            AuthRequirement::RequiredApiKey {
                env_var: "GEMINI_API_KEY"
            }
        );
        assert_eq!(
            ProviderChoice::OpenAiCompat.auth_requirement(),
            AuthRequirement::OptionalApiKey {
                env_var: "LLM_API_KEY"
            }
        );
        assert_eq!(
            ProviderChoice::OpenAiOAuth.auth_requirement(),
            AuthRequirement::OpenAiOAuthToken
        );
        assert_eq!(
            ProviderChoice::Copilot.auth_requirement(),
            AuthRequirement::CopilotToken
        );
        assert_eq!(
            ProviderChoice::AnthropicOAuth.auth_requirement(),
            AuthRequirement::AnthropicOAuthToken
        );
    }

    /// `reqwest` sends no `User-Agent` unless configured, which left every
    /// ai-memory provider request anonymous.
    #[test]
    fn default_user_agent_is_layered_under_operator_headers() {
        let headers =
            with_default_user_agent(ProviderChoice::OpenCode, crate::ExtraHeaders::default());
        assert_eq!(headers.get("user-agent"), Some(crate::DEFAULT_USER_AGENT));
    }

    #[test]
    fn an_operator_user_agent_wins_over_the_default() {
        let operator = crate::ExtraHeaders::parse(["user-agent: ai-memory-fork/9"]).unwrap();
        let headers = with_default_user_agent(ProviderChoice::OpenAi, operator);
        assert_eq!(headers.get("user-agent"), Some("ai-memory-fork/9"));
    }

    /// GitHub's Copilot API expects the editor-plugin agent; `ExtraHeaders`
    /// replaces rather than appends, so a default here would break it.
    #[test]
    fn copilot_is_left_with_its_editor_plugin_user_agent() {
        let headers =
            with_default_user_agent(ProviderChoice::Copilot, crate::ExtraHeaders::default());
        assert_eq!(headers.get("user-agent"), None);
    }

    /// "Properly identifies itself (no broad user agents)" means naming
    /// ai-memory and a version — never impersonating another client.
    #[test]
    fn default_user_agent_names_ai_memory_with_a_version() {
        let ua = crate::DEFAULT_USER_AGENT;
        assert!(ua.starts_with("ai-memory/"), "{ua}");
        assert!(ua.len() > "ai-memory/".len(), "{ua} carries no version");
        assert!(!ua.contains("opencode"), "{ua} impersonates another client");
    }

    #[test]
    fn missing_required_provider_auth_preserves_error_shape() {
        let cfg = ProviderConfig {
            provider: ProviderChoice::OpenAi,
            model: "gpt-4o-mini".into(),
            auth: ProviderAuth::required_api_key_from_env("OPENAI_API_KEY", None),
            base_url: None,
            compat_strict: false,
            request_timeout_secs: crate::DEFAULT_REQUEST_TIMEOUT_SECS,
            reasoning_effort: None,
            extra_headers: crate::ExtraHeaders::default(),
        };

        let err = match build_provider(cfg) {
            Ok(_) => panic!("provider should fail without OPENAI_API_KEY"),
            Err(err) => err,
        };
        assert!(matches!(err, LlmError::NotConfigured(msg) if msg == "OPENAI_API_KEY"));
    }
}
