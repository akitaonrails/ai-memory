//! `GET /` — project list cards.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;

use crate::state::WebState;
use crate::templates::{
    DashboardStats, IngestStats, MailSummaryItem, OkfDialog, ProjectCard, ProjectsView, humanize,
    project_href,
};

/// Projects the home page's mail summary names before saying "and N more".
const MAIL_SUMMARY_PROJECTS: usize = 5;

#[derive(Default, serde::Deserialize)]
pub(crate) struct IndexQuery {
    #[serde(default)]
    sort: String,
    #[serde(default)]
    state: String,
}

/// Handler for `GET /`.
///
/// Lists only the repositories the viewer may read once authorization is on
/// (#708): the card grid is a list of repository names, and a name is itself
/// what a team from another organisation must not see.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    level: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    Query(query): Query<IndexQuery>,
) -> Result<Html<String>, StatusCode> {
    if !matches!(
        query.sort.as_str(),
        "" | "sessions" | "events" | "pages" | "open"
    ) || !matches!(query.state.as_str(), "" | "all" | "open")
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let viewer_id = viewer.map(|axum::Extension(viewer)| viewer.user());
    let (mut summaries, counts, registered) = tokio::try_join!(
        state.reader.list_projects_with_stats(viewer_id),
        // One grouped query for every card; the badge shows only where mail waits.
        state.reader.pending_inbox_counts(viewer_id),
        state.reader.registered_agents(viewer_id),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut agent_names: HashMap<(String, String), Vec<&'static str>> = HashMap::new();
    for agent in registered {
        agent_names
            .entry((agent.workspace, agent.project))
            .or_default()
            .push(crate::templates::agent_label(agent.agent));
    }
    let mut pending: HashMap<String, HashMap<String, u64>> = HashMap::new();
    for c in counts {
        pending
            .entry(c.workspace_name)
            .or_default()
            .insert(c.project_name, c.pending);
    }

    let show_ingest = super::require_admin(&state, level).await.is_ok();
    let dashboard = DashboardStats {
        project_count: summaries.len(),
        page_count: summaries.iter().map(|s| s.page_count).sum(),
        session_count: summaries.iter().map(|s| s.session_count).sum(),
        observation_count: summaries.iter().map(|s| s.observation_count).sum(),
        open_session_count: summaries.iter().map(|s| s.open_session_count).sum(),
        last_activity_relative: summaries
            .iter()
            .find_map(|s| s.last_activity.as_deref())
            .map(humanize)
            .unwrap_or_default(),
        ingest: state
            .ingest_metrics
            .as_ref()
            .filter(|_| show_ingest)
            .map(|metrics| {
                let snapshot = metrics.snapshot();
                IngestStats {
                    accepted: snapshot.accepted,
                    dropped_by_policy: snapshot.dropped_by_policy,
                    shed_saturated: snapshot.shed_saturated,
                    shed_rate_limited: snapshot.shed_rate_limited,
                }
            }),
    };
    match query.sort.as_str() {
        "sessions" => summaries.sort_by_key(|item| std::cmp::Reverse(item.session_count)),
        "events" => summaries.sort_by_key(|item| std::cmp::Reverse(item.observation_count)),
        "pages" => summaries.sort_by_key(|item| std::cmp::Reverse(item.page_count)),
        "open" => summaries.sort_by_key(|item| std::cmp::Reverse(item.open_session_count)),
        _ => {}
    }
    let projects: Vec<ProjectCard> = summaries
        .into_iter()
        .map(|s| {
            let last_updated_relative =
                s.last_activity.as_deref().map(humanize).unwrap_or_default();
            let href = project_href(&s.workspace_name, &s.project_name);
            let pending_inbox = pending
                .get(&s.workspace_name)
                .and_then(|projects| projects.get(&s.project_name))
                .copied()
                .unwrap_or(0);
            let agent_summary = agent_names
                .get(&(s.workspace_name.clone(), s.project_name.clone()))
                .map(|v| v.join(" · "))
                .unwrap_or_default();
            ProjectCard {
                agent_summary,
                inbox_href: format!(
                    "inbox?workspace={}&project={}",
                    crate::templates::encode_segment(&s.workspace_name),
                    crate::templates::encode_segment(&s.project_name)
                ),
                workspace: s.workspace_name,
                project: s.project_name,
                page_count: s.page_count,
                session_count: s.session_count,
                observation_count: s.observation_count,
                open_session_count: s.open_session_count,
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
        dashboard,
        project_state: query.state,
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
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    // One decimal: 1023.95 KB renders as 1024.0. Carry while the formatted
    // magnitude would still be 1024.0. Stopping at `value < 1024.0` leaves
    // that sliver printing a size the unit cannot hold (1 MB − 51 bytes came
    // out as `1024.0 KB` on the migration dialog).
    while (value * 10.0).round() >= 10_240.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    /// A size just under a unit boundary rounds up at one decimal, so it must
    /// carry to the next unit rather than print a magnitude that unit cannot
    /// hold. The migration dialog reported `1024.0 KB` for 1 MB − 51 bytes.
    #[test]
    fn human_bytes_carries_a_value_that_would_round_up_to_the_next_unit() {
        for n in 1_048_525..=1_048_575 {
            assert_eq!(human_bytes(n), "1.0 MB", "{n}");
        }
        assert_eq!(human_bytes(1024 * 1024 * 1024 - 51), "1.0 GB");
        // Controls: a value that does not reach the boundary stays put.
        assert_eq!(human_bytes(1_048_524), "1023.9 KB");
        assert_eq!(human_bytes(1_047_976), "1023.4 KB");
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
    }
}
