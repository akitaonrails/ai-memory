//! `GET /w/:workspace/:project` — page tree + recent activity.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use jiff::Timestamp;

use ai_memory_store::ResolvedScope;

use ai_memory_store::lookup_existing_scope;

use crate::state::WebState;
use crate::templates::{
    AgentBadge, Folder, MailboxRow, PageRow, ProjectMemoryStats, ProjectView, SessionRow,
    agent_label, duration_between, fmt_tokens, page_href, project_href, session_href,
};

/// Pending messages listed on the project page before it says there are more.
const MAILBOX_LIMIT: usize = 5;

/// Recent sessions listed on the project page.
const SESSION_LIMIT: usize = 20;

/// Sidebar bucket for pages directly under the wiki root.
const ROOT_FOLDER: &str = "(root)";

/// Pages listed per sidebar folder before "View all".
const FOLDER_PREVIEW: usize = 5;

/// Leading body characters shown per message.
const SNIPPET_CHARS: usize = 160;
fn fmt_session_tokens(usage: Option<&ai_memory_store::SessionUsageView>) -> String {
    match usage {
        Some(u) => format!(
            "{} in / {} out",
            fmt_tokens(u.input_tokens),
            fmt_tokens(u.output_tokens)
        ),
        None => "—".to_owned(),
    }
}

/// Handler for `GET /w/:workspace/:project`.
pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    auth: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    Path((workspace, project)): Path<(String, String)>,
    Query(query): Query<crate::operations::UsageQuery>,
) -> Response {
    let viewer_id = viewer.as_ref().map(|axum::Extension(viewer)| viewer.user());
    let authorized = match super::authorize_read(&state, viewer, &workspace, &project).await {
        Ok(authorized) => authorized,
        Err(refusal) => return super::page::localized_refusal_response(&i18n, &refusal),
    };
    let owner = super::api::session_owner_filter_for(actor, auth);
    render(
        &i18n, &state, workspace, project, viewer_id, authorized, owner, query,
    )
    .await
    .into_response()
}

#[allow(clippy::too_many_arguments)]
async fn render(
    i18n: &ai_memory_core::i18n::Localizer,
    state: &WebState,
    workspace: String,
    project: String,
    viewer: Option<ai_memory_core::UserId>,
    authorized: Option<ResolvedScope>,
    owner: ai_memory_core::OwnerFilter,
    query: crate::operations::UsageQuery,
) -> Result<Html<String>, StatusCode> {
    let calls_query = query.store_query()?;
    let session_view = query.session_view()?;
    let (pages, (mailbox, mailbox_total), sessions) = tokio::try_join!(
        async {
            state
                .reader
                .list_unexpired_pages(&workspace, &project)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        },
        mailbox(i18n, state, &workspace, &project, viewer, authorized),
        sessions(
            i18n,
            state,
            &workspace,
            &project,
            authorized,
            owner.clone(),
            session_view
        ),
    )?;

    // The page tree alone is a poor explanation for projects where hooks are
    // working but compilation has not produced a page yet. Reuse the same
    // metadata-only aggregate behind the dashboard; raw observations remain
    // accessible only through scoped API detail routes.
    let summary = state
        .reader
        .list_projects_with_stats(viewer)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter()
        .find(|item| item.workspace_name == workspace && item.project_name == project);
    // Resolved once, reused for every scope-keyed lookup below (agent
    // breakdown, token total, session list). A project with no resolvable
    // scope yet (no sessions recorded) just shows none of those rather than
    // failing the whole page.
    let scope = lookup_existing_scope(&state.reader, &workspace, &project)
        .await
        .ok();

    let tokens_total = match &scope {
        Some(scope) => state
            .reader
            .total_session_usage(scope.workspace_id, scope.project_id, owner.clone())
            .await
            .ok()
            .flatten()
            .map(|u| {
                format!(
                    "{} / {}",
                    fmt_tokens(u.input_tokens),
                    fmt_tokens(u.output_tokens)
                )
            })
            .unwrap_or_else(|| "—".to_owned()),
        None => "—".to_owned(),
    };

    let stats = match summary {
        Some(item) => ProjectMemoryStats {
            page_count: item.page_count,
            session_count: item.session_count,
            observation_count: item.observation_count,
            open_session_count: item.open_session_count,
            last_activity_relative: item
                .last_activity
                .as_deref()
                .map(|iso| i18n.relative(iso))
                .unwrap_or_default(),
            tokens_total,
        },
        // A page listing may still be visible while aggregate metadata is
        // being refreshed. Preserve a truthful compiled-page count.
        None => ProjectMemoryStats {
            page_count: pages.len() as u64,
            session_count: 0,
            observation_count: 0,
            open_session_count: 0,
            last_activity_relative: String::new(),
            tokens_total,
        },
    };

    // Which agent CLIs produced this project's memory. Read-only,
    // unauthenticated dashboard shared by every operator on the project
    // (see the multi-session/multi-user invariant), so the count
    // deliberately covers every owner rather than the (nonexistent) caller
    // identity — same aggregate posture as `stats` above.
    let by_agent: Vec<AgentBadge> = match &scope {
        Some(scope) => state
            .reader
            .session_counts_by_agent(scope.workspace_id, scope.project_id, owner.clone(), None)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| AgentBadge {
                label: agent_label(ai_memory_core::AgentKind::from_wire(&row.agent)),
                count: row.sessions,
            })
            .collect(),
        None => Vec::new(),
    };

    // Best-effort per-page attribution: the harness whose session most
    // recently contributed evidence to each page. Decorative UI metadata,
    // so a lookup failure just means no badges rather than a broken page.
    let agent_by_path = state
        .reader
        .latest_page_agent_kinds(&workspace, &project)
        .await
        .unwrap_or_default();

    // Build sidebar folder trees (group by first path segment), split
    // into knowledge and machinery. A store accumulates far more
    // machinery pages (lint reports, session captures, monthly logs,
    // bundle indexes) than curated knowledge; listing them as peers
    // buried the concepts/decisions/rules a human actually opens this
    // UI for.
    let mut knowledge_map: BTreeMap<String, Vec<(Option<Timestamp>, PageRow)>> = BTreeMap::new();
    let mut system_map: BTreeMap<String, Vec<(Option<Timestamp>, PageRow)>> = BTreeMap::new();
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
            .unwrap_or_else(|| ROOT_FOLDER.to_owned());
        let map = if is_system_page(&p.path) {
            &mut system_map
        } else {
            &mut knowledge_map
        };
        map.entry(folder).or_default().push((
            p.updated_at.parse::<Timestamp>().ok(),
            PageRow {
                agent_label: None,
                path: p.path.clone(),
                href: page_href(&workspace, &project, &p.path),
                title: p.title.clone(),
                kind: p.kind.clone(),
                updated_relative: i18n.relative(&p.updated_at),
            },
        ));
    }
    let folders = to_folders(knowledge_map, &workspace, &project);
    let system = to_folders(system_map, &workspace, &project);

    // Recent pages: knowledge only, sorted by updated_at desc, take 20.
    // Machinery updates constantly (logs append every consolidation,
    // lint reruns daily), so an unfiltered recency sort would show
    // nothing else.
    let mut sorted: Vec<_> = pages
        .iter()
        .filter(|p| !is_system_page(&p.path))
        .cloned()
        .collect();
    sorted.sort_by_cached_key(|page| {
        (
            Reverse(page.updated_at.parse::<Timestamp>().ok()),
            page.path.clone(),
        )
    });
    sorted.truncate(20);
    let recent: Vec<PageRow> = sorted
        .into_iter()
        .map(|p| PageRow {
            agent_label: agent_by_path.get(&p.path).copied().map(agent_label),
            path: p.path.clone(),
            href: page_href(&workspace, &project, &p.path),
            title: p.title.clone(),
            kind: p.kind.clone(),
            updated_relative: i18n.relative(&p.updated_at),
        })
        .collect();

    let mailbox_capped = mailbox_total > mailbox.len() as u64;
    let graph_href = crate::templates::graph_href(&workspace, &project);
    let report = match &scope {
        Some(scope) => state
            .reader
            .token_usage_report_page(
                scope.workspace_id,
                scope.project_id,
                owner.clone(),
                None,
                calls_query,
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        None => ai_memory_store::TokenUsageReport::default(),
    };
    let token_usage = crate::operations::usage_panel(
        i18n,
        report,
        &workspace,
        &project,
        &query,
        &format!("{}?tab=tokens", project_href(&workspace, &project)),
    );
    let agents = match &scope {
        Some(scope) => state
            .reader
            .agent_usage(scope.workspace_id, scope.project_id, owner.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .into_iter()
            .map(|r| crate::operations::agent_card(r, &workspace, &project))
            .collect(),
        None => Vec::new(),
    };
    let html = ProjectView {
        i18n: i18n.clone(),
        agents,
        inbox_href: format!(
            "inbox?workspace={}&project={}",
            crate::templates::encode_segment(&workspace),
            crate::templates::encode_segment(&project)
        ),
        project_href: project_href(&workspace, &project),
        token_usage,
        workspace,
        project,
        stats,
        folders,
        system,
        recent,
        by_agent,
        mailbox,
        mailbox_total,
        mailbox_capped,
        sessions,
        graph_href,
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Html(html))
}

/// Turn grouped pages into sidebar folders: newest first, capped at
/// `FOLDER_PREVIEW`, each linked to its namespace listing.
fn to_folders(
    map: BTreeMap<String, Vec<(Option<Timestamp>, PageRow)>>,
    workspace: &str,
    project: &str,
) -> Vec<Folder> {
    map.into_iter()
        .map(|(name, mut rows)| {
            rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.path.cmp(&b.1.path)));
            let total = rows.len();
            // `(root)` has no namespace URL to hold the rest, so it lists all.
            let href =
                (name != ROOT_FOLDER).then(|| page_href(workspace, project, &format!("{name}/")));
            let keep = if href.is_some() {
                FOLDER_PREVIEW
            } else {
                total
            };
            Folder {
                href,
                total,
                pages: rows.into_iter().take(keep).map(|(_, row)| row).collect(),
                name,
            }
        })
        .collect()
}

/// The project's pending inbox, read-only: this page never pops or cancels,
/// so the claim-once queue stays the agents' alone.
///
/// Returns the rows to list and how many messages are pending in all, so the
/// page can say "5 of 12" rather than hide the rest. The route has
/// already authorized the viewer for this project; sender names are filtered
/// again per row in the store.
async fn mailbox(
    i18n: &ai_memory_core::i18n::Localizer,
    state: &WebState,
    workspace: &str,
    project: &str,
    viewer: Option<ai_memory_core::UserId>,
    authorized: Option<ResolvedScope>,
) -> Result<(Vec<MailboxRow>, u64), StatusCode> {
    let Some(scope) = mailbox_scope(state, workspace, project, authorized).await? else {
        return Ok((Vec::new(), 0));
    };
    let (entries, total) = tokio::try_join!(
        state.reader.list_inbox_with_senders(
            scope.workspace_id,
            scope.project_id,
            viewer,
            MAILBOX_LIMIT
        ),
        state
            .reader
            .pending_message_count(scope.workspace_id, scope.project_id),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total = total.max(entries.len() as u64);
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
                nature: entry.message.nature.as_str(),
                sender,
                sender_href,
                snippet: snippet(&entry.message.body),
                created_relative: i18n.relative(&entry.message.created_at.to_string()),
            }
        })
        .collect();
    Ok((rows, total))
}

/// The project's most recent sessions, open ones included, through the same
/// owner-filtered reader as `/api/v1/.../sessions`: a session another operator
/// owns is not listed.
async fn sessions(
    i18n: &ai_memory_core::i18n::Localizer,
    state: &WebState,
    workspace: &str,
    project: &str,
    authorized: Option<ResolvedScope>,
    owner: ai_memory_core::OwnerFilter,
    session_view: (bool, bool),
) -> Result<Vec<SessionRow>, StatusCode> {
    let Some(scope) = mailbox_scope(state, workspace, project, authorized).await? else {
        return Ok(Vec::new());
    };
    let (open_only, sort_events) = session_view;
    let sessions = state
        .reader
        .sessions_for_scope_view(
            scope.workspace_id,
            scope.project_id,
            owner,
            open_only,
            sort_events,
            SESSION_LIMIT,
            0,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(sessions
        .into_iter()
        .map(|s| {
            let id = s.session_id.to_string();
            SessionRow {
                agent_label: agent_label(ai_memory_core::AgentKind::from_wire(&s.agent_kind)),
                duration: duration_between(&s.started_at, s.ended_at.as_deref()),
                tokens: fmt_session_tokens(s.usage.as_ref()),
                href: session_href(workspace, project, &id),
                agent_kind: s.agent_kind,
                short_id: crate::templates::short_id(&id),
                observation_count: s.observation_count,
                started_relative: i18n.relative(&s.started_at),
                open: s.ended_at.is_none(),
            }
        })
        .collect())
}

/// The project whose pages this page lists, or `None` when there is none.
///
/// Matches the exact name, as `list_unexpired_pages` does, so an alias that
/// resolves to another project cannot put that project's mail beside these
/// pages. A URL that names no project renders an empty mailbox, as it renders
/// no pages.
async fn mailbox_scope(
    state: &WebState,
    workspace: &str,
    project: &str,
    authorized: Option<ResolvedScope>,
) -> Result<Option<ResolvedScope>, StatusCode> {
    let workspace_id = match authorized {
        Some(scope) => scope.workspace_id,
        None => match ai_memory_store::lookup_existing_workspace(&state.reader, workspace).await {
            Ok(workspace_id) => workspace_id,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        },
    };
    let project_id = state
        .reader
        .find_project(workspace_id, project.to_owned())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(project_id.map(|project_id| ResolvedScope {
        workspace_id,
        project_id,
    }))
}

/// First [`SNIPPET_CHARS`] characters of `body` on one line.
fn snippet(body: &str) -> String {
    // Lazy, so a large body is read only up to the cut.
    let mut flat = body
        .split_whitespace()
        .flat_map(|word| std::iter::once(' ').chain(word.chars()))
        .skip(1);
    let cut: String = flat.by_ref().take(SNIPPET_CHARS).collect();
    if flat.next().is_some() {
        format!("{cut}…")
    } else {
        cut
    }
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
