//! Local in-process embeddings (2.0 item 5, `docs/local-embeddings.md`).
//!
//! Pure-Rust BERT inference via candle — no API key, no server, no
//! native onnxruntime. The model (`all-MiniLM-L6-v2`, 384-dim, the
//! sentence-transformers workhorse the comparable memory servers ship)
//! is NOT bundled in the binary: ~87 MB of safetensors is fetched once
//! into `<data_dir>/models/` with pinned sha256s, or dropped there by
//! hand for offline installs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::embedding::Embedder;
use crate::error::{LlmError, LlmResult};

/// Model identity as stored on `page_embeddings` rows.
pub const LOCAL_MODEL: &str = "all-MiniLM-L6-v2";
/// Output dimensionality of [`LOCAL_MODEL`].
pub const LOCAL_DIM: u32 = 384;
/// Token cap per input (the model's positional limit is 512; sentence
/// embeddings degrade past ~256 anyway, matching sentence-transformers'
/// own default).
const MAX_TOKENS: usize = 512;

/// The three files that make up the model, with sha256s pinned on
/// 2026-09-01. A drifted upstream file fails loudly instead of silently
/// changing every vector.
pub const MODEL_FILES: [(&str, &str); 3] = [
    (
        "model.safetensors",
        "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db",
    ),
    (
        "tokenizer.json",
        "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037",
    ),
    (
        "config.json",
        "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41",
    ),
];

const MODEL_BASE_URL: &str =
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main";

/// Directory the model lives in under the data dir's `models/` root.
#[must_use]
pub fn model_dir(models_root: &Path) -> PathBuf {
    models_root.join(LOCAL_MODEL)
}

/// Whether every model file is present (checksums are verified at load,
/// not here — presence is the cheap serve-startup probe).
#[must_use]
pub fn model_present(models_root: &Path) -> bool {
    let dir = model_dir(models_root);
    MODEL_FILES.iter().all(|(name, _)| dir.join(name).exists())
}

/// Download the model files with pinned checksums (atomic: tmp +
/// verify + rename). Skips files already present and valid; a present
/// file with a wrong checksum is replaced.
///
/// # Errors
/// Network failures, checksum mismatches, and IO errors — the caller
/// (serve startup / embed) surfaces them with the offline instructions.
pub async fn fetch_model(models_root: &Path) -> LlmResult<()> {
    let dir = model_dir(models_root);
    std::fs::create_dir_all(&dir).map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    // Bound the fetch so a stalled connection can't hang startup forever
    // (#602). Unlike the inference clients' flat 120s, these are large
    // model files (~90 MB total), so a 120s *total* cap would spuriously
    // fail a slow-but-working download: bound the connection stall tightly
    // and give the body a generous ceiling instead.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    for (name, pinned) in MODEL_FILES {
        let dest = dir.join(name);
        if let Ok(existing) = std::fs::read(&dest)
            && hex(&Sha256::digest(&existing)) == pinned
        {
            continue;
        }
        tracing::info!(file = name, "fetching local embedding model file");
        let bytes = client
            .get(format!("{MODEL_BASE_URL}/{name}"))
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let digest = hex(&Sha256::digest(&bytes));
        if digest != pinned {
            return Err(LlmError::UnexpectedShape(format!(
                "downloaded {name} sha256 {digest} does not match pin {pinned}; \
                 refusing to install an unverified model"
            )));
        }
        let tmp = dest.with_extension("part");
        std::fs::write(&tmp, &bytes).map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        std::fs::rename(&tmp, &dest).map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// In-process sentence embedder. Cheap to clone-share via `Arc`; the
/// forward pass runs on `spawn_blocking` so the async runtime never
/// blocks on CPU inference.
pub struct LocalEmbedder {
    inner: Arc<Inner>,
}

struct Inner {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl LocalEmbedder {
    /// Load the model from `<models_root>/all-MiniLM-L6-v2/`, verifying
    /// every file against its pinned sha256 first — a tampered or torn
    /// model must not silently produce garbage vectors.
    ///
    /// # Errors
    /// Missing files (with the fetch/offline instructions), checksum
    /// mismatches, and model-load failures.
    pub fn load(models_root: &Path) -> LlmResult<Self> {
        let dir = model_dir(models_root);
        for (name, pinned) in MODEL_FILES {
            let path = dir.join(name);
            let bytes = std::fs::read(&path).map_err(|_| {
                LlmError::NotConfigured(format!(
                    "local embedding model file missing: {}. Start the server once \
                     with network access to fetch it, or download \
                     {MODEL_BASE_URL}/{name} manually into {}",
                    path.display(),
                    dir.display(),
                ))
            })?;
            let digest = hex(&Sha256::digest(&bytes));
            if digest != pinned {
                return Err(LlmError::NotConfigured(format!(
                    "local embedding model file {} sha256 {digest} does not match \
                     the pinned {pinned}; delete it and re-fetch",
                    path.display(),
                )));
            }
        }

        let device = Device::Cpu;
        let config: BertConfig = serde_json::from_str(
            &std::fs::read_to_string(dir.join("config.json"))
                .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?,
        )
        .map_err(|e| LlmError::UnexpectedShape(format!("parsing model config: {e}")))?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| LlmError::UnexpectedShape(format!("loading tokenizer: {e}")))?;
        // The shipped tokenizer.json enables fixed-length padding (128).
        // Left on, a single unbatched input carries a tail of [PAD]
        // tokens — and feeding those through with an all-ones attention
        // mask makes the model attend to padding, which flattens every
        // similarity toward ~0.8 (caught by the reference-comparison
        // test below). No batching here, so: no padding, and the real
        // attention mask is passed to the forward pass regardless.
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| LlmError::UnexpectedShape(format!("configuring truncation: {e}")))?;
        // Buffered (safe) loader: ~87 MB read once into memory — the
        // workspace forbids unsafe, and the mmap variant is `unsafe`.
        let weights = std::fs::read(dir.join("model.safetensors"))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DTYPE, &device)
            .map_err(|e| LlmError::UnexpectedShape(format!("loading model weights: {e}")))?;
        let model = BertModel::load(vb, &config)
            .map_err(|e| LlmError::UnexpectedShape(format!("building BERT model: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                model,
                tokenizer,
                device,
            }),
        })
    }

    fn embed_blocking(inner: &Inner, text: &str) -> LlmResult<Vec<f32>> {
        let encoding = inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| LlmError::UnexpectedShape(format!("tokenizing: {e}")))?;
        let ids = encoding.get_ids();
        if ids.is_empty() {
            return Err(LlmError::UnexpectedShape("empty tokenization".into()));
        }
        let mask_vals = encoding.get_attention_mask();
        let input_ids = Tensor::new(ids, &inner.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let token_type_ids = input_ids
            .zeros_like()
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let attention_mask = Tensor::new(mask_vals, &inner.device)
            .and_then(|t| t.unsqueeze(0))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let hidden = inner
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| LlmError::UnexpectedShape(format!("forward pass: {e}")))?;
        // Masked mean pooling over the token axis (identical to a plain
        // mean when nothing is padded, correct either way), then L2
        // normalise — the sentence-transformers contract, and the unit
        // vector the Embedder trait promises.
        let mask_f = attention_mask
            .to_dtype(hidden.dtype())
            .and_then(|m| m.unsqueeze(2))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let masked = hidden
            .broadcast_mul(&mask_f)
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let token_count = mask_vals.iter().map(|&m| m as f32).sum::<f32>().max(1.0);
        let pooled = masked
            .sum(1)
            .and_then(|t| t.squeeze(0))
            .and_then(|t| t / token_count as f64)
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let vec: Vec<f32> = pooled
            .to_vec1()
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm == 0.0 || !norm.is_finite() {
            return Err(LlmError::UnexpectedShape(
                "degenerate embedding norm".into(),
            ));
        }
        Ok(vec.into_iter().map(|v| v / norm).collect())
    }
}

#[async_trait::async_trait]
impl Embedder for LocalEmbedder {
    fn provider(&self) -> &'static str {
        "local"
    }

    fn model(&self) -> &str {
        LOCAL_MODEL
    }

    fn dim(&self) -> u32 {
        LOCAL_DIM
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        let inner = Arc::clone(&self.inner);
        let text = crate::text::truncate_for_embedding(text, MAX_TOKENS * 4);
        tokio::task::spawn_blocking(move || Self::embed_blocking(&inner, &text))
            .await
            .map_err(|e| LlmError::UnexpectedShape(format!("embedding task: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_presence_probe_requires_all_three_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!model_present(tmp.path()));
        let dir = model_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        for (name, _) in &MODEL_FILES[..2] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        assert!(!model_present(tmp.path()), "two of three is not present");
        std::fs::write(dir.join(MODEL_FILES[2].0), b"x").unwrap();
        assert!(model_present(tmp.path()));
    }

    #[test]
    fn load_refuses_tampered_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = model_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        for (name, _) in MODEL_FILES {
            std::fs::write(dir.join(name), b"not the real file").unwrap();
        }
        let err = match LocalEmbedder::load(tmp.path()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("tampered files must not load"),
        };
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn load_names_the_missing_file_and_the_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let err = match LocalEmbedder::load(tmp.path()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("missing files must not load"),
        };
        assert!(err.contains("model.safetensors"), "{err}");
        assert!(err.contains("manually"), "{err}");
    }

    /// Calibration guard: absolute similarity ranges for a known pair.
    /// The padding bug this catches (fixed-length padding in the shipped
    /// tokenizer.json + an all-ones attention mask) flattened every
    /// similarity toward ~0.85; correct masked-mean pooling puts this
    /// pair near 0.28. A pipeline change that shifts calibration out of
    /// range is a retrieval-quality regression even if orderings hold.
    #[tokio::test]
    #[ignore = "needs the fetched all-MiniLM-L6-v2 model files"]
    async fn similarity_calibration_matches_the_reference_range() {
        let root = std::env::var("AI_MEMORY_TEST_MODELS_DIR")
            .expect("set AI_MEMORY_TEST_MODELS_DIR to a dir containing all-MiniLM-L6-v2");
        let embedder = LocalEmbedder::load(Path::new(&root)).unwrap();
        let cat = embedder.embed("The cat sits outside").await.unwrap();
        let dog = embedder.embed("The dog plays in the garden").await.unwrap();
        let dot: f32 = cat.iter().zip(&dog).map(|(x, y)| x * y).sum();
        assert!(
            (0.15..=0.45).contains(&dot),
            "cat/dog similarity {dot:.3} outside the calibrated range \
             (0.15..0.45); pooling or masking has drifted"
        );
    }

    /// Full inference against the real fetched model: `#[ignore]`d like
    /// the eval smoke (needs ~87 MB in the workspace models dir; run
    /// `cargo test -p ai-memory-llm --lib local -- --ignored` after a
    /// fetch).
    #[tokio::test]
    #[ignore = "needs the fetched all-MiniLM-L6-v2 model files"]
    async fn real_model_produces_unit_vectors_with_semantic_order() {
        let root = std::env::var("AI_MEMORY_TEST_MODELS_DIR")
            .expect("set AI_MEMORY_TEST_MODELS_DIR to a dir containing all-MiniLM-L6-v2");
        let embedder = LocalEmbedder::load(Path::new(&root)).unwrap();
        assert_eq!(embedder.dim(), 384);
        let db = embedder.embed("the database write path").await.unwrap();
        assert_eq!(db.len(), 384);
        let norm: f32 = db.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "unit vector, got {norm}");
        let storage = embedder.embed("sqlite storage layer").await.unwrap();
        let weather = embedder.embed("tomorrow will be sunny").await.unwrap();
        let dot = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        assert!(
            dot(&db, &storage) > dot(&db, &weather),
            "semantic neighbour must beat the unrelated sentence"
        );
    }
}
