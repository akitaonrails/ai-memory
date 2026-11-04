//! Benchmark-only document-embedding cache (#1973).
//!
//! The LongMemEval harness in `evals/` replays the same haystack sessions on
//! every run, and in local-embeddings mode embedding them is almost all of the
//! ingest time. When the harness starts a server with
//! `AI_MEMORY_EVAL_EMBEDDING_CACHE_DIR` set, the server wraps its embedder in
//! [`EvalEmbeddingCache`], which stores each document vector on disk under
//! `{provider, model identity, dim, sha256(text)}` and serves it back on the
//! next run instead of calling the model.
//!
//! The text hash skips RFC 3339 UTC timestamps. The server stamps replayed
//! hook events with its own clock, and the session pages built from them list
//! those times (`started_at`, each observation's `@` time), so the same
//! haystack session never produces the same page text twice. With the times
//! masked, a later run reuses the vector the first run computed for that page;
//! the page text it ranks differs only in the replay's wall-clock times.
//!
//! Only [`Embedder::embed_document`] is cached. Query embeddings, ranking and
//! fusion stay live, so a cached run still measures the code under test. The
//! wrapper reports the inner embedder's identity unchanged, so every stored
//! row keeps its `{provider, model, dim}` (invariant #8), and a model or
//! dimension change lands in a different cache directory.
//!
//! The variable is read once at startup, only the eval harness sets it, and
//! it is deliberately absent from `config.toml` and the documented settings.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ai_memory_llm::{Embedder, LlmResult};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// Environment variable naming the cache root. Set only by the eval harness.
pub(crate) const EVAL_EMBEDDING_CACHE_DIR_ENV: &str = "AI_MEMORY_EVAL_EMBEDDING_CACHE_DIR";

/// Read the benchmark cache root once at startup. An empty value is off.
pub(crate) fn eval_embedding_cache_dir() -> Option<PathBuf> {
    std::env::var_os(EVAL_EMBEDDING_CACHE_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Wraps an embedder and caches its document vectors on disk.
pub(crate) struct EvalEmbeddingCache {
    inner: Arc<dyn Embedder>,
    /// `<root>/<provider>-<dim>-<model digest>`: one directory per identity.
    dir: PathBuf,
    dim: u32,
    tmp_seq: AtomicU64,
}

impl EvalEmbeddingCache {
    pub(crate) fn new(inner: Arc<dyn Embedder>, root: &Path) -> Self {
        let dim = inner.dim();
        let model_digest = hex_digest(inner.model_identity().as_bytes());
        let dir = root.join(format!(
            "{}-{dim}-{}",
            inner.provider(),
            &model_digest[..16]
        ));
        Self {
            inner,
            dir,
            dim,
            tmp_seq: AtomicU64::new(0),
        }
    }

    fn entry_path(&self, text: &str) -> PathBuf {
        let key = text_key(text);
        self.dir.join(&key[..2]).join(format!("{key}.f32"))
    }

    /// A stored vector, or `None` for a miss or an entry of the wrong size.
    async fn load(&self, path: &Path) -> Option<Vec<f32>> {
        let bytes = tokio::fs::read(path).await.ok()?;
        if bytes.len() != self.dim as usize * 4 {
            return None;
        }
        Some(
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        )
    }

    /// Best effort: a failed write only costs a recompute on the next run.
    /// Writes go through a unique temp file and a rename, so a concurrent
    /// reader never sees a partial vector.
    async fn store(&self, path: &Path, vector: &[f32]) {
        if vector.len() != self.dim as usize {
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        let bytes: Vec<u8> = vector.iter().flat_map(|v| v.to_le_bytes()).collect();
        let tmp = parent.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            self.tmp_seq.fetch_add(1, Ordering::Relaxed)
        ));
        let result = async {
            tokio::fs::create_dir_all(parent).await?;
            tokio::fs::write(&tmp, &bytes).await?;
            tokio::fs::rename(&tmp, path).await
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            tracing::debug!(%error, "eval embedding cache write failed");
        }
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 of `text` with every `YYYY-MM-DDTHH:MM:SS[.fff]Z` replaced by a
/// fixed marker, so a replay's wall-clock times do not change the key.
fn text_key(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut hasher = Sha256::new();
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        match timestamp_len(&bytes[i..]) {
            Some(len) => {
                hasher.update(&bytes[start..i]);
                hasher.update(b"\0ts\0");
                i += len;
                start = i;
            }
            None => i += 1,
        }
    }
    hasher.update(&bytes[start..]);
    hex(&hasher.finalize())
}

/// Length of an RFC 3339 UTC timestamp at the start of `b`, if one is there.
fn timestamp_len(b: &[u8]) -> Option<usize> {
    const SHAPE: &[u8; 19] = b"dddd-dd-ddTdd:dd:dd";
    if b.len() < SHAPE.len() + 1 {
        return None;
    }
    let shaped = SHAPE.iter().zip(b).all(|(want, got)| match want {
        b'd' => got.is_ascii_digit(),
        _ => want == got,
    });
    if !shaped {
        return None;
    }
    let mut len = SHAPE.len();
    if b[len] == b'.' {
        let digits = b[len + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        len += 1 + digits;
    }
    (b.get(len) == Some(&b'Z')).then_some(len + 1)
}

#[async_trait]
impl Embedder for EvalEmbeddingCache {
    fn provider(&self) -> &'static str {
        self.inner.provider()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn model_identity(&self) -> String {
        self.inner.model_identity()
    }

    fn dim(&self) -> u32 {
        self.inner.dim()
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.inner.embed(text).await
    }

    async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
        let path = self.entry_path(text);
        if let Some(vector) = self.load(&path).await {
            return Ok(vector);
        }
        let vector = self.inner.embed_document(text).await?;
        self.store(&path, &vector).await;
        Ok(vector)
    }

    async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
        self.inner.embed_query(text).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Counts calls per method; vectors encode the text length so a wrong
    /// entry would be visible.
    struct Counting {
        model: &'static str,
        dim: u32,
        documents: AtomicUsize,
        queries: AtomicUsize,
    }

    impl Counting {
        fn new(model: &'static str, dim: u32) -> Arc<Self> {
            Arc::new(Self {
                model,
                dim,
                documents: AtomicUsize::new(0),
                queries: AtomicUsize::new(0),
            })
        }

        fn vector(&self, text: &str) -> Vec<f32> {
            (0..self.dim)
                .map(|i| text.len() as f32 + i as f32)
                .collect()
        }
    }

    #[async_trait]
    impl Embedder for Counting {
        fn provider(&self) -> &'static str {
            "counting"
        }
        fn model(&self) -> &str {
            self.model
        }
        fn dim(&self) -> u32 {
            self.dim
        }
        async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
            Ok(self.vector(text))
        }
        async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
            self.documents.fetch_add(1, Ordering::SeqCst);
            Ok(self.vector(text))
        }
        async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            Ok(self.vector(text))
        }
    }

    #[tokio::test]
    async fn second_run_serves_documents_from_disk_and_keeps_queries_live() {
        let root = tempfile::tempdir().unwrap();
        let first_inner = Counting::new("m", 4);
        let first = EvalEmbeddingCache::new(first_inner.clone(), root.path());
        let stored = first.embed_document("haystack session").await.unwrap();

        // A fresh wrapper stands in for the next benchmark server process.
        let second_inner = Counting::new("m", 4);
        let second = EvalEmbeddingCache::new(second_inner.clone(), root.path());
        assert_eq!(
            second.embed_document("haystack session").await.unwrap(),
            stored
        );
        assert_eq!(second_inner.documents.load(Ordering::SeqCst), 0);

        // A different text is a miss, and queries are never cached.
        second.embed_document("another session").await.unwrap();
        second.embed_query("haystack session").await.unwrap();
        second.embed_query("haystack session").await.unwrap();
        assert_eq!(second_inner.documents.load(Ordering::SeqCst), 1);
        assert_eq!(second_inner.queries.load(Ordering::SeqCst), 2);
        assert_eq!(second.model_identity(), "m");
        assert_eq!((second.provider(), second.dim()), ("counting", 4));
    }

    #[test]
    fn replay_wall_clock_times_do_not_change_the_key() {
        let page = |t: &str| {
            format!("- **started_at:** {t}\n- `stop` @ {t} — stop\n[session date: 2023/07/12]")
        };
        assert_eq!(
            text_key(&page("2026-11-04T17:26:19Z")),
            text_key(&page("2026-11-05T09:01:02.123456Z"))
        );
        // Any other text change, including the dataset's own dates, misses.
        assert_ne!(
            text_key(&page("2026-11-04T17:26:19Z")),
            text_key(&page("2026-11-04T17:26:19Z").replace("2023/07/12", "2023/07/13"))
        );
        // Near-misses of the timestamp shape stay part of the key.
        for (a, b) in [
            ("2026-11-04T17:26:19", "2026-11-05T17:26:19"),
            ("2026-11-04 17:26:19Z", "2026-11-05 17:26:19Z"),
            ("2026-11-04T17:26:19.Z", "2026-11-05T17:26:19.Z"),
        ] {
            assert_ne!(text_key(a), text_key(b), "{a}");
        }
        assert_ne!(text_key("a"), text_key("b"));
    }

    #[tokio::test]
    async fn model_or_dimension_change_misses_the_cache() {
        let root = tempfile::tempdir().unwrap();
        let original = EvalEmbeddingCache::new(Counting::new("m", 4), root.path());
        original.embed_document("text").await.unwrap();

        for inner in [Counting::new("other-model", 4), Counting::new("m", 8)] {
            let cache = EvalEmbeddingCache::new(inner.clone(), root.path());
            let vector = cache.embed_document("text").await.unwrap();
            assert_eq!(vector.len(), inner.dim as usize);
            assert_eq!(inner.documents.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn a_truncated_entry_is_recomputed_and_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let inner = Counting::new("m", 4);
        let cache = EvalEmbeddingCache::new(inner.clone(), root.path());
        let path = cache.entry_path("text");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, [0u8; 6]).unwrap();

        let vector = cache.embed_document("text").await.unwrap();
        assert_eq!(vector, inner.vector("text"));
        assert_eq!(inner.documents.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&path).unwrap().len(), 16);
    }
}
