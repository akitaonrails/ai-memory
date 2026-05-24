//! Embedding provider abstraction.
//!
//! Three implementations ship in M9:
//!
//! * [`OpenAiEmbedder`] — production: hits OpenAI's `/v1/embeddings`.
//! * [`VoyageEmbedder`] — production: hits Voyage's `/v1/embeddings`.
//! * [`SyntheticEmbedder`] — test-only: deterministic bag-of-words
//!   embedding so integration tests can demonstrate semantic
//!   retrieval without an API key.
//!
//! Future: a local `ort` + `bge-small-en-v1.5` embedder. The trait is
//! generic so dropping it in later doesn't touch consumers.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::openai::normalize_openai_base;

/// Provider-agnostic embedding API.
///
/// Implementations must be `Send + Sync` (the MCP server / hook
/// router stash an `Arc<dyn Embedder>` and use it from any tokio
/// task).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Short identifier (e.g. `openai`, `voyage`, `synthetic`).
    fn provider(&self) -> &'static str;

    /// Model identifier (e.g. `text-embedding-3-small`).
    fn model(&self) -> &str;

    /// Vector dimensionality.
    fn dim(&self) -> u32;

    /// Embed one text. Returns a unit-normalised vector — callers can
    /// dot-product directly to get cosine similarity.
    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>>;
}

/// OpenAI Embeddings API (`text-embedding-3-small` by default, 1536 dim).
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
    dim: u32,
}

impl OpenAiEmbedder {
    /// Construct an embedder.
    ///
    /// # Errors
    /// Propagates any `reqwest::Error` thrown while building the HTTP
    /// client.
    pub fn new(api_key: SecretString, model: impl Into<String>, dim: u32) -> LlmResult<Self> {
        // 120s tolerates a cold-load of the embedding model on Ollama
        // (small model, but still up to ~30s on first request after
        // unload). Subsequent requests with OLLAMA_KEEP_ALIVE warm are
        // sub-second. When the embedder still fails, memory_query
        // degrades gracefully to BM25-only (see server.rs).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url: "https://api.openai.com".into(),
            model: model.into(),
            dim,
        })
    }

    /// Override the base URL (for tests against a wiremock).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[derive(Debug, Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingDatum>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingDatum {
    embedding: Vec<f32>,
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn provider(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> u32 {
        self.dim
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        let url = normalize_openai_base(&self.base_url, "embeddings");
        debug!(url, model = %self.model, "POST openai/embeddings");
        let req = OpenAiEmbeddingRequest {
            input: text,
            model: &self.model,
        };
        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.api_key.expose_secret())
            .json(&req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body: truncate(&body, 1024),
            });
        }
        let parsed: OpenAiEmbeddingResponse = resp.json().await?;
        let first =
            parsed.data.into_iter().next().ok_or_else(|| {
                LlmError::UnexpectedShape("openai response had no data[0]".into())
            })?;
        if first.embedding.len() as u32 != self.dim {
            return Err(LlmError::UnexpectedShape(format!(
                "expected dim {}, got {}",
                self.dim,
                first.embedding.len()
            )));
        }
        Ok(normalise(first.embedding))
    }
}

/// Voyage Embeddings API.
pub struct VoyageEmbedder {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
    dim: u32,
}

impl VoyageEmbedder {
    /// Construct a Voyage embedder.
    ///
    /// # Errors
    /// Propagates the HTTP client construction error.
    pub fn new(api_key: SecretString, model: impl Into<String>, dim: u32) -> LlmResult<Self> {
        // 120s tolerates a cold-load of the embedding model on Ollama
        // (small model, but still up to ~30s on first request after
        // unload). Subsequent requests with OLLAMA_KEEP_ALIVE warm are
        // sub-second. When the embedder still fails, memory_query
        // degrades gracefully to BM25-only (see server.rs).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url: "https://api.voyageai.com".into(),
            model: model.into(),
            dim,
        })
    }

    /// Override the base URL.
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }
}

#[derive(Debug, Serialize)]
struct VoyageRequest<'a> {
    input: [&'a str; 1],
    model: &'a str,
}

#[derive(Debug, Deserialize)]
struct VoyageResponse {
    data: Vec<VoyageDatum>,
}

#[derive(Debug, Deserialize)]
struct VoyageDatum {
    embedding: Vec<f32>,
}

#[async_trait]
impl Embedder for VoyageEmbedder {
    fn provider(&self) -> &'static str {
        "voyage"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> u32 {
        self.dim
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        let url = normalize_openai_base(&self.base_url, "embeddings");
        let req = VoyageRequest {
            input: [text],
            model: &self.model,
        };
        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.api_key.expose_secret())
            .json(&req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body: truncate(&body, 1024),
            });
        }
        let parsed: VoyageResponse = resp.json().await?;
        let first =
            parsed.data.into_iter().next().ok_or_else(|| {
                LlmError::UnexpectedShape("voyage response had no data[0]".into())
            })?;
        if first.embedding.len() as u32 != self.dim {
            return Err(LlmError::UnexpectedShape(format!(
                "expected dim {}, got {}",
                self.dim,
                first.embedding.len()
            )));
        }
        Ok(normalise(first.embedding))
    }
}

/// Deterministic synthetic embedder for tests.
///
/// Each whitespace-separated word in the text adds 1.0 to one
/// dimension chosen by the word's hash. Vectors are unit-normalised.
/// Similar text → similar vectors, which is enough to demonstrate
/// hybrid retrieval beats either ranker alone.
pub struct SyntheticEmbedder {
    dim: u32,
}

impl SyntheticEmbedder {
    /// Construct with the given dimensionality.
    #[must_use]
    pub const fn new(dim: u32) -> Self {
        Self { dim }
    }
}

#[async_trait]
impl Embedder for SyntheticEmbedder {
    fn provider(&self) -> &'static str {
        "synthetic"
    }

    fn model(&self) -> &str {
        "bag-of-words-v1"
    }

    fn dim(&self) -> u32 {
        self.dim
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        let mut v = vec![0.0_f32; self.dim as usize];
        for word in text.split(|c: char| !c.is_alphanumeric()) {
            if word.is_empty() {
                continue;
            }
            let lower = word.to_ascii_lowercase();
            let h = fnv1a(&lower) as usize;
            let idx = h % v.len();
            v[idx] += 1.0;
        }
        Ok(normalise(v))
    }
}

/// Unit-normalise so dot-product equals cosine similarity.
fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// Cosine similarity between two same-dim unit vectors. (= dot product
/// after normalisation.)
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn synthetic_embedder_produces_unit_vectors() {
        let e = SyntheticEmbedder::new(64);
        let v = e.embed("hello world hello").await.unwrap();
        assert_eq!(v.len(), 64);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[tokio::test]
    async fn synthetic_similar_text_has_higher_cosine() {
        let e = SyntheticEmbedder::new(256);
        let a = e
            .embed("retention sweep evicts stale episodic pages")
            .await
            .unwrap();
        let close = e
            .embed("the sweep evicts stale episodic pages")
            .await
            .unwrap();
        let far = e
            .embed("docker compose volumes and bind mounts")
            .await
            .unwrap();
        let s_close = cosine(&a, &close);
        let s_far = cosine(&a, &far);
        assert!(
            s_close > s_far,
            "similar text should cosine higher than unrelated: close={s_close} far={s_far}"
        );
    }

    #[tokio::test]
    async fn synthetic_is_deterministic() {
        let e = SyntheticEmbedder::new(64);
        let a = e.embed("compile not retrieve").await.unwrap();
        let b = e.embed("compile not retrieve").await.unwrap();
        assert_eq!(a, b);
    }
}
