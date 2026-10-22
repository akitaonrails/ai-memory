//! Google Gemini Embeddings API (`embedContent`).
//!
//! See <https://ai.google.dev/gemini-api/docs/embeddings>.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::embedding::{Embedder, OPENAI_EMBED_MAX_TOKENS, normalise, retry_delay};
use crate::error::{LlmError, LlmResult};
use crate::response::{provider_error, response_json_limited};
use crate::text::truncate_for_embedding;

/// Default Gemini API host.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Default text embedding model (Matryoshka-friendly 768-dim truncation).
pub const DEFAULT_MODEL: &str = "gemini-embedding-001";

/// Gemini / Google Generative Language embeddings.
pub struct GoogleEmbedder {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    /// Wire model id, e.g. `models/gemini-embedding-001`.
    model: String,
    dim: u32,
    /// True when the model id contains `embedding-2` (task prefixes in text).
    embedding_v2: bool,
}

impl GoogleEmbedder {
    /// Construct a Google embedder.
    ///
    /// # Errors
    /// Propagates HTTP client construction errors.
    pub fn new(api_key: SecretString, model: impl Into<String>, dim: u32) -> LlmResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(crate::redirect::same_origin_redirects())
            .timeout(Duration::from_secs(120))
            .build()?;
        let model = normalize_model_id(model.into());
        let embedding_v2 = model.contains("embedding-2");
        Ok(Self {
            client,
            api_key,
            base_url: DEFAULT_BASE_URL.into(),
            model,
            dim,
            embedding_v2,
        })
    }

    /// Override API host (tests).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    async fn embed_with_task(
        &self,
        text: &str,
        task_type: Option<&'static str>,
    ) -> LlmResult<Vec<f32>> {
        let prepared = if self.embedding_v2 {
            match task_type {
                Some("RETRIEVAL_DOCUMENT") => format_document_v2(text),
                Some("RETRIEVAL_QUERY") => format_query_v2(text),
                _ => text.to_string(),
            }
        } else {
            text.to_string()
        };
        // `gemini-embedding-001`, the default, takes at most 2,048 input tokens
        // and the API does not document whether a longer input is cut or
        // refused; `embed_page_version` only logs a refusal, which would leave
        // the page with no vector. The 8 KB head cut stays near that limit for
        // prose. Applied after the embedding-2 prefix so it bounds the whole
        // input. <https://ai.google.dev/gemini-api/docs/embeddings>
        let prepared = truncate_for_embedding(&prepared, OPENAI_EMBED_MAX_TOKENS);

        let url = embed_url(&self.base_url, &self.model);
        let body = GeminiEmbedRequest {
            content: GeminiContent {
                parts: vec![GeminiPart { text: &prepared }],
            },
            task_type: if self.embedding_v2 { None } else { task_type },
            output_dimensionality: Some(self.dim),
        };

        debug!(url, model = %self.model, ?task_type, "POST google/embedContent");
        let mut attempt = 0u32;
        let mut waited = Duration::ZERO;
        loop {
            let resp = self
                .client
                .post(&url)
                .header("x-goog-api-key", self.api_key.expose_secret())
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER);
            if let Some(delay) = retry_delay(status, retry_after, attempt + 1, waited) {
                attempt += 1;
                waited += delay;
                debug!(
                    attempt,
                    ?delay,
                    status = status.as_u16(),
                    "google embedContent 429/503; retrying"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
            if !status.is_success() {
                return Err(provider_error(resp).await);
            }
            let parsed: GeminiEmbedResponse = response_json_limited(resp).await?;
            let values = parsed.embedding.values;
            if values.len() as u32 != self.dim {
                return Err(LlmError::UnexpectedShape(format!(
                    "expected dim {}, got {}",
                    self.dim,
                    values.len()
                )));
            }
            return Ok(normalise(values));
        }
    }
}

#[derive(Debug, Serialize)]
struct GeminiEmbedRequest<'a> {
    content: GeminiContent<'a>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "taskType")]
    task_type: Option<&'a str>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        rename = "outputDimensionality"
    )]
    output_dimensionality: Option<u32>,
}

#[derive(Debug, Serialize)]
struct GeminiContent<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Debug, Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedResponse {
    embedding: GeminiEmbeddingValues,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbeddingValues {
    values: Vec<f32>,
}

#[async_trait]
impl Embedder for GoogleEmbedder {
    fn provider(&self) -> &'static str {
        "google"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> u32 {
        self.dim
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed_document(text).await
    }

    async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed_with_task(text, Some("RETRIEVAL_DOCUMENT")).await
    }

    async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed_with_task(text, Some("RETRIEVAL_QUERY")).await
    }
}

/// Prefix model id with `models/` when omitted.
#[must_use]
pub fn normalize_model_id(model: String) -> String {
    let trimmed = model.trim();
    if trimmed.starts_with("models/") {
        trimmed.to_string()
    } else {
        format!("models/{trimmed}")
    }
}

fn embed_url(base: &str, model: &str) -> String {
    format!(
        "{}/v1beta/{}:embedContent",
        base.trim_end_matches('/'),
        model
    )
}

/// Asymmetric document format for `gemini-embedding-2` (see Google docs).
#[must_use]
pub fn format_document_v2(text: &str) -> String {
    format!("title: none | text: {text}")
}

/// Asymmetric query format for `gemini-embedding-2`.
#[must_use]
pub fn format_query_v2(text: &str) -> String {
    format!("task: search result | query: {text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    #[derive(Clone)]
    struct AssertApiKeyHeader;

    impl Respond for AssertApiKeyHeader {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let api_key = req
                .headers
                .get("x-goog-api-key")
                .and_then(|value| value.to_str().ok());
            if api_key != Some("test-key") {
                return ResponseTemplate::new(500).set_body_string("missing x-goog-api-key header");
            }
            if req.headers.get("authorization").is_some() {
                return ResponseTemplate::new(500)
                    .set_body_string("unexpected authorization header");
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": { "values": [1.0, 0.0, 0.0] }
            }))
        }
    }

    #[test]
    fn normalize_model_adds_prefix() {
        assert_eq!(
            normalize_model_id("gemini-embedding-001".into()),
            "models/gemini-embedding-001"
        );
    }

    #[test]
    fn v2_document_and_query_prefixes() {
        assert!(format_document_v2("hello").contains("text: hello"));
        assert!(format_query_v2("find auth").contains("query: find auth"));
    }

    #[derive(Clone)]
    struct AssertBoundedInput;

    impl Respond for AssertBoundedInput {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: serde_json::Value = match serde_json::from_slice(&req.body) {
                Ok(value) => value,
                Err(err) => {
                    return ResponseTemplate::new(500).set_body_string(format!("bad body: {err}"));
                }
            };
            let text = body["content"]["parts"][0]["text"]
                .as_str()
                .unwrap_or_default();
            if text.len() > 8_000 {
                return ResponseTemplate::new(500).set_body_string(format!(
                    "unbounded embedContent input: {} bytes",
                    text.len()
                ));
            }
            if !text.ends_with('\u{2026}') {
                return ResponseTemplate::new(500)
                    .set_body_string("expected the truncation ellipsis");
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": { "values": [1.0, 0.0, 0.0] }
            }))
        }
    }

    #[tokio::test]
    async fn embed_content_bounds_a_long_input() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-embedding-001:embedContent"))
            .respond_with(AssertBoundedInput)
            .mount(&server)
            .await;

        let embedder =
            GoogleEmbedder::new(SecretString::from("test-key"), "gemini-embedding-001", 3)
                .expect("google embedder builds")
                .with_base_url(server.uri());

        let embedding = embedder
            .embed_document(&"word ".repeat(8_000))
            .await
            .expect("a long page is bounded before the request");

        assert_eq!(embedding, vec![1.0, 0.0, 0.0]);
    }

    #[tokio::test]
    async fn embed_content_uses_api_key_header_not_bearer_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-embedding-001:embedContent"))
            .respond_with(AssertApiKeyHeader)
            .mount(&server)
            .await;

        let embedder =
            GoogleEmbedder::new(SecretString::from("test-key"), "gemini-embedding-001", 3)
                .expect("google embedder builds")
                .with_base_url(server.uri());

        let embedding = embedder
            .embed_document("hello")
            .await
            .expect("embedContent request succeeds with API-key auth");

        assert_eq!(embedding, vec![1.0, 0.0, 0.0]);
    }
}
