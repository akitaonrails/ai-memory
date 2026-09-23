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
                evidence: Vec::new(),
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

/// A fake embedder standing in for `OpenAiEmbedder`/`OpenAiCompatEmbedder`
/// with a configurable document prefix: `identity_suffix` plays the role a
/// non-empty `document_prefix` plays there (`Embedder::model_identity`
/// diverges from `Embedder::model` when set), and `fail_on_substring` lets
/// one test drive a genuine per-page provider failure without a real
/// network call.
struct ConfigurableEmbedder {
    dim: u32,
    identity_suffix: String,
    fail_on_substring: Option<&'static str>,
}

#[async_trait::async_trait]
impl Embedder for ConfigurableEmbedder {
    fn provider(&self) -> &'static str {
        "synthetic"
    }

    fn model(&self) -> &str {
        "bag-of-words-v1"
    }

    fn model_identity(&self) -> String {
        if self.identity_suffix.is_empty() {
            self.model().to_string()
        } else {
            format!("{}+{}", self.model(), self.identity_suffix)
        }
    }

    fn dim(&self) -> u32 {
        self.dim
    }

    async fn embed(&self, text: &str) -> ai_memory_llm::LlmResult<Vec<f32>> {
        if let Some(marker) = self.fail_on_substring
            && text.contains(marker)
        {
            return Err(ai_memory_llm::LlmError::Provider {
                status: 500,
                body: "synthetic configured failure".into(),
            });
        }
        Ok(vec![0.5_f32; self.dim as usize])
    }
}

/// A document-prefix change (a new `identity_suffix` here) must make a
/// **normal** (non-force) backfill pass treat every page as needing a
/// fresh embedding, exactly like a model change would — this is what
/// keeps `memory_query`'s retrieval eligibility filter (keyed on the
/// currently configured embedder's `model_identity`) from ever mixing
/// vectors embedded under different prefixes. The old row is left in
/// place (nothing here deletes it — deletion is a separate, explicit step
/// mirroring `POST /admin/embed`'s `reembed` purge, covered by the next
/// test), but it is no longer reachable under the newly configured
/// identity.
#[tokio::test]
async fn document_prefix_change_makes_backfill_treat_pages_as_stale_and_reembeds_them() {
    let f = fixture().await;
    f.write_page("notes/a.md", "alpha content").await;

    let legacy: Arc<dyn Embedder> = Arc::new(ConfigurableEmbedder {
        dim: 8,
        identity_suffix: String::new(),
        fail_on_substring: None,
    });
    let first = run_embedding_backfill(
        &f.store.reader,
        &f.store.writer,
        &f.wiki,
        &legacy,
        f.ws,
        f.proj,
        EmbedBackfillOptions::default(),
    )
    .await
    .expect("first backfill");
    assert_eq!(first.embedded, 1);

    // Simulates reconfiguring `embedding_document_prefix`: same provider,
    // same wire model, same dim — only the document-side identity differs.
    let prefixed: Arc<dyn Embedder> = Arc::new(ConfigurableEmbedder {
        dim: 8,
        identity_suffix: "dp-test".into(),
        fail_on_substring: None,
    });
    assert_ne!(legacy.model_identity(), prefixed.model_identity());

    // Between the config change and the next backfill pass, the stored
    // row is still under the OLD identity — this is the property the fix
    // protects: a query using the NOW-configured embedder's identity (what
    // `memory_query`'s retrieval eligibility filter does) does not match
    // it, so the page is excluded from vector search rather than silently
    // matched with content embedded under the old prefix.
    let prefixed_ids_before = f
        .store
        .reader
        .embedded_page_ids(
            f.ws,
            f.proj,
            prefixed.provider().to_string(),
            prefixed.model_identity(),
            prefixed.dim(),
        )
        .await
        .expect("prefixed embedded_page_ids before backfill");
    assert_eq!(
        prefixed_ids_before.len(),
        0,
        "the pre-change row must not be matched under the new identity"
    );

    let second = run_embedding_backfill(
        &f.store.reader,
        &f.store.writer,
        &f.wiki,
        &prefixed,
        f.ws,
        f.proj,
        EmbedBackfillOptions::default(), // reembed: false — a NORMAL pass
    )
    .await
    .expect("backfill after a document-prefix change");
    assert_eq!(
        second.embedded, 1,
        "a document-prefix change must be treated as stale and re-embedded, \
         not silently skipped as \"already embedded\""
    );
    assert_eq!(second.skipped, 0);

    // `page_embeddings` is one row per page (replace-on-write), so the
    // fresh write replaces the pre-change row rather than accumulating
    // alongside it: the old identity now matches nothing …
    let legacy_ids_after = f
        .store
        .reader
        .embedded_page_ids(
            f.ws,
            f.proj,
            legacy.provider().to_string(),
            legacy.model_identity(),
            legacy.dim(),
        )
        .await
        .expect("legacy embedded_page_ids after backfill");
    assert_eq!(
        legacy_ids_after.len(),
        0,
        "the pre-change row was replaced, not left behind, by the re-embed"
    );
    // … and the new identity now does.
    let prefixed_ids_after = f
        .store
        .reader
        .embedded_page_ids(
            f.ws,
            f.proj,
            prefixed.provider().to_string(),
            prefixed.model_identity(),
            prefixed.dim(),
        )
        .await
        .expect("prefixed embedded_page_ids after backfill");
    assert_eq!(prefixed_ids_after.len(), 1);
}

/// Mirrors `POST /admin/embed`'s `reembed: true` sequence exactly
/// (`ai-memory-mcp/src/admin.rs::handle_embed`): purge stale rows for the
/// configured triple, then run a forced backfill. One of two pages fails
/// mid-run; the purge must still have applied to both, the surviving page
/// must still get a fresh row, and the failed page must be left with NO
/// row (not a stale one and not a half-written one) rather than silently
/// keeping a purged-then-never-replaced gap invisible to the operator —
/// `counts.failed` is what surfaces it.
#[tokio::test]
async fn force_rebuild_purges_first_and_counts_a_partial_failure_correctly() {
    let f = fixture().await;
    f.write_page("notes/ok.md", "alpha content").await;
    f.write_page("notes/bad.md", "FAIL_MARKER content").await;

    // Seed both pages under a stand-in "old" identity, matching a real
    // force-rebuild's starting point: pages already have rows, possibly
    // under a different identity than the one about to be forced.
    let legacy: Arc<dyn Embedder> = Arc::new(ConfigurableEmbedder {
        dim: 8,
        identity_suffix: "old".into(),
        fail_on_substring: None,
    });
    let seeded = run_embedding_backfill(
        &f.store.reader,
        &f.store.writer,
        &f.wiki,
        &legacy,
        f.ws,
        f.proj,
        EmbedBackfillOptions::default(),
    )
    .await
    .expect("seed backfill");
    assert_eq!(seeded.embedded, 2);

    let flaky: Arc<dyn Embedder> = Arc::new(ConfigurableEmbedder {
        dim: 8,
        identity_suffix: String::new(), // the configured (new) identity
        fail_on_substring: Some("FAIL_MARKER"),
    });

    // Step 1 (admin.rs's purge-before-reembed): delete every row in scope
    // that does not match the target triple — here that is BOTH seeded
    // rows, since they are under `legacy`'s identity.
    let purged = f
        .store
        .writer
        .delete_stale_page_embeddings(
            f.ws,
            Some(f.proj),
            flaky.provider().to_string(),
            flaky.model_identity(),
            flaky.dim(),
        )
        .await
        .expect("purge");
    assert_eq!(
        purged, 2,
        "purge removes every row under a non-matching identity"
    );

    // Step 2: forced backfill with the embedder that fails on notes/bad.md.
    let counts = run_embedding_backfill(
        &f.store.reader,
        &f.store.writer,
        &f.wiki,
        &flaky,
        f.ws,
        f.proj,
        EmbedBackfillOptions {
            reembed: true,
            dry_run: false,
        },
    )
    .await
    .expect("forced backfill with a partial failure");
    assert_eq!(counts.embedded, 1, "notes/ok.md succeeds");
    assert_eq!(counts.failed, 1, "notes/bad.md's provider call fails");

    let current_ids = f
        .store
        .reader
        .embedded_page_ids(
            f.ws,
            f.proj,
            flaky.provider().to_string(),
            flaky.model_identity(),
            flaky.dim(),
        )
        .await
        .expect("embedded_page_ids after the forced rebuild");
    assert_eq!(
        current_ids.len(),
        1,
        "exactly the succeeding page has a row under the new identity; \
         the failed page is left with none — purged, not silently kept \
         stale, and not half-written — so it surfaces as `failed` for the \
         operator to retry rather than looking either fully migrated or \
         still on the old prefix"
    );
}
