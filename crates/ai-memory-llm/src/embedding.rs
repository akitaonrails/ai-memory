//! Embedding provider abstraction.
//!
//! Embedding implementations ship in M9:
//!
//! * [`OpenAiEmbedder`] — production: hits OpenAI's `/v1/embeddings`.
//! * [`VoyageEmbedder`] — production: hits Voyage's `/v1/embeddings`.
//! * [`GoogleEmbedder`](crate::google::GoogleEmbedder) — Gemini `embedContent`.
//! * [`SyntheticEmbedder`] — test-only: deterministic bag-of-words
//!   embedding so integration tests can demonstrate semantic
//!   retrieval without an API key.
//!
//! * [`LocalEmbedder`](crate::local::LocalEmbedder) — in-process
//!   pure-Rust BERT (`all-MiniLM-L6-v2`), no key and no server; see
//!   `docs/local-embeddings.md`.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::openai::normalize_openai_base;
use crate::response::{provider_error_body, response_json_limited, response_text_limited};
use crate::text::{truncate_for_embedding, truncate_with_ellipsis};

/// Conservative per-request input cap for OpenAI-compatible embedding APIs
/// (8192 token server limit; we stay well below with head truncation).
pub(crate) const OPENAI_EMBED_MAX_TOKENS: usize = 5000;

/// Provider-agnostic embedding API.
///
/// Implementations must be `Send + Sync` (the MCP server / hook
/// router stash an `Arc<dyn Embedder>` and use it from any tokio
/// task).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Short identifier (e.g. `openai`, `voyage`, `synthetic`).
    fn provider(&self) -> &'static str;

    /// Model identifier (e.g. `text-embedding-3-small`). The exact string
    /// sent on the wire — never suffixed or altered. See
    /// [`Self::model_identity`] for the value that should be stored
    /// alongside a vector or used to select eligible stored vectors.
    fn model(&self) -> &str;

    /// The `model` component of the stored/matched `(provider, model, dim)`
    /// identity for a **document** embedding — distinct from [`Self::model`]
    /// (the wire model name) whenever the effective document-side input
    /// text is not what the model id alone implies. Defaults to
    /// `self.model().to_string()`: providers with no such distinction (every
    /// provider except the two below) are unaffected.
    ///
    /// `OpenAiEmbedder`/`OpenAiCompatEmbedder` override this to fold in a
    /// fingerprint of their configured `document_prefix` when it is
    /// non-empty, so a page embedded under one document prefix is never
    /// silently matched against, or mixed with, vectors embedded under a
    /// different (or no) prefix — the same `(provider, model, dim)`
    /// refuse-on-mismatch and stale-row machinery that already protects
    /// against a plain model swap now also protects against a document
    /// prefix change. An empty prefix reproduces `self.model()` exactly
    /// (the pre-existing identity), so upgrading installs with no prefix
    /// configured need no migration. The **query** prefix never affects
    /// this: only the text actually embedded and stored needs a distinct
    /// identity, and a query is never stored.
    fn model_identity(&self) -> String {
        self.model().to_string()
    }

    /// Vector dimensionality.
    fn dim(&self) -> u32;

    /// Embed one text. Returns a unit-normalised vector — callers can
    /// dot-product directly to get cosine similarity.
    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>>;

    /// Embed wiki page / document body (hybrid index writes).
    async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed(text).await
    }

    /// Embed a search query (hybrid retrieval).
    async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed(text).await
    }
}

/// OpenAI Embeddings API (`text-embedding-3-small` by default, 1536 dim).
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    api_key: SecretString,
    base_url: String,
    model: String,
    dim: u32,
    query_prefix: String,
    document_prefix: String,
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
        // degrades gracefully to FTS5 + entity + graph (see server.rs).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url: "https://api.openai.com".into(),
            model: model.into(),
            dim,
            query_prefix: String::new(),
            document_prefix: String::new(),
        })
    }

    /// Override the base URL (for tests against a wiremock).
    #[must_use]
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Set the query/document prefixes prepended before embedding (see
    /// [`OpenAiCompatEmbedder::with_prefixes`]). Empty strings are a no-op,
    /// so this is safe to call unconditionally with the configured values.
    #[must_use]
    pub fn with_prefixes(
        mut self,
        query_prefix: impl Into<String>,
        document_prefix: impl Into<String>,
    ) -> Self {
        self.query_prefix = query_prefix.into();
        self.document_prefix = document_prefix.into();
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

/// Parse OpenAI-compatible embedding responses, including OpenRouter error bodies.
pub(crate) fn parse_openai_embedding_values(body: &str, status: u16) -> LlmResult<Vec<f32>> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| LlmError::Provider {
        status,
        body: truncate_with_ellipsis(&format!("openai embeddings json: {e}; body={body}"), 1024),
    })?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| err.as_str())
            .unwrap_or("unknown");
        return Err(LlmError::Provider {
            status,
            body: truncate_with_ellipsis(msg, 1024),
        });
    }
    if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
        let first = data.first().ok_or_else(|| LlmError::Provider {
            status,
            body: truncate_with_ellipsis(
                &format!("openai embeddings data[] empty; body={body}"),
                512,
            ),
        })?;
        let emb = first
            .get("embedding")
            .and_then(|e| e.as_array())
            .ok_or_else(|| LlmError::Provider {
                status,
                body: truncate_with_ellipsis(
                    &format!("missing embedding array in data[0]; body={body}"),
                    512,
                ),
            })?;
        let mut out = Vec::with_capacity(emb.len());
        for n in emb {
            let f = n.as_f64().ok_or_else(|| LlmError::Provider {
                status,
                body: truncate_with_ellipsis(
                    &format!("non-numeric embedding value in data[0]; body={body}"),
                    512,
                ),
            })?;
            out.push(f as f32);
        }
        if !out.is_empty() {
            return Ok(out);
        }
    }
    // Strict OpenAI shape fallback.
    let parsed: OpenAiEmbeddingResponse =
        serde_json::from_value(v).map_err(|e| LlmError::Provider {
            status,
            body: truncate_with_ellipsis(
                &format!("openai embeddings shape: {e}; body={body}"),
                1024,
            ),
        })?;
    let first = parsed.data.into_iter().next().ok_or_else(|| {
        LlmError::UnexpectedShape(format!(
            "openai response had no data[0]; body={}",
            truncate_with_ellipsis(body, 512)
        ))
    })?;
    Ok(first.embedding)
}

/// Shared OpenAI-shape embedding request: head-truncate, POST
/// `/embeddings` (bearer auth only when a key is present — local
/// engines such as Ollama and LM Studio run keyless), retry 429 with
/// backoff, parse, dim-check, unit-normalise.
async fn openai_style_embed(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&SecretString>,
    model: &str,
    dim: u32,
    text: &str,
) -> LlmResult<Vec<f32>> {
    let input = truncate_for_embedding(text, OPENAI_EMBED_MAX_TOKENS);
    let url = normalize_openai_base(base_url, "embeddings");
    debug!(
        url,
        model,
        input_chars = input.len(),
        "POST openai/embeddings"
    );
    let req = OpenAiEmbeddingRequest {
        input: &input,
        model,
    };
    let mut attempt = 0u32;
    loop {
        let mut builder = client.post(&url);
        if let Some(key) = api_key {
            builder = builder.bearer_auth(key.expose_secret());
        }
        let resp = builder.json(&req).send().await?;
        let status = resp.status();
        if status.as_u16() == 429 && attempt < 5 {
            attempt += 1;
            let delay = Duration::from_secs(2u64.saturating_pow(attempt));
            debug!(attempt, ?delay, "openai embeddings rate-limited; retrying");
            tokio::time::sleep(delay).await;
            continue;
        }
        // Client errors (e.g. input > 8192 tokens) are not retried.
        if status.as_u16() == 400 {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        if !status.is_success() {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        let body = response_text_limited(resp).await?;
        let values = parse_openai_embedding_values(&body, status.as_u16())?;
        if values.len() as u32 != dim {
            return Err(LlmError::UnexpectedShape(format!(
                "expected dim {}, got {}",
                dim,
                values.len()
            )));
        }
        return Ok(normalise(values));
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn provider(&self) -> &'static str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn model_identity(&self) -> String {
        document_prefix_identity(&self.model, &self.document_prefix)
    }

    fn dim(&self) -> u32 {
        self.dim
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        openai_style_embed(
            &self.client,
            &self.base_url,
            Some(&self.api_key),
            &self.model,
            self.dim,
            text,
        )
        .await
    }

    async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
        let prefixed = prepend_prefix(&self.document_prefix, text);
        openai_style_embed(
            &self.client,
            &self.base_url,
            Some(&self.api_key),
            &self.model,
            self.dim,
            &prefixed,
        )
        .await
    }

    async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
        let prefixed = prepend_prefix(&self.query_prefix, text);
        openai_style_embed(
            &self.client,
            &self.base_url,
            Some(&self.api_key),
            &self.model,
            self.dim,
            &prefixed,
        )
        .await
    }
}

/// OpenAI-compatible embeddings endpoint (Ollama / LM Studio / vLLM /
/// OpenRouter). Same wire shape as [`OpenAiEmbedder`], but the base
/// URL is required, the API key is optional (local engines run
/// keyless), and vectors are stored under the distinct provider label
/// `openai-compat` so the `{provider, model, dim}` refuse-on-mismatch
/// check treats them as their own family.
pub struct OpenAiCompatEmbedder {
    client: reqwest::Client,
    api_key: Option<SecretString>,
    base_url: String,
    model: String,
    dim: u32,
    /// Prepended to every query text before embedding (before truncation).
    /// Empty by default: symmetric models (most OpenAI-compatible servers)
    /// see no behaviour change. Asymmetric models need a query-side
    /// instruction the OpenAI-compatible `/v1/embeddings` wire format has
    /// no field for — the client has to prepend it instead.
    /// `nvidia/Nemotron-3-Embed-1B-BF16` and base E5 models use a simple
    /// `"query: "` / `"passage: "` pair; `e5-mistral-7b-instruct` and
    /// Qwen3-Embedding instead need a full task-instruction string with
    /// different exact spacing each (documents plain for both — see
    /// `ai-memory-cli/src/config.rs`'s `embedding_query_prefix` field doc
    /// comment for the two exact templates). See `with_prefixes`.
    query_prefix: String,
    /// Document-side counterpart of `query_prefix` (e.g. `"passage: "` for
    /// Nemotron-3-Embed / base E5 — not every model needs one).
    document_prefix: String,
}

impl OpenAiCompatEmbedder {
    /// Construct a compat embedder against `base_url`.
    ///
    /// # Errors
    /// Propagates any `reqwest::Error` thrown while building the HTTP
    /// client.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<SecretString>,
        model: impl Into<String>,
        dim: u32,
    ) -> LlmResult<Self> {
        // Same cold-load tolerance as OpenAiEmbedder: first request
        // after an Ollama model unload can take ~30s.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url: base_url.into(),
            model: model.into(),
            dim,
            query_prefix: String::new(),
            document_prefix: String::new(),
        })
    }

    /// Set the strings prepended to query / document text before embedding,
    /// applied ahead of the existing truncation so truncation still bounds
    /// the whole request body (prefix included). Pass empty strings for no
    /// prefix (the default); safe to call unconditionally with a possibly
    /// unset operator setting.
    ///
    /// Publisher-specified prefixes are exact strings, often with a
    /// significant trailing space (e.g. Nemotron-3-Embed / the E5 family
    /// use `"query: "` and `"passage: "`) — callers must not trim them.
    #[must_use]
    pub fn with_prefixes(
        mut self,
        query_prefix: impl Into<String>,
        document_prefix: impl Into<String>,
    ) -> Self {
        self.query_prefix = query_prefix.into();
        self.document_prefix = document_prefix.into();
        self
    }
}

#[async_trait]
impl Embedder for OpenAiCompatEmbedder {
    fn provider(&self) -> &'static str {
        "openai-compat"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn model_identity(&self) -> String {
        document_prefix_identity(&self.model, &self.document_prefix)
    }

    fn dim(&self) -> u32 {
        self.dim
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        openai_style_embed(
            &self.client,
            &self.base_url,
            self.api_key.as_ref(),
            &self.model,
            self.dim,
            text,
        )
        .await
    }

    async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
        let prefixed = prepend_prefix(&self.document_prefix, text);
        openai_style_embed(
            &self.client,
            &self.base_url,
            self.api_key.as_ref(),
            &self.model,
            self.dim,
            &prefixed,
        )
        .await
    }

    async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
        let prefixed = prepend_prefix(&self.query_prefix, text);
        openai_style_embed(
            &self.client,
            &self.base_url,
            self.api_key.as_ref(),
            &self.model,
            self.dim,
            &prefixed,
        )
        .await
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
        // degrades gracefully to FTS5 + entity + graph (see server.rs).
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
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        let parsed: VoyageResponse = response_json_limited(resp).await?;
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

/// Prepend `prefix` to `text` for an asymmetric embedding model, applied
/// BEFORE [`truncate_for_embedding`] so truncation always bounds the whole
/// request body (prefix included) rather than letting a prefix push the
/// text itself past the server's token limit. An empty prefix — the
/// default — returns `text` unchanged and borrowed, so the unset case
/// allocates nothing.
pub(crate) fn prepend_prefix<'a>(prefix: &str, text: &'a str) -> std::borrow::Cow<'a, str> {
    if prefix.is_empty() {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(format!("{prefix}{text}"))
    }
}

/// The `model` component of the stored embedding identity for a document
/// embedder configured with `document_prefix`. An empty prefix returns
/// `model` unchanged — the pre-existing identity, so an install with no
/// document prefix configured needs no migration. A non-empty prefix
/// appends a versioned SHA-256 fingerprint of the prefix bytes: `+dp1-`
/// followed by the first 16 hex characters (64 bits) of
/// `SHA-256(document_prefix)`. Not the raw prefix text itself, which
/// could be long, contain characters awkward in a stored column, or leak
/// an operator's exact instruction string into logs/admin output more
/// than necessary — and not a 32-bit hash (an earlier revision used
/// `fnv1a` truncated to 32 bits, which a search for a same-length ASCII
/// collision found in minutes: `"Document category 5hvhw0: "` and
/// `"Document category 1i4yh8i: "` both fingerprinted to `5c7f37a2`,
/// which would have silently mixed two different prefixes' vectors under
/// one identity — see `document_prefix_identity_does_not_collide_on_the_known_fnv32_pair`
/// below). SHA-256 truncated to 64 bits keeps the collision probability
/// negligible for the small number of prefixes one deployment actually
/// configures over time, without needing the full 256-bit digest in a
/// column meant to stay human-scannable. The `dp1` version tag lets a
/// future change to this scheme be distinguished from today's rather than
/// risking a silent collision with an old identity under a new one.
pub(crate) fn document_prefix_identity(model: &str, document_prefix: &str) -> String {
    if document_prefix.is_empty() {
        return model.to_string();
    }
    let digest = format!("{:x}", Sha256::digest(document_prefix.as_bytes()));
    format!("{model}+dp1-{}", &digest[..16])
}

/// Unit-normalise so dot-product equals cosine similarity.
pub(crate) fn normalise(mut v: Vec<f32>) -> Vec<f32> {
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

/// Cosine similarity between two same-dim unit vectors. (= dot product
/// after normalisation.)
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_prefix_identity_is_unchanged_when_the_prefix_is_empty() {
        // The legacy-identity guarantee: an install with no document
        // prefix configured must key its vectors exactly as before this
        // feature, so it needs no migration.
        assert_eq!(
            document_prefix_identity("nomic-embed-text", ""),
            "nomic-embed-text"
        );
    }

    #[test]
    fn document_prefix_identity_changes_deterministically_with_the_prefix() {
        let base = document_prefix_identity("nomic-embed-text", "passage: ");
        assert_ne!(
            base, "nomic-embed-text",
            "a set prefix must not reuse the legacy identity"
        );
        assert!(base.starts_with("nomic-embed-text+dp1-"));
        // Deterministic: the same (model, prefix) always produces the same
        // identity, so pages embedded in different requests still land
        // under one queryable identity.
        assert_eq!(
            base,
            document_prefix_identity("nomic-embed-text", "passage: ")
        );
    }

    #[test]
    fn document_prefix_identity_distinguishes_different_prefixes() {
        // Two distinct document prefixes on the same model must not
        // collide — otherwise a prefix *change* would look like no change
        // at all to the refuse-on-mismatch / stale-row machinery.
        let a = document_prefix_identity("nomic-embed-text", "passage: ");
        let b = document_prefix_identity("nomic-embed-text", "document: ");
        assert_ne!(a, b);
    }

    /// Regression test for a real collision found in an earlier revision
    /// of `document_prefix_identity`, which truncated `fnv1a` to 32 bits:
    /// `"Document category 5hvhw0: "` and `"Document category 1i4yh8i: "`
    /// both fingerprinted to `5c7f37a2` (verified independently in
    /// Python), which would have silently mixed the two prefixes' vectors
    /// under one stored identity. The SHA-256-based fingerprint here does
    /// not collide on this pair (also independently verified: their first
    /// 16 hex characters are `b32006f5a41f3d24` and `513594f86d5883fd`).
    #[test]
    fn document_prefix_identity_does_not_collide_on_the_known_fnv32_pair() {
        let a = document_prefix_identity("nomic-embed-text", "Document category 5hvhw0: ");
        let b = document_prefix_identity("nomic-embed-text", "Document category 1i4yh8i: ");
        assert_ne!(
            a, b,
            "these two prefixes collided under the old 32-bit fnv1a fingerprint; \
             the new SHA-256-based one must not repeat that collision"
        );
    }

    #[test]
    fn model_identity_defaults_to_model_for_providers_without_prefixes() {
        // Providers that never got the document-prefix override (google,
        // voyage, local, copilot, synthetic) must see byte-identical
        // behaviour: `model_identity` defaults to `model().to_string()`.
        let e = SyntheticEmbedder::new(8);
        assert_eq!(e.model_identity(), e.model());
    }

    #[test]
    fn openai_compat_embedder_model_identity_reflects_the_document_prefix() {
        let unset = OpenAiCompatEmbedder::new("http://localhost:9/v1", None, "nomic-embed-text", 8)
            .expect("embedder builds");
        assert_eq!(unset.model_identity(), "nomic-embed-text");

        let with_prefix =
            OpenAiCompatEmbedder::new("http://localhost:9/v1", None, "nomic-embed-text", 8)
                .expect("embedder builds")
                .with_prefixes("query: ", "passage: ");
        assert_ne!(with_prefix.model_identity(), "nomic-embed-text");
        assert_eq!(
            with_prefix.model_identity(),
            document_prefix_identity("nomic-embed-text", "passage: ")
        );
        // The query prefix must NOT affect the stored identity — only
        // documents are stored; a query is never persisted.
        let query_only =
            OpenAiCompatEmbedder::new("http://localhost:9/v1", None, "nomic-embed-text", 8)
                .expect("embedder builds")
                .with_prefixes("query: ", "");
        assert_eq!(query_only.model_identity(), "nomic-embed-text");
    }

    #[test]
    fn openai_embedder_model_identity_reflects_the_document_prefix() {
        let unset =
            OpenAiEmbedder::new(SecretString::from("k"), "text-embedding-3-small", 1536).unwrap();
        assert_eq!(unset.model_identity(), "text-embedding-3-small");

        let with_prefix =
            OpenAiEmbedder::new(SecretString::from("k"), "text-embedding-3-small", 1536)
                .unwrap()
                .with_prefixes("query: ", "passage: ");
        assert_ne!(with_prefix.model_identity(), "text-embedding-3-small");
    }

    /// Transport-level proof that `OpenAiEmbedder` (not just
    /// `OpenAiCompatEmbedder`, covered in
    /// `tests/suite/openai_compat_embedder.rs`) actually sends the
    /// configured prefix on the wire — `with_prefixes` alone only proves
    /// the fields are stored, not that `embed_query`/`embed_document` use
    /// them in the real HTTP request body.
    #[tokio::test]
    async fn openai_embedder_sends_the_prefix_on_the_wire() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(move |req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(body["input"], "query: find the runbook");
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [{ "embedding": vec![0.5_f32; 4] }],
                }))
            })
            .expect(1)
            .mount(&server)
            .await;

        let e = OpenAiEmbedder::new(SecretString::from("sk-test"), "text-embedding-3-small", 4)
            .unwrap()
            .with_base_url(server.uri())
            .with_prefixes("query: ", "passage: ");

        e.embed_query("find the runbook")
            .await
            .expect("embed_query succeeds");
    }

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

    #[test]
    fn parse_openai_embedding_accepts_data_array() {
        let body = r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#;
        let v = parse_openai_embedding_values(body, 200).unwrap();
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn parse_openai_embedding_accepts_openrouter_error() {
        let body = r#"{"error":{"message":"Provider returned error","code":502}}"#;
        let err = parse_openai_embedding_values(body, 502).unwrap_err();
        assert!(matches!(err, LlmError::Provider { status: 502, .. }));
    }

    #[test]
    fn parse_openai_embedding_rejects_non_numeric_values() {
        let body = r#"{"data":[{"embedding":[0.1,"oops",0.3]}]}"#;
        let err = parse_openai_embedding_values(body, 200).unwrap_err();
        assert!(matches!(err, LlmError::Provider { status: 200, .. }));
    }

    #[test]
    fn prepend_prefix_applies_publisher_instruction() {
        // Nemotron-3-Embed / the E5 family: exact publisher strings,
        // including the significant trailing space.
        assert_eq!(
            prepend_prefix("query: ", "find the release runbook"),
            "query: find the release runbook"
        );
        assert_eq!(
            prepend_prefix("passage: ", "the release runbook says..."),
            "passage: the release runbook says..."
        );
    }

    #[test]
    fn prepend_prefix_unset_leaves_text_unchanged() {
        // The default (empty prefix) must be a true no-op, byte for byte,
        // and borrow rather than allocate.
        let text = "unchanged text";
        let out = prepend_prefix("", text);
        assert_eq!(out, text);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn prefix_is_applied_before_truncation_so_it_still_bounds_the_whole_input() {
        // A prefix pushes the *total* input closer to the cap; truncation
        // must run on prefix+text together, never on text alone with the
        // prefix appended afterwards (which could exceed the server limit).
        let long_text = "x".repeat(50_000);
        let prefixed = prepend_prefix("passage: ", &long_text);
        let truncated = truncate_for_embedding(&prefixed, OPENAI_EMBED_MAX_TOKENS);
        assert!(truncated.starts_with("passage: "));
        assert!(truncated.len() <= 8_000, "must respect the hard byte cap");
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn openai_compat_embedder_defaults_to_no_prefix() {
        // Construction without `with_prefixes` must be byte-identical to
        // before this feature existed.
        let e = OpenAiCompatEmbedder::new("http://localhost:9/v1", None, "nomic-embed-text", 8)
            .expect("embedder builds");
        assert_eq!(e.query_prefix, "");
        assert_eq!(e.document_prefix, "");
    }

    #[test]
    fn openai_compat_embedder_stores_configured_prefixes() {
        let e = OpenAiCompatEmbedder::new(
            "http://localhost:9/v1",
            None,
            "nvidia/Nemotron-3-Embed-1B-BF16",
            2048,
        )
        .expect("embedder builds")
        .with_prefixes("query: ", "passage: ");
        assert_eq!(e.query_prefix, "query: ");
        assert_eq!(e.document_prefix, "passage: ");
    }
}
