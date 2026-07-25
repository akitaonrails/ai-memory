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
}

impl EmbedBackfillCounts {
    /// Fold another run's counts into this one (multi-project sweeps).
    pub fn absorb(&mut self, other: Self) {
        self.embedded += other.embedded;
        self.skipped += other.skipped;
        self.failed += other.failed;
        self.would_embed += other.would_embed;
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

    let mut counts = EmbedBackfillCounts::default();
    let mut pending = Vec::with_capacity(EMBEDDING_WRITE_BATCH);

    for cand in candidates {
        if already.contains(&cand.id) {
            counts.skipped += 1;
            continue;
        }
        if options.dry_run {
            counts.would_embed += 1;
            continue;
        }
        let md = match wiki.read_page(workspace_id, project_id, &cand.path) {
            Ok(md) => md,
            Err(e) => {
                warn!(path = %cand.path, error = %e, "embed: skip unreadable page");
                counts.failed += 1;
                continue;
            }
        };
        if md.body.trim().is_empty() {
            counts.skipped += 1;
            continue;
        }
        let vec = match embedder.embed_document(&md.body).await {
            Ok(vec) => vec,
            Err(e) => {
                warn!(path = %cand.path, error = %e, "embed: provider call failed");
                counts.failed += 1;
                continue;
            }
        };
        pending.push(EmbeddingWrite {
            page_id: cand.id,
            vector_bytes: f32_vec_to_bytes(&vec),
            provider: provider.clone(),
            model: model.clone(),
            dim,
        });
        if pending.len() >= EMBEDDING_WRITE_BATCH {
            flush_embedding_batch(writer, &mut pending, &mut counts).await;
        }
    }
    flush_embedding_batch(writer, &mut pending, &mut counts).await;

    Ok(counts)
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
