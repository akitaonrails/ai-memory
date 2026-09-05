//! M9 vector-retrieval integration test.
//!
//! Demonstrates that:
//! 1. Writing a page through `Wiki::write_page` with an embedder
//!    attached actually persists an embedding row.
//! 2. The hybrid RRF path returns sensible results.
//! 3. The mismatch diagnostic query flags heterogeneous
//!    `(provider, model, dim)` triples.
//!
//! The test uses [`SyntheticEmbedder`] — a deterministic bag-of-words
//! embedder shipped specifically for tests like this so no API key is
//! needed. It doesn't capture real semantic similarity (a different
//! conjugation hashes to a different dimension), but it exercises the
//! full plumbing end-to-end.

use std::sync::Arc;

use ai_memory_core::{PagePath, Tier};
use ai_memory_llm::{Embedder, SyntheticEmbedder};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

#[tokio::test]
async fn m9_embeddings_roundtrip_via_synthetic() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .expect("proj");
    let embedder: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(64));
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_embedder(embedder.clone());

    let pages = [
        (
            "notes/karpathy.md",
            "Karpathy LLM Wiki: compile not retrieve, the artifact is the wiki",
        ),
        (
            "notes/writer.md",
            "writer actor uses an mpsc bounded channel for backpressure",
        ),
        (
            "notes/k8s.md",
            "kubernetes pod scheduling rules and node selectors",
        ),
    ];

    for (path, body) in pages {
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path.to_string()).expect("path"),
            frontmatter: serde_json::json!({"title": path}),
            body: body.to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .expect("write page");
    }

    // 1. Embedding rows landed for each page.
    let stored = store
        .reader
        .load_embeddings(ws, proj, "synthetic".into(), "bag-of-words-v1".into(), 64)
        .await
        .expect("load embeddings");
    assert_eq!(
        stored.len(),
        3,
        "every page with an embedder attached should get an embedding row, got {}",
        stored.len(),
    );

    // Stored vectors are unit-normalised (Embedder contract).
    for s in &stored {
        let norm: f32 = s.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "stored vector should be unit-normalised, got |v|={norm}",
        );
    }

    // 2. Mismatch diagnostics report stale triples.
    let mismatch_with_wrong_dim = store
        .reader
        .embedding_meta_for_mismatch(
            "synthetic".into(),
            "bag-of-words-v1".into(),
            128, // wrong dim
        )
        .await
        .expect("mismatch query");
    assert_eq!(
        mismatch_with_wrong_dim.len(),
        1,
        "querying with a different dim should surface the existing one as mismatch",
    );
    let (provider, model, existing_dim, count) = &mismatch_with_wrong_dim[0];
    assert_eq!(provider, "synthetic");
    assert_eq!(model, "bag-of-words-v1");
    assert_eq!(*existing_dim, 64);
    assert_eq!(*count, 3);

    let mismatch_clean = store
        .reader
        .embedding_meta_for_mismatch("synthetic".into(), "bag-of-words-v1".into(), 64)
        .await
        .expect("mismatch query (clean)");
    assert!(
        mismatch_clean.is_empty(),
        "matching (provider, model, dim) should report no mismatch, got {mismatch_clean:?}",
    );

    // 3. Hybrid search end-to-end.
    let q = "karpathy compile artifact";
    let qv = embedder.embed_query(q).await.expect("embed query");
    let hybrid_hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            q.to_string(),
            Some(qv),
            "synthetic".into(),
            "bag-of-words-v1".into(),
            64,
            5,
            None,
        )
        .await
        .expect("hybrid search");
    assert!(
        !hybrid_hits.is_empty(),
        "hybrid search should return at least one hit for {q:?}",
    );
    let top_path = hybrid_hits[0].path.as_str();
    assert_eq!(
        top_path, "notes/karpathy.md",
        "the karpathy page should be the top hybrid hit; got {top_path}",
    );

    // 4. Without a query vector, FTS5 + entity + graph still work through the
    //    same method; only the vector stream is absent.
    let lexical_graph = store
        .reader
        .hybrid_search(
            ws,
            proj,
            q.to_string(),
            None,
            "synthetic".into(),
            "bag-of-words-v1".into(),
            64,
            5,
            None,
        )
        .await
        .expect("hybrid (no query vec)");
    assert!(
        !lexical_graph.is_empty(),
        "FTS5 + entity + graph fallback should still return Karpathy page",
    );

    // 5. Re-writing a page produces a fresh embedding row (the
    //    ON CONFLICT in store_embedding does the replace).
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("notes/karpathy.md".to_string()).expect("path"),
        frontmatter: serde_json::json!({"title": "Karpathy revised"}),
        body: "Karpathy LLM Wiki: revised body, still compile not retrieve".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    })
    .await
    .expect("rewrite page");
    let stored_after = store
        .reader
        .load_embeddings(ws, proj, "synthetic".into(), "bag-of-words-v1".into(), 64)
        .await
        .expect("load embeddings after rewrite");
    // Still 3 latest pages, still 3 embeddings — the rewrite replaced
    // the karpathy row, didn't add a new one.
    assert_eq!(
        stored_after.len(),
        3,
        "rewriting the same path should keep us at 3 embeddings, not accumulate",
    );
}
