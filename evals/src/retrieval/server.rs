//! Spawn and supervise a real `ai-memory serve` subprocess for the
//! benchmark. The whole point of the harness is to measure the shipped
//! stack, so nothing here reaches into crates directly: the server is
//! the same binary a user runs, configured for deterministic zero-LLM
//! operation (no consolidation LLM, no embedder, no reranker).

use std::ffi::OsString;
use std::io::Read as _;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Fixed bearer for the eval server (ephemeral data dir, loopback bind).
pub const EVAL_AUTH_TOKEN: &str = "ai-memory-eval-token";

pub struct EvalServer {
    child: Child,
    pub base_url: String,
    /// Kept for the lifetime of the server unless --keep-data-dir moved it.
    _data_dir: Option<tempfile::TempDir>,
    pub data_dir_path: PathBuf,
    /// Continuously drained child stderr. A piped stderr that nobody reads
    /// deadlocks the server the moment its startup logging fills the OS
    /// pipe buffer (~64 KiB) — and an A/B run launches two servers, so it
    /// hits reliably. A background reader keeps the pipe empty and the
    /// captured text is surfaced on a startup failure.
    stderr: Arc<Mutex<String>>,
}

/// Which embedding configuration the eval server runs with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EvalEmbeddings {
    /// Deterministic zero-LLM mode: no embedder at all (vectors OFF).
    None,
    /// In-process local embeddings (all-MiniLM-L6-v2), i.e. vectors ON.
    /// The model files must already sit in the given models root; the
    /// harness seeds the server's data dir from there so the server never
    /// fetches.
    Local,
}

/// One server launch configuration — the knob matrix an A/B run varies
/// between its baseline and candidate.
pub struct LaunchConfig<'a> {
    /// Embedding provider (`none` = zero-LLM/vectors off, `local` = vectors on).
    pub embeddings: EvalEmbeddings,
    /// When `embeddings == Local`, the models root to seed the server from.
    pub models_root: Option<&'a std::path::Path>,
    /// Document-embedding cache root (`AI_MEMORY_EVAL_EMBEDDING_CACHE_DIR`),
    /// `None` for a cold run.
    pub embedding_cache: Option<&'a std::path::Path>,
    /// Turn the post-RRF reranker on (`AI_MEMORY_RERANKER=llm`). Requires
    /// an LLM provider to be supplied through `env` — the harness does not
    /// configure one, so a bare `reranker=true` with no provider is a no-op
    /// the server logs and ignores.
    pub reranker: bool,
    /// Extra server env overrides, applied last so a config can set any
    /// server knob (reranker provider, retrieval flags, future toggles)
    /// without a new field here. These win over the harness defaults.
    pub env: &'a [(String, String)],
}

/// Keep operating-system settings while removing unrecorded `AI_MEMORY_*`
/// values; the explicit benchmark overrides are applied afterward.
fn clear_ambient_product_env(cmd: &mut Command, keys: impl IntoIterator<Item = OsString>) {
    for key in keys {
        if key
            .as_encoded_bytes()
            .get(..10)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"AI_MEMORY_"))
        {
            cmd.env_remove(key);
        }
    }
}

impl EvalServer {
    /// Launch `server_bin` on a free loopback port with a fresh data dir.
    pub async fn launch(
        server_bin: &PathBuf,
        keep_data_dir: bool,
        config: &LaunchConfig<'_>,
    ) -> Result<Self> {
        let embeddings = config.embeddings;
        let models_root = config.models_root;
        let tmp = tempfile::Builder::new()
            .prefix("ai-memory-eval-")
            .tempdir()
            .context("creating eval data dir")?;
        let data_dir_path = tmp.path().to_path_buf();

        // Reserve a free port; the tiny close-to-bind window is fine for a
        // local benchmark run.
        let port = {
            let l = TcpListener::bind("127.0.0.1:0")?;
            l.local_addr()?.port()
        };
        let bind = format!("127.0.0.1:{port}");

        if embeddings == EvalEmbeddings::Local {
            let src = models_root
                .ok_or_else(|| anyhow::anyhow!("local embeddings need a models root"))?
                .join("all-MiniLM-L6-v2");
            let dest = data_dir_path.join("models/all-MiniLM-L6-v2");
            std::fs::create_dir_all(&dest)?;
            for entry in std::fs::read_dir(&src).context("reading model dir")? {
                let entry = entry?;
                std::fs::copy(entry.path(), dest.join(entry.file_name()))?;
            }
        }
        let mut cmd = Command::new(server_bin);
        clear_ambient_product_env(&mut cmd, std::env::vars_os().map(|(key, _)| key));
        cmd.args([
            "serve",
            "--transport",
            "http",
            "--bind",
            &bind,
            "--data-dir",
        ])
        .arg(&data_dir_path)
        .env("AI_MEMORY_AUTH_TOKEN", EVAL_AUTH_TOKEN)
        .env("AI_MEMORY_CAPTURE_ASSISTANT", "true")
        // The knob matrix below explicitly selects a provider and reranker;
        // do not inherit the common provider keys into the child either.
        .env_remove("AI_MEMORY_LLM_PROVIDER")
        .env_remove("AI_MEMORY_RERANKER")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("LLM_API_KEY");
        match embeddings {
            EvalEmbeddings::None => {
                // Explicit: with 2.0's default-on local embeddings, an
                // UNSET provider would start a background model download
                // per spawned server. The baseline must be hermetic.
                cmd.env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
            }
            EvalEmbeddings::Local => {
                cmd.env("AI_MEMORY_EMBEDDING_PROVIDER", "local");
            }
        }
        // Eval-only server hook: reuse haystack document vectors across runs.
        // An inherited value would silently warm a run meant to be cold.
        match config.embedding_cache {
            Some(root) => cmd.env("AI_MEMORY_EVAL_EMBEDDING_CACHE_DIR", root),
            None => cmd.env_remove("AI_MEMORY_EVAL_EMBEDDING_CACHE_DIR"),
        };
        if config.reranker {
            // Only `llm` is a supported reranker; it needs a provider,
            // which the config must supply via `env`.
            cmd.env("AI_MEMORY_RERANKER", "llm");
        }
        // Config env overrides win over every default above.
        for (key, value) in config.env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {} serve", server_bin.display()))?;

        // Drain stderr continuously so the server never blocks on a full
        // pipe, keeping the captured text for failure diagnostics.
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = stderr.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match pipe.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut s) = sink.lock() {
                                s.push_str(&String::from_utf8_lossy(&buf[..n]));
                            }
                        }
                    }
                }
            });
        }

        let base_url = format!("http://{bind}");
        let (guard, data_dir_path) = if keep_data_dir {
            // Persist the tempdir so it survives the run for inspection.
            (None, tmp.keep())
        } else {
            (Some(tmp), data_dir_path)
        };
        let mut server = Self {
            child,
            base_url,
            _data_dir: guard,
            data_dir_path,
            stderr,
        };
        server.wait_ready().await?;
        Ok(server)
    }

    /// Snapshot of the child's captured stderr so far.
    fn stderr_snapshot(&self) -> String {
        self.stderr
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "(stderr capture poisoned)".into())
    }

    /// Poll until the HTTP surface answers (any status counts as up).
    async fn wait_ready(&mut self) -> Result<()> {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "eval server exited during startup ({status}): {}",
                    self.stderr_snapshot()
                );
            }
            if client
                .get(format!("{}/", self.base_url))
                .timeout(Duration::from_secs(1))
                .send()
                .await
                .is_ok()
            {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "eval server did not answer on {} within 120s: {}",
                    self.base_url,
                    self.stderr_snapshot()
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for EvalServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_settings_are_removed_without_dropping_system_environment() {
        let mut cmd = Command::new("fixture");
        let keys = [
            OsString::from("AI_MEMORY_INGEST__DROP_EVENTS"),
            OsString::from("ai_memory_RERANKER"),
            OsString::from("Ai_MeMoRy_AUTH_TOKEN"),
            OsString::from("PATH"),
            OsString::from("SYSTEMROOT"),
        ];
        clear_ambient_product_env(&mut cmd, keys.clone());
        let removed: Vec<_> = cmd
            .get_envs()
            .filter_map(|(key, value)| value.is_none().then_some(key.to_owned()))
            .collect();
        assert_eq!(removed.len(), 3);
        for key in &keys[..3] {
            assert!(removed.contains(key), "missing removal for {key:?}");
        }
        for key in &keys[3..] {
            assert!(!removed.contains(key), "system variable removed: {key:?}");
        }
    }
}
