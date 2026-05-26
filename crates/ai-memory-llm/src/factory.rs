//! Provider factory.
//!
//! Maps the user-visible `ProviderChoice` + env config into a
//! concrete `Arc<dyn LlmProvider>`.

use std::sync::Arc;

use secrecy::SecretString;

use crate::AnthropicProvider;
use crate::GeminiProvider;
use crate::OpenAiCompatProvider;
use crate::OpenAiProvider;
use crate::embedding::{Embedder, OpenAiEmbedder, VoyageEmbedder};
use crate::error::{LlmError, LlmResult};
use crate::google::GoogleEmbedder;
use crate::openai_oauth::OpenAiOAuthProvider;
use crate::provider::LlmProvider;

/// LLM provider selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderChoice {
    /// Anthropic Messages API.
    Anthropic,
    /// OpenAI Chat Completions (static API key).
    OpenAi,
    /// Google Gemini (Generative Language API).
    Gemini,
    /// OpenAI-compatible (Ollama / vLLM / LM Studio).
    OpenAiCompat,
    /// OpenAI Chat Completions authenticated via OAuth 2.0 PKCE (ChatGPT
    /// subscription). Token is loaded from `token_path` in `ProviderConfig`;
    /// refresh is on-demand before each request. Run `ai-memory auth login`
    /// to obtain the initial token.
    OpenAiOAuth,
}

/// All settings needed to construct one of the providers.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    /// Provider selection.
    pub provider: ProviderChoice,
    /// Model id (`claude-opus-4-7`, `gpt-4o-mini`, `llama3.1:8b`, …).
    pub model: String,
    /// API key. Required for Anthropic + OpenAI; optional for compat.
    pub api_key: Option<SecretString>,
    /// Base URL override (required for OpenAI-compat).
    pub base_url: Option<String>,
    /// Path to the OAuth token file. Required for `OpenAiOAuth`; ignored
    /// by all other providers.
    pub token_path: Option<std::path::PathBuf>,
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
            let key = config
                .api_key
                .ok_or_else(|| LlmError::NotConfigured("ANTHROPIC_API_KEY".into()))?;
            Ok(Arc::new(AnthropicProvider::new(key, config.model)?))
        }
        ProviderChoice::OpenAi => {
            let key = config
                .api_key
                .ok_or_else(|| LlmError::NotConfigured("OPENAI_API_KEY".into()))?;
            Ok(Arc::new(OpenAiProvider::new(key, config.model)?))
        }
        ProviderChoice::Gemini => {
            let key = config
                .api_key
                .ok_or_else(|| LlmError::NotConfigured("GEMINI_API_KEY".into()))?;
            Ok(Arc::new(GeminiProvider::new(key, config.model)?))
        }
        ProviderChoice::OpenAiCompat => {
            let base = config
                .base_url
                .ok_or_else(|| LlmError::NotConfigured("LLM_BASE_URL".into()))?;
            Ok(Arc::new(OpenAiCompatProvider::new(
                base,
                config.api_key,
                config.model,
            )?))
        }
        ProviderChoice::OpenAiOAuth => {
            let token_path = config.token_path.ok_or_else(|| {
                LlmError::NotConfigured(
                    "openai-oauth requires a token_path (run `ai-memory auth login`)".into(),
                )
            })?;
            Ok(Arc::new(OpenAiOAuthProvider::new(
                token_path,
                config.model,
            )?))
        }
    }
}
