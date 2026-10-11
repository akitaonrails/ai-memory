//! `GET /w/:workspace/:project` — page tree + recent activity.

use std::collections::BTreeMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::state::WebState;
use crate::templates::{
    Folder, MailboxRow, PageRow, ProjectView, humanize, page_href, project_href,
};

/// Pending messages listed on the project page before it says there are more.
const MAILBOX_LIMIT: usize = 5;

/// Leading body characters shown per message.
const SNIPPET_CHARS: usize = 160;

/// Handler for `GET /w/:workspace/:project`.
pub(crate) async fn handler(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    Path((workspace, project)): Path<(String, String)>,
) -> Response {
    let viewer_id = viewer.as_ref().map(|axum::Extension(viewer)| viewer.user());
    if let Err(refusal) = super::authorize_read(&state, viewer, &workspace, &project).await {
        return super::page::refusal_response(&refusal);
    }
    render(&state, workspace, project, viewer_id)
        .await
        .into_response()
}

async fn render(
    state: &WebState,
    workspace: String,
    project: String,
    viewer: Option<ai_memory_core::UserId>,
) -> Result<Html<String>, StatusCode> {
    let pages = state
        .reader
        .list_pages(&workspace, &project)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Build sidebar folder trees (group by first path segment), split
    // into knowledge and machinery. A store accumulates far more
    // machinery pages (lint reports, session captures, monthly logs,
    // bundle indexes) than curated knowledge; listing them as peers
    // buried the concepts/decisions/rules a human actually opens this
    // UI for.
    let mut knowledge_map: BTreeMap<String, Vec<PageRow>> = BTreeMap::new();
    let mut system_map: BTreeMap<String, Vec<PageRow>> = BTreeMap::new();
    for p in &pages {
        let folder = p
            .path
            .split('/')
            .next()
            .and_then(|seg| {
                // Only treat it as a folder prefix if there's a slash in the path.
                if p.path.contains('/') {
                    Some(seg.to_owned())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "(root)".to_owned());
        let map = if is_system_page(&p.path) {
            &mut system_map
        } else {
            &mut knowledge_map
        };
        map.entry(folder).or_default().push(PageRow {
            path: p.path.clone(),
            href: page_href(&workspace, &project, &p.path),
            title: p.title.clone(),
            kind: p.kind.clone(),
            updated_relative: humanize(&p.updated_at),
        });
    }
    let folders: Vec<Folder> = knowledge_map
        .into_iter()
        .map(|(name, pages)| Folder { name, pages })
        .collect();
    let system: Vec<Folder> = system_map
        .into_iter()
        .map(|(name, pages)| Folder { name, pages })
        .collect();

    // Recent pages: knowledge only, sorted by updated_at desc, take 20.
    // Machinery updates constantly (logs append every consolidation,
    // lint reruns daily), so an unfiltered recency sort would show
    // nothing else.
    let mut sorted: Vec<_> = pages
        .iter()
        .filter(|p| !is_system_page(&p.path))
        .cloned()
        .collect();
    sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sorted.truncate(20);
    let recent: Vec<PageRow> = sorted
        .into_iter()
        .map(|p| PageRow {
            path: p.path.clone(),
            href: page_href(&workspace, &project, &p.path),
            title: p.title.clone(),
            kind: p.kind.clone(),
            updated_relative: humanize(&p.updated_at),
        })
        .collect();

    let (mailbox, mailbox_total) = mailbox(state, &workspace, &project, viewer).await?;

    let mailbox_capped = mailbox_total > mailbox.len() as u64;
    let html = ProjectView {
        workspace,
        project,
        folders,
        system,
        recent,
        mailbox,
        mailbox_total,
        mailbox_capped,
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html))
}

/// The project's pending inbox, read-only: this page never pops or cancels,
/// so the claim-once queue stays the agents' alone.
///
/// Returns the rows to list and how many messages are pending in all, so the
/// page can say "5 of 12" rather than hide the rest. The route has
/// already authorized the viewer for this project; sender names are filtered
/// again per row in the store.
async fn mailbox(
    state: &WebState,
    workspace: &str,
    project: &str,
    viewer: Option<ai_memory_core::UserId>,
) -> Result<(Vec<MailboxRow>, u64), StatusCode> {
    let scope = ai_memory_store::lookup_existing_scope(&state.reader, workspace, project)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let entries = state
        .reader
        .list_inbox_with_senders(scope.workspace_id, scope.project_id, viewer, MAILBOX_LIMIT)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total = state
        .reader
        .pending_message_count(scope.workspace_id, scope.project_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .max(entries.len() as u64);
    let rows = entries
        .into_iter()
        .map(|entry| {
            let (sender, sender_href) = match entry.sender {
                Some((workspace, project)) => (
                    format!("{workspace}/{project}"),
                    Some(project_href(&workspace, &project)),
                ),
                None => ("a project you cannot read".to_owned(), None),
            };
            MailboxRow {
                subject: entry
                    .message
                    .subject
                    .filter(|subject| !subject.trim().is_empty())
                    .unwrap_or_else(|| "(no subject)".to_owned()),
                sender,
                sender_href,
                snippet: snippet(&entry.message.body),
                created_relative: humanize(&entry.message.created_at.to_string()),
            }
        })
        .collect();
    Ok((rows, total))
}

/// First [`SNIPPET_CHARS`] characters of `body` on one line.
fn snippet(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SNIPPET_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(SNIPPET_CHARS).collect();
    format!("{cut}…")
}

/// Machinery rather than knowledge: hidden from Recent Activity and
/// collapsed into the sidebar's System section. Underscore-prefixed
/// trees are system surfaces — except `_rules`, which holds standing
/// human-authored rules — as are session captures and the root-level
/// bookkeeping pages (monthly logs, the OKF bundle index, `_meta.md`).
fn is_system_page(path: &str) -> bool {
    if path.starts_with("_rules/") {
        return false;
    }
    if path.starts_with('_') || path.starts_with("sessions/") {
        return true;
    }
    if path.contains('/') {
        return false;
    }
    path == "index.md" || path == "_meta.md" || (path.starts_with("log-") && path.ends_with(".md"))
}
