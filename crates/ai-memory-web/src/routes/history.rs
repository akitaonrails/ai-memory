//! `GET /w/:workspace/:project/h/*path` — a page's version history and a
//! server-side diff between two of its versions.
//!
//! Read-only: every row comes from `ReaderPool::page_versions` and
//! `ReaderPool::page_version_body`, which read by the resolved
//! `(workspace_id, project_id, path)`. A version id from anywhere else reads
//! as not found.

use std::sync::Arc;

use ai_memory_core::{PageId, PagePath};
use ai_memory_store::{PageVersion, ScopeResolutionError};
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::diff::{self, Diff};
use crate::state::WebState;
use crate::templates::{
    HistoryDiffLine, HistoryHunk, HistoryVersionRow, HistoryView, history_href, page_href,
    project_href,
};

/// Versions listed per page when the request names no `limit`.
pub(crate) const DEFAULT_VERSION_LIMIT: usize = 50;

/// Query string shared by the HTML and JSON history routes.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HistoryQuery {
    /// Older side of the diff. Defaults to the version `to` replaced.
    #[serde(default)]
    pub from: Option<String>,
    /// Newer side of the diff. Defaults to the newest version.
    #[serde(default)]
    pub to: Option<String>,
    /// Versions per listing page, clamped to
    /// `1..=`[`ai_memory_store::PAGE_VERSIONS_MAX_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
    /// Versions to skip, newest first.
    #[serde(default)]
    pub offset: usize,
}

/// One side of a diff.
pub(crate) struct DiffSide {
    pub id: PageId,
    pub title: String,
    pub created_at: String,
}

/// Everything both history routes render.
pub(crate) struct History {
    pub total: u64,
    pub limit: usize,
    pub offset: usize,
    pub versions: Vec<PageVersion>,
    /// `None` only for a path with a single version and no `from`.
    pub from: Option<DiffSide>,
    pub to: DiffSide,
    pub diff: Diff,
}

/// Why a history request was refused.
pub(crate) enum HistoryError {
    Scope(ScopeResolutionError),
    BadRequest(String),
    NotFound(&'static str),
    Internal(String),
}

/// Resolve the scope, list the versions and diff the selected pair.
pub(crate) async fn load(
    state: &WebState,
    viewer: Option<ai_memory_core::UserId>,
    workspace: &str,
    project: &str,
    path: &str,
    query: &HistoryQuery,
) -> Result<History, HistoryError> {
    let scope = super::resolve_read_scope(state, viewer, workspace, project)
        .await
        .map_err(HistoryError::Scope)?;
    let path = PagePath::new(path)
        .map_err(|e| HistoryError::BadRequest(format!("invalid path: {e}")))?
        .as_str()
        .to_owned();
    let parse = |raw: &Option<String>, name: &str| -> Result<Option<PageId>, HistoryError> {
        match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(raw) => raw
                .parse::<PageId>()
                .map(Some)
                .map_err(|_| HistoryError::BadRequest(format!("invalid {name} version id"))),
        }
    };
    let from_id = parse(&query.from, "from")?;
    let to_id = parse(&query.to, "to")?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_VERSION_LIMIT)
        .clamp(1, ai_memory_store::PAGE_VERSIONS_MAX_LIMIT);

    let internal = |e: ai_memory_store::StoreError| HistoryError::Internal(e.to_string());
    let listing = state
        .reader
        .page_versions(
            scope.workspace_id,
            scope.project_id,
            path.clone(),
            limit,
            query.offset,
        )
        .await
        .map_err(internal)?;
    if listing.total == 0 {
        return Err(HistoryError::NotFound("page not found"));
    }
    let body = |id: PageId| {
        let path = path.clone();
        async move {
            state
                .reader
                .page_version_body(scope.workspace_id, scope.project_id, path, id)
                .await
                .map_err(internal)?
                .ok_or(HistoryError::NotFound("version not found"))
        }
    };

    // The newer side: the named version, else the newest one at the path.
    let to_id = match to_id {
        Some(id) => id,
        None if query.offset == 0 => {
            listing
                .versions
                .first()
                .ok_or(HistoryError::NotFound("page not found"))?
                .id
        }
        None => {
            state
                .reader
                .page_versions(scope.workspace_id, scope.project_id, path.clone(), 1, 0)
                .await
                .map_err(internal)?
                .versions
                .first()
                .ok_or(HistoryError::NotFound("page not found"))?
                .id
        }
    };
    let to = body(to_id).await?;
    // The older side: the named version, else the one `to` replaced. A
    // predecessor stored at another path (a resurrected or moved page) is
    // not part of this path's history, so the default diff starts from empty
    // rather than failing; only a named `from` must exist here.
    let from = match (from_id, to.supersedes) {
        (Some(id), _) => Some(body(id).await?),
        (None, Some(prev)) => match body(prev).await {
            Ok(found) => Some(found),
            Err(HistoryError::NotFound(_)) => None,
            Err(other) => return Err(other),
        },
        (None, None) => None,
    };
    let diff = diff::unified(from.as_ref().map_or("", |f| f.body.as_str()), &to.body);
    Ok(History {
        total: listing.total,
        limit,
        offset: query.offset,
        versions: listing.versions,
        from: from.map(|f| DiffSide {
            id: f.id,
            title: f.title,
            created_at: f.created_at,
        }),
        to: DiffSide {
            id: to.id,
            title: to.title,
            created_at: to.created_at,
        },
        diff,
    })
}

/// Handler for `GET /w/:workspace/:project/h/*path`.
pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    Path((workspace, project, path)): Path<(String, String, String)>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let viewer = viewer.map(|axum::Extension(viewer)| viewer.user());
    let history = match load(&state, viewer, &workspace, &project, &path, &query).await {
        Ok(history) => history,
        Err(HistoryError::Scope(refusal)) => {
            return super::page::localized_refusal_response(&i18n, &refusal);
        }
        Err(HistoryError::BadRequest(message)) => {
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
        Err(HistoryError::NotFound(_)) => return super::page::not_found_response(&i18n),
        Err(HistoryError::Internal(error)) => {
            tracing::error!(%error, "web history read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    render(&i18n, &workspace, &project, &path, history)
}

fn render(
    i18n: &ai_memory_core::i18n::Localizer,
    workspace: &str,
    project: &str,
    path: &str,
    history: History,
) -> Response {
    let base = history_href(workspace, project, path);
    let from_id = history.from.as_ref().map(|f| f.id);
    let to_id = history.to.id;
    let versions = history
        .versions
        .iter()
        .map(|v| HistoryVersionRow {
            id: v.id.to_string(),
            short_id: short(v.id),
            supersedes_short: v.supersedes.map(short).unwrap_or_default(),
            is_latest: v.is_latest,
            title: v.title.clone(),
            created_at: v.created_at.clone(),
            created_relative: i18n.relative(&v.created_at),
            author: v
                .author
                .as_ref()
                .map(|a| a.username.clone())
                .unwrap_or_default(),
            body_chars: v.body_chars,
            is_from: Some(v.id) == from_id,
            is_to: v.id == to_id,
            compare_href: v.supersedes.map_or_else(String::new, |prev| {
                format!("{base}?from={prev}&to={}", v.id)
            }),
        })
        .collect();
    let (same, too_large, truncated, hunks) = match history.diff {
        Diff::Same => (true, String::new(), false, Vec::new()),
        Diff::TooLarge(why) => (false, why.to_owned(), false, Vec::new()),
        Diff::Changed { hunks, truncated } => (
            false,
            String::new(),
            truncated,
            hunks
                .into_iter()
                .map(|h| HistoryHunk {
                    header: h.header(),
                    lines: h
                        .lines
                        .into_iter()
                        .map(|l| HistoryDiffLine {
                            sign: l.op.to_string(),
                            text: l.text,
                        })
                        .collect(),
                })
                .collect(),
        ),
    };
    let page_param = |offset: usize| {
        let mut href = format!("{base}?offset={offset}&limit={}", history.limit);
        if let Some(from) = from_id {
            href.push_str(&format!("&from={from}"));
        }
        href.push_str(&format!("&to={to_id}"));
        href
    };
    let shown_end = history.offset + history.versions.len();
    let newer_href = if history.offset > 0 {
        page_param(history.offset.saturating_sub(history.limit))
    } else {
        String::new()
    };
    let older_href = if (shown_end as u64) < history.total {
        page_param(shown_end)
    } else {
        String::new()
    };
    let view = HistoryView {
        i18n: i18n.clone(),
        workspace: workspace.to_owned(),
        project: project.to_owned(),
        project_href: project_href(workspace, project),
        page_href: page_href(workspace, project, path),
        history_href: base,
        path: path.to_owned(),
        total: history.total,
        shown_start: if history.versions.is_empty() {
            0
        } else {
            history.offset + 1
        },
        shown_end,
        limit: history.limit,
        newer_href,
        older_href,
        versions,
        from_short: history
            .from
            .as_ref()
            .map(|f| short(f.id))
            .unwrap_or_default(),
        from_created: history
            .from
            .as_ref()
            .map(|f| f.created_at.clone())
            .unwrap_or_default(),
        from_title: history
            .from
            .as_ref()
            .map(|f| f.title.clone())
            .unwrap_or_default(),
        to_short: short(to_id),
        to_created: history.to.created_at,
        to_title: history.to.title,
        same,
        too_large,
        truncated,
        hunks,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Last eight characters of a version id. Ids are UUIDv7, so their leading
/// characters are a timestamp that versions written close together share;
/// the tail is random and tells them apart.
fn short(id: PageId) -> String {
    crate::templates::short_id(&id.to_string())
}
