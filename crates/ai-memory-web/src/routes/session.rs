//! `GET /w/:workspace/:project/s/:session_id` — one session's observations in
//! capture order, a page at a time, with a prev/next/jump scrubber.
//!
//! Reads through the same scoped readers and caps as the `/api/v1` sessions
//! routes: `session_summary_scoped` decides visibility (row or observation in
//! this project, owner filter passes), and `session_observations_scoped`
//! returns only this project's rows. Bodies were sanitized on ingest and are
//! still untrusted text: the template escapes every field.

use std::sync::Arc;

use ai_memory_core::SessionId;
use ai_memory_store::{ObservationOrder, ObservationPage};
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::api::{
    SESSION_OBSERVATIONS_DEFAULT_BODY_CHARS, SESSION_OBSERVATIONS_MAX_LIMIT, cap_body,
    session_owner_filter_for,
};
use crate::state::WebState;
use crate::templates::{SessionObservationRow, SessionView, project_href, session_href};

/// Observations per page when the request names no `limit`.
pub(crate) const DEFAULT_PAGE_SIZE: usize = 50;

/// Query string for the session view.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SessionQuery {
    /// Observations to skip, in capture order.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Observations per page, clamped like the API's `limit`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// 1-based position to jump to; overrides `offset` with the page that
    /// holds it.
    #[serde(default)]
    pub at: Option<usize>,
    #[serde(flatten)]
    pub usage: crate::operations::UsageQuery,
}

/// Handler for `GET /w/:workspace/:project/s/:session_id`.
pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    auth: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    Path((workspace, project, raw_session)): Path<(String, String, String)>,
    Query(query): Query<SessionQuery>,
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
        tracing::error!(%error, "web session read failed");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    };
    // Same predicate as the API: a session that left nothing in this project,
    // or that the owner filter hides, reads as not found.
    let session = match state
        .reader
        .session_summary_scoped(
            scope.workspace_id,
            scope.project_id,
            session_id,
            owner.clone(),
        )
        .await
    {
        Ok(Some(session)) => session,
        Ok(None) => return super::page::not_found_response(&i18n),
        Err(error) => return internal(error),
    };

    let limit = query
        .limit
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, SESSION_OBSERVATIONS_MAX_LIMIT);
    let total = session.observation_count;
    let offset = match query.at {
        Some(at) => {
            let at = at.clamp(1, usize::try_from(total).unwrap_or(usize::MAX).max(1));
            (at - 1) / limit * limit
        }
        // Past the end is an empty page; clamping keeps the arithmetic below
        // from overflowing on a hostile offset.
        None => query
            .offset
            .unwrap_or(0)
            .min(usize::try_from(total).unwrap_or(usize::MAX)),
    };
    let page = match state
        .reader
        .session_observations_scoped(
            scope.workspace_id,
            scope.project_id,
            session_id,
            ObservationPage {
                limit,
                offset,
                order: ObservationOrder::Asc,
                kinds: None,
                query: None,
            },
        )
        .await
    {
        Ok(page) => page,
        Err(error) => return internal(error),
    };

    let base = session_href(&workspace, &project, &session_id.to_string());
    let total = page.total;
    let total_usize = usize::try_from(total).unwrap_or(usize::MAX);
    let shown = page.records.len();
    let rows: Vec<SessionObservationRow> = page
        .records
        .into_iter()
        .enumerate()
        .map(|(i, record)| SessionObservationRow {
            position: offset + i + 1,
            kind: record.kind,
            title: record.title,
            body: cap_body(&record.body, SESSION_OBSERVATIONS_DEFAULT_BODY_CHARS),
            created_at: record.created_at.clone(),
            created_relative: i18n.relative(&record.created_at),
        })
        .collect();
    let page_href = |offset: usize| format!("{base}?offset={offset}&limit={limit}");
    let last_offset = total_usize.saturating_sub(1) / limit * limit;
    let calls_query = match query.usage.store_query() {
        Ok(query) => query,
        Err(status) => return status.into_response(),
    };
    let report = match state
        .reader
        .token_usage_report_page(
            scope.workspace_id,
            scope.project_id,
            owner,
            Some(session_id),
            calls_query,
        )
        .await
    {
        Ok(report) => report,
        Err(error) => return internal(error),
    };
    let mut token_usage = crate::operations::usage_panel(
        &i18n,
        report,
        &workspace,
        &project,
        &query.usage,
        &format!("{base}?offset={offset}&limit={limit}"),
    );
    token_usage.observation_offset = offset;
    token_usage.observation_limit = limit;
    let view = SessionView {
        i18n: i18n.clone(),
        token_usage,
        workspace: workspace.clone(),
        project: project.clone(),
        project_href: project_href(&workspace, &project),
        session_href: base.clone(),
        session_id: session_id.to_string(),
        agent_kind: session.agent_kind,
        started_at: session.started_at,
        ended_at: session.ended_at.unwrap_or_default(),
        total,
        limit,
        shown_start: if shown == 0 { 0 } else { offset + 1 },
        shown_end: offset + shown,
        elided_other_scope: page.elided_other_scope,
        first_href: if offset > 0 {
            page_href(0)
        } else {
            String::new()
        },
        prev_href: if offset > 0 {
            page_href(offset.saturating_sub(limit))
        } else {
            String::new()
        },
        next_href: if offset + shown < total_usize {
            page_href(offset + limit)
        } else {
            String::new()
        },
        last_href: if offset + shown < total_usize {
            page_href(last_offset)
        } else {
            String::new()
        },
        jump_value: offset + 1,
        observations: rows,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
