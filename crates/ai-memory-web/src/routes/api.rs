//! JSON routes for third-party read-only frontends.

use std::collections::HashMap;
use std::sync::Arc;

use ai_memory_core::{ObservationKind, PageId, PagePath, ProjectId, SessionId, WorkspaceId};
use ai_memory_store::{
    BriefingSnapshot, HealthPage, ObservationOrder, ObservationPage, ObservationRecord, PageHit,
    RelatedPage, ScopeName, ScopeResolutionError, SessionSummary, lookup_existing_scope,
    resolve_many_existing_scopes,
};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::state::WebState;

/// Cache TTL for page-list, workspace, search, and summary endpoints.
const LIST_CACHE_MAX_AGE: u32 = 30;
/// Cache TTL for project page-list endpoint.
const PAGES_LIST_CACHE_MAX_AGE: u32 = 60;
/// Cache TTL (and ETag max-age) for single-page reads.
const PAGE_CACHE_MAX_AGE: u32 = 300;

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
        .route(
            "/workspaces/{workspace}/projects/{project}/handoffs",
            axum::routing::get(handoffs_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/sessions",
            axum::routing::get(sessions_handler),
        )
        .route(
            "/workspaces/{workspace}/projects/{project}/sessions/{session_id}/observations",
            axum::routing::get(session_observations_handler),
        )
        .route("/graph", axum::routing::get(graph_handler))
        .with_state(state)
}

/// Attach `Cache-Control: private, max-age=N` to a successful response.
///
/// Applied only to 2xx bodies — error paths return their responses
/// directly without calling this, so error responses stay uncached.
fn with_cache(resp: Response, max_age: u32) -> Response {
    let mut resp = resp;
    if let Ok(val) = HeaderValue::from_str(&format!("private, max-age={max_age}")) {
        resp.headers_mut().insert(header::CACHE_CONTROL, val);
    }
    resp
}

/// Prevent browser reuse of a response whose body depends on request identity.
///
/// `private` alone still permits a browser cache to serve Alice's response
/// after credentials at the same URL switch to Bob. These endpoints contain
/// owner-scoped prompt text, so successful responses must be revalidated by
/// executing the authorization/filtering path on every request.
fn with_no_store(resp: Response) -> Response {
    let mut resp = resp;
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    resp
}

async fn workspaces_handler(State(state): State<Arc<WebState>>) -> Result<Response, Response> {
    let workspaces = state
        .reader
        .list_workspaces_with_stats()
        .await
        .map_err(internal_error)?;
    Ok(with_cache(
        Json(workspaces).into_response(),
        LIST_CACHE_MAX_AGE,
    ))
}

/// Cross-project dependency graph: every resolved link whose endpoints are
/// in different projects, each carrying both endpoints' workspace/project/
/// path. The UI builds nodes from the endpoints (and may aggregate to a
/// project-level dependency graph). Global for now; project scoping is a
/// follow-up query param.
async fn graph_handler(State(state): State<Arc<WebState>>) -> Result<Response, Response> {
    let edges = state
        .reader
        .cross_project_edges(None)
        .await
        .map_err(internal_error)?;
    Ok(with_cache(
        Json(serde_json::json!({ "edges": edges })).into_response(),
        LIST_CACHE_MAX_AGE,
    ))
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
    Ok(with_cache(
        Json(projects).into_response(),
        LIST_CACHE_MAX_AGE,
    ))
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
    Ok(with_cache(
        Json(pages).into_response(),
        PAGES_LIST_CACHE_MAX_AGE,
    ))
}

async fn page_handler(
    State(state): State<Arc<WebState>>,
    headers: axum::http::HeaderMap,
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

    // ETag is computed over the markdown body PLUS the resolved author
    // identity (P1.7). Author change without body change (e.g. operator
    // rotates a token then user A re-writes a previously-anonymous page)
    // must invalidate the cached response. SHA-256 over a stable
    // concatenation: body || "\n--\n" || username || "\n" || email.
    // The "\n--\n" separator prevents `body=foo, user=bar` from hashing
    // the same as `body=foobar, user=` if either ever happened to slide
    // empty.
    let mut hasher = Sha256::new();
    hasher.update(markdown.body.as_bytes());
    if let Some(author) = meta.author.as_ref() {
        hasher.update(b"\n--\n");
        hasher.update(author.username.as_bytes());
        hasher.update(b"\n");
        if let Some(email) = author.email.as_deref() {
            hasher.update(email.as_bytes());
        }
    }
    let digest = hasher.finalize();
    let etag_hex = digest.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    let etag_value = format!("\"{etag_hex}\"");
    let etag_header = HeaderValue::from_str(&etag_value).expect("sha256 hex is always valid ASCII");

    // If-None-Match: if the client sends back the exact ETag we issued,
    // return 304 with no body (no re-serialisation needed).
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && inm == etag_value
    {
        let mut resp = axum::http::Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(axum::body::Body::empty())
            .expect("static builder cannot fail");
        resp.headers_mut().insert(header::ETAG, etag_header.clone());
        if let Ok(cc) = HeaderValue::from_str(&format!("private, max-age={PAGE_CACHE_MAX_AGE}")) {
            resp.headers_mut().insert(header::CACHE_CONTROL, cc);
        }
        return Ok(resp);
    }

    let links = state
        .reader
        .page_links(meta.workspace_id, meta.project_id, meta.path.clone())
        .await
        .map_err(internal_error)?;

    let mut resp = with_cache(
        Json(ApiPage {
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
            author: meta.author,
        })
        .into_response(),
        PAGE_CACHE_MAX_AGE,
    );
    resp.headers_mut().insert(header::ETAG, etag_header);
    Ok(resp)
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
// RRF-fuses FTS5 + entity matching + cosine over stored embeddings + link-graph
// expansion, but it needs a query embedding — and `WebState` is read-only
// (reader + wiki), with no embedder. Wiring true semantic search means injecting
// an embedding client into `WebState` (touching `lib.rs`/`serve.rs`/`Cargo.toml`)
// and confirming the embedding provider (Ollama) is reachable from the
// deployment. Until then this handler stays FTS5-only. Link-graph "related
// pages" already ship via the page-view `links`/`backlinks`
// (`ReaderPool::page_links`).
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

    Ok(with_cache(
        Json(enrich_hits(state, hits).await?).into_response(),
        LIST_CACHE_MAX_AGE,
    ))
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
    let names: Vec<_> = scopes
        .iter()
        .map(|scope| ScopeName::new(&scope.workspace, &scope.project))
        .collect();
    resolve_many_existing_scopes(&state.reader, &names, MAX_SEARCH_SCOPES)
        .await
        .map(|scopes| {
            scopes
                .into_iter()
                .map(|scope| ResolvedSearchScope {
                    workspace_id: scope.workspace_id,
                    project_id: scope.project_id,
                })
                .collect()
        })
        .map_err(scope_error_response)
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
            .search_pages_for_project(
                scope.workspace_id,
                scope.project_id,
                term.clone(),
                limit,
                None,
            )
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
    Ok(with_cache(Json(pages).into_response(), LIST_CACHE_MAX_AGE))
}

async fn briefing_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Response> {
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let briefing = state
        .reader
        .briefing_for_project(
            workspace_id,
            project_id,
            query.limit.clamp(1, 100),
            owner_filter_for(actor),
            false,
        )
        .await
        .map_err(internal_error)?;
    Ok(with_no_store(Json(briefing).into_response()))
}

async fn overview_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Path(workspace): Path<String>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Response> {
    let workspace_id = state
        .reader
        .find_workspace(workspace.clone())
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found(format!("workspace '{workspace}' not found")))?;

    // Scoped to the requesting actor, like the briefing below and like
    // `project_overview_handler`: an identified caller sees their own baton plus
    // the shared ones, a browser the server cannot attribute sees only the
    // shared ones — so an owned handoff, and the raw prompt text inside it, is
    // never rendered to someone it does not belong to. Pinning this to
    // "unattributed" instead would make the card permanently empty on any server
    // that stamps an owner, while `pending_handoff_count` in the briefing beside
    // it — which does apply the actor's filter — kept counting the same row.
    let owner_filter = owner_filter_for(actor);
    let handoff = match state
        .reader
        .latest_open_handoff_for_workspace(workspace_id, owner_filter.clone())
        .await
        .map_err(internal_error)?
    {
        Some(h) => {
            let project = state
                .reader
                .project_name_by_id(workspace_id, h.scope.project_id)
                .await
                .map_err(internal_error)?
                .unwrap_or_default();
            Some(ApiHandoff {
                agent: h.origin.from_agent.as_str().to_owned(),
                at: h.lifecycle.created_at.to_string(),
                project,
                summary: h.content.summary,
                open_questions: h.content.open_questions,
                next_steps: h.content.next_steps,
            })
        }
        None => None,
    };

    let briefing = state
        .reader
        .briefing_for_workspace(workspace_id, query.limit.clamp(1, 100), owner_filter)
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

    Ok(with_no_store(
        Json(ApiOverview {
            handoff,
            briefing,
            health,
        })
        .into_response(),
    ))
}

/// Build the owner filter for a read-only API request.
///
/// A caller the server can name sees their own rows plus the shared ones; a
/// caller it cannot sees only shared ones, so an owned handoff — and the
/// prompt-derived text inside it — never reaches someone it does not belong to.
///
/// "Can name" is [`ai_memory_core::ActorContext::identity_key`], not
/// `actor.user`: an ingress that forwards a complete OIDC issuer/subject pair
/// may leave `user` empty, and reading `user` here would file that operator as
/// unattributed while the auth layer calls them a user — hiding their own
/// handoffs, and the bodies of the shared ones, from them in their own UI.
fn owner_filter_for(
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
) -> ai_memory_core::OwnerFilter {
    ai_memory_core::OwnerFilter::for_actor_context(
        &actor.map_or_else(ai_memory_core::ActorContext::anonymous, |ext| ext.0),
    )
}

/// May this request read the prompt-derived body of a handoff?
///
/// Three ways to qualify: an authenticated caller resolved to an identity and
/// is reading rows that are their own or shared; the caller holds root, which is
/// the operator —
/// they already read every page body through the wiki API, so redacting their
/// own handoffs from their own UI costs them and protects nobody; or the server
/// does not authenticate at all and the whole wiki is open by design.
/// `require_bearer` stamps [`ai_memory_core::AuthLevel::Anonymous`] on every
/// request of a server with no configured token; the extension is missing only
/// when the API is mounted with no auth layer whatsoever, which is the same open
/// posture. What is left — an auth-configured server, a caller it can neither
/// name nor recognise as root — is the only case that redacts. See
/// [`handoffs_handler`] for why the body is treated differently from the
/// metadata beside it.
///
/// [`ai_memory_core::OwnerFilter::Any`] is the cross-owner recovery filter, so
/// it is root-only here. The `all_owners` query switch produces it only after
/// checking the request's resolved [`ai_memory_core::AuthLevel`]. Keeping the
/// body check local as well prevents a future caller from constructing `Any`
/// without the matching authorization gate.
///
/// # What actually reaches the `Unattributed` arm
///
/// Two live cases, both served, and one that redacts and has no producer today.
/// `Unattributed` + [`ai_memory_core::AuthLevel::Anonymous`] is rung 0 — no
/// `[auth].bearer_token`, the default — which stamps
/// [`ai_memory_core::ActorContext::anonymous`] on **every** request; and
/// `Unattributed` + [`ai_memory_core::AuthLevel::Root`] is an authenticating
/// server with no `[auth].root_username`, where the root template names nobody.
/// Both are open postures and the arm serves them.
///
/// What has no producer is `Unattributed` at a NAMED tier: the DB-user rung
/// fills `user` from the row, and the proxy downgrade only reaches
/// [`ai_memory_core::AuthLevel::User`] when it asserted a username or a
/// complete issuer/subject pair. Both are resolved by
/// [`ai_memory_core::ActorContext::identity_key`], so the filter is `User(_)`
/// and the body is served by the arm above. That gap is the fail-safe: a future
/// rung, or a mount that injects a tier without an actor, redacts by default
/// instead of leaking. Weakening the arm because "nothing produces it" removes
/// the fail-safe, not dead code.
fn serves_handoff_body(
    owner_filter: &ai_memory_core::OwnerFilter,
    auth: Option<axum::Extension<ai_memory_core::AuthLevel>>,
) -> bool {
    let level = auth.map(|axum::Extension(level)| level);
    let root = level == Some(ai_memory_core::AuthLevel::Root);
    match owner_filter {
        ai_memory_core::OwnerFilter::User(_) => {
            matches!(
                level,
                Some(ai_memory_core::AuthLevel::User | ai_memory_core::AuthLevel::Root)
            )
        }
        ai_memory_core::OwnerFilter::Any => root,
        ai_memory_core::OwnerFilter::Unattributed => {
            root || matches!(level, None | Some(ai_memory_core::AuthLevel::Anonymous))
        }
    }
}

/// One handoff in the listing. Richer than [`ApiHandoff`] (the single "pending"
/// card): it carries the state and ownership needed to answer "where did my
/// baton go".
#[derive(Serialize)]
struct ApiHandoffEntry {
    id: String,
    agent: String,
    at: String,
    state: String,
    /// Prompt-derived text, withheld from a caller the server can neither name
    /// nor recognise as root — see [`handoffs_handler`]. Absent together with
    /// `open_questions` and `next_steps` whenever `redacted` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_questions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_steps: Option<Vec<String>>,
    /// True when the three fields above were withheld, so a frontend can say
    /// "sign in to read this" instead of rendering a blank card.
    redacted: bool,
    files_touched: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    /// Operator the handoff belongs to (qualified `oidc:`/`user:` storage key);
    /// absent when shared with the project.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    /// Operator that consumed it, when it has been accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    accepted_at: Option<String>,
}

#[derive(Deserialize)]
struct HandoffListQuery {
    /// `open` | `accepted` | `expired`. Omit for every state — which is how an
    /// operator finds a baton that was already consumed.
    #[serde(default)]
    state: Option<String>,
    #[serde(default = "default_handoff_limit")]
    limit: usize,
    /// Root-only recovery view across every operator. Omitted/false keeps the
    /// normal own-plus-shared ownership boundary.
    #[serde(default)]
    all_owners: bool,
}

const fn default_handoff_limit() -> usize {
    50
}

/// List a project's handoffs.
///
/// Before this there was no way to enumerate handoffs anywhere in the system:
/// readers only ever fetched the single pending one and consumed it, so a baton
/// that went to the wrong place simply vanished. Scoped by owner, so this does
/// not become a way to read other operators' context.
///
/// # The prompt-derived body needs a caller the server can vouch for
///
/// The metadata — state, timestamps, agent, cwd, touched files, ownership — is
/// what makes the listing useful and is served to anyone who may call it.
/// `summary`, `open_questions` and `next_steps` are not metadata: an automatic
/// SessionEnd handoff synthesises them verbatim from the operator's prompts, and
/// the listing returns the project's whole history rather than the single newest
/// open row the overview card shows. A caller with no identity matches every
/// *shared* handoff — which is all of them on a server that stamps no owner — so
/// serving those three fields to one would hand the entire prompt trail to
/// whoever can reach the endpoint.
///
/// [`serves_handoff_body`] therefore withholds them from exactly one caller: an
/// auth-configured server's request that resolved to neither an identity nor
/// root. Root is the operator — `[auth].root_username` is optional, so the
/// ordinary single-operator deployment authenticates as root while stamping no
/// owner on anything, and redacting there would blank out the operator's own
/// handoffs in their own UI while the overview card beside it, which never
/// gated the same three fields, kept rendering them.
///
/// A server with no auth configured is the other open case: it already serves
/// every page body unauthenticated, so withholding here would narrow nothing
/// while making the listing useless. The rule is about not widening what a
/// server that *does* authenticate shows to a caller it cannot place.
async fn handoffs_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    auth: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<HandoffListQuery>,
) -> Result<Response, Response> {
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let handoff_state = match query.state.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(raw.parse::<ai_memory_core::HandoffState>().map_err(|_| {
            json_error(
                StatusCode::BAD_REQUEST,
                format!("unknown handoff state: {raw}"),
            )
        })?),
    };
    let level = auth.as_ref().map(|axum::Extension(level)| *level);
    let owner_filter = if query.all_owners {
        if level != Some(ai_memory_core::AuthLevel::Root) {
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "all_owners requires root authorization",
            ));
        }
        ai_memory_core::OwnerFilter::Any
    } else {
        owner_filter_for(actor)
    };
    let with_body = serves_handoff_body(&owner_filter, auth);
    let handoffs = state
        .reader
        .list_handoffs(
            workspace_id,
            project_id,
            handoff_state,
            owner_filter,
            query.limit.clamp(1, 200),
        )
        .await
        .map_err(internal_error)?;
    let entries: Vec<ApiHandoffEntry> = handoffs
        .into_iter()
        .map(|h| ApiHandoffEntry {
            id: h.scope.id.to_string(),
            agent: h.origin.from_agent.as_str().to_owned(),
            at: h.lifecycle.created_at.to_string(),
            state: h.lifecycle.state.as_str().to_owned(),
            summary: with_body.then_some(h.content.summary),
            open_questions: with_body.then_some(h.content.open_questions),
            next_steps: with_body.then_some(h.content.next_steps),
            redacted: !with_body,
            files_touched: h.content.files_touched,
            cwd: h.origin.cwd,
            owner: h.origin.owner_user,
            accepted_by: h.lifecycle.accepted_by_user,
            accepted_at: h.lifecycle.accepted_at.map(|t| t.to_string()),
        })
        .collect();
    Ok(with_no_store(
        Json(serde_json::json!({ "handoffs": entries })).into_response(),
    ))
}

async fn project_overview_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<LimitQuery>,
) -> Result<Response, Response> {
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let limit = query.limit.clamp(1, 100);

    // Same scoping as `overview_handler`; computed once and reused for the
    // briefing below so both halves of the response agree on visibility.
    let owner_filter = owner_filter_for(actor);
    let handoff = state
        .reader
        .latest_open_handoff(workspace_id, project_id, None, owner_filter.clone())
        .await
        .map_err(internal_error)?
        .map(|h| ApiHandoff {
            agent: h.origin.from_agent.as_str().to_owned(),
            at: h.lifecycle.created_at.to_string(),
            project: project.clone(),
            summary: h.content.summary,
            open_questions: h.content.open_questions,
            next_steps: h.content.next_steps,
        });

    let briefing = state
        .reader
        .briefing_for_project(workspace_id, project_id, limit, owner_filter, false)
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

    Ok(with_no_store(
        Json(ApiOverview {
            handoff,
            briefing,
            health,
        })
        .into_response(),
    ))
}

/// Bounds for the session routes. They mirror the MCP
/// `memory_read_session_observations` tool so a frontend and an agent see
/// the same page sizes and body caps.
const SESSION_LIST_MAX_LIMIT: usize = 100;
const SESSION_OBSERVATIONS_MAX_LIMIT: usize = 200;
const SESSION_OBSERVATIONS_DEFAULT_BODY_CHARS: usize = 4_000;
const SESSION_OBSERVATIONS_MIN_BODY_CHARS: usize = 200;
const SESSION_OBSERVATIONS_MAX_BODY_CHARS: usize = 16_384;

async fn sessions_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<SessionListQuery>,
) -> Result<Response, Response> {
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let sessions = state
        .reader
        .sessions_for_scope(
            workspace_id,
            project_id,
            owner_filter_for(actor),
            query.include_open,
            query.limit.clamp(1, SESSION_LIST_MAX_LIMIT),
            query.offset,
        )
        .await
        .map_err(internal_error)?;
    Ok(with_no_store(
        Json(ApiSessionList { sessions }).into_response(),
    ))
}

async fn session_observations_handler(
    State(state): State<Arc<WebState>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Path((workspace, project, session_id)): Path<(String, String, String)>,
    Query(query): Query<SessionObservationsQuery>,
) -> Result<Response, Response> {
    let (workspace_id, project_id) = lookup_project(&state, &workspace, &project).await?;
    let session_id = session_id.parse::<SessionId>().map_err(|_| {
        json_error(
            StatusCode::BAD_REQUEST,
            format!("invalid session id: {session_id}"),
        )
    })?;
    let limit = query.limit.clamp(1, SESSION_OBSERVATIONS_MAX_LIMIT);
    let body_max_chars = query
        .body_max_chars
        .unwrap_or(SESSION_OBSERVATIONS_DEFAULT_BODY_CHARS)
        .clamp(
            SESSION_OBSERVATIONS_MIN_BODY_CHARS,
            SESSION_OBSERVATIONS_MAX_BODY_CHARS,
        );
    let order = match query.order.as_deref().map(str::trim) {
        None | Some("") => ObservationOrder::Asc,
        Some(raw) if raw.eq_ignore_ascii_case("asc") => ObservationOrder::Asc,
        Some(raw) if raw.eq_ignore_ascii_case("desc") => ObservationOrder::Desc,
        Some(raw) => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                format!("unknown order: {raw} (expected asc or desc)"),
            ));
        }
    };
    let mut kinds = Vec::new();
    for kind in query
        .kinds
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
    {
        let Ok(parsed) = kind.parse::<ObservationKind>() else {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                format!("unknown observation kind: {kind}"),
            ));
        };
        kinds.push(parsed);
    }
    let kinds = (!kinds.is_empty()).then_some(kinds);

    // Same visibility predicate as the MCP tool: the session must have its
    // row or at least one observation in this scope and pass the owner
    // filter, otherwise the id reads as not found so a known uuid cannot
    // probe another project or operator.
    let session = state
        .reader
        .session_summary_scoped(
            workspace_id,
            project_id,
            session_id,
            owner_filter_for(actor),
        )
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found(format!("session '{session_id}' not found")))?;
    let page = state
        .reader
        .session_observations_scoped(
            workspace_id,
            project_id,
            session_id,
            ObservationPage {
                limit,
                offset: query.offset,
                order,
                kinds,
                query: query.q.clone(),
            },
        )
        .await
        .map_err(internal_error)?;
    let observations = page
        .records
        .into_iter()
        .map(|mut record| {
            record.body = cap_body(&record.body, body_max_chars);
            record
        })
        .collect();
    Ok(with_no_store(
        Json(ApiSessionObservations {
            session,
            observations,
            total: page.total,
            offset: query.offset,
            limit,
            order,
            elided_other_scope: page.elided_other_scope,
            body_max_chars,
        })
        .into_response(),
    ))
}

/// Cap one observation body with a visible marker. Same shape as
/// `ai_memory_consolidate::projection::cap_text_with_marker`, which the MCP
/// tool uses; duplicated here because the web crate does not depend on the
/// consolidation pipeline.
fn cap_body(body: &str, max_chars: usize) -> String {
    let total = body.chars().count();
    if total <= max_chars {
        return body.to_owned();
    }
    let mut out: String = body.chars().take(max_chars).collect();
    out.push_str(&format!(
        "\n[body truncated; {} chars omitted]",
        total - max_chars
    ));
    out
}

async fn lookup_project(
    state: &WebState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), Response> {
    lookup_existing_scope(&state.reader, workspace, project)
        .await
        .map(ai_memory_store::ResolvedScope::as_tuple)
        .map_err(|err| match err {
            ScopeResolutionError::ProjectNotFoundInWorkspace { project, .. } => {
                not_found(format!("project '{project}' not found"))
            }
            other => scope_error_response(other),
        })
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
    tracing::error!(error = %e, "web API internal error");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
}

fn not_found(message: impl Into<String>) -> Response {
    json_error(StatusCode::NOT_FOUND, message)
}

fn scope_error_response(err: ScopeResolutionError) -> Response {
    if err.is_bad_request() {
        json_error(StatusCode::BAD_REQUEST, err.to_string())
    } else if err.is_not_found() {
        json_error(StatusCode::NOT_FOUND, err.to_string())
    } else {
        internal_error(err)
    }
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

fn default_session_list_limit() -> usize {
    20
}

fn default_session_observations_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct SessionListQuery {
    #[serde(default = "default_session_list_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    /// Also list sessions that have not ended yet. Default false.
    #[serde(default)]
    include_open: bool,
}

#[derive(Debug, Deserialize)]
struct SessionObservationsQuery {
    #[serde(default = "default_session_observations_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    /// `asc` (capture order, default) or `desc`.
    #[serde(default)]
    order: Option<String>,
    /// Comma-separated observation kinds, e.g. `user-prompt,stop`.
    #[serde(default)]
    kinds: Option<String>,
    /// Full-text query restricted to the session.
    #[serde(default)]
    q: Option<String>,
    /// Per-body character cap; clamped to `200..=16384`, default `4000`.
    #[serde(default)]
    body_max_chars: Option<usize>,
}

#[derive(Debug, Serialize)]
struct ApiSessionList {
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Serialize)]
struct ApiSessionObservations {
    session: SessionSummary,
    observations: Vec<ObservationRecord>,
    total: u64,
    offset: usize,
    limit: usize,
    order: ObservationOrder,
    elided_other_scope: u64,
    body_max_chars: usize,
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
    /// Multi-user attribution (P1.7). `None` for pre-multi-user pages
    /// and root / anonymous writes; `Some` when JOIN against the
    /// `users` table resolved a row at read time. Omitted from the
    /// serialised payload when `None` so the response shape stays
    /// backward-compatible with consumers that pre-date v0.8.
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<ai_memory_store::PageAuthor>,
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

#[cfg(test)]
mod tests {
    use super::{internal_error, serves_handoff_body};
    use ai_memory_core::{AuthLevel, OwnerFilter};
    use axum::{Extension, body::to_bytes};

    /// The `all_owners` recovery switch routes `Any` into the listing. Reading
    /// past ownership must cost root — not merely a token the server accepted,
    /// and not a name the request happens to carry, since the rows returned then
    /// belong to other operators.
    #[test]
    fn cross_owner_filter_serves_body_only_to_root() {
        for level in [None, Some(AuthLevel::Anonymous), Some(AuthLevel::User)] {
            assert!(
                !serves_handoff_body(&OwnerFilter::Any, level.map(Extension)),
                "cross-owner body served at {level:?}"
            );
        }
        assert!(serves_handoff_body(
            &OwnerFilter::Any,
            Some(Extension(AuthLevel::Root))
        ));
    }

    #[test]
    fn named_owner_filter_requires_an_authenticated_tier() {
        let filter = OwnerFilter::User("user:alice".into());
        for level in [None, Some(AuthLevel::Anonymous)] {
            assert!(
                !serves_handoff_body(&filter, level.map(Extension)),
                "actor identity without a matching authenticated tier served a body"
            );
        }
        for level in [AuthLevel::User, AuthLevel::Root] {
            assert!(serves_handoff_body(&filter, Some(Extension(level))));
        }
    }

    /// The operator of the ordinary authenticated deployment: root bearer, no
    /// `[auth].root_username`, so nothing names them and every handoff they
    /// wrote is shared. Redacting there hides their own data from them.
    #[test]
    fn root_reads_the_body_it_wrote_unattributed() {
        assert!(serves_handoff_body(
            &OwnerFilter::Unattributed,
            Some(Extension(AuthLevel::Root))
        ));
        assert!(!serves_handoff_body(
            &OwnerFilter::Unattributed,
            Some(Extension(AuthLevel::User))
        ));
    }

    #[tokio::test]
    async fn internal_errors_do_not_expose_their_cause() {
        let sentinel = "web-api-secret /srv/ai-memory/db/memory.sqlite";
        let response = internal_error(sentinel);

        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error response body should be readable");
        let body = std::str::from_utf8(&body).expect("error response body should be UTF-8");
        assert_eq!(body, r#"{"error":"internal server error"}"#);
        assert!(!body.contains(sentinel));
        assert!(!body.contains("/srv/ai-memory"));
    }
}
