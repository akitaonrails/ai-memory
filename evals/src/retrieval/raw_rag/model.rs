//! Pinned local cross-encoder, used only by the evaluation executable.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use candle_core::{Device, IndexOp, Tensor};
use candle_nn::{Linear, Module, VarBuilder, linear};
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

pub(super) const MODEL: &str = "cross-encoder/ms-marco-MiniLM-L6-v2";
pub(super) const REVISION: &str = "233902d25c440f23af6f7d6e94d2946bac0bee0a";
const DIRECTORY: &str = "ms-marco-MiniLM-L6-v2";
pub(super) const FILES: [(&str, &str); 3] = [
    (
        "model.safetensors",
        "821d1aa69520101d6e0737f78a042ae25b19e5cb9160701909d10434f4aeb0ae",
    ),
    (
        "tokenizer.json",
        "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    ),
    (
        "config.json",
        "380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc",
    ),
];

fn verify(bytes: &[u8], expected: &str) -> Result<()> {
    let digest = format!("{:x}", Sha256::digest(bytes));
    if digest != expected {
        bail!("cross-encoder checksum mismatch: expected {expected}, got {digest}");
    }
    Ok(())
}

pub(super) async fn fetch(root: &Path) -> Result<()> {
    let dir = root.join(DIRECTORY);
    std::fs::create_dir_all(&dir)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .build()?;
    for (name, expected) in FILES {
        let path = dir.join(name);
        if std::fs::read(&path).is_ok_and(|bytes| verify(&bytes, expected).is_ok()) {
            continue;
        }
        let mut response = client
            .get(format!(
                "https://huggingface.co/{MODEL}/resolve/{REVISION}/{name}"
            ))
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > 128 * 1024 * 1024 {
                bail!("cross-encoder model file exceeds 128 MiB");
            }
            bytes.extend_from_slice(&chunk);
        }
        verify(&bytes, expected).with_context(|| format!("verifying {name}"))?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
        std::io::Write::write_all(&mut temp, &bytes)?;
        temp.persist(&path)?;
    }
    Ok(())
}

pub(super) struct CrossEncoder {
    bert: BertModel,
    pooler: Linear,
    classifier: Linear,
    tokenizer: Tokenizer,
    max_tokens: usize,
}

impl CrossEncoder {
    pub(super) fn load(root: &Path) -> Result<Self> {
        let dir = root.join(DIRECTORY);
        for (name, expected) in FILES {
            let bytes = std::fs::read(dir.join(name)).with_context(|| {
                format!(
                    "missing cross-encoder file {}; pass --raw-rag-fetch-models to download pinned models",
                    dir.join(name).display()
                )
            })?;
            verify(&bytes, expected).with_context(|| format!("verifying {name}"))?;
        }
        let config: Config = serde_json::from_slice(&std::fs::read(dir.join("config.json"))?)?;
        let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("loading cross-encoder tokenizer: {e}"))?;
        tokenizer.with_padding(None);
        tokenizer
            .with_truncation(None)
            .map_err(|e| anyhow::anyhow!("configuring cross-encoder tokenizer: {e}"))?;
        let vb = VarBuilder::from_buffered_safetensors(
            std::fs::read(dir.join("model.safetensors"))?,
            DTYPE,
            &Device::Cpu,
        )?;
        Ok(Self {
            bert: BertModel::load(vb.pp("bert"), &config)?,
            pooler: linear(
                config.hidden_size,
                config.hidden_size,
                vb.pp("bert.pooler.dense"),
            )?,
            classifier: linear(config.hidden_size, 1, vb.pp("classifier"))?,
            tokenizer,
            max_tokens: config.max_position_embeddings,
        })
    }

    pub(super) fn score(&self, query: &str, passage: &str) -> Result<f32> {
        let encoding = self
            .tokenizer
            .encode((query, passage), true)
            .map_err(|e| anyhow::anyhow!("tokenizing cross-encoder pair: {e}"))?;
        if encoding.len() > self.max_tokens {
            bail!(
                "query/passage pair has {} tokens, exceeding cross-encoder limit {}; reduce --raw-rag-chunk-tokens (no silent truncation)",
                encoding.len(),
                self.max_tokens
            );
        }
        let device = &Device::Cpu;
        let ids = Tensor::new(encoding.get_ids(), device)?.unsqueeze(0)?;
        let types = Tensor::new(encoding.get_type_ids(), device)?.unsqueeze(0)?;
        let mask = Tensor::new(encoding.get_attention_mask(), device)?.unsqueeze(0)?;
        let hidden = self.bert.forward(&ids, &types, Some(&mask))?;
        // BertForSequenceClassification uses the pooled CLS state, not the
        // mean pooling used by the sentence embedder.
        let pooled = self.pooler.forward(&hidden.i((.., 0, ..))?)?.tanh()?;
        let score = self
            .classifier
            .forward(&pooled)?
            .squeeze(0)?
            .squeeze(0)?
            .to_scalar::<f32>()?;
        if !score.is_finite() {
            bail!("cross-encoder returned a non-finite score");
        }
        Ok(score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_checksum_rejects_a_changed_file() {
        let bytes = b"a model";
        let sha = format!("{:x}", Sha256::digest(bytes));
        verify(bytes, &sha).unwrap();
        assert!(verify(b"another model", &sha).is_err());
    }

    #[test]
    #[ignore = "needs the explicitly downloaded, checksum-pinned cross-encoder"]
    fn cross_encoder_matches_the_model_cards_reference_logits() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("models");
        let model = CrossEncoder::load(&root).unwrap();
        let query = "How many people live in Berlin?";
        let positive = model.score(query, "Berlin had a population of 3,520,031 registered inhabitants in an area of 891.82 square kilometers.").unwrap();
        let negative = model
            .score(query, "Berlin is well known for its museums.")
            .unwrap();
        assert!((positive - 8.607_138).abs() < 0.02, "{positive}");
        assert!((negative + 4.320_078).abs() < 0.02, "{negative}");
        assert!(model.score(&"hello ".repeat(600), "Berlin").is_err());
    }
}
