//! Local in-process embeddings (2.0 item 5, `docs/local-embeddings.md`).
//!
//! Pure-Rust BERT inference via candle — no API key, no server, no
//! native onnxruntime. Models are NOT bundled in the binary: safetensors,
//! tokenizer, and config are fetched once into `<data_dir>/models/<dir>/`
//! with pinned sha256s, or dropped there by hand for offline installs.
//!
//! Supported models:
//! - `all-MiniLM-L6-v2` (default, 384-dim, English-focused, ~87 MB)
//! - `intfloat/multilingual-e5-small` (alias `multilingual-e5-small`,
//!   384-dim, 100 languages, ~470 MB, asymmetric `"query: "` / `"passage: "`
//!   prefixes applied automatically)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig, DTYPE};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

use crate::embedding::{Embedder, prepend_prefix};
use crate::error::{LlmError, LlmResult};

/// Default local model identity as stored on `page_embeddings` rows.
pub const LOCAL_MODEL: &str = "all-MiniLM-L6-v2";
/// Output dimensionality of [`LOCAL_MODEL`].
pub const LOCAL_DIM: u32 = 384;
/// Canonical model identity for the pinned multilingual E5-small local model.
pub const MULTILINGUAL_E5_SMALL_MODEL: &str = "intfloat/multilingual-e5-small";
/// Token cap per input (the models' positional limit is 512; sentence
/// embeddings degrade past ~256 anyway, matching sentence-transformers'
/// own default).
const MAX_TOKENS: usize = 512;

/// The three files that make up the default (`all-MiniLM-L6-v2`) model, with
/// sha256s pinned on 2026-09-01. A drifted upstream file fails loudly instead
/// of silently changing every vector.
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

/// Static specification for a pinned in-process local embedding model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalModelSpec {
    /// Canonical model identity as stored on `page_embeddings` rows.
    pub id: &'static str,
    /// Accepted shorthand aliases (in addition to `id`).
    pub aliases: &'static [&'static str],
    /// Subdirectory name under `<data_dir>/models/`.
    pub dir_name: &'static str,
    /// Base URL for fetching the pinned model files.
    pub base_url: &'static str,
    /// Pinned `(filename, sha256_hex)` pairs required to load the model.
    pub files: [(&'static str, &'static str); 3],
    /// Output vector dimensionality.
    pub dim: u32,
    /// Prefix prepended to search queries before truncation and tokenization.
    pub query_prefix: &'static str,
    /// Prefix prepended to documents before truncation and tokenization.
    pub document_prefix: &'static str,
    /// Approximate total download size in megabytes (for startup logs).
    pub approx_download_mb: u32,
}

/// All supported pinned local embedding models.
pub const LOCAL_MODELS: &[LocalModelSpec] = &[
    LocalModelSpec {
        id: LOCAL_MODEL,
        aliases: &[],
        dir_name: LOCAL_MODEL,
        base_url: MODEL_BASE_URL,
        files: MODEL_FILES,
        dim: LOCAL_DIM,
        query_prefix: "",
        document_prefix: "",
        approx_download_mb: 87,
    },
    LocalModelSpec {
        id: MULTILINGUAL_E5_SMALL_MODEL,
        aliases: &["multilingual-e5-small"],
        dir_name: "multilingual-e5-small",
        base_url: "https://huggingface.co/intfloat/multilingual-e5-small/resolve/614241f622f53c4eeff9890bdc4f31cfecc418b3",
        files: [
            (
                "model.safetensors",
                "1a55775f53449dac10a2bcbc312469fac40b96d53198c407081a831f81c98477",
            ),
            (
                "tokenizer.json",
                "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
            ),
            (
                "config.json",
                "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959",
            ),
        ],
        dim: 384,
        query_prefix: "query: ",
        document_prefix: "passage: ",
        approx_download_mb: 470,
    },
];

/// Default local embedding model specification (`all-MiniLM-L6-v2`).
pub const DEFAULT_LOCAL_MODEL: &LocalModelSpec = &LOCAL_MODELS[0];

/// Resolve a configured local embedding model name (canonical id or alias)
/// to its pinned [`LocalModelSpec`].
///
/// # Errors
/// Returns [`LlmError::NotConfigured`] listing the supported local models
/// when `name` is unknown.
pub fn resolve_local_model(name: &str) -> LlmResult<&'static LocalModelSpec> {
    let trimmed = name.trim();
    if let Some(spec) = LOCAL_MODELS
        .iter()
        .find(|spec| spec.id == trimmed || spec.aliases.contains(&trimmed))
    {
        return Ok(spec);
    }
    let supported = LOCAL_MODELS
        .iter()
        .map(|spec| spec.id)
        .collect::<Vec<_>>()
        .join(", ");
    Err(LlmError::NotConfigured(format!(
        "unknown local embedding model '{trimmed}'; supported local models: {supported}"
    )))
}

/// Directory a specific local model lives in under the data dir's `models/` root.
#[must_use]
pub fn model_dir_for(models_root: &Path, spec: &LocalModelSpec) -> PathBuf {
    models_root.join(spec.dir_name)
}

/// Directory the default model lives in under the data dir's `models/` root.
#[must_use]
pub fn model_dir(models_root: &Path) -> PathBuf {
    model_dir_for(models_root, DEFAULT_LOCAL_MODEL)
}

/// Whether every file for `spec` is present (checksums are verified at load,
/// not here — presence is the cheap serve-startup probe).
#[must_use]
pub fn model_present_for(models_root: &Path, spec: &LocalModelSpec) -> bool {
    let dir = model_dir_for(models_root, spec);
    spec.files.iter().all(|(name, _)| dir.join(name).exists())
}

/// Whether every file for the default model is present.
#[must_use]
pub fn model_present(models_root: &Path) -> bool {
    model_present_for(models_root, DEFAULT_LOCAL_MODEL)
}

/// Download the files for `spec` with pinned checksums (atomic: tmp +
/// verify + rename). Skips files already present and valid; a present
/// file with a wrong checksum is replaced.
///
/// # Errors
/// Network failures, checksum mismatches, and IO errors — the caller
/// (serve startup / embed) surfaces them with the offline instructions.
pub async fn fetch_model_for(models_root: &Path, spec: &LocalModelSpec) -> LlmResult<()> {
    let dir = model_dir_for(models_root, spec);
    std::fs::create_dir_all(&dir).map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    // Bound the fetch so a stalled connection can't hang startup forever
    // (#602). Unlike the inference clients' flat 120s, these are large
    // model files (~87-470 MB total), so a 120s *total* cap would spuriously
    // fail a slow-but-working download: bound the connection stall tightly
    // and give the body a generous ceiling instead.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(900))
        .build()
        .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
    for (name, pinned) in spec.files {
        let dest = dir.join(name);
        if let Ok(existing) = std::fs::read(&dest)
            && hex(&Sha256::digest(&existing)) == pinned
        {
            continue;
        }
        tracing::info!(
            model = spec.id,
            file = name,
            "fetching local embedding model file"
        );
        let bytes = client
            .get(format!("{}/{name}", spec.base_url))
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

/// Download the default (`all-MiniLM-L6-v2`) model files with pinned checksums.
///
/// # Errors
/// Network failures, checksum mismatches, and IO errors — see [`fetch_model_for`].
pub async fn fetch_model(models_root: &Path) -> LlmResult<()> {
    fetch_model_for(models_root, DEFAULT_LOCAL_MODEL).await
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
    spec: &'static LocalModelSpec,
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl LocalEmbedder {
    /// Load the default (`all-MiniLM-L6-v2`) model from
    /// `<models_root>/all-MiniLM-L6-v2/`, verifying every file against its
    /// pinned sha256 first.
    ///
    /// # Errors
    /// Missing files (with the fetch/offline instructions), checksum
    /// mismatches, and model-load failures.
    pub fn load(models_root: &Path) -> LlmResult<Self> {
        Self::load_model(models_root, DEFAULT_LOCAL_MODEL)
    }

    /// Load `spec` from `<models_root>/<spec.dir_name>/`, verifying every file
    /// against its pinned sha256 first — a tampered or torn model must not
    /// silently produce garbage vectors.
    ///
    /// # Errors
    /// Missing files (with the fetch/offline instructions), checksum
    /// mismatches, and model-load failures.
    pub fn load_model(models_root: &Path, spec: &'static LocalModelSpec) -> LlmResult<Self> {
        let dir = model_dir_for(models_root, spec);
        for (name, pinned) in spec.files {
            let path = dir.join(name);
            let bytes = std::fs::read(&path).map_err(|_| {
                LlmError::NotConfigured(format!(
                    "local embedding model file missing: {}. Start the server once \
                     with network access to fetch it, or download \
                     {}/{name} manually into {}",
                    path.display(),
                    spec.base_url,
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
        // Shipped tokenizer.json files can enable fixed-length padding (e.g.
        // 128). Left on, a single unbatched input carries a tail of [PAD]
        // tokens — and feeding those through with an all-ones attention mask
        // makes the model attend to padding, which flattens every similarity
        // toward ~0.8 (caught by the reference-comparison test below). No
        // batching here, so: no padding, and the real attention mask is passed
        // to the forward pass regardless.
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| LlmError::UnexpectedShape(format!("configuring truncation: {e}")))?;
        // Buffered (safe) loader: read once into memory — the workspace
        // forbids unsafe, and the mmap variant is `unsafe`.
        let weights = std::fs::read(dir.join("model.safetensors"))
            .map_err(|e| LlmError::UnexpectedShape(e.to_string()))?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DTYPE, &device)
            .map_err(|e| LlmError::UnexpectedShape(format!("loading model weights: {e}")))?;
        let model = BertModel::load(vb, &config)
            .map_err(|e| LlmError::UnexpectedShape(format!("building BERT model: {e}")))?;
        Ok(Self {
            inner: Arc::new(Inner {
                spec,
                model,
                tokenizer,
                device,
            }),
        })
    }

    fn prepare_input(prefix: &str, text: &str) -> String {
        let prefixed = prepend_prefix(prefix, text);
        crate::text::truncate_for_embedding(&prefixed, MAX_TOKENS * 4)
    }

    async fn embed_with_prefix(&self, prefix: &str, text: &str) -> LlmResult<Vec<f32>> {
        let inner = Arc::clone(&self.inner);
        let text = Self::prepare_input(prefix, text);
        tokio::task::spawn_blocking(move || Self::embed_blocking(&inner, &text))
            .await
            .map_err(|e| LlmError::UnexpectedShape(format!("embedding task: {e}")))?
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
        self.inner.spec.id
    }

    fn dim(&self) -> u32 {
        self.inner.spec.dim
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed_with_prefix("", text).await
    }

    async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed_with_prefix(self.inner.spec.document_prefix, text)
            .await
    }

    async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.embed_with_prefix(self.inner.spec.query_prefix, text)
            .await
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

    #[test]
    fn resolve_local_model_handles_canonical_names_aliases_and_unknowns() {
        let minilm = resolve_local_model("all-MiniLM-L6-v2").unwrap();
        assert_eq!(minilm.id, LOCAL_MODEL);
        assert_eq!(minilm.dir_name, "all-MiniLM-L6-v2");
        assert_eq!(minilm.dim, 384);
        assert_eq!(minilm.query_prefix, "");
        assert_eq!(minilm.document_prefix, "");
        assert_eq!(minilm.approx_download_mb, 87);

        let e5_canonical = resolve_local_model("intfloat/multilingual-e5-small").unwrap();
        let e5_alias = resolve_local_model("multilingual-e5-small").unwrap();
        let e5_trimmed = resolve_local_model("  multilingual-e5-small  ").unwrap();
        assert_eq!(e5_canonical, e5_alias);
        assert_eq!(e5_canonical, e5_trimmed);
        assert_eq!(e5_canonical.id, "intfloat/multilingual-e5-small");
        assert_eq!(e5_canonical.dir_name, "multilingual-e5-small");
        assert_eq!(e5_canonical.dim, 384);
        assert_eq!(e5_canonical.query_prefix, "query: ");
        assert_eq!(e5_canonical.document_prefix, "passage: ");
        assert_eq!(e5_canonical.approx_download_mb, 470);

        let err = resolve_local_model("bge-small-en-v1.5").unwrap_err();
        assert!(matches!(err, LlmError::NotConfigured(_)), "{err}");
        let msg = err.to_string();
        assert!(msg.contains("bge-small-en-v1.5"), "{msg}");
        assert!(msg.contains("all-MiniLM-L6-v2"), "{msg}");
        assert!(msg.contains("intfloat/multilingual-e5-small"), "{msg}");
    }

    #[test]
    fn per_model_directory_presence_and_tamper_checks_are_isolated() {
        let tmp = tempfile::tempdir().unwrap();
        let minilm = resolve_local_model("all-MiniLM-L6-v2").unwrap();
        let e5 = resolve_local_model("multilingual-e5-small").unwrap();

        assert_eq!(
            model_dir_for(tmp.path(), minilm),
            tmp.path().join("all-MiniLM-L6-v2")
        );
        assert_eq!(
            model_dir_for(tmp.path(), e5),
            tmp.path().join("multilingual-e5-small")
        );

        let minilm_dir = model_dir_for(tmp.path(), minilm);
        std::fs::create_dir_all(&minilm_dir).unwrap();
        for (name, _) in minilm.files {
            std::fs::write(minilm_dir.join(name), b"x").unwrap();
        }
        assert!(model_present_for(tmp.path(), minilm));
        assert!(
            !model_present_for(tmp.path(), e5),
            "presence of MiniLM must not satisfy multilingual-e5-small"
        );

        let missing_err = match LocalEmbedder::load_model(tmp.path(), e5) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("missing e5 files must not load"),
        };
        assert!(
            missing_err.contains("multilingual-e5-small"),
            "{missing_err}"
        );
        assert!(missing_err.contains(e5.base_url), "{missing_err}");

        let e5_dir = model_dir_for(tmp.path(), e5);
        std::fs::create_dir_all(&e5_dir).unwrap();
        for (name, _) in e5.files {
            std::fs::write(e5_dir.join(name), b"tampered").unwrap();
        }
        assert!(model_present_for(tmp.path(), e5));
        let tamper_err = match LocalEmbedder::load_model(tmp.path(), e5) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("tampered e5 files must not load"),
        };
        assert!(tamper_err.contains("does not match"), "{tamper_err}");
        assert!(tamper_err.contains("multilingual-e5-small"), "{tamper_err}");
    }

    #[test]
    fn prepare_input_prepends_prefix_before_truncation() {
        let long_body = "word ".repeat(2000);
        let q = LocalEmbedder::prepare_input("query: ", &long_body);
        let d = LocalEmbedder::prepare_input("passage: ", &long_body);
        assert!(q.starts_with("query: "), "{q}");
        assert!(d.starts_with("passage: "), "{d}");
        assert!(q.len() <= MAX_TOKENS * 4 * 4);
        assert!(d.len() <= MAX_TOKENS * 4 * 4);

        let plain = LocalEmbedder::prepare_input("", "hello");
        assert_eq!(plain, "hello");
    }

    fn tiny_embedder(spec: &'static LocalModelSpec) -> LocalEmbedder {
        use std::collections::HashMap;

        let config: BertConfig = serde_json::from_value(serde_json::json!({
            "vocab_size": 8,
            "hidden_size": 4,
            "num_hidden_layers": 1,
            "num_attention_heads": 1,
            "intermediate_size": 8,
            "hidden_act": "gelu",
            "hidden_dropout_prob": 0.0,
            "attention_probs_dropout_prob": 0.0,
            "max_position_embeddings": 32,
            "type_vocab_size": 2,
            "initializer_range": 0.02,
            "layer_norm_eps": 1e-12,
            "pad_token_id": 0,
            "position_embedding_type": "absolute",
            "use_cache": true
        }))
        .unwrap();

        let device = Device::Cpu;
        // Distinct per-token embeddings so `"query: hello"` and
        // `"passage: hello"` produce distinct pooled vectors, with all other
        // BERT parameters initialized deterministically.
        let word_emb = Tensor::from_slice(
            &[
                0.1_f32, 0.2, 0.3, 0.4, // [UNK]
                1.0, 0.0, 0.0, 0.0, // query
                0.0, 1.0, 0.0, 0.0, // passage
                0.0, 0.0, 1.0, 0.0, // :
                0.0, 0.0, 0.0, 1.0, // hello
                0.5, 0.5, 0.0, 0.0, // unused
                0.0, 0.5, 0.5, 0.0, // unused
                0.0, 0.0, 0.5, 0.5, // unused
            ],
            (8, 4),
            &device,
        )
        .unwrap();
        let pos_emb = Tensor::zeros((32, 4), DTYPE, &device).unwrap();
        let type_emb = Tensor::zeros((2, 4), DTYPE, &device).unwrap();
        let ones4 = Tensor::ones(4, DTYPE, &device).unwrap();
        let zeros4 = Tensor::zeros(4, DTYPE, &device).unwrap();
        let eye4 = Tensor::eye(4, DTYPE, &device).unwrap();
        let w_up = Tensor::ones((8, 4), DTYPE, &device).unwrap();
        let b_up = Tensor::zeros(8, DTYPE, &device).unwrap();
        let w_down = Tensor::ones((4, 8), DTYPE, &device).unwrap();

        let tensors = HashMap::from([
            ("embeddings.word_embeddings.weight".to_string(), word_emb),
            ("embeddings.position_embeddings.weight".to_string(), pos_emb),
            (
                "embeddings.token_type_embeddings.weight".to_string(),
                type_emb,
            ),
            ("embeddings.LayerNorm.weight".to_string(), ones4.clone()),
            ("embeddings.LayerNorm.bias".to_string(), zeros4.clone()),
            (
                "encoder.layer.0.attention.self.query.weight".to_string(),
                eye4.clone(),
            ),
            (
                "encoder.layer.0.attention.self.query.bias".to_string(),
                zeros4.clone(),
            ),
            (
                "encoder.layer.0.attention.self.key.weight".to_string(),
                eye4.clone(),
            ),
            (
                "encoder.layer.0.attention.self.key.bias".to_string(),
                zeros4.clone(),
            ),
            (
                "encoder.layer.0.attention.self.value.weight".to_string(),
                eye4.clone(),
            ),
            (
                "encoder.layer.0.attention.self.value.bias".to_string(),
                zeros4.clone(),
            ),
            (
                "encoder.layer.0.attention.output.dense.weight".to_string(),
                eye4,
            ),
            (
                "encoder.layer.0.attention.output.dense.bias".to_string(),
                zeros4.clone(),
            ),
            (
                "encoder.layer.0.attention.output.LayerNorm.weight".to_string(),
                ones4.clone(),
            ),
            (
                "encoder.layer.0.attention.output.LayerNorm.bias".to_string(),
                zeros4.clone(),
            ),
            (
                "encoder.layer.0.intermediate.dense.weight".to_string(),
                w_up,
            ),
            ("encoder.layer.0.intermediate.dense.bias".to_string(), b_up),
            ("encoder.layer.0.output.dense.weight".to_string(), w_down),
            (
                "encoder.layer.0.output.dense.bias".to_string(),
                zeros4.clone(),
            ),
            ("encoder.layer.0.output.LayerNorm.weight".to_string(), ones4),
            ("encoder.layer.0.output.LayerNorm.bias".to_string(), zeros4),
        ]);
        let vb = VarBuilder::from_tensors(tensors, DTYPE, &device);
        let model = BertModel::load(vb, &config).unwrap();

        let tokenizer_json = serde_json::json!({
            "version": "1.0",
            "truncation": null,
            "padding": null,
            "added_tokens": [],
            "normalizer": null,
            "pre_tokenizer": { "type": "Whitespace" },
            "post_processor": null,
            "decoder": null,
            "model": {
                "type": "WordLevel",
                "vocab": {
                    "[UNK]": 0,
                    "query": 1,
                    "passage": 2,
                    ":": 3,
                    "hello": 4
                },
                "unk_token": "[UNK]"
            }
        });
        let tokenizer: Tokenizer = tokenizer_json.to_string().parse().unwrap();

        LocalEmbedder {
            inner: Arc::new(Inner {
                spec,
                model,
                tokenizer,
                device,
            }),
        }
    }

    #[tokio::test]
    async fn embed_query_and_embed_document_apply_spec_prefixes_and_distinct_identity() {
        let minilm = tiny_embedder(resolve_local_model("all-MiniLM-L6-v2").unwrap());
        let e5 = tiny_embedder(resolve_local_model("multilingual-e5-small").unwrap());

        assert_eq!(minilm.model(), "all-MiniLM-L6-v2");
        assert_eq!(minilm.model_identity(), "all-MiniLM-L6-v2");
        assert_eq!(e5.model(), "intfloat/multilingual-e5-small");
        assert_eq!(e5.model_identity(), "intfloat/multilingual-e5-small");
        assert_ne!(minilm.model_identity(), e5.model_identity());

        let m_plain = minilm.embed("hello").await.unwrap();
        let m_query = minilm.embed_query("hello").await.unwrap();
        let m_doc = minilm.embed_document("hello").await.unwrap();
        assert_eq!(m_plain, m_query);
        assert_eq!(m_plain, m_doc);

        let e_plain = e5.embed("hello").await.unwrap();
        let e_query = e5.embed_query("hello").await.unwrap();
        let e_doc = e5.embed_document("hello").await.unwrap();
        assert_ne!(
            e_query, e_doc,
            "multilingual-e5-small must differentiate query and passage prefixes"
        );
        assert_ne!(
            e_query, e_plain,
            "query prefix must alter the embedded text"
        );
        assert_ne!(
            e_doc, e_plain,
            "document prefix must alter the embedded text"
        );
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
