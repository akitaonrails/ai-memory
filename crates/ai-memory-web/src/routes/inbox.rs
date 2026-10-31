//! Non-consuming mailbox history and an authorized browser message composer.

use crate::{
    state::WebState,
    templates::{agent_label, encode_segment, project_href},
    views::{InboxView, MessageRow, RecipientOption, ScopeOption},
};
use ai_memory_core::{
    ActorContext, AgentKind, AuthLevel, Capability, MessageBox, MessageState, NewAgentMessage,
};
use ai_memory_store::{ProjectAccess, lookup_existing_scope_guarded};
use askama::Template;
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

#[derive(Default, Deserialize)]
pub(crate) struct InboxQuery {
    workspace: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    #[serde(default)]
    offset: usize,
    #[serde(default, rename = "box")]
    mailbox: Option<String>,
    state: Option<String>,
}

fn mailbox_href(workspace: &str, project: &str, outbox: bool, offset: usize) -> String {
    format!(
        "inbox?workspace={}&project={}&box={}&offset={offset}",
        encode_segment(workspace),
        encode_segment(project),
        if outbox { "outbox" } else { "inbox" }
    )
}

pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    legacy: Option<axum::Extension<ai_memory_core::LegacyBrowserAuth>>,
    Query(query): Query<InboxQuery>,
) -> Response {
    let viewer = viewer.map(|v| v.0.user());
    let (projects, counts, directory) = match tokio::try_join!(
        state.reader.list_projects_with_stats(viewer),
        state.reader.pending_inbox_counts(viewer),
        state.reader.registered_agents(viewer),
    ) {
        Ok(data) => data,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let pending: HashMap<_, _> = counts
        .into_iter()
        .map(|c| ((c.workspace_name, c.project_name), c.pending))
        .collect();
    let requested = match (query.workspace, query.project) {
        (Some(w), Some(p)) => (w, p),
        (None, None) => projects
            .iter()
            .find(|p| pending.contains_key(&(p.workspace_name.clone(), p.project_name.clone())))
            .or_else(|| projects.first())
            .map(|p| (p.workspace_name.clone(), p.project_name.clone()))
            .unwrap_or_default(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Select both workspace and project.",
            )
                .into_response();
        }
    };
    let (workspace, project) = requested;
    let mailbox = match query.mailbox.as_deref() {
        None | Some("inbox") => MessageBox::Inbox,
        Some("outbox") => MessageBox::Outbox,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let inbox = mailbox == MessageBox::Inbox;
    let message_state = match query.state.as_deref() {
        None if inbox => Some(MessageState::Pending),
        None | Some("") => None,
        Some(raw) => match raw.parse::<MessageState>() {
            Ok(state) => Some(state),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        },
    };
    let filtered_href = |outbox, offset| {
        let mut href = mailbox_href(&workspace, &project, outbox, offset);
        if let Some(state) = message_state {
            href.push_str("&state=");
            href.push_str(state.as_str());
        } else {
            href.push_str("&state=");
        }
        href
    };
    let scope = if workspace.is_empty() && project.is_empty() {
        None
    } else {
        match super::resolve_read_scope(&state, viewer, &workspace, &project).await {
            Ok(s) => Some(s),
            Err(e) => return super::page::localized_refusal_response(&i18n, &e),
        }
    };
    let recipients = directory
        .into_iter()
        .map(|r| {
            let selected = r.workspace == workspace
                && r.project == project
                && query.agent.as_deref() == Some(r.agent.as_str());
            RecipientOption {
                label: agent_label(r.agent).into(),
                workspace: r.workspace,
                project: r.project,
                agent: r.agent.as_str().into(),
                selected,
            }
        })
        .collect::<Vec<_>>();
    let visible: HashMap<_, _> = projects
        .iter()
        .map(|p| {
            (
                (p.workspace_id.clone(), p.project_id.clone()),
                format!("{} / {}", p.workspace_name, p.project_name),
            )
        })
        .collect();
    let mut messages = match scope {
        Some(scope) => match state
            .reader
            .message_history_filtered(
                scope.workspace_id,
                scope.project_id,
                mailbox,
                message_state,
                51,
                query.offset,
            )
            .await
        {
            Ok(m) => m,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        None => Vec::new(),
    };
    let more = messages.len() > 50;
    messages.truncate(50);
    let editing_count = match (&scope, inbox) {
        (Some(scope), true) => match state
            .reader
            .message_history_filtered(
                scope.workspace_id,
                scope.project_id,
                MessageBox::Inbox,
                Some(MessageState::Editing),
                100,
                0,
            )
            .await
        {
            Ok(held) => held.len(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        _ => 0,
    };
    let rows = messages
        .into_iter()
        .map(|m| {
            let from = visible
                .get(&(
                    m.origin.from_workspace_id.to_string(),
                    m.origin.from_project_id.to_string(),
                ))
                .cloned()
                .unwrap_or_else(|| "A project you cannot read".into());
            let to = visible
                .get(&(m.to_workspace_id.to_string(), m.to_project_id.to_string()))
                .cloned()
                .unwrap_or_else(|| "A project you cannot read".into());
            let raw_subject = m.subject.clone().unwrap_or_default();
            MessageRow {
                id: m.id.to_string(),
                raw_subject,
                subject: m
                    .subject
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| i18n.text("web-untitled-message")),
                body: m.body,
                from,
                to,
                agent: m
                    .to_agent
                    .map(|a| agent_label(a).to_owned())
                    .unwrap_or_else(|| i18n.text("web-any-project-agent")),
                state: m.state.as_str().into(),
                state_label: if i18n.tag() == "en" {
                    match m.state {
                        MessageState::Pending => "Queued",
                        MessageState::Claimed => "Collected",
                        MessageState::Cancelled => "Cancelled",
                        MessageState::Editing => "Editing",
                    }
                    .to_owned()
                } else {
                    i18n.status(m.state.as_str())
                },
                when: i18n.relative(&m.created_at.to_string()),
            }
        })
        .collect();
    let scopes: Vec<_> = projects
        .into_iter()
        .map(|p| ScopeOption {
            pending: pending
                .get(&(p.workspace_name.clone(), p.project_name.clone()))
                .copied()
                .unwrap_or(0),
            inbox_href: format!(
                "{}&state=pending",
                mailbox_href(&p.workspace_name, &p.project_name, false, 0)
            ),
            selected: p.workspace_name == workspace && p.project_name == project,
            workspace: p.workspace_name,
            project: p.project_name,
        })
        .collect();
    let view = InboxView {
        i18n: i18n.clone(),
        pending_total: scopes
            .iter()
            .fold(0_u64, |total, s| total.saturating_add(s.pending)),
        pending_projects: scopes.iter().filter(|s| s.pending > 0).count(),
        scopes,
        editing_count,
        editing_href: format!(
            "{}&state=editing",
            mailbox_href(&workspace, &project, false, 0)
        ),
        inbox_href: filtered_href(false, 0),
        outbox_href: filtered_href(true, 0),
        state: message_state.map_or("", |s| s.as_str()).into(),
        previous: if query.offset > 0 {
            filtered_href(!inbox, query.offset.saturating_sub(50))
        } else {
            String::new()
        },
        next: if more {
            filtered_href(!inbox, query.offset.saturating_add(50))
        } else {
            String::new()
        },
        can_send: recipients
            .iter()
            .any(|r| r.workspace == workspace && r.project == project),
        needs_write_token: legacy.is_some(),
        project_href: project_href(&workspace, &project),
        workspace,
        project,
        recipients,
        rows,
        inbox,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
pub(crate) struct ComposeMessage {
    from_workspace: String,
    from_project: String,
    to_workspace: String,
    to_project: String,
    to_agent: String,
    subject: String,
    body: String,
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({"error":message}))).into_response()
}

/// What a browser write needs after the checks every write shares.
struct BrowserWriter {
    viewer: Option<ai_memory_core::UserId>,
    distinguishes: bool,
}

/// The checks every browser write to the mailbox shares: the UI header, the
/// same-origin check, a signed-in operator when operators are distinguished,
/// and the normal-write capability.
async fn authorize_browser_write(
    state: &WebState,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    level: Option<axum::Extension<AuthLevel>>,
    headers: &HeaderMap,
) -> Result<BrowserWriter, Response> {
    // A custom header plus JSON prevents a cross-site form from using cached
    // Basic credentials. Cookie sessions additionally pass dual-auth CSRF.
    if headers.get("x-ai-memory-ui").and_then(|h| h.to_str().ok()) != Some("1") {
        return Err(error(
            StatusCode::FORBIDDEN,
            "Refresh the inbox and try again.",
        ));
    }
    if let Some(origin) = headers.get("origin").and_then(|h| h.to_str().ok()) {
        let authority = origin
            .parse::<axum::http::Uri>()
            .ok()
            .and_then(|u| u.authority().map(|a| a.as_str().to_owned()));
        if authority.as_deref() != headers.get("host").and_then(|h| h.to_str().ok()) {
            return Err(error(
                StatusCode::FORBIDDEN,
                "Messages must be sent from this dashboard.",
            ));
        }
    }
    let distinguishes = state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    let level = level.map_or(AuthLevel::Anonymous, |l| l.0);
    if distinguishes && level == AuthLevel::Anonymous {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "Sign in to send a message.",
        ));
    }
    if level
        .authorize(Capability::NormalWrite, distinguishes)
        .is_err()
    {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    let viewer = viewer.map(|v| v.0.user());
    if level == AuthLevel::User && viewer.is_none() {
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    Ok(BrowserWriter {
        viewer,
        distinguishes,
    })
}

pub(crate) async fn send(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ActorContext>>,
    level: Option<axum::Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(input): Json<ComposeMessage>,
) -> Response {
    let BrowserWriter {
        viewer,
        distinguishes,
    } = match authorize_browser_write(&state, viewer, level, &headers).await {
        Ok(writer) => writer,
        Err(refusal) => return refusal,
    };
    let source = match lookup_existing_scope_guarded(
        &state.reader,
        &input.from_workspace,
        &input.from_project,
        viewer,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return super::page::refusal_response(&e),
    };
    let target = match lookup_existing_scope_guarded(
        &state.reader,
        &input.to_workspace,
        &input.to_project,
        viewer,
        ProjectAccess::Write,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return super::page::refusal_response(&e),
    };
    let agent = AgentKind::from_wire(&input.to_agent);
    if agent.as_str() != input.to_agent {
        return error(StatusCode::BAD_REQUEST, "Select a registered agent.");
    }
    let known = match state
        .reader
        .agent_usage(
            target.workspace_id,
            target.project_id,
            ai_memory_core::OwnerFilter::Any,
        )
        .await
    {
        Ok(rows) => rows.iter().any(|r| r.agent == input.to_agent),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if !known {
        return error(
            StatusCode::BAD_REQUEST,
            "That agent has no registered session in the recipient project.",
        );
    }
    if input.body.trim().is_empty()
        || input.body.chars().count() > 8_000
        || input.subject.chars().count() > 200
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Enter a message up to 8,000 characters and a subject up to 200 characters.",
        );
    }
    let actor = actor.map_or_else(ActorContext::anonymous, |a| a.0);
    let message = NewAgentMessage {
        from_workspace_id: source.workspace_id,
        from_project_id: source.project_id,
        from_agent: AgentKind::Other,
        from_session_id: None,
        from_owner_user: ai_memory_core::owner_stamp(actor.identity_key().as_ref(), distinguishes),
        to_workspace_id: target.workspace_id,
        to_project_id: target.project_id,
        to_agent: Some(agent),
        subject: Some(input.subject.trim().to_owned()),
        body: input.body.trim().to_owned(),
        nature: ai_memory_core::MessageNature::Unknown,
        evidence: None,
    };
    match state.wiki.send_agent_message(message, actor).await {
        Ok(id) => (
            StatusCode::CREATED,
            Json(serde_json::json!({"message_id":id.to_string(),"state":"pending"})),
        )
            .into_response(),
        Err(ai_memory_wiki::WikiError::Store(ai_memory_store::StoreError::InvalidState(_))) => {
            error(
                StatusCode::CONFLICT,
                "This inbox is full. An agent must collect pending mail first.",
            )
        }
        Err(e) => {
            tracing::warn!(error=%e,"browser message refused");
            error(
                StatusCode::FORBIDDEN,
                "The message could not be queued. Check project access and message admission policy.",
            )
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct EditMessage {
    workspace: String,
    project: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

/// Resolve the message a browser edit targets, and refuse unless the caller
/// could have sent it: write access to the recipient mailbox it sits in and to
/// the project it came from. Without the second check, anyone who can write to
/// a project could rewrite what another project's agent asked it to do.
async fn editable_message(
    state: &WebState,
    writer: &BrowserWriter,
    raw_id: &str,
    input: &EditMessage,
) -> Result<
    (
        ai_memory_core::WorkspaceId,
        ai_memory_core::ProjectId,
        ai_memory_core::MessageId,
    ),
    Response,
> {
    let id = raw_id
        .parse::<ai_memory_core::MessageId>()
        .map_err(|_| error(StatusCode::BAD_REQUEST, "That is not a message id."))?;
    let target = lookup_existing_scope_guarded(
        &state.reader,
        &input.workspace,
        &input.project,
        writer.viewer,
        ProjectAccess::Write,
    )
    .await
    .map_err(|e| super::page::refusal_response(&e))?;
    let message = state
        .reader
        .message_in_mailbox(target.workspace_id, target.project_id, id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?
        .ok_or_else(|| {
            error(
                StatusCode::NOT_FOUND,
                "That message is not in this mailbox.",
            )
        })?;
    let visible = state
        .reader
        .list_projects_with_stats(writer.viewer)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    let Some(origin) = visible.into_iter().find(|p| {
        p.workspace_id == message.origin.from_workspace_id.to_string()
            && p.project_id == message.origin.from_project_id.to_string()
    }) else {
        return Err(error(
            StatusCode::FORBIDDEN,
            "You cannot edit mail sent from a project you cannot write to.",
        ));
    };
    lookup_existing_scope_guarded(
        &state.reader,
        &origin.workspace_name,
        &origin.project_name,
        writer.viewer,
        ProjectAccess::Write,
    )
    .await
    .map_err(|e| super::page::refusal_response(&e))?;
    Ok((target.workspace_id, target.project_id, id))
}

/// Take a queued message out of the queue so it can be edited.
pub(crate) async fn begin_edit(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    level: Option<axum::Extension<AuthLevel>>,
    headers: HeaderMap,
    axum::extract::Path(raw_id): axum::extract::Path<String>,
    Json(input): Json<EditMessage>,
) -> Response {
    let writer = match authorize_browser_write(&state, viewer, level, &headers).await {
        Ok(writer) => writer,
        Err(refusal) => return refusal,
    };
    let (workspace_id, project_id, id) =
        match editable_message(&state, &writer, &raw_id, &input).await {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
    match state
        .wiki
        .begin_agent_message_edit(workspace_id, project_id, id)
        .await
    {
        Ok(Some(message)) => Json(serde_json::json!({
            "message_id": message.id.to_string(),
            "state": message.state.as_str(),
            "subject": message.subject.unwrap_or_default(),
            "body": message.body,
        }))
        .into_response(),
        Ok(None) => error(
            StatusCode::CONFLICT,
            "An agent already collected this message, or it is no longer queued.",
        ),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Save new text for a held message and return it to the queue.
pub(crate) async fn save_edit(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ActorContext>>,
    level: Option<axum::Extension<AuthLevel>>,
    headers: HeaderMap,
    axum::extract::Path(raw_id): axum::extract::Path<String>,
    Json(input): Json<EditMessage>,
) -> Response {
    let writer = match authorize_browser_write(&state, viewer, level, &headers).await {
        Ok(writer) => writer,
        Err(refusal) => return refusal,
    };
    let (workspace_id, project_id, id) =
        match editable_message(&state, &writer, &raw_id, &input).await {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
    let body = input.body.clone().unwrap_or_default();
    let subject = input.subject.clone().unwrap_or_default();
    if body.trim().is_empty() || body.chars().count() > 8_000 || subject.chars().count() > 200 {
        return error(
            StatusCode::BAD_REQUEST,
            "Enter a message up to 8,000 characters and a subject up to 200 characters.",
        );
    }
    let actor = actor.map_or_else(ActorContext::anonymous, |a| a.0);
    match state
        .wiki
        .save_agent_message_edit(
            workspace_id,
            project_id,
            id,
            Some(subject.trim().to_owned()),
            body.trim().to_owned(),
            actor,
        )
        .await
    {
        Ok(true) => Json(serde_json::json!({"message_id": id.to_string(), "state": "pending"}))
            .into_response(),
        Ok(false) => error(
            StatusCode::CONFLICT,
            "This message is no longer being edited.",
        ),
        Err(e) => {
            tracing::warn!(error=%e,"browser message edit refused");
            error(
                StatusCode::FORBIDDEN,
                "The message could not be saved. Check project access and message admission policy.",
            )
        }
    }
}

/// Return a held message to the queue without changing it.
pub(crate) async fn release_edit(
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    level: Option<axum::Extension<AuthLevel>>,
    headers: HeaderMap,
    axum::extract::Path(raw_id): axum::extract::Path<String>,
    Json(input): Json<EditMessage>,
) -> Response {
    let writer = match authorize_browser_write(&state, viewer, level, &headers).await {
        Ok(writer) => writer,
        Err(refusal) => return refusal,
    };
    let (workspace_id, project_id, id) =
        match editable_message(&state, &writer, &raw_id, &input).await {
            Ok(target) => target,
            Err(refusal) => return refusal,
        };
    match state
        .wiki
        .release_agent_message_edit(workspace_id, project_id, id)
        .await
    {
        Ok(true) => Json(serde_json::json!({"message_id": id.to_string(), "state": "pending"}))
            .into_response(),
        Ok(false) => error(
            StatusCode::CONFLICT,
            "This message is no longer being edited.",
        ),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
