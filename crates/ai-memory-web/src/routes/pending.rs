//! `GET /pending` — root-only triage page for pending auto-improvement
//! proposals across every project.
//!
//! The page only reads. Approve and reject buttons post from the browser to
//! the existing `/admin/pending-writes/{id}/approve|reject` handlers, which
//! keep admission, audit, and the single writer. This crate adds no write
//! path.

use std::sync::Arc;

use ai_memory_core::{AuthLevel, Capability};
use ai_memory_store::{PendingAutoImproveReview, PendingAutoImproveScope};
use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::html_auth::AdminRequired;
use crate::state::WebState;
use crate::templates::{
    AdminRequiredView, PendingRow, PendingView, SelectOption, encode_segment, humanize, page_href,
};

/// Most proposals one page load reads. A queue this long needs the CLI, and
/// the cap keeps the page (which embeds every body) bounded.
const PENDING_REVIEW_LIMIT: usize = 500;

const RULES_PREFIX: &str = "_rules/";

/// Sort orders the page offers, as `(value, label)`. The first is the default.
const SORTS: [(&str, &str); 4] = [
    (SORT_PROJECT, "Project, then staged date"),
    (SORT_CONFIDENCE_ASC, "Confidence, low first"),
    (SORT_CONFIDENCE_DESC, "Confidence, high first"),
    (SORT_STAGED_DESC, "Staged, newest first"),
];
const SORT_PROJECT: &str = "project";
const SORT_CONFIDENCE_ASC: &str = "confidence-asc";
const SORT_CONFIDENCE_DESC: &str = "confidence-desc";
const SORT_STAGED_DESC: &str = "staged-desc";

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PendingQuery {
    /// Percent-encoded `workspace/project` key; empty means every project.
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    sort: Option<String>,
}

/// Handler for `GET /pending`.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    Query(query): Query<PendingQuery>,
) -> Response {
    if let Err(response) = require_admin(&state, level).await {
        return response;
    }

    let scopes = match state.reader.list_pending_auto_improve_scopes().await {
        Ok(scopes) => scopes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    // Match the request against known scopes only, so the filter can never
    // name a project that has no pending proposal.
    let filter = query
        .project
        .as_deref()
        .and_then(|key| scopes.iter().find(|scope| scope_key(scope) == key));
    let sort = query
        .sort
        .as_deref()
        .and_then(|raw| SORTS.iter().find(|(value, _)| *value == raw))
        .map_or(SORT_PROJECT, |(value, _)| *value);

    let mut reviews = match state
        .reader
        .list_pending_auto_improve_reviews(
            filter.map(|scope| (scope.workspace_id, scope.project_id)),
            PENDING_REVIEW_LIMIT + 1,
        )
        .await
    {
        Ok(reviews) => reviews,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let truncated = reviews.len() > PENDING_REVIEW_LIMIT;
    reviews.truncate(PENDING_REVIEW_LIMIT);
    sort_reviews(&mut reviews, sort);

    let total: u64 = scopes.iter().map(|scope| scope.pending).sum();
    let mut projects = vec![SelectOption {
        value: String::new(),
        label: format!("All projects ({total})"),
        selected: filter.is_none(),
    }];
    projects.extend(scopes.iter().map(|scope| SelectOption {
        value: scope_key(scope),
        label: format!(
            "{} ({})",
            display_scope(&scope.workspace_name, &scope.project_name),
            scope.pending
        ),
        selected: filter.is_some_and(|f| std::ptr::eq(f, scope)),
    }));
    let sorts = SORTS
        .iter()
        .map(|(value, label)| SelectOption {
            value: (*value).to_owned(),
            label: (*label).to_owned(),
            selected: *value == sort,
        })
        .collect();

    render(
        PendingView {
            rows: reviews.into_iter().map(pending_row).collect(),
            projects,
            sorts,
            total,
            project_count: scopes.len(),
            truncated,
            limit: PENDING_REVIEW_LIMIT,
        },
        StatusCode::OK,
    )
}

/// Filter value for one project. Each name is percent-encoded, so a `/`
/// inside a name cannot make two projects share one key.
fn scope_key(scope: &PendingAutoImproveScope) -> String {
    format!(
        "{}/{}",
        encode_segment(&scope.workspace_name),
        encode_segment(&scope.project_name)
    )
}

/// Refuse the page unless the caller may use `/admin`, with the same
/// `Capability::Admin` decision `require_root_for_multiuser_admin` makes.
async fn require_admin(
    state: &WebState,
    level: Option<Extension<AuthLevel>>,
) -> Result<(), Response> {
    let level = level.map_or(AuthLevel::Anonymous, |Extension(level)| level);
    let distinguishes_operators = state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    match level.authorize(Capability::Admin, distinguishes_operators) {
        Ok(()) => Ok(()),
        Err(e) if e.is_authentication_required() => Err(StatusCode::UNAUTHORIZED.into_response()),
        Err(_) => {
            let mut response = render(AdminRequiredView {}, StatusCode::FORBIDDEN);
            // A 403 here means "not root", not "change your password": keep
            // the HTML auth redirect from sending the user to that form.
            response.extensions_mut().insert(AdminRequired);
            Err(response)
        }
    }
}

fn scope_label(review: &PendingAutoImproveReview) -> String {
    display_scope(&review.workspace_name, &review.project_name)
}

/// `workspace/project` for display. When a name holds a `/`, both names are
/// quoted, so `a/b` + `c` and `a` + `b/c` stay distinct on screen.
fn display_scope(workspace: &str, project: &str) -> String {
    if workspace.contains('/') || project.contains('/') {
        format!("{workspace:?}/{project:?}")
    } else {
        format!("{workspace}/{project}")
    }
}

fn sort_reviews(reviews: &mut [PendingAutoImproveReview], sort: &str) {
    reviews.sort_by(|a, b| {
        let by_scope = || scope_label(a).cmp(&scope_label(b));
        let by_staged = a.summary.staged_at.cmp(&b.summary.staged_at);
        let by_confidence = a.summary.confidence.total_cmp(&b.summary.confidence);
        match sort {
            SORT_CONFIDENCE_ASC => by_confidence.then_with(by_scope).then(by_staged),
            SORT_CONFIDENCE_DESC => by_confidence.reverse().then_with(by_scope).then(by_staged),
            SORT_STAGED_DESC => by_staged.reverse().then_with(by_scope),
            _ => by_scope().then(by_staged),
        }
    });
}

fn pending_row(review: PendingAutoImproveReview) -> PendingRow {
    let label = scope_label(&review);
    let summary = review.summary;
    let target_path = summary.target_path.as_str().to_owned();
    let staged_relative = jiff::Timestamp::from_microsecond(summary.staged_at)
        .map(|ts| humanize(&ts.to_string()))
        .unwrap_or_default();
    PendingRow {
        id: summary.id.to_string(),
        scope_label: label,
        target_href: page_href(&review.workspace_name, &review.project_name, &target_path),
        workspace: review.workspace_name,
        project: review.project_name,
        kind: summary.kind,
        operation: summary.operation.as_str().to_owned(),
        is_rule: target_path.starts_with(RULES_PREFIX),
        rewrites_existing: summary.operation
            != ai_memory_store::AutoImproveProposalOperation::Create,
        target_path,
        title: summary.title,
        confidence_pct: (summary.confidence * 100.0).round() as i64,
        staged_relative,
        edit_mode: review.edit_mode,
        rationale: review.rationale,
        body_markdown: review.body_markdown,
    }
}

fn render(view: impl Template, status: StatusCode) -> Response {
    match view.render() {
        Ok(body) => (status, Html(body)).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
