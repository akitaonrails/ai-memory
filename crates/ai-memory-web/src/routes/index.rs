//! `GET /` — project list cards.

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;

use crate::state::WebState;
use crate::templates::{OkfDialog, ProjectCard, ProjectsView, humanize, project_href};

/// Handler for `GET /`.
///
/// Lists only the repositories the viewer may read once authorization is on
/// (#708): the card grid is a list of repository names, and a name is itself
/// what a team from another organisation must not see.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
) -> Result<Html<String>, StatusCode> {
    let summaries = state
        .reader
        .list_projects_with_stats(viewer.map(|axum::Extension(viewer)| viewer.user()))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let projects = summaries
        .into_iter()
        .map(|s| {
            let last_updated_relative = s.last_updated.as_deref().map(humanize).unwrap_or_default();
            let href = project_href(&s.workspace_name, &s.project_name);
            ProjectCard {
                workspace: s.workspace_name,
                project: s.project_name,
                page_count: s.page_count,
                last_updated_relative,
                href,
            }
        })
        .collect();

    let okf_dialog = okf_dialog(&state);
    let html = ProjectsView {
        projects,
        okf_dialog,
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html))
}

/// The one-time migration explainer: rendered whenever a receipt
/// exists — even after the archive was deleted, so the "what happened"
/// context stays reachable — and dismissed per browser client-side.
fn okf_dialog(state: &WebState) -> Option<OkfDialog> {
    let receipt = ai_memory_wiki::backup::BackupReceipt::load(state.wiki.data_dir())?;
    Some(OkfDialog {
        archive_present: receipt.archive_present(),
        archive_path: receipt.archive_path.display().to_string(),
        size_human: human_bytes(receipt.size_bytes),
        created_at: receipt.created_at,
    })
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
