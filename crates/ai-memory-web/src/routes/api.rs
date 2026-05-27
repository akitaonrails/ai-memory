//! JSON routes for third-party read-only frontends.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ai_memory_core::{PageId, PagePath, ProjectId, WorkspaceId};
use ai_memory_store::{BriefingSnapshot, HealthPage, PageHit, RelatedPage};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::state::WebState;

/// Build the `/api/v1` router from a shared [`WebState`].
pub(crate) fn build(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/workspaces", axum::routing::get(workspaces_handler))
        .route("/projects", axum::routing::get(projects_handler))
        .route(
            "/workspaces/{workspace}/projects/{project}/pages",
            axum::routing::get(pages_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/pages/{*path}",
            axum::routing::get(page_handler),
        )
        .route(
            "/search",
            axum::routing::get(search_handler).post(search_post_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/recent",
            axum::routing::get(recent_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/briefing",
            axum::routing::get(briefing_handler),
        )
        .route(
            "/workspaces/{workspace}/overview",
            axum::routing::get(overview_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/overview",
            axum::routing::get(project_overview_handler),
        )
        .with_state(state)
}

async fn workspaces_handler(State(state): State<Arc<WebState>>) -> Result<Response, Response> {
    let workspaces = state
        .reader
        .list_workspaces_with_stats()
        .await
        .map_err(internal_error)?;
    Ok(Json(workspaces).into_response())
}

async fn projects_handler(
    State(state): State<Arc<WebState>>,
    Query(query): Query<ProjectListQuery>,
) -> Result<Response, Response> {
    let workspace = query
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let projects = if let Some(workspace) = workspace {
        state
            .reader
            .list_projects_with_stats_for_workspace(workspace.to_owned())
            .await
    } else {
        state.reader.list_projects_with_stats().await
    }
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

    let links = state
        .reader
        .page_links(meta.workspace_id, meta.project_id, meta.path.clone())
        .await
        .map_err(internal_error)?;

    Ok(Json(ApiPage {
        backlinks: links.backlinks,
        body_markdown: markdown.body,
        created_at: meta.created_at,
        frontmatter: markdown.frontmatter,
        kind: meta.kind,
        links: links.links,
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
    RawQuery(raw_query): RawQuery,
) -> Result<Response, Response> {
    let query = SearchQuery::from_raw(raw_query.as_deref()).map_err(ApiFailure::into_response)?;
    let request = query
        .try_into_request()
        .map_err(ApiFailure::into_response)?;
    search_with_request(&state, request).await
}

async fn search_post_handler(
    State(state): State<Arc<WebState>>,
    Json(request): Json<SearchRequest>,
) -> Result<Response, Response> {
    search_with_request(&state, request).await
}

// NOTE (deferred): vector/semantic search. `ReaderPool::hybrid_search` already
// RRF-fuses FTS5 + cosine over stored embeddings + link-graph expansion, but it
// needs a query embedding — and `WebState` is read-only (reader + wiki), with no
// embedder. Wiring true semantic search means injecting an embedding client into
// `WebState` (touching `lib.rs`/`serve.rs`/`Cargo.toml`) and confirming the
// embedding provider (Ollama) is reachable from the deployment. Until then this
// handler stays FTS5-only. Link-graph "related pages" already ship via the
// page-view `links`/`backlinks` (`ReaderPool::page_links`).
async fn search_with_request(
    state: &WebState,
    request: SearchRequest,
) -> Result<Response, Response> {
    let term = request.q.trim().to_owned();
    if term.is_empty() {
        return Ok(Json(Vec::<ApiSearchHit>::new()).into_response());
    }

    let limit = request.limit.unwrap_or_else(default_limit).clamp(1, 100);
    if !request.scopes.is_empty()
        && (request
            .workspace
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
            || request
                .project
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty()))
    {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "scopes cannot be combined with workspace/project",
        ));
    }
    let hits = match scoped_search_mode(state, &request).await? {
        SearchMode::Global => state.reader.search_pages(term, limit).await,
        SearchMode::Scoped(scopes) => search_scopes(state, scopes, term, limit).await,
    }
    .map_err(internal_error)?;

    Ok(Json(enrich_hits(state, hits).await?).into_response())
}

async fn scoped_search_mode(
    state: &WebState,
    request: &SearchRequest,
) -> Result<SearchMode, Response> {
    if !request.scopes.is_empty() {
        if request.scopes.len() > MAX_SEARCH_SCOPES {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                format!("at most {MAX_SEARCH_SCOPES} scopes are allowed"),
            ));
        }
        let scopes = resolve_scopes(state, &request.scopes).await?;
        return Ok(SearchMode::Scoped(scopes));
    }

    match (
        trimmed_opt(request.workspace.as_deref()),
        trimmed_opt(request.project.as_deref()),
    ) {
        (Some(workspace), Some(project)) => {
            let (workspace_id, project_id) = lookup_project(state, workspace, project).await?;
            Ok(SearchMode::Scoped(vec![ResolvedSearchScope {
                project_id,
                workspace_id,
            }]))
        }
        (Some(_), None) | (None, Some(_)) => Err(json_error(
            StatusCode::BAD_REQUEST,
            "workspace and project must be provided together",
        )),
        _ => Ok(SearchMode::Global),
    }
}

async fn resolve_scopes(
    state: &WebState,
    scopes: &[ApiSearchScope],
) -> Result<Vec<ResolvedSearchScope>, Response> {
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    for scope in scopes {
        let workspace = trimmed_opt(Some(&scope.workspace)).ok_or_else(|| {
            json_error(StatusCode::BAD_REQUEST, "scope workspace cannot be empty")
        })?;
        let project = trimmed_opt(Some(&scope.project))
            .ok_or_else(|| json_error(StatusCode::BAD_REQUEST, "scope project cannot be empty"))?;
        let (workspace_id, project_id) = lookup_project(state, workspace, project).await?;
        if seen.insert((workspace_id, project_id)) {
            resolved.push(ResolvedSearchScope {
                project_id,
                workspace_id,
            });
        }
    }
    Ok(resolved)
}

async fn search_scopes(
    state: &WebState,
    scopes: Vec<ResolvedSearchScope>,
    term: String,
    limit: usize,
) -> ai_memory_store::StoreResult<Vec<PageHit>> {
    let mut hits_by_id: HashMap<PageId, PageHit> = HashMap::new();
    for scope in scopes {
        let hits = state
            .reader
            .search_pages_for_project(scope.workspace_id, scope.project_id, term.clone(), limit)
            .await?;
        for hit in hits {
            hits_by_id
                .entry(hit.id)
                .and_modify(|existing| {
                    if hit.rank < existing.rank {
                        *existing = hit.clone();
                    }
                })
                .or_insert(hit);
        }
    }
    let mut hits: Vec<PageHit> = hits_by_id.into_values().collect();
    hits.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
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

async fn overview_handler(
    State(state): State<Arc<WebState>>,
    Path(workspace): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Response> {
    let workspace_id = state
        .reader
        .find_workspace(workspace.clone())
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found(format!("workspace '{workspace}' not found")))?;

    let handoff = match state
        .reader
        .latest_open_handoff_for_workspace(workspace_id)
        .await
        .map_err(internal_error)?
    {
        Some(h) => {
            let project = state
                .reader
                .project_name_by_id(workspace_id, h.project_id)
                .await
                .map_err(internal_error)?
                .unwrap_or_default();
            Some(ApiHandoff {
                agent: h.from_agent.as_str().to_owned(),
                at: h.created_at.to_string(),
                project,
                summary: h.summary,
                open_questions: h.open_questions,
                next_steps: h.next_steps,
            })
        }
        None => None,
    };

    let briefing = state
        .reader
        .briefing_for_workspace(workspace_id, query.limit.clamp(1, 100))
        .await
        .map_err(internal_error)?;

    let (stale, duplicates, orphans) = state
        .reader
        .memory_health_for_workspace(workspace_id)
        .await
        .map_err(internal_error)?;
    let detail = state
        .reader
        .health_detail_for_workspace(workspace_id, query.limit.clamp(1, 100))
        .await
        .map_err(internal_error)?;
    let health = ApiHealth {
        stale,
        duplicates,
        contradictions: 0,
        orphans,
        audited_at: None,
        stale_pages: detail.stale,
        duplicate_pages: detail.duplicates,
        orphan_pages: detail.orphans,
    };

    Ok(Json(ApiOverview {
        handoff,
        briefing,
        health,
    })
    .into_response())
}

async fn project_overview_handler(
    State(state): State<Arc<WebState>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Response> {
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let limit = query.limit.clamp(1, 100);

    let handoff = state
        .reader
        .latest_open_handoff(workspace_id, project_id, None)
        .await
        .map_err(internal_error)?
        .map(|h| ApiHandoff {
            agent: h.from_agent.as_str().to_owned(),
            at: h.created_at.to_string(),
            project: project.clone(),
            summary: h.summary,
            open_questions: h.open_questions,
            next_steps: h.next_steps,
        });

    let briefing = state
        .reader
        .briefing_for_project(workspace_id, project_id, limit)
        .await
        .map_err(internal_error)?;

    let (stale, duplicates, orphans) = state
        .reader
        .memory_health_for_project(workspace_id, project_id)
        .await
        .map_err(internal_error)?;
    let detail = state
        .reader
        .health_detail_for_project(workspace_id, project_id, limit)
        .await
        .map_err(internal_error)?;
    let health = ApiHealth {
        stale,
        duplicates,
        contradictions: 0,
        orphans,
        audited_at: None,
        stale_pages: detail.stale,
        duplicate_pages: detail.duplicates,
        orphan_pages: detail.orphans,
    };

    Ok(Json(ApiOverview {
        handoff,
        briefing,
        health,
    })
    .into_response())
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

const MAX_SEARCH_SCOPES: usize = 25;

#[derive(Debug, Deserialize)]
struct ProjectListQuery {
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug)]
struct SearchQuery {
    q: String,
    workspace: Option<String>,
    project: Option<String>,
    scope: Vec<String>,
    limit: usize,
}

impl SearchQuery {
    fn from_raw(raw_query: Option<&str>) -> ApiParseResult<Self> {
        let mut query = Self {
            limit: default_limit(),
            project: None,
            q: String::new(),
            scope: Vec::new(),
            workspace: None,
        };
        let Some(raw_query) = raw_query else {
            return Ok(query);
        };
        for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
            let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode_query_component(raw_key)?;
            let value = decode_query_component(raw_value)?;
            match key.as_str() {
                "limit" => {
                    query.limit = value
                        .parse::<usize>()
                        .map_err(|_| ApiFailure::bad_request("limit must be an integer"))?;
                }
                "project" => query.project = Some(value),
                "q" | "query" => query.q = value,
                "scope" => query.scope.push(value),
                "workspace" => query.workspace = Some(value),
                _ => {}
            }
        }
        Ok(query)
    }

    fn try_into_request(self) -> ApiParseResult<SearchRequest> {
        let scopes = self
            .scope
            .iter()
            .flat_map(|raw| raw.split(','))
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(parse_scope_param)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SearchRequest {
            limit: Some(self.limit),
            project: self.project,
            q: self.q,
            scopes,
            workspace: self.workspace,
        })
    }
}

#[derive(Debug, Deserialize)]
struct SearchRequest {
    #[serde(default, alias = "query")]
    q: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    scopes: Vec<ApiSearchScope>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ApiSearchScope {
    workspace: String,
    project: String,
}

#[derive(Debug)]
struct ResolvedSearchScope {
    workspace_id: WorkspaceId,
    project_id: ProjectId,
}

#[derive(Debug)]
enum SearchMode {
    Global,
    Scoped(Vec<ResolvedSearchScope>),
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
    /// Latest pages this page references (resolved outgoing links).
    links: Vec<RelatedPage>,
    /// Latest pages that reference this page (incoming back-links).
    backlinks: Vec<RelatedPage>,
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
struct ApiOverview {
    handoff: Option<ApiHandoff>,
    briefing: BriefingSnapshot,
    health: ApiHealth,
}

#[derive(Debug, Serialize)]
struct ApiHandoff {
    agent: String,
    at: String,
    project: String,
    summary: String,
    open_questions: Vec<String>,
    next_steps: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ApiHealth {
    stale: u64,
    duplicates: u64,
    contradictions: u64,
    orphans: u64,
    audited_at: Option<String>,
    /// Capped drill-down lists explaining each counter.
    stale_pages: Vec<HealthPage>,
    duplicate_pages: Vec<HealthPage>,
    orphan_pages: Vec<HealthPage>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

type ApiParseResult<T> = Result<T, ApiFailure>;

#[derive(Debug)]
struct ApiFailure {
    message: String,
    status: StatusCode,
}

impl ApiFailure {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: StatusCode::BAD_REQUEST,
        }
    }

    fn into_response(self) -> Response {
        json_error(self.status, self.message)
    }
}

fn parse_scope_param(raw: &str) -> ApiParseResult<ApiSearchScope> {
    let Some((workspace, project)) = raw.split_once('/') else {
        return Err(ApiFailure::bad_request(
            "scope must use the workspace/project format",
        ));
    };
    let workspace = workspace.trim();
    let project = project.trim();
    if workspace.is_empty() || project.is_empty() || project.contains('/') {
        return Err(ApiFailure::bad_request(
            "scope must use the workspace/project format",
        ));
    }
    Ok(ApiSearchScope {
        project: project.to_owned(),
        workspace: workspace.to_owned(),
    })
}

fn trimmed_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

fn decode_query_component(raw: &str) -> ApiParseResult<String> {
    let mut bytes = Vec::with_capacity(raw.len());
    let raw_bytes = raw.as_bytes();
    let mut i = 0;
    while i < raw_bytes.len() {
        match raw_bytes[i] {
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= raw_bytes.len() {
                    return Err(ApiFailure::bad_request("invalid percent-encoding in query"));
                }
                let hi = hex_value(raw_bytes[i + 1])
                    .ok_or_else(|| ApiFailure::bad_request("invalid percent-encoding in query"))?;
                let lo = hex_value(raw_bytes[i + 2])
                    .ok_or_else(|| ApiFailure::bad_request("invalid percent-encoding in query"))?;
                bytes.push((hi << 4) | lo);
                i += 3;
            }
            byte => {
                bytes.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| ApiFailure::bad_request("query must be valid UTF-8"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
