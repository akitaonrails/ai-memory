//! `GET /` — project list cards.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Html;

use crate::state::WebState;
use crate::templates::{
    MailSummaryItem, OkfDialog, ProjectCard, ProjectsView, humanize, project_href,
};

/// Projects the home page's mail summary names before saying "and N more".
const MAIL_SUMMARY_PROJECTS: usize = 5;

/// Handler for `GET /`.
///
/// Lists only the repositories the viewer may read once authorization is on
/// (#708): the card grid is a list of repository names, and a name is itself
/// what a team from another organisation must not see.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
) -> Result<Html<String>, StatusCode> {
    let viewer_id = viewer.map(|axum::Extension(viewer)| viewer.user());
    let (summaries, counts) = tokio::try_join!(
        state.reader.list_projects_with_stats(viewer_id),
        // One grouped query for every card; the badge shows only where mail waits.
        state.reader.pending_inbox_counts(viewer_id),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut pending: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for c in counts {
        pending
            .entry(c.workspace_name)
            .or_default()
            .insert(c.project_name, c.pending);
    }

    let projects: Vec<ProjectCard> = summaries
        .into_iter()
        .map(|s| {
            let last_updated_relative = s.last_updated.as_deref().map(humanize).unwrap_or_default();
            let href = project_href(&s.workspace_name, &s.project_name);
            let pending_inbox = pending
                .get(&s.workspace_name)
                .and_then(|projects| projects.get(&s.project_name))
                .copied()
                .unwrap_or(0);
            ProjectCard {
                workspace: s.workspace_name,
                project: s.project_name,
                page_count: s.page_count,
                last_updated_relative,
                pending_inbox,
                href,
            }
        })
        .collect();

    // The summary reads from the cards the viewer may see, so it can never
    // name a project the grid below it does not.
    let mut with_mail: Vec<&ProjectCard> = projects
        .iter()
        .filter(|card| card.pending_inbox > 0)
        .collect();
    with_mail.sort_by(|a, b| {
        b.pending_inbox
            .cmp(&a.pending_inbox)
            .then_with(|| a.workspace.cmp(&b.workspace))
            .then_with(|| a.project.cmp(&b.project))
    });
    let mail_total: u64 = with_mail.iter().map(|card| card.pending_inbox).sum();
    let mail_hidden = with_mail.len().saturating_sub(MAIL_SUMMARY_PROJECTS);
    let mail_projects = with_mail
        .into_iter()
        .take(MAIL_SUMMARY_PROJECTS)
        .map(|card| MailSummaryItem {
            label: format!("{}/{}", card.workspace, card.project),
            href: card.href.clone(),
            pending: card.pending_inbox,
        })
        .collect();

    let okf_dialog = okf_dialog(&state);
    let html = ProjectsView {
        projects,
        okf_dialog,
        mail_total,
        mail_projects,
        mail_hidden,
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
