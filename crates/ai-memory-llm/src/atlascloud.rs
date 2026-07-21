//! Atlas Cloud provider.
//!
//! Thin wrapper around [`OpenAiCompatProvider`] that bakes in Atlas Cloud's
//! OpenAI-compatible chat-completions endpoint and names the provider
//! `"atlascloud"`. Accepts an API key from `ATLASCLOUD_API_KEY`.

use async_trait::async_trait;
use secrecy::SecretString;

use crate::error::LlmResult;
use crate::openai_compat::OpenAiCompatProvider;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse};

/// Public Atlas Cloud OpenAI-compatible base URL.
pub const ATLASCLOUD_BASE_URL: &str = "https://api.atlascloud.ai/v1";

/// Default model when `AI_MEMORY_LLM_MODEL` is not set.
pub const ATLASCLOUD_DEFAULT_MODEL: &str = "qwen/qwen3.5-flash";

/// Atlas Cloud LLM provider.
///
/// Routes through `https://api.atlascloud.ai/v1` using the OpenAI chat
/// completions wire format. Authenticate with `ATLASCLOUD_API_KEY`.
pub struct AtlasCloudProvider {
    inner: OpenAiCompatProvider,
}

impl AtlasCloudProvider {
    /// Construct an Atlas Cloud provider.
    ///
    /// # Errors
    /// Returns a `reqwest::Error` if the HTTP client cannot be built.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> LlmResult<Self> {
        let inner = OpenAiCompatProvider::new(ATLASCLOUD_BASE_URL, Some(api_key), model.into())?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl LlmProvider for AtlasCloudProvider {
    fn name(&self) -> &'static str {
        "atlascloud"
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        self.inner.complete(request).await
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.inner.complete_structured_raw(request, schema).await
    }
}
