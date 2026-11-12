//! `GET /w/:workspace/:project/s/:session_id/retrievals` — one session's opt-in
//! retrieval history (#2038), newest first, a page at a time.
//!
//! Visibility reuses the session timeline's gate exactly: `session_summary_scoped`
//! decides whether the caller may see this session at all (a row or observation
//! in this project, owner filter passes), and only then does
//! `retrieval_history_page` read this project's recorded retrievals. The reader
//! is strictly scoped to `(workspace, project, session)`, so a foreign scope can
//! never read another's history through a guessed session id, and the owner gate
//! stops one operator reading another's on a shared server.
//!
//! Recorded query text was scrubbed on capture and is still untrusted: the
//! template escapes every field.

use std::sync::Arc;

use ai_memory_core::SessionId;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::api::session_owner_filter_for;
use crate::state::WebState;
use crate::templates::{
    RetrievalHistoryHitRow, RetrievalHistoryView, RetrievalRecordRow, page_href, project_href,
    retrievals_href, session_href,
};

/// Records per page when the request names no `limit`.
pub(crate) const DEFAULT_PAGE_SIZE: usize = 25;

/// Upper bound on the page size, mirroring the session timeline's cap.
pub(crate) const MAX_PAGE_SIZE: usize = 200;

/// Query string for the retrieval-history view.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RetrievalsQuery {
    /// Records to skip, newest first.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub offset: Option<usize>,
    /// Records per page, clamped to `1..=MAX_PAGE_SIZE`.
    #[serde(default, deserialize_with = "blank_as_none")]
    pub limit: Option<usize>,
}

/// A blank number field reads as omitted rather than axum's 400.
fn blank_as_none<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<String>::deserialize(deserializer)?
        .as_deref()
        .map(str::trim)
    {
        None | Some("") => Ok(None),
        Some(raw) => raw.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

/// Handler for `GET /w/:workspace/:project/s/:session_id/retrievals`.
pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    auth: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    Path((workspace, project, raw_session)): Path<(String, String, String)>,
    Query(query): Query<RetrievalsQuery>,
) -> Response {
    let owner = session_owner_filter_for(actor, auth);
    let viewer = viewer.map(|axum::Extension(viewer)| viewer.user());
    let scope = match super::resolve_read_scope(&state, viewer, &workspace, &project).await {
        Ok(scope) => scope,
        Err(refusal) => return super::page::localized_refusal_response(&i18n, &refusal),
    };
    let Ok(session_id) = raw_session.parse::<SessionId>() else {
        return (StatusCode::BAD_REQUEST, "invalid session id").into_response();
    };
    let internal = |error: ai_memory_store::StoreError| {
        tracing::error!(%error, "web retrieval-history read failed");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    };
    // The session-visibility gate: a session that left nothing in this project,
    // or that the owner filter hides, reads as not found — exactly as the
    // timeline route decides, so the dashboard never leaks a foreign session.
    match state
        .reader
        .session_summary_scoped(scope.workspace_id, scope.project_id, session_id, owner)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return super::page::not_found_response(&i18n),
        Err(error) => return internal(error),
    }

    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    let page = match state
        .reader
        .retrieval_history_page(
            scope.workspace_id,
            scope.project_id,
            session_id,
            i64::try_from(query.offset.unwrap_or(0)).unwrap_or(i64::MAX),
            i64::try_from(limit).unwrap_or(i64::MAX),
        )
        .await
    {
        Ok(page) => page,
        Err(error) => return internal(error),
    };

    let total = u64::try_from(page.total).unwrap_or(0);
    let total_usize = usize::try_from(page.total).unwrap_or(usize::MAX);
    let offset = query.offset.unwrap_or(0).min(total_usize.saturating_sub(1));
    let shown = page.records.len();
    let records: Vec<RetrievalRecordRow> = page
        .records
        .into_iter()
        .map(|record| RetrievalRecordRow {
            created_at: record.created_at.to_string(),
            created_relative: i18n.relative(&record.created_at.to_string()),
            query: record.query,
            scope_kind: record.scope_kind,
            source: record.source,
            result_limit: record
                .result_limit
                .map_or_else(|| "—".to_string(), |v| v.to_string()),
            min_relevance: record
                .min_relevance
                .map_or_else(|| "—".to_string(), |v| format!("{v:.3}")),
            filters: record.filters.unwrap_or_default(),
            duration_ms: format!("{:.1}", record.duration_us as f64 / 1000.0),
            result_count: record.result_count,
            notice: record.notice.unwrap_or_default(),
            hits: record
                .hits
                .into_iter()
                .map(|hit| RetrievalHistoryHitRow {
                    position: hit.position,
                    page_href: page_href(&workspace, &project, &hit.page_path),
                    page_path: hit.page_path,
                    page_version: hit.page_version.unwrap_or_default(),
                    relevance: hit
                        .relevance
                        .map_or_else(|| "—".to_string(), |v| format!("{v:.3}")),
                    explanation: hit.explanation.unwrap_or_default(),
                })
                .collect(),
        })
        .collect();

    let base = retrievals_href(&workspace, &project, &session_id.to_string());
    let page_link = |offset: usize| format!("{base}?offset={offset}&limit={limit}");
    let last_offset = total_usize.saturating_sub(1) / limit * limit;
    let view = RetrievalHistoryView {
        i18n: i18n.clone(),
        workspace: workspace.clone(),
        project: project.clone(),
        project_href: project_href(&workspace, &project),
        session_href: session_href(&workspace, &project, &session_id.to_string()),
        session_id: session_id.to_string(),
        total,
        shown_start: if shown == 0 { 0 } else { offset + 1 },
        shown_end: offset + shown,
        first_href: if offset > 0 {
            page_link(0)
        } else {
            String::new()
        },
        prev_href: if offset > 0 {
            page_link(offset.saturating_sub(limit))
        } else {
            String::new()
        },
        next_href: if offset + shown < total_usize {
            page_link(offset + limit)
        } else {
            String::new()
        },
        last_href: if offset + shown < total_usize {
            page_link(last_offset)
        } else {
            String::new()
        },
        records,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
