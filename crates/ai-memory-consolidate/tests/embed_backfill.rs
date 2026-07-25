//! Integration tests for [`run_embedding_backfill`]: candidate
//! scanning, skip/dry-run/reembed semantics, and batched writes, using
//! a temp-dir store + wiki and the deterministic `SyntheticEmbedder`.

use std::sync::Arc;

use ai_memory_consolidate::{EmbedBackfillCounts, EmbedBackfillOptions, run_embedding_backfill};
use ai_memory_core::{PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{Embedder, SyntheticEmbedder};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
    embedder: Arc<dyn Embedder>,
}

async fn fixture() -> Fixture {
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
    // No embedder attached: pages written here start without embedding
    // rows so the backfill has work to do.
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    Fixture {
        _tmp: tmp,
        store,
        wiki,
        ws,
        proj,
        embedder,
    }
}

impl Fixture {
    async fn write_page(&self, path: &str, body: &str) {
        self.wiki
            .write_page(WritePageRequest {
                workspace_id: self.ws,
                project_id: self.proj,
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

    async fn embedded_count(&self) -> usize {
        self.store
            .reader
            .embedded_page_ids(
                self.ws,
                self.proj,
                self.embedder.provider().to_string(),
                self.embedder.model().to_string(),
                self.embedder.dim(),
            )
            .await
            .expect("embedded page ids")
            .len()
    }

    async fn backfill(&self, reembed: bool, dry_run: bool) -> EmbedBackfillCounts {
        run_embedding_backfill(
            &self.store.reader,
            &self.store.writer,
            &self.wiki,
            &self.embedder,
            self.ws,
            self.proj,
            EmbedBackfillOptions { reembed, dry_run },
        )
        .await
        .expect("backfill")
    }
}

#[tokio::test]
async fn backfill_embeds_missing_and_skips_current_and_empty_pages() {
    let f = fixture().await;
    f.write_page("notes/a.md", "writer actor uses an mpsc channel")
        .await;
    f.write_page("notes/b.md", "hybrid retrieval combines fts and vectors")
        .await;
    f.write_page("notes/empty.md", "   \n").await;

    let counts = f.backfill(false, false).await;
    assert_eq!(counts.embedded, 2);
    assert_eq!(counts.failed, 0);
    assert_eq!(
        counts.skipped, 1,
        "the whitespace-only page should be skipped"
    );
    assert_eq!(f.embedded_count().await, 2);

    let second = f.backfill(false, false).await;
    assert_eq!(second.embedded, 0, "second pass embeds nothing new");
    assert_eq!(
        second.skipped, 3,
        "already-embedded pages and the empty page are skipped"
    );
    assert_eq!(f.embedded_count().await, 2);
}

#[tokio::test]
async fn backfill_dry_run_counts_without_writing() {
    let f = fixture().await;
    f.write_page("notes/a.md", "dry run should not write embeddings")
        .await;

    let counts = f.backfill(false, true).await;
    assert_eq!(counts.would_embed, 1);
    assert_eq!(counts.embedded, 0);
    assert_eq!(
        f.embedded_count().await,
        0,
        "dry run must not store embeddings"
    );
}

#[tokio::test]
async fn backfill_reembed_regenerates_existing_rows() {
    let f = fixture().await;
    f.write_page("notes/a.md", "reembed regenerates current rows")
        .await;
    assert_eq!(f.backfill(false, false).await.embedded, 1);

    let counts = f.backfill(true, false).await;
    assert_eq!(
        counts.embedded, 1,
        "reembed should regenerate the existing row"
    );
    assert_eq!(counts.skipped, 0);
    assert_eq!(f.embedded_count().await, 1);
}
