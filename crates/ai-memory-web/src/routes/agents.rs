//! Harness diagnostics below a project.

use crate::{
    operations::{agent_card, session_row},
    state::WebState,
    templates::project_href,
    views::AgentView,
};
use ai_memory_core::AgentKind;
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Default, Deserialize)]
pub(crate) struct AgentQuery {
    #[serde(default)]
    offset: usize,
}

pub(crate) async fn handler(
    axum::Extension(i18n): axum::Extension<ai_memory_core::i18n::Localizer>,
    State(state): State<Arc<WebState>>,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<axum::Extension<ai_memory_core::ActorContext>>,
    auth: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    Path((workspace, project, raw)): Path<(String, String, String)>,
    Query(query): Query<AgentQuery>,
) -> Response {
    let agent = AgentKind::from_wire(&raw);
    if agent.as_str() != raw {
        return StatusCode::NOT_FOUND.into_response();
    }
    let scope =
        match super::resolve_read_scope(&state, viewer.map(|v| v.0.user()), &workspace, &project)
            .await
        {
            Ok(scope) => scope,
            Err(e) => return super::page::localized_refusal_response(&i18n, &e),
        };
    let owner = super::api::session_owner_filter_for(actor, auth);
    let rows = match state
        .reader
        .agent_usage(scope.workspace_id, scope.project_id, owner.clone())
        .await
    {
        Ok(rows) => rows,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(row) = rows.into_iter().find(|r| r.agent == raw) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut sessions = match state
        .reader
        .agent_sessions(
            scope.workspace_id,
            scope.project_id,
            agent,
            owner,
            query.offset,
        )
        .await
    {
        Ok(rows) => rows,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let more = sessions.len() > 50;
    sessions.truncate(50);
    let card = agent_card(row, &workspace, &project);
    let href = &card.href;
    let view = AgentView {
        i18n: i18n.clone(),
        project_href: project_href(&workspace, &project),
        sessions: sessions
            .into_iter()
            .map(|s| session_row(&i18n, s, &workspace, &project))
            .collect(),
        previous: if query.offset > 0 {
            format!("{href}?offset={}", query.offset.saturating_sub(50))
        } else {
            String::new()
        },
        next: if more {
            format!("{href}?offset={}", query.offset.saturating_add(50))
        } else {
            String::new()
        },
        workspace,
        project,
        card,
    };
    match view.render() {
        Ok(html) => Html(html).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
