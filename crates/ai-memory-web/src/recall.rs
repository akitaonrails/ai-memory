//! Project search through the same hybrid ranker as `memory_query`.
//!
//! `memory_query` ranks a project or `scopes` search in three steps: embed the
//! query with the configured embedder, fuse the streams with
//! [`ReaderPool::hybrid_search_scopes`], then let the optional reranker reorder
//! a bounded prefix. This module runs the same three steps over the same
//! shared functions (`ai_memory_llm::embed_search_query`,
//! `hybrid_search_scopes`, `ai_memory_llm::RerankStage`), so a query returns
//! the same order from the web as from MCP. The parity test in
//! `ai-memory-mcp/tests/suite/web_search_parity.rs` holds the two together.
//!
//! The caller resolves the scopes through the guarded scope helpers, so a
//! search never reaches past what the viewer may read.
//!
//! [`ReaderPool::hybrid_search_scopes`]: ai_memory_store::ReaderPool::hybrid_search_scopes

use std::sync::Arc;

use ai_memory_core::{OwnerFilter, ProjectId, WorkspaceId};
use ai_memory_llm::{Embedder, QueryVector, RerankCandidate, RerankStage};
use ai_memory_store::{
    HybridSearch, PageFilter, PageHit, SearchExplain, StoreResult, StreamTimings, VectorQuery,
};

use crate::state::WebState;

/// The server's configured retrieval models, handed to the web state once
/// at startup (invariant 1: `serve` reads config once and passes it down).
#[derive(Clone, Default)]
pub struct Recall {
    /// The embedder `memory_query` uses; `None` skips the vector stream.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// The rerank stage `memory_query` uses, sharing its concurrency gate;
    /// `None` keeps the fused order.
    pub rerank: Option<RerankStage>,
}

/// One ranked search and, when asked for, how it was ranked.
pub(crate) struct RecallResult {
    /// Hits best first, each with its explain details when `explain` was set.
    pub hits: Vec<(PageHit, Option<SearchExplain>)>,
    /// Per-stream wall time, summed over the scopes; `explain` only.
    pub timings: Option<StreamTimings>,
    /// The streams that ran, in `memory_query`'s `streams_active` order.
    pub streams_active: Vec<&'static str>,
}

/// Rank `query` over `scopes` as a `memory_query` project/scopes search does
/// (latest versions only, expired pages hidden), keeping only pages that pass
/// `filter`. Every stream applies the filter, so the reranker scores only
/// pages the caller asked for. `owner` is the viewer's
/// session-timeline filter: an observation excerpt is shown only from a
/// session it admits, as in `memory_query`.
pub(crate) async fn search_scopes(
    state: &WebState,
    scopes: &[(WorkspaceId, ProjectId)],
    query: &str,
    limit: usize,
    explain: bool,
    owner: &OwnerFilter,
    filter: PageFilter<'_>,
) -> StoreResult<RecallResult> {
    let query_vector: Option<QueryVector> = match &state.recall.embedder {
        Some(embedder) => ai_memory_llm::embed_search_query(embedder.as_ref(), query).await,
        None => None,
    };
    let fetch_limit = state
        .recall
        .rerank
        .as_ref()
        .map_or(limit, |stage| stage.fetch_limit(limit));
    let (hits, mut timings) = state
        .reader
        .hybrid_search_scopes(
            scopes,
            HybridSearch {
                query,
                vector: query_vector.as_ref().map(|qv| VectorQuery {
                    vector: &qv.vector,
                    provider: &qv.provider,
                    model: &qv.model,
                    dim: qv.dim,
                }),
                limit: fetch_limit,
                expiry_cutoff_us: None,
                include_superseded: false,
                explain,
                owner,
                filter,
            },
        )
        .await?;
    let hits = match &state.recall.rerank {
        Some(stage) => {
            let started = explain.then(std::time::Instant::now);
            let hits = stage
                .apply(
                    query,
                    hits,
                    limit,
                    stage.timeout(),
                    |(hit, _)| RerankCandidate {
                        id: hit.id.to_string(),
                        title: hit.title.clone(),
                        snippet: ai_memory_store::plain_snippet(&hit.snippet),
                    },
                    |(_, details), relevance| {
                        if let Some(details) = details.as_mut() {
                            details.rerank_score = Some(relevance);
                        }
                    },
                )
                .await;
            if let (Some(timings), Some(started)) = (timings.as_mut(), started) {
                timings.rerank_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
            }
            hits
        }
        None => {
            let mut hits = hits;
            hits.truncate(limit);
            hits
        }
    };
    Ok(RecallResult {
        hits,
        timings,
        streams_active: state.reader.active_streams(query_vector.is_some(), filter),
    })
}
