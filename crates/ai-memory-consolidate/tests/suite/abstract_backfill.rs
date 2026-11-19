//! The L0 abstract backfill: a page whose frontmatter carries `abstract:`
//! gets that line embedded into `page_abstract_embeddings` by the same
//! backfill pass as its body, so the opt-in `[retrieval] abstract_vectors`
//! stream has data even for pages written before the feature (or with the
//! embedder detached).

use std::sync::Arc;

use ai_memory_core::{PagePath, Tier};
use ai_memory_llm::{Embedder, SyntheticEmbedder};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

#[tokio::test]
async fn embedding_backfill_embeds_frontmatter_abstract() {
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
    // Write with the embedder detached: the backfill under test owns both
    // the body and the abstract embedding passes.
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("notes/with-abstract.md").unwrap(),
        frontmatter: serde_json::json!({
            "title": "with abstract",
            "abstract": "one-line summary of the page for the abstract stream"
        }),
        body: "alpha bravo".to_string(),
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

    let embedder: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(64));
    let counts = ai_memory_consolidate::run_embedding_backfill(
        &store.reader,
        &store.writer,
        &wiki,
        &embedder,
        ws,
        proj,
        ai_memory_consolidate::EmbedBackfillOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(counts.embedded, 1, "body embedded");
    assert_eq!(counts.abstracts_embedded, 1, "abstract embedded");
    let abstract_ids = store
        .reader
        .abstract_embedded_page_ids(
            ws,
            proj,
            "synthetic".to_string(),
            "bag-of-words-v1".to_string(),
            64,
        )
        .await
        .unwrap();
    assert_eq!(abstract_ids.len(), 1);

    // A page without the key is untouched by the abstract pass.
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("notes/plain.md").unwrap(),
        frontmatter: serde_json::json!({"title": "plain"}),
        body: "charlie delta".to_string(),
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
    let counts = ai_memory_consolidate::run_embedding_backfill(
        &store.reader,
        &store.writer,
        &wiki,
        &embedder,
        ws,
        proj,
        ai_memory_consolidate::EmbedBackfillOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(counts.embedded, 1, "plain page body embedded");
    assert_eq!(counts.abstracts_embedded, 0, "no abstract to embed");
}

/// A dry run must count the page it would call the embedder for even when only
/// the L0 abstract vector is missing. That is the population the abstract
/// backfill documents itself as serving — pages written before the feature, or
/// with the embedder detached — so reporting zero work tells the operator to
/// skip a run that would in fact write.
#[tokio::test]
async fn dry_run_counts_a_page_needing_only_an_abstract() {
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("notes/with-abstract.md").unwrap(),
        frontmatter: serde_json::json!({
            "title": "with abstract",
            "abstract": "one-line summary of the page for the abstract stream"
        }),
        body: "alpha bravo".to_string(),
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

    let embedder: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(64));
    let opts = ai_memory_consolidate::EmbedBackfillOptions::default();

    // A live run lands both vectors, so the page is fully up to date.
    let counts = ai_memory_consolidate::run_embedding_backfill(
        &store.reader,
        &store.writer,
        &wiki,
        &embedder,
        ws,
        proj,
        opts,
    )
    .await
    .unwrap();
    assert_eq!(counts.embedded, 1);
    assert_eq!(counts.abstracts_embedded, 1);

    // Control: with both vectors present the dry run reports no work.
    let dry = ai_memory_consolidate::run_embedding_backfill(
        &store.reader,
        &store.writer,
        &wiki,
        &embedder,
        ws,
        proj,
        ai_memory_consolidate::EmbedBackfillOptions {
            dry_run: true,
            ..opts
        },
    )
    .await
    .unwrap();
    assert_eq!(
        dry.would_embed, 0,
        "control: nothing to do while both vectors are current"
    );
    assert_eq!(dry.skipped, 1, "control: the page is counted as up to date");

    // Drop ONLY the abstract vector: `need_body` is false, `need_abstract` is
    // true — the case the dry run used to drop from every count.
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let removed = conn
        .execute("DELETE FROM page_abstract_embeddings", [])
        .unwrap();
    assert_eq!(removed, 1, "exactly the one abstract vector was removed");

    let dry = ai_memory_consolidate::run_embedding_backfill(
        &store.reader,
        &store.writer,
        &wiki,
        &embedder,
        ws,
        proj,
        ai_memory_consolidate::EmbedBackfillOptions {
            dry_run: true,
            ..opts
        },
    )
    .await
    .unwrap();
    assert_eq!(
        dry.would_embed, 1,
        "the abstract-only page is counted as work the live run would do"
    );
    assert_eq!(
        dry.skipped, 0,
        "and it is not reported as already up to date"
    );

    // And a live run does in fact write that vector, so the dry run was right.
    let counts = ai_memory_consolidate::run_embedding_backfill(
        &store.reader,
        &store.writer,
        &wiki,
        &embedder,
        ws,
        proj,
        opts,
    )
    .await
    .unwrap();
    assert_eq!(counts.abstracts_embedded, 1);
    assert_eq!(counts.embedded, 0, "the body vector was already current");
}
