//! Embedding backfill — embed latest wiki pages that lack a current
//! `(provider, model, dim)` embedding row.
//!
//! Shared by the `POST /admin/embed` endpoint (which adds `reembed` /
//! `dry_run` and stale-row purging on top) and the server's scheduled
//! maintenance tick. Candidates are scanned once, embedded one at a
//! time, and flushed to the store in batches of
//! [`EMBEDDING_WRITE_BATCH`] so writes stay inside the single-writer
//! actor.

use std::collections::HashSet;
use std::sync::Arc;

use ai_memory_core::{ProjectId, WorkspaceId};
use ai_memory_llm::Embedder;
use ai_memory_store::{EmbeddingWrite, ReaderPool, WriterHandle, f32_vec_to_bytes};
use ai_memory_wiki::Wiki;
use serde::Serialize;
use thiserror::Error;
use tracing::warn;

/// Number of embedding rows written per `store_embeddings` call.
pub const EMBEDDING_WRITE_BATCH: usize = 100;

/// Outcome counts for one backfill run.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct EmbedBackfillCounts {
    /// Pages that were actually embedded (zero in dry-run).
    pub embedded: usize,
    /// Pages skipped because a matching embedding already existed or
    /// the body is empty.
    pub skipped: usize,
    /// Pages that failed to embed (read error or provider error).
    pub failed: usize,
    /// Pages that would be embedded in a live run (only meaningful
    /// when `dry_run` was requested).
    pub would_embed: usize,
    /// L0 abstracts (frontmatter `abstract:`) embedded into
    /// `page_abstract_embeddings` — the opt-in fifth retrieval stream.
    pub abstracts_embedded: usize,
}

impl EmbedBackfillCounts {
    /// Fold another run's counts into this one (multi-project sweeps).
    pub fn absorb(&mut self, other: Self) {
        self.embedded += other.embedded;
        self.skipped += other.skipped;
        self.failed += other.failed;
        self.would_embed += other.would_embed;
        self.abstracts_embedded += other.abstracts_embedded;
    }
}

/// Options for one backfill run.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmbedBackfillOptions {
    /// When true, regenerates embeddings even for pages that already
    /// have one matching the current `(provider, model, dim)`.
    pub reembed: bool,
    /// When true, counts pages that would be embedded/skipped without
    /// calling the embedder or writing anything.
    pub dry_run: bool,
}

/// Errors raised by the backfill.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EmbedBackfillError {
    /// Underlying store error.
    #[error(transparent)]
    Store(#[from] ai_memory_store::StoreError),
}

/// Backfill embeddings for one workspace/project.
///
/// Pages whose body is empty (after trimming) are skipped rather than
/// embedded. Per-page read/provider failures increment
/// [`EmbedBackfillCounts::failed`] and do not abort the run; only store
/// errors on the candidate lookups propagate.
///
/// # Errors
/// Propagates any store error encountered while reading candidates or
/// the set of already-embedded page ids.
pub async fn run_embedding_backfill(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    embedder: &Arc<dyn Embedder>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    options: EmbedBackfillOptions,
) -> Result<EmbedBackfillCounts, EmbedBackfillError> {
    let provider = embedder.provider().to_string();
    let model = embedder.model().to_string();
    let dim = embedder.dim();

    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let already: HashSet<_> = if options.reembed {
        HashSet::new()
    } else {
        reader
            .embedded_page_ids(
                workspace_id,
                project_id,
                provider.clone(),
                model.clone(),
                dim,
            )
            .await?
            .into_iter()
            .collect()
    };
    // L0 abstracts ride the same pass: only pages whose frontmatter carries
    // an `abstract:` can need one, so a page with neither a missing body
    // vector nor a missing abstract vector is still skipped without a read.
    let abstract_bearing: HashSet<_> = reader
        .abstract_bearing_page_ids(workspace_id, project_id)
        .await?
        .into_iter()
        .collect();
    let already_abstract: HashSet<_> = if options.reembed {
        HashSet::new()
    } else {
        reader
            .abstract_embedded_page_ids(
                workspace_id,
                project_id,
                provider.clone(),
                model.clone(),
                dim,
            )
            .await?
            .into_iter()
            .collect()
    };

    let mut counts = EmbedBackfillCounts::default();
    let mut pending = Vec::with_capacity(EMBEDDING_WRITE_BATCH);
    let mut pending_abstract = Vec::with_capacity(EMBEDDING_WRITE_BATCH);

    for cand in candidates {
        let need_body = !already.contains(&cand.id);
        let need_abstract =
            abstract_bearing.contains(&cand.id) && !already_abstract.contains(&cand.id);
        if !need_body && !need_abstract {
            counts.skipped += 1;
            continue;
        }
        if options.dry_run {
            if need_body {
                counts.would_embed += 1;
            }
            continue;
        }
        let md = match wiki.read_page(workspace_id, project_id, &cand.path) {
            Ok(md) => md,
            Err(e) => {
                warn!(path = %cand.path, error = %e, "embed: skip unreadable page");
                // Durable, so the page is attributable after the container
                // that logged this is gone (#528).
                let _ = writer
                    .record_embed_failure(
                        cand.id,
                        ai_memory_store::EmbedOutcome::Unreadable,
                        Some(e.to_string()),
                    )
                    .await;
                counts.failed += 1;
                continue;
            }
        };
        if need_body {
            if md.body.trim().is_empty() {
                // Permanent until the body changes. Recorded because a page
                // skipped on every pass is otherwise indistinguishable from an
                // idle one, which is what made #509 undiagnosable.
                let _ = writer
                    .record_embed_failure(
                        cand.id,
                        ai_memory_store::EmbedOutcome::SkippedEmpty,
                        None,
                    )
                    .await;
                counts.skipped += 1;
            } else {
                match embedder.embed_document(&md.body).await {
                    Ok(vec) => pending.push(EmbeddingWrite {
                        page_id: cand.id,
                        vector_bytes: f32_vec_to_bytes(&vec),
                        provider: provider.clone(),
                        model: model.clone(),
                        dim,
                    }),
                    Err(e) => {
                        warn!(path = %cand.path, error = %e, "embed: provider call failed");
                        let _ = writer
                            .record_embed_failure(
                                cand.id,
                                ai_memory_store::EmbedOutcome::Failed,
                                Some(e.to_string()),
                            )
                            .await;
                        counts.failed += 1;
                    }
                }
                if pending.len() >= EMBEDDING_WRITE_BATCH {
                    flush_embedding_batch(writer, &mut pending, &mut counts).await;
                }
            }
        }
        if need_abstract {
            if let Some(abstract_text) = frontmatter_abstract(&md.frontmatter) {
                match embedder.embed_document(abstract_text).await {
                    Ok(vec) => pending_abstract.push(EmbeddingWrite {
                        page_id: cand.id,
                        vector_bytes: f32_vec_to_bytes(&vec),
                        provider: provider.clone(),
                        model: model.clone(),
                        dim,
                    }),
                    Err(e) => {
                        warn!(path = %cand.path, error = %e, "embed: abstract provider call failed");
                        counts.failed += 1;
                    }
                }
                if pending_abstract.len() >= EMBEDDING_WRITE_BATCH {
                    flush_abstract_batch(writer, &mut pending_abstract, &mut counts).await;
                }
            }
        }
    }
    flush_embedding_batch(writer, &mut pending, &mut counts).await;
    flush_abstract_batch(writer, &mut pending_abstract, &mut counts).await;

    Ok(counts)
}

/// The frontmatter `abstract:` line, when it is a non-empty string.
fn frontmatter_abstract(frontmatter: &serde_json::Value) -> Option<&str> {
    frontmatter
        .get("abstract")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

async fn flush_abstract_batch(
    writer: &WriterHandle,
    pending: &mut Vec<EmbeddingWrite>,
    counts: &mut EmbedBackfillCounts,
) {
    if pending.is_empty() {
        return;
    }
    let batch = std::mem::replace(pending, Vec::with_capacity(EMBEDDING_WRITE_BATCH));
    let count = batch.len();
    if let Err(e) = writer.store_abstract_embeddings(batch).await {
        counts.failed += count;
        warn!(count, error = %e, "embed: store_abstract_embeddings failed");
    } else {
        counts.abstracts_embedded += count;
    }
}

async fn flush_embedding_batch(
    writer: &WriterHandle,
    pending: &mut Vec<EmbeddingWrite>,
    counts: &mut EmbedBackfillCounts,
) {
    if pending.is_empty() {
        return;
    }
    let batch = std::mem::replace(pending, Vec::with_capacity(EMBEDDING_WRITE_BATCH));
    let count = batch.len();
    if let Err(e) = writer.store_embeddings(batch).await {
        counts.failed += count;
        warn!(count, error = %e, "embed: store_embeddings failed");
    } else {
        counts.embedded += count;
    }
}
