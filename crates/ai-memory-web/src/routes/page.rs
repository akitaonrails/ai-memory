//! `GET /w/:workspace/:project/p/*path` — rendered markdown page.

use std::sync::Arc;

use ai_memory_core::PagePath;
use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::markdown;
use crate::state::WebState;
use crate::templates::{NotFoundView, PageView, humanize, page_href, project_href};

/// Handler for `GET /w/:workspace/:project/p/*path`.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project, path)): Path<(String, String, String)>,
) -> Response {
    let meta = match state.reader.page_meta(&workspace, &project, &path).await {
        Ok(Some(m)) => m,
        Ok(None) => return not_found_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let page_path = match PagePath::new(&path) {
        Ok(p) => p,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let markdown_doc = match state
        .wiki
        .read_page(meta.workspace_id, meta.project_id, &page_path)
        .await
    {
        Ok(doc) => doc,
        Err(_) => return not_found_response(),
    };

    // Drop the leading H1 — the template already renders the title
    // in its header, so leaving it in the body duplicates it.
    let body_html = markdown::render(
        markdown::strip_leading_h1(&markdown_doc.body),
        &workspace,
        &project,
    );

    let project_href = project_href(&workspace, &project);
    let supersedes_path = meta.supersedes.unwrap_or_default();
    let supersedes_href = if supersedes_path.is_empty() {
        String::new()
    } else {
        page_href(&workspace, &project, &supersedes_path)
    };

    let (author_username, author_name, author_email) = meta.author.map_or_else(
        || (String::new(), String::new(), String::new()),
        |a| {
            (
                a.username,
                a.name.unwrap_or_default(),
                a.email.unwrap_or_default(),
            )
        },
    );

    match (PageView {
        workspace,
        project,
        project_href,
        path: meta.path,
        title: meta.title,
        kind: meta.kind,
        tier: meta.tier,
        pinned: meta.pinned,
        updated_relative: humanize(&meta.updated_at),
        created_relative: humanize(&meta.created_at),
        supersedes_path,
        supersedes_href,
        body_html,
        author_username,
        author_name,
        author_email,
    }
    .render())
    {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Render a 404 response with the not-found template body.
fn not_found_response() -> Response {
    let html = NotFoundView {}
        .render()
        .unwrap_or_else(|_| "<h1>Not found</h1>".to_owned());
    (StatusCode::NOT_FOUND, Html(html)).into_response()
}
