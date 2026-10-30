//! `GET /search?q=…` — search, with an optional recall trace.
//!
//! Two modes, the same two `memory_query` has:
//!
//! - **Project search** (`workspace` + `project`): the hybrid ranker
//!   (FTS5 + entity + vector + graph + observation RRF, authority, optional
//!   rerank) through [`crate::recall`], so the order matches `memory_query`
//!   for the same project. `explain=1` adds the recall trace: per-hit stream
//!   ranks, raw scores, RRF contributions, authority and rerank score, plus
//!   per-stream timings.
//! - **Cross-project search** (no scope): FTS5 over every repository the
//!   viewer may read, as `memory_query(global=true)` does. Its trace is the
//!   FTS time alone; there are no per-hit streams to show.
//!
//! `kind` and `tier` narrow either mode inside each stream's SQL, so a rare
//! kind is never crowded out by other kinds. Survivors are ranked among
//! themselves, so their order can differ slightly from the unfiltered hits of
//! the same query, and graph expansion seeds only from pages the filter keeps.

use std::collections::BTreeMap;
use std::sync::Arc;

use ai_memory_core::Tier;
use ai_memory_store::{PAGE_KINDS, PageFilter, SearchExplain, StreamTimings};
use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use crate::markdown;
use crate::state::WebState;
use crate::templates::{
    FilterOption, HitTrace, ScopeGroup, ScopeOption, SearchHit, SearchView, StreamTraceRow,
    TimingRow, encode_segment, page_href, project_href,
};

/// Hits a cross-project search returns when the request names no `limit`.
const GLOBAL_DEFAULT_LIMIT: usize = 50;
/// Hits a project search returns when the request names no `limit`.
const SCOPED_DEFAULT_LIMIT: usize = 20;
/// Upper bound on `limit`, the same clamp `memory_query` applies.
const MAX_LIMIT: usize = 100;

/// Query-string parameters for the search endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct SearchParams {
    /// The free-text search query.
    #[serde(default)]
    pub q: String,
    /// Workspace of a project search; must come with `project`.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Project of a project search; must come with `workspace`.
    #[serde(default)]
    pub project: Option<String>,
    /// `workspace/project` from the search page's selector; empty means every
    /// project. Wins over `workspace` + `project` when both are sent.
    #[serde(default)]
    pub scope: Option<String>,
    /// Keep only pages of this kind; empty keeps every kind.
    #[serde(default)]
    pub kind: Option<String>,
    /// Keep only pages of this tier; empty keeps every tier.
    #[serde(default)]
    pub tier: Option<String>,
    /// Most hits to show, clamped to `1..=100`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// `1`/`true`/`on` shows the recall trace.
    #[serde(default)]
    pub explain: Option<String>,
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// Handler for `GET /search?q=…`.
///
/// A project search resolves the scope through the guarded, no-create
/// lookup (#708): a project the viewer may not read is refused before any
/// ranking runs, so the trace cannot describe it. A cross-project search
/// covers only readable repositories — see
/// `ReaderPool::search_pages_with_meta`. No viewer (no database users, or
/// root) reads everything, as before.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Query(params): Query<SearchParams>,
) -> Response {
    let query = params.q.trim().to_owned();
    let explain = params
        .explain
        .as_deref()
        .is_some_and(|v| matches!(v, "1" | "true" | "on" | "yes"));
    let viewer = viewer.map(|axum::Extension(viewer)| viewer.user());
    let mut scope = match non_empty(params.scope.as_deref()) {
        Some(value) => match value.split_once('/') {
            Some((workspace, project)) if !workspace.is_empty() && !project.is_empty() => {
                Some((workspace.to_owned(), project.to_owned()))
            }
            _ => {
                return (StatusCode::BAD_REQUEST, "scope must be workspace/project")
                    .into_response();
            }
        },
        None => match (
            non_empty(params.workspace.as_deref()),
            non_empty(params.project.as_deref()),
        ) {
            (Some(workspace), Some(project)) => Some((workspace.to_owned(), project.to_owned())),
            (None, None) => None,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    "workspace and project must be provided together",
                )
                    .into_response();
            }
        },
    };
    let kind = non_empty(params.kind.as_deref()).map(str::to_owned);
    // Matched like `kind`, ignoring ASCII case; the canonical name is what
    // the SQL compares.
    let tier = match non_empty(params.tier.as_deref()) {
        Some(value) => match Tier::ALL
            .iter()
            .find(|t| t.as_str().eq_ignore_ascii_case(value))
        {
            Some(t) => Some(t.as_str()),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    "tier must be working, episodic, semantic or procedural",
                )
                    .into_response();
            }
        },
        None => None,
    };
    let filtered = kind.is_some() || tier.is_some();
    let filter = PageFilter {
        kind: kind.as_deref(),
        tier,
        ..PageFilter::default()
    };
    let internal = |error: ai_memory_store::StoreError| {
        tracing::error!(%error, "web search failed");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    };

    let mut hits = Vec::new();
    // One more hit than shown is requested, so a cut-off list can be told
    // apart from one that has exactly `limit` matches.
    let mut limit_used = 0;
    let mut truncated = false;
    let mut timings = None;
    let mut streams_active: Vec<&'static str> = Vec::new();
    match &mut scope {
        Some((workspace, project)) => {
            let resolved = match super::resolve_read_scope(&state, viewer, workspace, project).await
            {
                Ok(resolved) => resolved,
                Err(refusal) => return super::page::refusal_response(&refusal),
            };
            // Scope lookup accepts canonical and legacy keys, but page URLs
            // use the exact stored name. Resolve it only after authorization,
            // without promoting the project as a side effect of a read.
            *project = match state
                .reader
                .project_name_by_id(resolved.workspace_id, resolved.project_id)
                .await
            {
                Ok(Some(name)) => name,
                Ok(None) => return super::page::not_found_response(),
                Err(error) => return internal(error),
            };
            if !query.is_empty() {
                let limit = params
                    .limit
                    .unwrap_or(SCOPED_DEFAULT_LIMIT)
                    .clamp(1, MAX_LIMIT);
                limit_used = limit;
                let result = match crate::recall::search_scopes(
                    &state,
                    &[(resolved.workspace_id, resolved.project_id)],
                    &query,
                    limit + 1,
                    explain,
                    &super::api::owner_filter_for(actor),
                    filter,
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => return internal(error),
                };
                timings = result.timings;
                streams_active = result.streams_active;
                truncated = result.hits.len() > limit;
                // The trace position is the rank among the hits shown.
                for (position, (hit, details)) in result.hits.into_iter().take(limit).enumerate() {
                    let path = hit.path.as_str().to_owned();
                    hits.push(SearchHit {
                        href: context_href(
                            &page_href(workspace, project, &path),
                            &params,
                            false,
                            limit,
                        ),
                        workspace: workspace.clone(),
                        project: project.clone(),
                        path,
                        title: hit.title,
                        snippet: markdown::escape_snippet(&hit.snippet),
                        trace: details
                            .filter(|_| explain)
                            .map(|details| hit_trace(position + 1, hit.rank, &details)),
                    });
                }
            }
        }
        None if !query.is_empty() => {
            let limit = params
                .limit
                .unwrap_or(GLOBAL_DEFAULT_LIMIT)
                .clamp(1, MAX_LIMIT);
            limit_used = limit;
            let started = explain.then(std::time::Instant::now);
            let raw = match state
                .reader
                .search_pages_with_meta(query.clone(), limit + 1, None, viewer, filter)
                .await
            {
                Ok(raw) => raw,
                Err(error) => return internal(error),
            };
            if let Some(started) = started {
                timings = Some(StreamTimings {
                    fts_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
                    ..StreamTimings::default()
                });
                streams_active = vec!["fts"];
            }
            truncated = raw.len() > limit;
            for h in raw.into_iter().take(limit) {
                let path = h.path.as_str().to_owned();
                hits.push(SearchHit {
                    href: context_href(
                        &page_href(&h.workspace_name, &h.project_name, &path),
                        &params,
                        true,
                        limit,
                    ),
                    workspace: h.workspace_name,
                    project: h.project_name,
                    path,
                    title: h.title,
                    snippet: markdown::escape_snippet(&h.snippet),
                    trace: None,
                });
            }
        }
        None => {}
    }

    let (workspace, project) = scope.unwrap_or_default();
    // Only readable projects, like the home page's cards, so the selector
    // never names a repository the viewer cannot open.
    let summaries = match state.reader.list_projects_with_stats(viewer).await {
        Ok(summaries) => summaries,
        Err(error) => return internal(error),
    };
    // The reader returns projects newest first, which interleaves
    // workspaces; group and order by name instead.
    let mut by_workspace: BTreeMap<String, Vec<ScopeOption>> = BTreeMap::new();
    for s in summaries {
        let option = ScopeOption {
            value: format!("{}/{}", s.workspace_name, s.project_name),
            selected: s.workspace_name == workspace && s.project_name == project,
            label: s.project_name,
        };
        by_workspace
            .entry(s.workspace_name)
            .or_default()
            .push(option);
    }
    let scope_groups: Vec<ScopeGroup> = by_workspace
        .into_iter()
        .map(|(workspace, mut projects)| {
            projects.sort_by(|a, b| a.label.cmp(&b.label));
            ScopeGroup {
                workspace,
                projects,
            }
        })
        .collect();
    let hit_count = hits.len();
    let unknown_kind = kind
        .as_deref()
        .filter(|k| !PAGE_KINDS.iter().any(|known| known.eq_ignore_ascii_case(k)))
        .unwrap_or_default()
        .to_owned();
    let kind_options = filter_options(&PAGE_KINDS, kind.as_deref());
    let tiers = Tier::ALL.map(|t| t.as_str());
    let tier_options = filter_options(&tiers, tier);
    // Leaving the project keeps the filters, so an empty filtered result here
    // is compared with the same filters everywhere.
    let mut global_href = format!("search?q={}", encode_segment(&query));
    for (name, value) in [("kind", kind.as_deref()), ("tier", tier)] {
        if let Some(value) = value {
            global_href.push_str(&format!("&{name}={}", encode_segment(value)));
        }
    }
    let view = SearchView {
        global_href,
        project_href: project_href(&workspace, &project),
        scope_groups,
        query,
        hits,
        hit_count,
        filtered,
        unknown_kind,
        kind_options,
        tier_options,
        workspace,
        project,
        explain,
        streams_active: streams_active.join(", "),
        timings: timings.as_ref().map(timing_rows).unwrap_or_default(),
        embedder: state.recall.embedder.as_ref().map_or_else(
            || "none".to_owned(),
            |e| format!("{} / {} (dim {})", e.provider(), e.model(), e.dim()),
        ),
        reranker: state.recall.rerank.as_ref().map_or_else(
            || "none".to_owned(),
            |stage| format!("{} / {}", stage.reranker().name(), stage.reranker().model()),
        ),
        truncated,
        max_results: limit_used,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The options of one filter select, with the current value marked. A value
/// outside `known` (a custom frontmatter kind) is kept so the form round-trips.
fn filter_options(known: &[&str], current: Option<&str>) -> Vec<FilterOption> {
    let mut values: Vec<&str> = known.to_vec();
    if let Some(current) = current.filter(|c| !known.iter().any(|k| k.eq_ignore_ascii_case(c))) {
        values.push(current);
    }
    values
        .into_iter()
        .map(|value| FilterOption {
            value: value.to_owned(),
            selected: current.is_some_and(|c| c.eq_ignore_ascii_case(value)),
        })
        .collect()
}

fn dash() -> String {
    "—".to_owned()
}

fn fmt_rank(rank: Option<usize>) -> String {
    rank.map_or_else(dash, |r| r.to_string())
}

fn fmt_f64(value: Option<f64>) -> String {
    value.map_or_else(dash, |v| format!("{v:.4}"))
}

fn fmt_rrf(value: f64) -> String {
    if value == 0.0 {
        dash()
    } else {
        format!("{value:.5}")
    }
}

/// One hit's trace rows, in the stream order `streams_active` uses.
pub(crate) fn hit_trace(position: usize, rank: f64, d: &SearchExplain) -> HitTrace {
    let mut streams = vec![
        StreamTraceRow {
            stream: "fts",
            rank: fmt_rank(d.fts_rank),
            raw: fmt_f64(d.fts_score),
            rrf: fmt_rrf(d.rrf.fts),
        },
        StreamTraceRow {
            stream: "entity",
            rank: fmt_rank(d.entity_rank),
            raw: fmt_f64(d.entity_weight),
            rrf: fmt_rrf(d.rrf.entity),
        },
        StreamTraceRow {
            stream: "vector",
            rank: fmt_rank(d.vector_rank),
            raw: fmt_f64(d.cosine.map(f64::from)),
            rrf: fmt_rrf(d.rrf.vector),
        },
    ];
    if d.abstract_rank.is_some() || d.rrf.abstract_vector != 0.0 {
        streams.push(StreamTraceRow {
            stream: "abstract",
            rank: fmt_rank(d.abstract_rank),
            raw: fmt_f64(d.abstract_cosine.map(f64::from)),
            rrf: fmt_rrf(d.rrf.abstract_vector),
        });
    }
    streams.push(StreamTraceRow {
        stream: "graph",
        rank: fmt_rank(d.graph_rank),
        raw: dash(),
        rrf: fmt_rrf(d.rrf.graph),
    });
    if d.observation_rank.is_some() || d.rrf.observation != 0.0 {
        streams.push(StreamTraceRow {
            stream: "observation",
            rank: fmt_rank(d.observation_rank),
            raw: dash(),
            rrf: fmt_rrf(d.rrf.observation),
        });
    }
    HitTrace {
        position,
        rank: format!("{rank:.5}"),
        fused: format!("{:.5}", d.fused),
        authority: fmt_f64(d.authority),
        intent_boost: d.intent_boost.map(|v| format!("{v:.3}")),
        rerank_score: d.rerank_score.map(|v| format!("{v:.3}")),
        streams,
        matched_entities: d.matched_entities.join(", "),
        graph_via: d.graph_via.as_ref().map(|via| match &via.edge {
            Some(edge) => format!("{} ({}, {edge})", via.seed_path, via.direction),
            None => format!("{} ({})", via.seed_path, via.direction),
        }),
    }
}

fn timing_rows(t: &StreamTimings) -> Vec<TimingRow> {
    [
        ("fts", t.fts_ms),
        ("entity", t.entity_ms),
        ("vector", t.vector_ms),
        ("graph", t.graph_ms),
        ("observation", t.observation_ms),
        ("rerank", t.rerank_ms),
    ]
    .into_iter()
    .filter_map(|(stream, ms)| {
        ms.map(|ms| TimingRow {
            stream,
            ms: format!("{ms:.2}"),
        })
    })
    .collect()
}

fn context_href(base: &str, params: &SearchParams, global: bool, limit: usize) -> String {
    let mut href = format!(
        "{base}?recall_q={}&recall_limit={limit}&recall_global={}",
        encode_segment(params.q.trim()),
        if global { "1" } else { "0" }
    );
    for (key, value) in [
        ("recall_kind", params.kind.as_deref()),
        ("recall_tier", params.tier.as_deref()),
    ] {
        if let Some(value) = non_empty(value) {
            href.push_str(&format!("&{key}={}", encode_segment(value)));
        }
    }
    href
}
