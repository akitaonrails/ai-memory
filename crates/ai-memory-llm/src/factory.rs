//! Provider factory.
//!
//! Maps the user-visible `ProviderChoice` + env config into a
//! concrete `Arc<dyn LlmProvider>`.

use std::sync::Arc;

use secrecy::SecretString;

use crate::AnthropicProvider;
use crate::CopilotProvider;
use crate::GeminiProvider;
use crate::OpenAiCompatProvider;
use crate::OpenAiOAuthProvider;
use crate::OpenAiProvider;
use crate::auth::{AuthRequirement, ProviderAuth};
use crate::embedding::{Embedder, OpenAiEmbedder, VoyageEmbedder};
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
    /// API key.
    pub api_key: SecretString,
    /// Optional base URL override.
    pub base_url: Option<String>,
}

/// Construct an `Arc<dyn Embedder>` from the config.
///
/// # Errors
/// Propagates HTTP-client construction errors.
pub fn build_embedder(config: EmbedderConfig) -> LlmResult<Arc<dyn Embedder>> {
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
    };
    Ok(arc)
}

/// Default dim for known embedding models. Used when the operator
/// omits `AI_MEMORY_EMBEDDING_DIM`. Falls back to a model-family
/// default; unknown models still require an explicit dim.
#[must_use]
pub fn default_embedding_dim(provider: EmbedderChoice, model: &str) -> u32 {
    match (provider, model) {
        (EmbedderChoice::OpenAi, "text-embedding-3-small") => 1536,
        (EmbedderChoice::OpenAi, "text-embedding-3-large") => 3072,
        (EmbedderChoice::OpenAi, _) => 1536,
        (EmbedderChoice::Voyage, "voyage-3-large") => 1024,
        (EmbedderChoice::Voyage, _) => 1024,
        (EmbedderChoice::Google, "gemini-embedding-2") => 768,
        (EmbedderChoice::Google, "gemini-embedding-001") => 768,
        (EmbedderChoice::Google, _) => 768,
    }
}

/// Construct an `Arc<dyn LlmProvider>` matching the config.
///
/// # Errors
/// Returns [`LlmError::NotConfigured`] if a required env value (API
/// key, base URL) is missing.
pub fn build_provider(config: ProviderConfig) -> LlmResult<Arc<dyn LlmProvider>> {
    match config.provider {
        ProviderChoice::Anthropic => {
            let key = config.auth.require_api_key()?;
            Ok(Arc::new(AnthropicProvider::new(key, config.model)?))
        }
        ProviderChoice::OpenAi => {
            let key = config.auth.require_api_key()?;
            Ok(Arc::new(OpenAiProvider::new(key, config.model)?))
        }
        ProviderChoice::Gemini => {
            let key = config.auth.require_api_key()?;
            Ok(Arc::new(GeminiProvider::new(key, config.model)?))
        }
        ProviderChoice::OpenAiCompat => {
            let base = config
                .base_url
                .ok_or_else(|| LlmError::NotConfigured("LLM_BASE_URL".into()))?;
            Ok(Arc::new(OpenAiCompatProvider::new(
                base,
                config.auth.optional_api_key(),
                config.model,
            )?))
        }
        ProviderChoice::OpenAiOAuth => {
            let path = config.auth.require_openai_oauth_token_file()?.to_path_buf();
            Ok(Arc::new(OpenAiOAuthProvider::new(path, config.model)?))
        }
        ProviderChoice::Copilot => {
            let auth = config.auth.require_copilot_auth()?;
            Ok(Arc::new(CopilotProvider::new(auth, config.model)?))
        }
        ProviderChoice::AnthropicOAuth => {
            let token = config.auth.require_anthropic_oauth_token()?;
            let mut provider = AnthropicProvider::new_oauth(token, config.model)?;
            if let Some(url) = config.base_url {
                provider = provider.with_base_url(url);
            }
            Ok(Arc::new(provider))
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

    #[test]
    fn missing_required_provider_auth_preserves_error_shape() {
        let cfg = ProviderConfig {
            provider: ProviderChoice::OpenAi,
            model: "gpt-4o-mini".into(),
            auth: ProviderAuth::required_api_key_from_env("OPENAI_API_KEY", None),
            base_url: None,
        };

        let err = match build_provider(cfg) {
            Ok(_) => panic!("provider should fail without OPENAI_API_KEY"),
            Err(err) => err,
        };
        assert!(matches!(err, LlmError::NotConfigured(msg) if msg == "OPENAI_API_KEY"));
    }
}
