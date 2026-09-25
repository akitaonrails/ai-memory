//! `GET /search?q=…` — FTS5 search results.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use serde::Deserialize;

use crate::markdown;
use crate::state::WebState;
use crate::templates::{SearchHit, SearchView, page_href};

/// Query-string parameters for the search endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct SearchParams {
    /// The free-text search query.
    #[serde(default)]
    pub q: String,
}

/// Handler for `GET /search?q=…`.
///
/// Searches only the repositories the viewer may read (#708) — see
/// `ReaderPool::search_pages_with_meta`. No viewer (no database users, or
/// root) searches everything, as before.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    Query(params): Query<SearchParams>,
) -> Result<Html<String>, StatusCode> {
    let query = params.q.trim().to_owned();

    let hits = if query.is_empty() {
        Vec::new()
    } else {
        let raw = state
            .reader
            .search_pages_with_meta(
                query.clone(),
                50,
                None,
                viewer.map(|axum::Extension(viewer)| viewer.user()),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let mut results = Vec::with_capacity(raw.len());
        for h in raw {
            let path = h.path.as_str().to_owned();
            let href = page_href(&h.workspace_name, &h.project_name, &path);
            results.push(SearchHit {
                workspace: h.workspace_name,
                project: h.project_name,
                path,
                href,
                title: h.title,
                snippet: markdown::escape_snippet(&h.snippet),
            });
        }
        results
    };

    let hit_count = hits.len();
    let html = SearchView {
        query,
        hits,
        hit_count,
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html))
}
