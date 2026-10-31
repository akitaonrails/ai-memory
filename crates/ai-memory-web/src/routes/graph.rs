//! Read-only wiki link graph pages.
//!
//! `GET /graph` draws resolved wikilinks whose endpoints sit in different
//! projects. `GET /w/:workspace/:project/graph` draws the latest unexpired
//! pages in that project and the resolved same-project links among them.
//! Both answers are HTML: a circular SVG plus a table, with no JavaScript.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use ai_memory_store::{CrossProjectEdge, LinkGraph, PROJECT_LINK_GRAPH_MAX_NODES};

use crate::state::WebState;
use crate::templates::{
    GraphEdgeRow, GraphSvgEdge, GraphSvgNode, GraphView, graph_href, page_href, project_href,
};

/// Pixel centre of the SVG viewBox.
const SVG_CX: f64 = 400.0;
/// Pixel centre of the SVG viewBox.
const SVG_CY: f64 = 400.0;
/// Radius of the node circle.
const SVG_RADIUS: f64 = 300.0;
/// Characters kept in an SVG node label before an ellipsis.
const SVG_LABEL_CHARS: usize = 24;

/// Handler for `GET /graph`.
pub(crate) async fn global_handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
) -> Response {
    let viewer_id = viewer.map(|axum::Extension(viewer)| viewer.user());
    match render_global(&i18n, &state, viewer_id).await {
        Ok(html) => html.into_response(),
        Err(status) => status.into_response(),
    }
}

/// Handler for `GET /w/:workspace/:project/graph`.
pub(crate) async fn project_handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let viewer_id = viewer.as_ref().map(|axum::Extension(viewer)| viewer.user());
    let scope = match super::resolve_read_scope(&state, viewer_id, &workspace, &project).await {
        Ok(scope) => scope,
        Err(refusal) => return super::page::localized_refusal_response(&i18n, &refusal),
    };
    let graph = match state
        .reader
        .project_link_graph(scope.workspace_id, scope.project_id)
        .await
    {
        Ok(graph) => graph,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match render_project(&i18n, &workspace, &project, graph) {
        Ok(html) => html.into_response(),
        Err(status) => status.into_response(),
    }
}

async fn render_global(
    i18n: &ai_memory_core::i18n::Localizer,
    state: &WebState,
    viewer: Option<ai_memory_core::UserId>,
) -> Result<Html<String>, StatusCode> {
    let edges = state
        .reader
        .cross_project_edges(None, viewer)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (svg_nodes, svg_edges, rows) = aggregate_projects(&edges);
    GraphView {
        i18n: i18n.clone(),
        is_project: false,
        workspace: String::new(),
        project: String::new(),
        project_href: String::new(),
        heading: "Link graph".into(),
        blurb: "Resolved wikilinks whose endpoints sit in different projects. \
                An edge is shown only when both ends are readable."
            .into(),
        empty_message: "No cross-project links yet.".into(),
        truncated: false,
        shown: svg_nodes.len(),
        node_cap: 0,
        svg_nodes,
        svg_edges,
        rows,
    }
    .render()
    .map(Html)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn render_project(
    i18n: &ai_memory_core::i18n::Localizer,
    workspace: &str,
    project: &str,
    graph: LinkGraph,
) -> Result<Html<String>, StatusCode> {
    let (svg_nodes, svg_edges) = layout_pages(workspace, project, &graph);
    let rows = graph
        .edges
        .iter()
        .map(|edge| GraphEdgeRow {
            from_href: page_href(workspace, project, &edge.from_path),
            from_label: edge.from_path.clone(),
            to_href: page_href(workspace, project, &edge.to_path),
            to_label: edge.to_path.clone(),
        })
        .collect();
    GraphView {
        i18n: i18n.clone(),
        is_project: true,
        workspace: workspace.to_owned(),
        project: project.to_owned(),
        project_href: project_href(workspace, project),
        heading: format!("{workspace}/{project} link graph"),
        blurb: "Latest pages in this project and the resolved wikilinks between them.".into(),
        empty_message: "No pages in this project yet.".into(),
        truncated: graph.truncated,
        shown: graph.nodes.len(),
        node_cap: PROJECT_LINK_GRAPH_MAX_NODES,
        svg_nodes,
        svg_edges,
        rows,
    }
    .render()
    .map(Html)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn aggregate_projects(
    edges: &[CrossProjectEdge],
) -> (Vec<GraphSvgNode>, Vec<GraphSvgEdge>, Vec<GraphEdgeRow>) {
    let mut projects: BTreeSet<(String, String)> = BTreeSet::new();
    for edge in edges {
        projects.insert((edge.from_workspace.clone(), edge.from_project.clone()));
        projects.insert((edge.to_workspace.clone(), edge.to_project.clone()));
    }
    let list: Vec<(String, String)> = projects.into_iter().collect();
    let coords = circular_layout(list.len(), SVG_CX, SVG_CY, SVG_RADIUS);
    let mut index: BTreeMap<(String, String), usize> = BTreeMap::new();
    let svg_nodes: Vec<GraphSvgNode> = list
        .iter()
        .enumerate()
        .map(|(i, (workspace, project))| {
            index.insert((workspace.clone(), project.clone()), i);
            let (x, y) = coords[i];
            let full = format!("{workspace}/{project}");
            GraphSvgNode {
                href: graph_href(workspace, project),
                tooltip: full.clone(),
                label: truncate_label(&full, SVG_LABEL_CHARS),
                x: x.round() as i32,
                y: y.round() as i32,
                label_y: (y + 22.0).round() as i32,
            }
        })
        .collect();

    let mut drawn: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut svg_edges = Vec::new();
    for edge in edges {
        let Some(&from) = index.get(&(edge.from_workspace.clone(), edge.from_project.clone()))
        else {
            continue;
        };
        let Some(&to) = index.get(&(edge.to_workspace.clone(), edge.to_project.clone())) else {
            continue;
        };
        let pair = if from <= to { (from, to) } else { (to, from) };
        if from != to && drawn.insert(pair) {
            svg_edges.push(svg_line(&coords[from], &coords[to]));
        }
    }

    let rows = edges
        .iter()
        .map(|edge| GraphEdgeRow {
            from_href: page_href(&edge.from_workspace, &edge.from_project, &edge.from_path),
            from_label: format!(
                "{}/{}:{}",
                edge.from_workspace, edge.from_project, edge.from_path
            ),
            to_href: page_href(&edge.to_workspace, &edge.to_project, &edge.to_path),
            to_label: format!("{}/{}:{}", edge.to_workspace, edge.to_project, edge.to_path),
        })
        .collect();
    (svg_nodes, svg_edges, rows)
}

fn layout_pages(
    workspace: &str,
    project: &str,
    graph: &LinkGraph,
) -> (Vec<GraphSvgNode>, Vec<GraphSvgEdge>) {
    let coords = circular_layout(graph.nodes.len(), SVG_CX, SVG_CY, SVG_RADIUS);
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    let svg_nodes: Vec<GraphSvgNode> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            index.insert(node.path.as_str(), i);
            let (x, y) = coords[i];
            GraphSvgNode {
                href: page_href(workspace, project, &node.path),
                tooltip: node.title.clone(),
                label: truncate_label(&node.title, SVG_LABEL_CHARS),
                x: x.round() as i32,
                y: y.round() as i32,
                label_y: (y + 22.0).round() as i32,
            }
        })
        .collect();
    let svg_edges = graph
        .edges
        .iter()
        .filter_map(|edge| {
            let from = *index.get(edge.from_path.as_str())?;
            let to = *index.get(edge.to_path.as_str())?;
            if from == to {
                return None;
            }
            Some(svg_line(&coords[from], &coords[to]))
        })
        .collect();
    (svg_nodes, svg_edges)
}

fn svg_line(from: &(f64, f64), to: &(f64, f64)) -> GraphSvgEdge {
    GraphSvgEdge {
        x1: from.0.round() as i32,
        y1: from.1.round() as i32,
        x2: to.0.round() as i32,
        y2: to.1.round() as i32,
    }
}

/// Place `n` points evenly around a circle, starting at the top.
#[must_use]
pub(crate) fn circular_layout(n: usize, cx: f64, cy: f64, radius: f64) -> Vec<(f64, f64)> {
    match n {
        0 => Vec::new(),
        1 => vec![(cx, cy)],
        _ => (0..n)
            .map(|i| {
                let angle =
                    std::f64::consts::TAU * (i as f64) / (n as f64) - std::f64::consts::FRAC_PI_2;
                (cx + radius * angle.cos(), cy + radius * angle.sin())
            })
            .collect(),
    }
}

fn truncate_label(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let taken: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{taken}…")
    } else {
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circular_layout_is_empty_for_zero_and_centred_for_one() {
        assert!(circular_layout(0, 400.0, 400.0, 300.0).is_empty());
        assert_eq!(
            circular_layout(1, 400.0, 400.0, 300.0),
            vec![(400.0, 400.0)]
        );
    }

    #[test]
    fn circular_layout_starts_at_the_top() {
        let pts = circular_layout(4, 0.0, 0.0, 10.0);
        assert_eq!(pts.len(), 4);
        assert!((pts[0].0 - 0.0).abs() < 1e-9);
        assert!((pts[0].1 + 10.0).abs() < 1e-9);
    }

    #[test]
    fn truncate_label_keeps_short_text_and_ellipsises_long() {
        assert_eq!(truncate_label("short", 24), "short");
        assert_eq!(
            truncate_label("abcdefghijklmnopqrstuvwxyz", 24)
                .chars()
                .count(),
            25
        );
        assert!(truncate_label("abcdefghijklmnopqrstuvwxyz", 24).ends_with('…'));
    }
}
