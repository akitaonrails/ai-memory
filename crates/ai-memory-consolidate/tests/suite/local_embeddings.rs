//! Local-embeddings usefulness suite (2.0 item 5).
//!
//! The unit tests in `ai-memory-llm` prove the plumbing (checksums,
//! loading, unit norms). These prove the *point*: with the local
//! embedder wired into the real write + hybrid-search path, a query
//! that shares **no tokens** with the target page still finds it —
//! and the FTS-only control in the same test misses it, so the vector
//! stream is demonstrably the reason.
//!
//! `#[ignore]`d like the eval smoke: the ~87 MB model is not in CI.
//! Run after fetching the model (any of these seeds it):
//!
//! ```bash
//! cargo run -p ai-memory-eval -- retrieval --sample 1 --embeddings local
//! AI_MEMORY_TEST_MODELS_DIR=$PWD/evals/models \
//!   cargo test -p ai-memory-consolidate --test local_embeddings -- --ignored
//! ```

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_memory_core::{PagePath, Tier};
use ai_memory_llm::{Embedder, EmbeddingCapacity, LocalEmbedder};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

fn models_root() -> String {
    std::env::var("AI_MEMORY_TEST_MODELS_DIR")
        .expect("set AI_MEMORY_TEST_MODELS_DIR to a dir containing all-MiniLM-L6-v2")
}

async fn wiki_with_local_embedder() -> (
    TempDir,
    Store,
    Wiki,
    Arc<dyn Embedder>,
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let embedder: Arc<dyn Embedder> =
        Arc::new(LocalEmbedder::load(Path::new(&models_root())).unwrap());
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(embedder.clone());
    (tmp, store, wiki, embedder, ws, proj)
}

async fn write(
    wiki: &Wiki,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    path: &str,
    body: &str,
) {
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        frontmatter: serde_json::json!({"title": path}),
        body: body.to_string(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
        trust_class: None,
    })
    .await
    .unwrap();
}

/// The usefulness proof: the query shares NO tokens with the target
/// body ("deploy/ship/version" vs "release/tagging/pushing/registry"),
/// so FTS alone cannot rank it — and doesn't (the control). The vector
/// stream does.
#[tokio::test]
#[ignore = "needs the fetched all-MiniLM-L6-v2 model files"]
async fn paraphrase_recall_fts_alone_cannot_do() {
    let (_tmp, store, wiki, embedder, ws, proj) = wiki_with_local_embedder().await;

    // Bodies deliberately avoid every token of the query — including
    // stopwords, because our FTS has no stopword list and BM25 on a
    // tiny corpus is otherwise dominated by "the"-frequency noise (a
    // real lexical-only failure, but not the one under test here).
    //
    // The target answers the query with zero shared tokens ("rollout /
    // shipping / build" vs "deploy / app / production").
    write(
        &wiki,
        ws,
        proj,
        "procedures/release.md",
        "Rollout procedure for shipping a build: tag main, wait for CI, \
         push image into registry, restart compose stack.",
    )
    .await;
    // The lexical decoy matches a content word of the query
    // ("production") while being semantically unrelated — the ranking
    // mistake BM25 must make and the vector stream must correct.
    write(
        &wiki,
        ws,
        proj,
        "notes/olive-oil.md",
        "Production of olive oil: harvest olives, press, filter, bottle.",
    )
    .await;
    write(
        &wiki,
        ws,
        proj,
        "notes/pasta.md",
        "Boil salted water, add spaghetti, stir occasionally.",
    )
    .await;
    write(
        &wiki,
        ws,
        proj,
        "notes/gardening.md",
        "Tomatoes need six hours direct sun plus weekly fertiliser.",
    )
    .await;

    let q = "how do we deploy the app to production?";

    // Control first: FTS + entity + graph, no vector stream. The decoy
    // wins on its real matches and the answer is not in the list at all
    // — zero shared tokens means lexical retrieval CANNOT surface it.
    let lexical = store
        .reader
        .hybrid_search(
            ws,
            proj,
            q.to_string(),
            None,
            "local".into(),
            "all-MiniLM-L6-v2".into(),
            384,
            4,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        lexical.first().map(|h| h.path.as_str()),
        Some("notes/olive-oil.md"),
        "control violated: without vectors the lexical decoy must win: {lexical:?}"
    );
    assert!(
        !lexical
            .iter()
            .any(|h| h.path.as_str() == "procedures/release.md"),
        "control violated: the answer shares no token with the query and \
         must be lexically invisible: {lexical:?}"
    );

    // With the vector stream the lexically-invisible answer is
    // RETRIEVED — that is the recall guarantee local embeddings add.
    // (Rank note, kept honest: RRF fusion can still place a page that
    // appears weakly in TWO streams above a page that is strong in one
    // — the olive decoy here. Whether the fusion should weight vector
    // strength differently is a tuning question the LongMemEval
    // harness measures; this test pins recall, not rank.)
    let qv = embedder.embed_query(q).await.unwrap();
    let hybrid = store
        .reader
        .hybrid_search(
            ws,
            proj,
            q.to_string(),
            Some(qv.clone()),
            "local".into(),
            "all-MiniLM-L6-v2".into(),
            384,
            4,
            None,
            false,
        )
        .await
        .unwrap();
    assert!(
        hybrid
            .iter()
            .any(|h| h.path.as_str() == "procedures/release.md"),
        "semantic recall failed - the answer stayed invisible: {hybrid:?}"
    );

    // And the vector ordering itself puts the answer first by a wide
    // margin - the stream is not just present, it is RIGHT.
    let dot = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
    let release = embedder
        .embed_document(
            "Rollout procedure for shipping a build: tag main, wait for CI, \
             push image into registry, restart compose stack.",
        )
        .await
        .unwrap();
    let olive = embedder
        .embed_document("Production of olive oil: harvest olives, press, filter, bottle.")
        .await
        .unwrap();
    assert!(
        dot(&qv, &release) > dot(&qv, &olive) + 0.15,
        "the answer must beat the lexical decoy in vector space by a clear \
         margin: release={:.3} olive={:.3}",
        dot(&qv, &release),
        dot(&qv, &olive)
    );
}

/// Stored rows carry the local triple and pass the mismatch diagnostic
/// — coexistence with provider vectors rests on this metadata.
#[tokio::test]
#[ignore = "needs the fetched all-MiniLM-L6-v2 model files"]
async fn local_rows_carry_the_pinned_triple() {
    let (_tmp, store, wiki, _embedder, ws, proj) = wiki_with_local_embedder().await;
    write(&wiki, ws, proj, "notes/one.md", "a single page").await;

    let stored = store
        .reader
        .load_embeddings(ws, proj, "local".into(), "all-MiniLM-L6-v2".into(), 384)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    let norm: f32 = stored[0].vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4, "stored vector not unit: {norm}");

    // A different dim reports the stored triple as mismatched.
    let mismatch = store
        .reader
        .embedding_meta_for_mismatch("local".into(), "all-MiniLM-L6-v2".into(), 768)
        .await
        .unwrap();
    assert_eq!(mismatch.len(), 1);
    assert_eq!(mismatch[0].2, 384);
}

/// Same text, same vector — the reproducibility the pinned model files
/// promise (no provider-side model drift).
#[tokio::test]
#[ignore = "needs the fetched all-MiniLM-L6-v2 model files"]
async fn embedding_is_deterministic() {
    let embedder = LocalEmbedder::load(Path::new(&models_root())).unwrap();
    let a = embedder
        .embed("the writer actor coalesces WAL commits")
        .await
        .unwrap();
    let b = embedder
        .embed("the writer actor coalesces WAL commits")
        .await
        .unwrap();
    assert_eq!(a, b, "same input must produce identical vectors");
}

fn percentile(mut samples: Vec<Duration>, percentile: usize) -> Duration {
    samples.sort_unstable();
    let index = (samples.len() * percentile).div_ceil(100).saturating_sub(1);
    samples[index]
}

async fn measure_queries(embedder: &Arc<dyn Embedder>, count: usize) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(count);
    for sequence in 0..count {
        let started = Instant::now();
        embedder
            .embed_query(&format!(
                "where is the deployment runbook for production? sample {sequence}"
            ))
            .await
            .unwrap();
        samples.push(started.elapsed());
    }
    samples
}

async fn measure_queries_under_document_load(
    embedder: &Arc<dyn Embedder>,
    workers: usize,
    document: &str,
    query_samples: usize,
    documents_per_worker: usize,
) -> Vec<Duration> {
    let barrier = Arc::new(tokio::sync::Barrier::new(workers + 1));
    let mut background = Vec::with_capacity(workers);
    for worker in 0..workers {
        let embedder = Arc::clone(embedder);
        let barrier = Arc::clone(&barrier);
        let document = document.to_owned();
        background.push(tokio::spawn(async move {
            barrier.wait().await;
            for sequence in 0..documents_per_worker {
                embedder
                    .embed_document(&format!("{document} worker {worker} item {sequence}"))
                    .await
                    .unwrap();
            }
        }));
    }

    barrier.wait().await;
    let loaded = measure_queries(embedder, query_samples).await;
    for task in background {
        task.await.unwrap();
    }
    loaded
}

/// Manual capacity probe for the shared local embedder. It is deliberately a
/// benchmark, not a CI assertion: CPU topology and scheduler load determine
/// the ratio. Run it before adding an admission-control policy so the policy
/// responds to measured contention rather than an assumed bottleneck.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual local-model contention benchmark"]
async fn local_query_latency_under_document_load() {
    const QUERY_SAMPLES: usize = 9;
    const DOCUMENTS_PER_WORKER: usize = 12;

    let embedder: Arc<dyn Embedder> =
        Arc::new(LocalEmbedder::load(Path::new(&models_root())).unwrap());
    embedder.embed_query("warm the local model").await.unwrap();

    let idle = measure_queries(&embedder, QUERY_SAMPLES).await;
    let workers = std::env::var("AI_MEMORY_EMBED_BENCH_WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(2)
                .clamp(2, 8)
        });
    assert!(workers > 0, "benchmark needs at least one worker");
    let document = "A long-lived project memory page describes architecture decisions, \
        deployment procedures, incident recovery, database migrations, authentication \
        boundaries, contributor workflow, release validation, and operational runbooks. "
        .repeat(32);
    let loaded = measure_queries_under_document_load(
        &embedder,
        workers,
        &document,
        QUERY_SAMPLES,
        DOCUMENTS_PER_WORKER,
    )
    .await;
    let cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4);
    let bounded: Arc<dyn Embedder> =
        Arc::new(EmbeddingCapacity::new(embedder, ai_memory_llm::local_capacity(cores)).unwrap());
    let bounded_loaded = measure_queries_under_document_load(
        &bounded,
        workers,
        &document,
        QUERY_SAMPLES,
        DOCUMENTS_PER_WORKER,
    )
    .await;

    let idle_p50 = percentile(idle.clone(), 50);
    let idle_p95 = percentile(idle, 95);
    let loaded_p50 = percentile(loaded.clone(), 50);
    let loaded_p95 = percentile(loaded, 95);
    let bounded_p50 = percentile(bounded_loaded.clone(), 50);
    let bounded_p95 = percentile(bounded_loaded, 95);
    eprintln!(
        "local embedding contention: workers={workers} idle_p50_ms={} \
         idle_p95_ms={} loaded_p50_ms={} loaded_p95_ms={} \
         loaded_p50_ratio={:.2} loaded_p95_ratio={:.2} \
         bounded_p50_ms={} bounded_p95_ms={} bounded_p50_ratio={:.2} \
         bounded_p95_ratio={:.2}",
        idle_p50.as_millis(),
        idle_p95.as_millis(),
        loaded_p50.as_millis(),
        loaded_p95.as_millis(),
        loaded_p50.as_secs_f64() / idle_p50.as_secs_f64(),
        loaded_p95.as_secs_f64() / idle_p95.as_secs_f64(),
        bounded_p50.as_millis(),
        bounded_p95.as_millis(),
        bounded_p50.as_secs_f64() / idle_p50.as_secs_f64(),
        bounded_p95.as_secs_f64() / idle_p95.as_secs_f64(),
    );
}

/// A handful of synonym/paraphrase orderings beyond the single case in
/// the unit test — cheap confidence that the pooling isn't degenerate.
#[tokio::test]
#[ignore = "needs the fetched all-MiniLM-L6-v2 model files"]
async fn synonym_pairs_rank_above_unrelated_text() {
    let embedder = LocalEmbedder::load(Path::new(&models_root())).unwrap();
    let dot = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
    for (anchor, near, far) in [
        (
            "fix the memory leak",
            "resolve the RAM growth bug",
            "paint the fence",
        ),
        (
            "user authentication flow",
            "login and session handling",
            "banana bread recipe",
        ),
        (
            "database schema migration",
            "altering tables between versions",
            "morning jog route",
        ),
    ] {
        let a = embedder.embed(anchor).await.unwrap();
        let n = embedder.embed(near).await.unwrap();
        let f = embedder.embed(far).await.unwrap();
        assert!(
            dot(&a, &n) > dot(&a, &f),
            "{anchor:?}: near {:.3} must beat far {:.3}",
            dot(&a, &n),
            dot(&a, &f)
        );
    }
}
