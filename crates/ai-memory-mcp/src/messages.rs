//! HTTP routes for cross-project agent messaging, used by `ai-memory message`.
//!
//! The same handlers serve two mounts. [`router`] publishes `/messages*` behind
//! the machine bearer middleware, for any authenticated operator.
//! `/admin/messages*` stays in the admin router behind its root gate, for CLIs
//! older than these routes. Authentication is the mount's job; authorization
//! here is per project, matching the `memory_message_*` MCP tools: a database
//! user carries an `AuthorizedViewer`, so a restricted project admits them only
//! with a grant, and root carries none.

use ai_memory_core::{ActorContext, AgentKind, AuthLevel, AuthorizedViewer, MessageId, UserId};
use ai_memory_core::{ProjectId, WorkspaceId};
use ai_memory_store::{
    ProjectAccess, ReaderPool, ResolvedScope, StoreError, WriterHandle, authorize_scope_for,
    lookup_existing_scope,
};
use ai_memory_wiki::{AdmissionOp, Wiki};
use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::admin::{internal_err, scope_err, skip_webhooks_for_admin_request};

/// Caps on stored message text. The body crosses a project-isolation boundary
/// and is untrusted on the receiving side.
const MESSAGE_BODY_MAX_CHARS: usize = 8000;
const MESSAGE_SUBJECT_MAX_CHARS: usize = 200;
const MESSAGE_EVIDENCE_MAX_CHARS: usize = 1_000;

type Reply = (StatusCode, Json<Value>);
type Viewer = Option<Extension<AuthorizedViewer>>;
type Actor = Option<Extension<ActorContext>>;

/// Dependencies of the message handlers.
#[derive(Clone)]
pub struct MessageState {
    /// Read pool for scope, grant and mailbox lookups.
    pub reader: ReaderPool,
    /// Writer actor for mailbox changes.
    pub writer: WriterHandle,
    /// Admission and observer webhooks.
    pub wiki: Wiki,
    /// Whether a trusted proxy can assert operator identities.
    pub trusted_proxy_identity: bool,
}

/// Build the `/messages*` router. The caller must apply `require_bearer`, just
/// as for MCP and hooks.
pub fn router(state: MessageState) -> Router {
    Router::new()
        .route("/messages", get(list))
        .route("/messages/send", post(send))
        .route("/messages/pop", post(pop))
        .route("/messages/cancel", post(cancel))
        .with_state(state)
}

fn viewer_id(viewer: Viewer) -> Option<UserId> {
    viewer.map(|Extension(viewer)| viewer.user())
}

/// Look up an existing scope and check the caller's grant on it. A write is
/// decided on the writer's connection so it cannot race a grant change.
async fn authorized_scope(
    state: &MessageState,
    workspace: &str,
    project: &str,
    viewer: Option<UserId>,
    need: ProjectAccess,
) -> Result<(WorkspaceId, ProjectId), Reply> {
    let scope = lookup_existing_scope(&state.reader, workspace, project)
        .await
        .map_err(scope_err)?;
    let writer = (need == ProjectAccess::Write).then_some(&state.writer);
    authorize_scope_for(&state.reader, writer, scope, viewer, need)
        .await
        .map(ResolvedScope::as_tuple)
        .map_err(scope_err)
}

async fn owner_stamp(
    state: &MessageState,
    actor: Option<&ActorContext>,
) -> Result<Option<String>, Reply> {
    let distinguishes = state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    Ok(ai_memory_core::owner_stamp(
        actor.and_then(ActorContext::identity_key).as_ref(),
        distinguishes,
    ))
}

fn parse_message_id(raw: Option<&str>) -> Result<Option<MessageId>, Reply> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse::<MessageId>)
        .transpose()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "message_id must be a full UUID" })),
            )
        })
}

fn admission_refused(operation: &str, error: impl std::fmt::Display) -> Reply {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": format!("message {operation} refused by admission: {error}") })),
    )
}

/// An `InvalidState` is the full-inbox rejection: a caller error, not a fault.
fn store_err(error: StoreError) -> Reply {
    match error {
        StoreError::InvalidState(msg) => (StatusCode::CONFLICT, Json(json!({ "error": msg }))),
        other => internal_err(other.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    workspace: String,
    project: String,
    #[serde(rename = "box", default = "default_box")]
    mailbox: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_box() -> String {
    "inbox".into()
}

const fn default_limit() -> usize {
    50
}

/// `GET /messages` and `GET /admin/messages`: pending mail, oldest first.
pub(crate) async fn list(
    State(state): State<MessageState>,
    viewer: Viewer,
    Query(query): Query<ListQuery>,
) -> Result<Reply, Reply> {
    let (ws, proj) = authorized_scope(
        &state,
        &query.workspace,
        &query.project,
        viewer_id(viewer),
        ProjectAccess::Read,
    )
    .await?;
    let mailbox = match query.mailbox.as_str() {
        "inbox" => ai_memory_core::MessageBox::Inbox,
        "outbox" => ai_memory_core::MessageBox::Outbox,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": format!("box must be \"inbox\" or \"outbox\", got {other:?}")
                })),
            ));
        }
    };
    let messages = state
        .reader
        .list_messages(ws, proj, mailbox, query.limit.clamp(1, 200))
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    Ok((StatusCode::OK, Json(json!({ "messages": messages }))))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SendBody {
    from_workspace: String,
    from_project: String,
    to_workspace: String,
    to_project: String,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    nature: ai_memory_core::MessageNature,
    #[serde(default)]
    evidence: Option<String>,
    body: String,
}

/// `POST /messages/send` and `POST /admin/messages/send`.
///
/// Both scopes use the no-create lookup, so a typo'd project fails closed. The
/// recipient needs write access: delivery puts text into its agents' context,
/// and the mailbox must not be a way around a restricted project's grants.
/// Text is secret-scrubbed with the hook ingress sanitizer before it is capped.
pub(crate) async fn send(
    State(state): State<MessageState>,
    viewer: Viewer,
    actor: Actor,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(req): Json<SendBody>,
) -> Result<Reply, Reply> {
    let viewer = viewer_id(viewer);
    let (from_ws, from_proj) = authorized_scope(
        &state,
        &req.from_workspace,
        &req.from_project,
        viewer,
        ProjectAccess::Read,
    )
    .await?;
    let (to_ws, to_proj) = authorized_scope(
        &state,
        &req.to_workspace,
        &req.to_project,
        viewer,
        ProjectAccess::Write,
    )
    .await?;

    let sanitizer = ai_memory_core::Sanitizer::builtin();
    let cap = ai_memory_core::truncate_utf8_bytes;
    let body = cap(&sanitizer.scrub(&req.body), MESSAGE_BODY_MAX_CHARS);
    let subject = req
        .subject
        .as_deref()
        .map(|s| cap(&sanitizer.scrub(s), MESSAGE_SUBJECT_MAX_CHARS))
        .filter(|s| !s.is_empty());
    let evidence = req
        .evidence
        .as_deref()
        .map(|s| cap(&sanitizer.scrub(s), MESSAGE_EVIDENCE_MAX_CHARS))
        .filter(|s| !s.trim().is_empty());
    if req.nature == ai_memory_core::MessageNature::Measured && evidence.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "evidence_required: measured messages require non-empty evidence"
            })),
        ));
    }

    let actor = actor.map(|Extension(actor)| actor);
    let from_owner_user = owner_stamp(&state, actor.as_ref()).await?;
    let message = ai_memory_core::NewAgentMessage {
        from_workspace_id: from_ws,
        from_project_id: from_proj,
        from_agent: AgentKind::Other,
        from_session_id: None,
        from_owner_user,
        to_workspace_id: to_ws,
        to_project_id: to_proj,
        to_agent: None,
        subject,
        body,
        nature: req.nature,
        evidence,
    };

    // Admission is asked at the crossing point, the recipient scope.
    let admission = state
        .wiki
        .authorize_operation(
            to_ws,
            to_proj,
            AdmissionOp::MessageSend,
            actor.unwrap_or_else(ActorContext::anonymous),
            skip_webhooks_for_admin_request(level, &headers),
        )
        .await
        .map_err(|e| admission_refused("send", e))?;
    let queued_to_agent = message.to_agent;
    let id = state
        .writer
        .insert_message(message)
        .await
        .map_err(store_err)?;
    if let Some(mut ctx) = admission {
        state
            .wiki
            .describe_queued_message(&mut ctx, id, queued_to_agent, from_ws, from_proj)
            .await;
        state.wiki.notify_operation_observers(&ctx);
    }
    Ok((
        StatusCode::OK,
        Json(json!({ "message_id": id.to_string() })),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PopBody {
    #[serde(default)]
    agent: Option<String>,
    workspace: String,
    project: String,
    #[serde(default)]
    message_id: Option<String>,
}

/// `POST /messages/pop` and `POST /admin/messages/pop`: claim one inbox message
/// exactly once. Returns `{"message": null}` when nothing matched.
pub(crate) async fn pop(
    State(state): State<MessageState>,
    viewer: Viewer,
    actor: Actor,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(req): Json<PopBody>,
) -> Result<Reply, Reply> {
    let (ws, proj) = authorized_scope(
        &state,
        &req.workspace,
        &req.project,
        viewer_id(viewer),
        ProjectAccess::Write,
    )
    .await?;
    let specific_id = parse_message_id(req.message_id.as_deref())?;
    let declared_agent = req
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            let agent = AgentKind::from_wire(raw);
            if agent == AgentKind::Other && raw != "other" {
                Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "unknown receiving agent" })),
                ))
            } else {
                Ok(agent)
            }
        })
        .transpose()?;

    let actor = actor.map(|Extension(actor)| actor);
    let claiming_user = owner_stamp(&state, actor.as_ref()).await?;
    let claiming_agent = declared_agent
        .or_else(|| {
            actor
                .as_ref()
                .and_then(|actor| actor.agent.as_deref())
                .map(AgentKind::from_wire)
        })
        .unwrap_or(AgentKind::Other);
    let admission = state
        .wiki
        .authorize_operation(
            ws,
            proj,
            AdmissionOp::MessagePop,
            actor.unwrap_or_else(ActorContext::anonymous),
            skip_webhooks_for_admin_request(level, &headers),
        )
        .await
        .map_err(|e| admission_refused("pop", e))?;

    let claim = ai_memory_core::MessageClaim {
        workspace_id: ws,
        project_id: proj,
        claiming_agent,
        claiming_session: None,
        claiming_user,
    };
    match state.writer.pop_message(claim, specific_id).await {
        Ok(Some(message)) => {
            if let Some(ctx) = admission.as_ref() {
                state.wiki.notify_operation_observers(ctx);
            }
            Ok((
                StatusCode::OK,
                Json(json!({
                    "message": message,
                    "security_notice": ai_memory_core::UNTRUSTED_MESSAGE_NOTICE,
                })),
            ))
        }
        Ok(None) => Ok((StatusCode::OK, Json(json!({ "message": null })))),
        Err(e) => Err(internal_err(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CancelBody {
    workspace: String,
    project: String,
    #[serde(default)]
    message_id: Option<String>,
}

/// `POST /messages/cancel` and `POST /admin/messages/cancel`: retract pending
/// mail this project sent, one message or all of it.
pub(crate) async fn cancel(
    State(state): State<MessageState>,
    viewer: Viewer,
    actor: Actor,
    level: Option<Extension<AuthLevel>>,
    headers: HeaderMap,
    Json(req): Json<CancelBody>,
) -> Result<Reply, Reply> {
    let (ws, proj) = authorized_scope(
        &state,
        &req.workspace,
        &req.project,
        viewer_id(viewer),
        ProjectAccess::Write,
    )
    .await?;
    let specific_id = parse_message_id(req.message_id.as_deref())?;
    let admission = state
        .wiki
        .authorize_operation(
            ws,
            proj,
            AdmissionOp::MessageCancel,
            actor
                .map(|Extension(actor)| actor)
                .unwrap_or_else(ActorContext::anonymous),
            skip_webhooks_for_admin_request(level, &headers),
        )
        .await
        .map_err(|e| admission_refused("cancel", e))?;
    let cancelled = state
        .writer
        .cancel_messages(ws, proj, specific_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    if cancelled > 0
        && let Some(ctx) = admission.as_ref()
    {
        state.wiki.notify_operation_observers(ctx);
    }
    Ok((StatusCode::OK, Json(json!({ "cancelled": cancelled }))))
}
