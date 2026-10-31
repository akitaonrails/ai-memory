//! `GET /workstreams` — root/operator overview of managed workstreams.
//!
//! Existing managed-workstream discovery is intentionally checkout-local.
//! This cross-scope aggregate therefore stays behind the same Admin
//! capability as `/pending` and `/health`.

use std::sync::Arc;

use ai_memory_core::AuthLevel;
use ai_memory_store::{OperationalWorkstreamSummary, WorkstreamOperationalStatus};
use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::state::WebState;
use crate::templates::{WorkstreamRow, WorkstreamsView, project_href, short_id};

const PAGE_SIZE: usize = 100;
const MAX_PAGE: usize = 10_000;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WorkstreamsQuery {
    #[serde(default)]
    page: Option<usize>,
}

pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    Query(query): Query<WorkstreamsQuery>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
        return response;
    }
    let page = query.page.unwrap_or(1).clamp(1, MAX_PAGE);
    let offset = (page - 1).saturating_mul(PAGE_SIZE);
    let mut rows = match state
        .reader
        .operational_workstreams(PAGE_SIZE + 1, offset)
        .await
    {
        Ok(rows) => rows,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let has_next = rows.len() > PAGE_SIZE;
    rows.truncate(PAGE_SIZE);
    let view = WorkstreamsView {
        i18n: i18n.clone(),
        rows: rows
            .into_iter()
            .map(|summary| workstream_row(&i18n, summary))
            .collect(),
        page,
        prev_href: (page > 1).then(|| page_href(page - 1)),
        next_href: has_next.then(|| page_href(page + 1)),
    };
    let body = match view.render() {
        Ok(body) => body,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    ([(header::CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

fn workstream_row(
    i18n: &ai_memory_core::i18n::Localizer,
    summary: OperationalWorkstreamSummary,
) -> WorkstreamRow {
    let status = summary.status.as_str().to_owned();
    let status_label = match summary.status {
        WorkstreamOperationalStatus::Active => "Active",
        WorkstreamOperationalStatus::Interrupted => "Interrupted",
        WorkstreamOperationalStatus::Completed => "Completed",
        WorkstreamOperationalStatus::Empty => "Empty",
    }
    .to_owned();
    WorkstreamRow {
        project_href: project_href(&summary.workspace_name, &summary.project_name),
        scope_label: display_scope(&summary.workspace_name, &summary.project_name),
        name: summary.name,
        short_id: short_id(&summary.workstream_id.to_string()),
        status,
        status_label: if i18n.tag() == "en" {
            status_label
        } else {
            i18n.status(summary.status.as_str())
        },
        latest_agent: summary
            .latest_agent
            .map_or_else(String::new, |agent| agent.as_str().to_owned()),
        latest_run_short: summary
            .latest_run_id
            .map_or_else(String::new, |id| short_id(&id.to_string())),
        linked_harnesses: summary
            .linked_harnesses
            .into_iter()
            .map(|agent| agent.as_str().to_owned())
            .collect(),
        event_count: summary.event_count,
        last_active_relative: humanize_micros(i18n, summary.last_active_at),
        latest_started_relative: summary
            .latest_started_at
            .map_or_else(String::new, |value| humanize_micros(i18n, value)),
        latest_ended_relative: summary
            .latest_ended_at
            .map_or_else(String::new, |value| humanize_micros(i18n, value)),
    }
}

fn humanize_micros(i18n: &ai_memory_core::i18n::Localizer, value: i64) -> String {
    jiff::Timestamp::from_microsecond(value)
        .map(|timestamp| i18n.relative(&timestamp.to_string()))
        .unwrap_or_else(|_| value.to_string())
}

fn page_href(page: usize) -> String {
    format!("workstreams?page={page}")
}

fn display_scope(workspace: &str, project: &str) -> String {
    if workspace.contains('/') || project.contains('/') {
        format!("{workspace:?}/{project:?}")
    } else {
        format!("{workspace}/{project}")
    }
}
