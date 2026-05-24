//! JSON routes for third-party read-only frontends.

use std::sync::Arc;

use ai_memory_core::{PagePath, ProjectId, WorkspaceId};
use ai_memory_store::PageHit;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::state::WebState;

/// Build the `/api/v1` router from a shared [`WebState`].
pub(crate) fn build(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/projects", axum::routing::get(projects_handler))
        .route(
            "/workspaces/{workspace}/projects/{project}/pages",
            axum::routing::get(pages_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/pages/{*path}",
            axum::routing::get(page_handler),
        )
        .route("/search", axum::routing::get(search_handler))
        .route(
            "/workspaces/{workspace}/projects/{project}/recent",
            axum::routing::get(recent_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/briefing",
            axum::routing::get(briefing_handler),
        )
        .with_state(state)
}

async fn projects_handler(State(state): State<Arc<WebState>>) -> Result<Response, Response> {
    let projects = state
        .reader
        .list_projects_with_stats()
        .await
        .map_err(internal_error)?;
    Ok(Json(projects).into_response())
}

async fn pages_handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Result<Response, Response> {
    let _ = lookup_project(&state, &workspace, &project).await?;
    let pages = state
        .reader
        .list_pages(&workspace, &project)
        .await
        .map_err(internal_error)?;
    Ok(Json(pages).into_response())
}

async fn page_handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project, path)): Path<(String, String, String)>,
) -> Result<Response, Response> {
    let meta = state
        .reader
        .page_meta(&workspace, &project, &path)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found("page not found"))?;

    let page_path = PagePath::new(&path)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("invalid path: {e}")))?;
    let markdown = state
        .wiki
        .read_page(meta.workspace_id, meta.project_id, &page_path)
        .map_err(|_| not_found("page file not found"))?;

    Ok(Json(ApiPage {
        body_markdown: markdown.body,
        created_at: meta.created_at,
        frontmatter: markdown.frontmatter,
        kind: meta.kind,
        path: meta.path,
        pinned: meta.pinned,
        project: meta.project_name,
        supersedes: meta.supersedes,
        tier: meta.tier,
        title: meta.title,
        updated_at: meta.updated_at,
        workspace: meta.workspace_name,
    })
    .into_response())
}

async fn search_handler(
    State(state): State<Arc<WebState>>,
    Query(query): Query<SearchQuery>,
) -> Result<Response, Response> {
    let term = query.q.trim().to_owned();
    if term.is_empty() {
        return Ok(Json(Vec::<ApiSearchHit>::new()).into_response());
    }

    let limit = query.limit.clamp(1, 100);
    let hits = match (query.workspace.as_deref(), query.project.as_deref()) {
        (Some(workspace), Some(project)) => {
            let (workspace_id, project_id) = lookup_project(&state, workspace, project).await?;
            state
                .reader
                .search_pages_for_project(workspace_id, project_id, term, limit)
                .await
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                "workspace and project must be provided together",
            ));
        }
        _ => state.reader.search_pages(term, limit).await,
    }
    .map_err(internal_error)?;

    Ok(Json(enrich_hits(&state, hits).await?).into_response())
}

async fn recent_handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Response> {
    let _ = lookup_project(&state, &workspace, &project).await?;
    let mut pages = state
        .reader
        .list_pages(&workspace, &project)
        .await
        .map_err(internal_error)?;
    pages.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    pages.truncate(query.limit.clamp(1, 100));
    Ok(Json(pages).into_response())
}

async fn briefing_handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Response> {
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let briefing = state
        .reader
        .briefing_for_project(workspace_id, project_id, query.limit.clamp(1, 100))
        .await
        .map_err(internal_error)?;
    Ok(Json(briefing).into_response())
}

async fn lookup_project(
    state: &WebState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), Response> {
    let workspace_id = state
        .reader
        .find_workspace(workspace.to_owned())
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found(format!("workspace '{workspace}' not found")))?;
    let project_id = state
        .reader
        .find_project(workspace_id, project.to_owned())
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found(format!("project '{project}' not found")))?;
    Ok((workspace_id, project_id))
}

async fn enrich_hits(state: &WebState, hits: Vec<PageHit>) -> Result<Vec<ApiSearchHit>, Response> {
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        if let Some(meta) = state
            .reader
            .page_meta_by_id(hit.id)
            .await
            .map_err(internal_error)?
        {
            out.push(ApiSearchHit {
                kind: meta.kind,
                path: meta.path,
                project: meta.project_name,
                rank: hit.rank,
                snippet: hit.snippet,
                title: hit.title,
                workspace: meta.workspace_name,
            });
        }
    }
    Ok(out)
}

fn internal_error(e: impl std::fmt::Display) -> Response {
    json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn not_found(message: impl Into<String>) -> Response {
    json_error(StatusCode::NOT_FOUND, message)
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: message.into(),
        }),
    )
        .into_response()
}

fn default_limit() -> usize {
    10
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct LimitQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Debug, Serialize)]
struct ApiPage {
    workspace: String,
    project: String,
    path: String,
    title: String,
    kind: String,
    tier: String,
    pinned: bool,
    created_at: String,
    updated_at: String,
    supersedes: Option<String>,
    frontmatter: serde_json::Value,
    body_markdown: String,
}

#[derive(Debug, Serialize)]
struct ApiSearchHit {
    workspace: String,
    project: String,
    path: String,
    title: String,
    kind: String,
    snippet: String,
    rank: f64,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}
