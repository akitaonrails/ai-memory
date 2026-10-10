//! Authenticated HTTP ingress for optional managed workstreams.

use std::fmt::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::str::FromStr as _;

use ai_memory_core::{
    AgentKind, AuthLevel, Capability, FinishManagedRunRequest, FinishManagedRunResponse,
    LinkManagedRunRequest, ListManagedWorkstreamsRequest, ManagedRunContextResponse, ManagedRunId,
    ManagedRunStatus, ManagedWorkstreamSummary, NativeSessionIdentity, NewWorkstreamEvent,
    PrepareManagedRunRequest, PrepareManagedRunResponse, RenameManagedWorkstreamRequest,
    RenamedManagedWorkstream, Sanitizer, WorkstreamEventKind, WorkstreamId,
};
use ai_memory_store::{
    FinishWorkstreamRun, PrepareWorkstreamRun, ReaderPool, RenameWorkstream, ScopeResolutionError,
    StoreError, WorkstreamSelection, WorkstreamSelector, WriterHandle, authorize_scope_for,
    create_explicit_scope_guarded, lookup_existing_scope_guarded,
};
use ai_memory_wiki::Wiki;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tracing::warn;

const MAX_EVENTS_PER_FINISH: usize = 4_096;
const MAX_EVENT_CONTENT_BYTES: usize = 64 * 1024;
const MAX_EVENT_ID_BYTES: usize = 512;
const MAX_NAME_BYTES: usize = 256;
const MAX_CWD_BYTES: usize = 16 * 1024;

/// State shared by the managed-workstream HTTP endpoints.
#[derive(Clone)]
pub struct WorkstreamState {
    /// Single-writer store actor.
    pub writer: WriterHandle,
    /// Read pool for status and packet assembly.
    pub reader: ReaderPool,
    /// Privacy scrubber applied before raw or indexed persistence.
    pub sanitizer: Sanitizer,
    /// Wiki handle for refreshing a scope manifest after name promotion.
    pub wiki: Wiki,
    /// ai-memory data root containing `raw/workstreams`.
    pub data_dir: PathBuf,
    /// Operator home directory; a project rooted there never claims a run's
    /// checkout by cwd prefix, as in hook routing.
    pub home_dir: Option<String>,
    /// Whether a trusted proxy can distinguish operators without DB users.
    pub trusted_proxy_identity: bool,
    #[cfg(test)]
    finish_barrier: Option<std::sync::Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
}

pub(crate) async fn managed_run_owner_stamp(
    reader: &ReaderPool,
    identity: Option<&ai_memory_core::IdentityKey>,
    trusted_proxy_identity: bool,
) -> Result<Option<String>, StoreError> {
    let Some(identity) = identity else {
        return Ok(None);
    };
    let distinguishes = reader
        .distinguishes_operators(trusted_proxy_identity)
        .await?;
    Ok(ai_memory_core::owner_stamp(Some(identity), distinguishes))
}

/// Build the host-wrapper API. It is mounted beside `/hook` and therefore
/// receives the same bearer-auth middleware as MCP and hook ingress.
pub fn workstream_router(state: WorkstreamState) -> Router {
    Router::new()
        .route("/workstream/runs", post(prepare_run))
        .route("/workstream/runs/{run_id}", get(run_status))
        .route("/workstream/runs/{run_id}/heartbeat", post(heartbeat_run))
        .route("/workstream/runs/{run_id}/cancel", post(cancel_run))
        .route("/workstream/runs/{run_id}/context", post(run_context))
        .route(
            "/workstream/runs/{run_id}/context/accept",
            post(accept_run_context),
        )
        .route("/workstream/runs/{run_id}/link", post(link_run))
        .route("/workstream/runs/{run_id}/finish", post(finish_run))
        .route(
            "/workstream/runs/{run_id}/recover/finish",
            post(recover_finish_run),
        )
        .route("/workstream/recent", post(list_recent_workstreams))
        .route("/workstream/rename", post(rename_workstream))
        .route("/workstream/{workstream_id}/events", get(search_events))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
    #[serde(flatten)]
    warning: ai_memory_core::repository_identity::ManifestWarningContext,
}

type ApiFailure = (StatusCode, Json<ApiError>);

fn api_failure(status: StatusCode, message: impl Into<String>) -> ApiFailure {
    api_failure_with_manifest_warning(status, message, None)
}

fn api_failure_with_manifest_warning(
    status: StatusCode,
    message: impl Into<String>,
    manifest_warning: Option<ai_memory_core::repository_identity::ManifestWarning>,
) -> ApiFailure {
    (
        status,
        Json(ApiError {
            error: message.into(),
            warning: ai_memory_core::repository_identity::ManifestWarningContext {
                manifest_warning,
            },
        }),
    )
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    api_failure(status, message).into_response()
}

fn authorize(
    level: Option<Extension<AuthLevel>>,
    capability: Capability,
) -> Result<(), ApiFailure> {
    let level = level.map_or(AuthLevel::Anonymous, |Extension(level)| level);
    level.authorize(capability, true).map_err(|failure| {
        let status = if failure.is_authentication_required() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::FORBIDDEN
        };
        api_failure(status, failure.message())
    })
}

/// The user whose grants apply to this request, if any do.
///
/// [`ai_memory_core::AuthorizedViewer`] is stamped only when an operator has
/// switched per-repository authorization on, and never for root. `None`
/// therefore means no per-repository check applies, which leaves every
/// existing install behaving exactly as it did before the guard existed.
fn actor_user(
    actor: Option<Extension<ai_memory_core::AuthorizedViewer>>,
) -> Option<ai_memory_core::UserId> {
    actor.map(|Extension(viewer)| viewer.user())
}

/// Authorize the caller on the repository `run_id` belongs to — see
/// [`crate::grants::authorize_resolved`]. The run routes take the id in the
/// URL, so without this they would act on any repository's run.
async fn authorize_run(
    state: &WorkstreamState,
    run_id: ManagedRunId,
    viewer: Option<ai_memory_core::UserId>,
    required: ai_memory_store::ProjectAccess,
) -> Result<(), Response> {
    if viewer.is_none() {
        return Ok(());
    }
    let scope = state
        .reader
        .managed_run_scope(run_id)
        .await
        .map_err(|failure| error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()))?;
    crate::grants::authorize_resolved(&state.reader, scope, viewer, required)
        .await
        .map_err(scope_refusal)
}

/// Admit a mutation of `run_id` only for its owner with current Write access
/// on the run's project — the rule finish applies, shared by every route that
/// releases, renews or rebinds a run so none of them is a side door around it.
/// The authority comes from authenticated middleware extensions only.
async fn authorize_run_owner(
    state: &WorkstreamState,
    run_id: ManagedRunId,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
) -> Result<ai_memory_store::ManagedRunAuthority, Response> {
    let authority = ai_memory_store::ManagedRunAuthority::from_auth(
        level.map_or(AuthLevel::Anonymous, |Extension(level)| level),
        viewer.map(|Extension(viewer)| viewer),
        user_id.map(|Extension(user_id)| user_id),
        &actor.map_or_else(
            ai_memory_core::ActorContext::anonymous,
            |Extension(actor)| actor,
        ),
        state.trusted_proxy_identity,
    );
    state
        .reader
        .authorize_managed_run(run_id, authority.clone())
        .await
        .map_err(store_error_response)?;
    Ok(authority)
}

/// A refusal as a response: 403 for an access problem, 500 otherwise.
fn scope_refusal(failure: ScopeResolutionError) -> Response {
    if failure.is_forbidden() {
        error(StatusCode::FORBIDDEN, failure.to_string())
    } else {
        error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string())
    }
}

/// The scope a managed run opens in, and whether resolving it renamed the
/// project in place.
///
/// A run whose `project` is the name its checkout's git remote derives routes
/// by that identity, through the same writer transaction hook capture uses:
/// the project already carrying the identity wins, else the project whose
/// `repo_path` contains the cwd is claimed. Routing by name alone would miss a
/// project created from the folder name before identities were recorded,
/// create an identity-less twin under the derived name, and leave every later
/// run ambiguous once capture claimed the original. Any other run keeps the
/// explicit-name path.
async fn resolve_run_scope(
    state: &WorkstreamState,
    request: &PrepareManagedRunRequest,
    viewer: Option<ai_memory_core::UserId>,
) -> Result<(ai_memory_store::ResolvedScope, bool), ScopeResolutionError> {
    use ai_memory_core::repository_identity::{
        IdentitySource, IdentityStyle, accept_wire_identity, path_style_name,
    };
    let workspace = request.workspace.trim();
    let project = request.project.trim();
    let identity = request
        .repository_identity
        .as_deref()
        .and_then(|identity| accept_wire_identity(identity, IdentitySource::GitRemote.as_str()))
        .filter(|identity| path_style_name(identity).as_deref() == Some(project))
        .filter(|_| !ai_memory_core::profile::is_reserved_scope_project(project));
    let Some(identity) = identity else {
        let resolved =
            create_explicit_scope_guarded(&state.reader, &state.writer, workspace, project, viewer)
                .await?;
        return Ok((resolved.scope, resolved.promoted_from.is_some()));
    };
    let workspace_id = state
        .writer
        .get_or_create_workspace(workspace.to_owned())
        .await?;
    let candidate = state
        .reader
        .find_project_by_cwd_prefix(workspace_id, request.cwd.clone(), state.home_dir.as_deref())
        .await?
        .map(|(project_id, _)| project_id);
    let (project_id, resolution) = state
        .writer
        .resolve_project_by_identity_for_capture(
            workspace_id,
            identity,
            IdentityStyle::Path,
            project,
            None,
            candidate,
            viewer,
        )
        .await?;
    // The writer claims or renames only for a caller who may write; matching
    // or finding a project is not access to it.
    let scope = authorize_scope_for(
        &state.reader,
        Some(&state.writer),
        ai_memory_store::ResolvedScope {
            workspace_id,
            project_id,
        },
        viewer,
        ai_memory_store::ProjectAccess::Write,
    )
    .await?;
    Ok((
        scope,
        resolution == ai_memory_store::IdentityResolution::Promoted,
    ))
}

async fn prepare_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    Json(request): Json<PrepareManagedRunRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    if request.workspace.trim().is_empty()
        || request.project.trim().is_empty()
        || request.cwd.trim().is_empty()
        || request.repo_fingerprint.trim().is_empty()
        || request.worktree_fingerprint.trim().is_empty()
        || request.lease_owner.trim().is_empty()
    {
        return error(
            StatusCode::BAD_REQUEST,
            "managed run fields cannot be empty",
        );
    }
    if request.cwd.len() > MAX_CWD_BYTES {
        return error(StatusCode::BAD_REQUEST, "managed run cwd is too long");
    }
    if request.workstream.is_some() && request.new_workstream.is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "workstream and new_workstream are mutually exclusive",
        );
    }
    if !matches!(
        request.agent,
        AgentKind::ClaudeCode
            | AgentKind::Codex
            | AgentKind::OpenCode
            | AgentKind::Pi
            | AgentKind::Crush
            | AgentKind::Omp
            | AgentKind::KimiCode
            | AgentKind::CommandCode
            | AgentKind::KiroCli
            | AgentKind::Grok
            | AgentKind::AntigravityCli
            | AgentKind::CopilotCli
    ) {
        return error(
            StatusCode::BAD_REQUEST,
            "managed run requires a supported command-line harness",
        );
    }
    const AUTO_AGENTS: [AgentKind; 8] = [
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::OpenCode,
        AgentKind::Pi,
        AgentKind::Crush,
        AgentKind::KimiCode,
        AgentKind::CommandCode,
        AgentKind::KiroCli,
    ];
    if request.automatic_harness
        && (!AUTO_AGENTS.contains(&request.agent)
            || !request.available_agents.contains(&request.agent)
            || request
                .available_agents
                .iter()
                .any(|agent| !AUTO_AGENTS.contains(agent)))
    {
        return error(
            StatusCode::BAD_REQUEST,
            "automatic managed run requires supported checkout-local harnesses",
        );
    }
    for (label, value) in [
        ("workspace", request.workspace.as_str()),
        ("project", request.project.as_str()),
        ("repo_fingerprint", request.repo_fingerprint.as_str()),
        (
            "worktree_fingerprint",
            request.worktree_fingerprint.as_str(),
        ),
        ("lease_owner", request.lease_owner.as_str()),
    ] {
        if value.len() > MAX_NAME_BYTES {
            return error(StatusCode::BAD_REQUEST, format!("{label} is too long"));
        }
    }
    let selection = match (&request.workstream, &request.new_workstream) {
        (Some(name), None) => WorkstreamSelection::Named(name.trim().to_string()),
        (None, Some(name)) => WorkstreamSelection::New(name.trim().to_string()),
        (None, None) => WorkstreamSelection::Current,
        (Some(_), Some(_)) => {
            return error(
                StatusCode::BAD_REQUEST,
                "workstream and new_workstream are mutually exclusive",
            );
        }
    };
    if let Err(failure) = selection.validate() {
        return store_error_response(failure);
    }
    let (scope, promoted) = match resolve_run_scope(&state, &request, actor_user(viewer)).await {
        Ok(resolved) => resolved,
        Err(failure) if failure.is_forbidden() => {
            return error(StatusCode::FORBIDDEN, failure.to_string());
        }
        Err(failure) => return error(StatusCode::BAD_REQUEST, failure.to_string()),
    };
    let manifest_warning = if promoted {
        match state
            .wiki
            .refresh_renamed_scope(scope.workspace_id, scope.project_id)
            .await
        {
            Ok(_) => None,
            Err(failure) => {
                warn!(
                    error = %failure,
                    workspace_id = %scope.workspace_id,
                    project_id = %scope.project_id,
                    "project name promotion committed; manifest refresh/checkpoint failed; startup backfill can repair"
                );
                Some(
                    ai_memory_core::repository_identity::ManifestWarning::promotion_refresh_failed(
                        failure,
                    ),
                )
            }
        }
    } else {
        None
    };
    let identity = actor.and_then(|Extension(actor)| actor.identity_key());
    let owner_user = match managed_run_owner_stamp(
        &state.reader,
        identity.as_ref(),
        state.trusted_proxy_identity,
    )
    .await
    {
        Ok(owner) => owner,
        Err(failure) => {
            return api_failure_with_manifest_warning(
                StatusCode::INTERNAL_SERVER_ERROR,
                failure.to_string(),
                manifest_warning,
            )
            .into_response();
        }
    };
    let force_unlock = request.force_unlock;
    let prepared = state
        .writer
        .prepare_workstream_run_owned_with_unlock(
            PrepareWorkstreamRun {
                workspace_id: scope.workspace_id,
                project_id: scope.project_id,
                repo_fingerprint: request.repo_fingerprint,
                worktree_fingerprint: request.worktree_fingerprint,
                cwd: request.cwd,
                agent: request.agent,
                automatic_harness: request.automatic_harness,
                available_agents: request.available_agents,
                selection,
                lease_owner: request.lease_owner,
            },
            owner_user,
            force_unlock,
        )
        .await;
    match prepared {
        Ok(prepared) => Json(PrepareManagedRunResponse {
            workstream_id: prepared.workstream_id,
            workstream_name: prepared.workstream_name,
            run_id: prepared.run_id,
            resolved_agent: Some(prepared.agent),
            native_session_id: prepared.native_session_id,
            source_cursor: prepared.source_cursor,
            sync_after: prepared.sync_after,
            sync_through: prepared.sync_through,
            may_adopt_existing_session: prepared.may_adopt_existing_session,
            manifest_warning,
        })
        .into_response(),
        Err(failure) => store_error_response_with_manifest_warning(failure, manifest_warning),
    }
}

async fn run_status(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalRead) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if let Err(response) = authorize_run(
        &state,
        run_id,
        actor_user(actor),
        ai_memory_store::ProjectAccess::Read,
    )
    .await
    {
        return response;
    }
    match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => Json(ManagedRunStatus {
            run_id: status.run_id,
            workstream_id: status.workstream_id,
            agent: status.agent,
            native_session_id: status.native_session_id,
            native_session_linked: status.native_session_linked,
            context_delivered: status.context_delivered,
            state: status.state,
        })
        .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    }
}

async fn heartbeat_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if let Err(response) = authorize_run_owner(&state, run_id, level, viewer, user_id, actor).await
    {
        return response;
    }
    match state.writer.heartbeat_managed_run(run_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::CONFLICT, "managed run lease is not active"),
        Err(failure) => store_error_response(failure),
    }
}

async fn cancel_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if let Err(response) = authorize_run_owner(&state, run_id, level, viewer, user_id, actor).await
    {
        return response;
    }
    match state.writer.cancel_managed_run(run_id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(failure) => store_error_response(failure),
    }
}

async fn run_context(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if let Err(response) = authorize_run(
        &state,
        run_id,
        actor_user(actor),
        ai_memory_store::ProjectAccess::Write,
    )
    .await
    {
        return response;
    }
    let context = match state.reader.managed_run_context(run_id, 256).await {
        Ok(Some(context)) => context,
        Ok(None) => return error(StatusCode::NOT_FOUND, "active managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if !matches!(context.agent, AgentKind::Crush | AgentKind::Grok) {
        return error(
            StatusCode::BAD_REQUEST,
            "direct managed context is only supported for Crush and Grok",
        );
    }
    if context.context_delivered {
        return Json(ManagedRunContextResponse { context: None }).into_response();
    }
    let rendered = crate::router::render_managed_context(
        &context.events,
        &context.workstream_name,
        context.workstream_id,
        context.sync_after,
    );
    Json(ManagedRunContextResponse { context: rendered }).into_response()
}

async fn accept_run_context(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    AxumPath(raw_run_id): AxumPath<String>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if let Err(response) = authorize_run_owner(&state, run_id, level, viewer, user_id, actor).await
    {
        return response;
    }
    let status = match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if !matches!(status.agent, AgentKind::Crush | AgentKind::Grok) {
        return error(
            StatusCode::BAD_REQUEST,
            "direct managed context is only supported for Crush and Grok",
        );
    }
    match state.writer.accept_managed_run_context(run_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::CONFLICT, "managed run lease is not active"),
        Err(failure) => store_error_response(failure),
    }
}

#[derive(Debug, Default, Deserialize)]
struct EventQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_event_limit")]
    limit: usize,
}

const fn default_event_limit() -> usize {
    20
}

async fn list_recent_workstreams(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    Json(request): Json<ListManagedWorkstreamsRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalRead) {
        return response.into_response();
    }
    for (label, value) in [
        ("workspace", request.workspace.as_str()),
        ("project", request.project.as_str()),
        ("repo_fingerprint", request.repo_fingerprint.as_str()),
        (
            "worktree_fingerprint",
            request.worktree_fingerprint.as_str(),
        ),
    ] {
        if value.trim().is_empty() {
            return error(StatusCode::BAD_REQUEST, format!("{label} cannot be empty"));
        }
        if value.len() > MAX_NAME_BYTES {
            return error(StatusCode::BAD_REQUEST, format!("{label} is too long"));
        }
    }
    let scope = match lookup_existing_scope_guarded(
        &state.reader,
        request.workspace.trim(),
        request.project.trim(),
        actor_user(actor),
        ai_memory_store::ProjectAccess::Read,
    )
    .await
    {
        Ok(scope) => scope,
        Err(failure) if failure.is_forbidden() => {
            return error(StatusCode::FORBIDDEN, failure.to_string());
        }
        Err(failure) if failure.is_not_found() => {
            return error(StatusCode::NOT_FOUND, failure.to_string());
        }
        Err(failure) if failure.is_bad_request() => {
            return error(StatusCode::BAD_REQUEST, failure.to_string());
        }
        Err(ScopeResolutionError::Store(message)) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, message);
        }
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    let summaries = match state
        .reader
        .recent_workstreams_page(
            scope.workspace_id,
            scope.project_id,
            request.repo_fingerprint,
            request.worktree_fingerprint,
            request.limit.clamp(1, 100),
            request.offset,
        )
        .await
    {
        Ok(summaries) => summaries,
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    let mut response = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let created_at = match Timestamp::from_microsecond(summary.created_at) {
            Ok(timestamp) => timestamp.to_string(),
            Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
        };
        let last_active_at = match Timestamp::from_microsecond(summary.last_active_at) {
            Ok(timestamp) => timestamp.to_string(),
            Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
        };
        response.push(ManagedWorkstreamSummary {
            workstream_id: summary.workstream_id,
            name: summary.name,
            created_at,
            last_active_at,
            current: summary.current,
            linked_harnesses: summary.linked_harnesses,
        });
    }
    Json(response).into_response()
}

/// Retitle one checkout-local managed workstream.
///
/// A write surface, so it takes `NormalWrite` rather than the `NormalRead`
/// the sibling discovery read uses. Scope still resolves through
/// `lookup_existing_scope`: a rename never creates the workspace or project
/// it names, and the store repeats the checkout predicate on the id lookup so
/// an id belonging to another checkout reads as absent rather than renamable.
async fn rename_workstream(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    Json(request): Json<RenameManagedWorkstreamRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    for (label, value) in [
        ("workspace", request.workspace.as_str()),
        ("project", request.project.as_str()),
        ("repo_fingerprint", request.repo_fingerprint.as_str()),
        (
            "worktree_fingerprint",
            request.worktree_fingerprint.as_str(),
        ),
        ("to", request.to.as_str()),
    ] {
        if value.trim().is_empty() {
            return error(StatusCode::BAD_REQUEST, format!("{label} cannot be empty"));
        }
        if value.len() > MAX_NAME_BYTES {
            return error(StatusCode::BAD_REQUEST, format!("{label} is too long"));
        }
    }
    let selector = match (request.from.as_deref(), request.workstream_id) {
        (Some(name), None) => {
            let name = name.trim();
            if name.is_empty() {
                return error(StatusCode::BAD_REQUEST, "from cannot be empty");
            }
            if name.len() > MAX_NAME_BYTES {
                return error(StatusCode::BAD_REQUEST, "from is too long");
            }
            WorkstreamSelector::Name(name.to_string())
        }
        (None, Some(id)) => WorkstreamSelector::Id(id),
        (Some(_), Some(_)) => {
            return error(
                StatusCode::BAD_REQUEST,
                "from and workstream_id are mutually exclusive",
            );
        }
        (None, None) => {
            return error(
                StatusCode::BAD_REQUEST,
                "one of from or workstream_id is required",
            );
        }
    };
    let scope = match lookup_existing_scope_guarded(
        &state.reader,
        request.workspace.trim(),
        request.project.trim(),
        actor_user(actor),
        ai_memory_store::ProjectAccess::Write,
    )
    .await
    {
        Ok(scope) => scope,
        Err(failure) if failure.is_forbidden() => {
            return error(StatusCode::FORBIDDEN, failure.to_string());
        }
        Err(failure) if failure.is_not_found() => {
            return error(StatusCode::NOT_FOUND, failure.to_string());
        }
        Err(failure) if failure.is_bad_request() => {
            return error(StatusCode::BAD_REQUEST, failure.to_string());
        }
        Err(ScopeResolutionError::Store(message)) => {
            return error(StatusCode::INTERNAL_SERVER_ERROR, message);
        }
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    let renamed = match state
        .writer
        .rename_workstream(RenameWorkstream {
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            repo_fingerprint: request.repo_fingerprint,
            worktree_fingerprint: request.worktree_fingerprint,
            selector,
            new_name: request.to,
        })
        .await
    {
        Ok(renamed) => renamed,
        Err(failure) => return store_error_response(failure),
    };
    Json(RenamedManagedWorkstream {
        workstream_id: renamed.workstream_id,
        from: renamed.from,
        to: renamed.to,
    })
    .into_response()
}

async fn search_events(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    actor: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    AxumPath(raw_workstream_id): AxumPath<String>,
    Query(query): Query<EventQuery>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalRead) {
        return response.into_response();
    }
    let workstream_id = match WorkstreamId::from_str(&raw_workstream_id) {
        Ok(id) => id,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid workstream id"),
    };
    // The id reaches a repository's event history without naming it.
    if let Some(viewer) = actor_user(actor) {
        let scope = match state.reader.workstream_scope(workstream_id).await {
            Ok(scope) => scope,
            Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
        };
        if let Err(failure) = crate::grants::authorize_resolved(
            &state.reader,
            scope,
            Some(viewer),
            ai_memory_store::ProjectAccess::Read,
        )
        .await
        {
            return scope_refusal(failure);
        }
    }
    match state
        .reader
        .search_workstream_events(
            workstream_id,
            query.q,
            query.limit.clamp(1, 100),
            state.sanitizer.clone(),
        )
        .await
    {
        Ok(mut events) => {
            for event in &mut events {
                event.native_session_id =
                    NativeSessionIdentity::project(&event.native_session_id, &state.sanitizer);
            }
            Json(events).into_response()
        }
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    }
}

async fn link_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    AxumPath(raw_run_id): AxumPath<String>,
    Json(request): Json<LinkManagedRunRequest>,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    if let Err(response) = authorize_run_owner(&state, run_id, level, viewer, user_id, actor).await
    {
        return response;
    }
    let native_identity =
        match NativeSessionIdentity::parse(&request.native_session_id, &state.sanitizer) {
            Ok(id) => id,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "native session identity is UNKNOWN; refusing managed binding",
                );
            }
        };
    let status = match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if status.state != "active" {
        return error(StatusCode::CONFLICT, "managed run is not active");
    }
    match state
        .writer
        .link_managed_run_session(run_id, status.agent, native_identity.into_string())
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::CONFLICT, "managed run is not active"),
        Err(failure) => store_error_response(failure),
    }
}

async fn recover_finish_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    AxumPath(raw_run_id): AxumPath<String>,
    Json(request): Json<FinishManagedRunRequest>,
) -> Response {
    finish_run_inner(
        state, level, viewer, actor, user_id, raw_run_id, request, true,
    )
    .await
}

async fn finish_run(
    State(state): State<WorkstreamState>,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    AxumPath(raw_run_id): AxumPath<String>,
    Json(request): Json<FinishManagedRunRequest>,
) -> Response {
    finish_run_inner(
        state, level, viewer, actor, user_id, raw_run_id, request, false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_run_inner(
    state: WorkstreamState,
    level: Option<Extension<AuthLevel>>,
    viewer: Option<Extension<ai_memory_core::AuthorizedViewer>>,
    actor: Option<Extension<ai_memory_core::ActorContext>>,
    user_id: Option<Extension<ai_memory_core::UserId>>,
    raw_run_id: String,
    mut request: FinishManagedRunRequest,
    recover_expired: bool,
) -> Response {
    if let Err(response) = authorize(level, Capability::NormalWrite) {
        return response.into_response();
    }
    let run_id = match parse_run_id(&raw_run_id) {
        Ok(id) => id,
        Err(response) => return response.into_response(),
    };
    let authority = match authorize_run_owner(&state, run_id, level, viewer, user_id, actor).await {
        Ok(authority) => authority,
        Err(response) => return response,
    };
    #[cfg(test)]
    if let Some(barrier) = &state.finish_barrier {
        barrier.0.notify_one();
        barrier.1.notified().await;
    }
    if request.events.len() > MAX_EVENTS_PER_FINISH {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("at most {MAX_EVENTS_PER_FINISH} events are accepted per finish"),
        );
    }
    if request
        .events
        .iter()
        .any(|event| event.event_id.starts_with("managed-run:"))
    {
        return error(StatusCode::BAD_REQUEST, "reserved workstream event id");
    }
    for id in request.native_session_id.as_deref().into_iter().chain(
        request
            .events
            .iter()
            .map(|event| event.native_session_id.as_str()),
    ) {
        if NativeSessionIdentity::parse(id, &state.sanitizer).is_err() {
            return error(
                StatusCode::BAD_REQUEST,
                "native session identity is UNKNOWN; refusing managed binding",
            );
        }
    }
    let status = match state.reader.managed_run_status(run_id).await {
        Ok(Some(status)) => status,
        Ok(None) => return error(StatusCode::NOT_FOUND, "managed run not found"),
        Err(failure) => return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string()),
    };
    if status.state == "finished" && !recover_expired {
        return match state
            .writer
            .finish_workstream_run(
                authority,
                FinishWorkstreamRun {
                    sanitizer: state.sanitizer.clone(),
                    run_id,
                    native_session_id: None,
                    source_cursor: None,
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: None,
                },
            )
            .await
        {
            Ok(result) => Json(FinishManagedRunResponse {
                imported_events: 0,
                latest_sequence: result.latest_sequence,
            })
            .into_response(),
            Err(failure) => store_error_response(failure),
        };
    }
    if status.state != "active"
        && !(recover_expired && matches!(status.state.as_str(), "expired" | "finished"))
    {
        return error(StatusCode::CONFLICT, "managed run is not recoverable");
    }
    let native_identity = match request
        .native_session_id
        .as_deref()
        .or(status.native_session_id.as_deref())
        .map(|id| NativeSessionIdentity::parse(id, &state.sanitizer))
        .transpose()
    {
        Ok(id) => id,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "native session identity is UNKNOWN; refusing managed binding",
            );
        }
    };
    let native_session_id = native_identity
        .as_ref()
        .map(|id| id.as_str().to_owned())
        .unwrap_or_else(|| format!("unresolved:{run_id}"));
    if let Err(message) = sanitize_events(
        &state.sanitizer,
        status.agent,
        &native_session_id,
        &mut request.events,
    ) {
        return error(StatusCode::BAD_REQUEST, message);
    }
    if request.complete && status.state != "finished" {
        append_boundary_events(
            &state.sanitizer,
            run_id,
            status.agent,
            &native_session_id,
            &mut request,
        );
    }
    let segment_path = if status.state == "finished" {
        None
    } else {
        match write_segment(
            &state.data_dir,
            status.workstream_id,
            run_id,
            &request.events,
        ) {
            Ok(path) => Some(path),
            Err(failure) => {
                warn!(error = %failure, run = %run_id, "managed transcript segment write failed");
                return error(StatusCode::INTERNAL_SERVER_ERROR, failure.to_string());
            }
        }
    };
    let input = FinishWorkstreamRun {
        sanitizer: state.sanitizer.clone(),
        run_id,
        native_session_id: native_identity.map(NativeSessionIdentity::into_string),
        source_cursor: request.source_cursor,
        events: request.events,
        complete: request.complete,
        segment_path,
        exit_code: request.exit_code,
    };
    let result = if recover_expired {
        state
            .writer
            .recover_expired_workstream_run(authority, input)
            .await
    } else {
        state.writer.finish_workstream_run(authority, input).await
    };
    match result {
        Ok(result) => Json(FinishManagedRunResponse {
            imported_events: result.imported_events,
            latest_sequence: result.latest_sequence,
        })
        .into_response(),
        Err(failure) => store_error_response(failure),
    }
}

fn parse_run_id(raw: &str) -> Result<ManagedRunId, ApiFailure> {
    ManagedRunId::from_str(raw).map_err(|_| api_failure(StatusCode::BAD_REQUEST, "invalid run id"))
}

fn store_error_response(failure: StoreError) -> Response {
    store_error_response_with_manifest_warning(failure, None)
}

fn store_error_response_with_manifest_warning(
    failure: StoreError,
    manifest_warning: Option<ai_memory_core::repository_identity::ManifestWarning>,
) -> Response {
    let status = match failure {
        StoreError::Forbidden(_) => StatusCode::FORBIDDEN,
        StoreError::WorkstreamBusy(_)
        | StoreError::Duplicate(_)
        | StoreError::WorkstreamNameTaken(_) => StatusCode::CONFLICT,
        StoreError::NotFound(_) => StatusCode::NOT_FOUND,
        StoreError::InvalidState(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_failure_with_manifest_warning(status, failure.to_string(), manifest_warning).into_response()
}

fn sanitize_events(
    sanitizer: &Sanitizer,
    expected_agent: AgentKind,
    expected_session: &str,
    events: &mut [NewWorkstreamEvent],
) -> Result<(), String> {
    for event in events {
        if event.agent != expected_agent {
            return Err(format!(
                "event {} agent does not match managed run",
                event.event_id
            ));
        }
        if event.native_session_id != expected_session {
            return Err(format!(
                "event {} native session does not match managed run",
                event.event_id
            ));
        }
        if event.event_id.trim().is_empty() || event.event_id.len() > MAX_EVENT_ID_BYTES {
            return Err("invalid workstream event id".to_string());
        }
        // Scrub the full content before the cap: truncating first could cut a
        // secret straddling the cap into a fragment too short for any pattern
        // to match, persisting the prefix verbatim (#1113, same class as
        // #980/#1109).
        event.content = sanitizer.scrub(&event.content);
        truncate_owned(&mut event.content, MAX_EVENT_CONTENT_BYTES);
        if let Some(role) = &event.role
            && (role.len() > 32
                || !role
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')))
        {
            return Err("invalid workstream message role".to_string());
        }
        (event.source_record_id, event.metadata) = ai_memory_core::scrub_workstream_provenance(
            sanitizer,
            event.source_record_id.as_deref(),
            &event.metadata,
        );
    }
    Ok(())
}

fn append_boundary_events(
    sanitizer: &Sanitizer,
    run_id: ManagedRunId,
    agent: AgentKind,
    native_session_id: &str,
    request: &mut FinishManagedRunRequest,
) {
    let mut checkpoint = String::new();
    if let Some(head) = &request.checkpoint.head {
        let _ = writeln!(checkpoint, "HEAD: {head}");
    }
    if let Some(branch) = &request.checkpoint.branch {
        let _ = writeln!(checkpoint, "Branch: {branch}");
    }
    if let Some(dirty_hash) = &request.checkpoint.dirty_hash {
        let _ = writeln!(checkpoint, "Dirty-state hash: {dirty_hash}");
    }
    if !request.checkpoint.changed_paths.is_empty() {
        checkpoint.push_str("Changed paths:\n");
        for path in &request.checkpoint.changed_paths {
            let _ = writeln!(checkpoint, "- {path}");
        }
    }
    if checkpoint.is_empty() {
        checkpoint.push_str("No Git repository checkpoint was available.");
    }
    // Scrub-then-cap, same ordering as `sanitize_events` above (#1113).
    let mut checkpoint = sanitizer.scrub(&checkpoint);
    truncate_owned(&mut checkpoint, MAX_EVENT_CONTENT_BYTES);
    request.events.push(NewWorkstreamEvent {
        event_id: format!("managed-run:{run_id}:checkpoint"),
        agent,
        native_session_id: native_session_id.to_string(),
        source_record_id: None,
        kind: WorkstreamEventKind::Checkpoint,
        role: None,
        content: checkpoint,
        occurred_at: None,
        metadata: serde_json::json!({ "exit_code": request.exit_code }),
    });
    if !request.losses.is_empty() {
        let mut content = String::from("Transcript extraction losses:\n");
        for loss in &request.losses {
            let _ = writeln!(content, "- {loss}");
        }
        let mut content = sanitizer.scrub(&content);
        truncate_owned(&mut content, MAX_EVENT_CONTENT_BYTES);
        request.events.push(NewWorkstreamEvent {
            event_id: format!("managed-run:{run_id}:losses"),
            agent,
            native_session_id: native_session_id.to_string(),
            source_record_id: None,
            kind: WorkstreamEventKind::Annotation,
            role: None,
            content,
            occurred_at: None,
            metadata: serde_json::json!({ "loss_count": request.losses.len() }),
        });
    }
}

fn write_segment(
    data_dir: &Path,
    workstream_id: ai_memory_core::WorkstreamId,
    run_id: ManagedRunId,
    events: &[NewWorkstreamEvent],
) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
    }
    let digest = Sha256::digest(&bytes);
    let digest_hex = format!("{digest:x}");
    let relative = PathBuf::from("workstreams")
        .join(workstream_id.to_string())
        .join("segments")
        .join(format!("{run_id}-{}.jsonl", &digest_hex[..16]));
    let target = data_dir.join("raw").join(&relative);
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("managed segment has no parent"))?;
    create_private_dir_all(parent)?;
    if target.exists() {
        return Ok(relative.to_string_lossy().replace('\\', "/"));
    }
    let temp = parent.join(format!(".{run_id}-{}.tmp", ManagedRunId::new()));
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    if let Err(failure) = std::fs::rename(&temp, &target) {
        let target_won_race = target.exists();
        let _ = std::fs::remove_file(&temp);
        if !target_won_race {
            return Err(failure);
        }
    }
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)
}

fn truncate_owned(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str("\n[truncated by ai-memory]");
}

#[cfg(test)]
mod tests {
    use ai_memory_store::Store;
    use axum::body::to_bytes;
    use tempfile::TempDir;

    use super::*;

    fn test_state(store: &Store, data_dir: &Path) -> WorkstreamState {
        WorkstreamState {
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            sanitizer: Sanitizer::default(),
            wiki: Wiki::new(data_dir, store.writer.clone())
                .unwrap()
                .with_store_reader(store.reader.clone()),
            data_dir: data_dir.to_path_buf(),
            home_dir: None,
            trusted_proxy_identity: false,
            finish_barrier: None,
        }
    }

    fn prepare_input(
        workspace_id: ai_memory_core::WorkspaceId,
        project_id: ai_memory_core::ProjectId,
        agent: AgentKind,
        owner: &str,
    ) -> PrepareWorkstreamRun {
        PrepareWorkstreamRun {
            workspace_id,
            project_id,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: "/repo".into(),
            agent,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: owner.into(),
        }
    }

    async fn seed_scope(store: &Store) -> (ai_memory_core::WorkspaceId, ai_memory_core::ProjectId) {
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let project_id = store
            .writer
            .get_or_create_project(workspace_id, "managed", None)
            .await
            .unwrap();
        (workspace_id, project_id)
    }

    #[tokio::test]
    async fn managed_run_promotion_refreshes_the_scope_manifest() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (project_id, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                ai_memory_core::repository_identity::RepositoryIdentity {
                    identity: "github.com/acme/api".into(),
                    source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
                },
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "api",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        state.wiki.backfill_scope_manifests().await.unwrap();
        state.wiki.commit_all("fixture baseline").unwrap();
        let reader = store
            .writer
            .create_user(
                ai_memory_core::NewUser {
                    username: "promotion-reader".into(),
                    name: None,
                    email: None,
                },
                [7; ai_memory_store::TOKEN_HASH_LEN],
            )
            .await
            .unwrap();
        store
            .writer
            .set_access_mode(project_id, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        store
            .writer
            .grant_memory(reader, project_id, ai_memory_store::GrantLevel::Read, None)
            .await
            .unwrap();
        let request = || PrepareManagedRunRequest {
            workspace: "default".into(),
            project: "acme-api".into(),
            cwd: "/repo".into(),
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            workstream: None,
            new_workstream: None,
            force_unlock: false,
            lease_owner: "launcher".into(),
            repository_identity: None,
        };
        let refused = prepare_run(
            State(state.clone()),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(ai_memory_core::AuthorizedViewer(reader))),
            Json(request()),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, project_id)
                .await
                .unwrap()
                .as_deref(),
            Some("api")
        );
        let manifest_path = temp
            .path()
            .join("wiki")
            .join(workspace_id.to_string())
            .join(project_id.to_string())
            .join("_meta.md");
        assert!(
            std::fs::read_to_string(&manifest_path)
                .unwrap()
                .contains("project: api")
        );
        let response = prepare_run(
            State(state.clone()),
            Some(Extension(AuthLevel::Root)),
            None,
            None,
            Json(request()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert!(prepared.manifest_warning.is_none());
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, project_id)
                .await
                .unwrap()
                .as_deref(),
            Some("acme-api")
        );
        let manifest = std::fs::read_to_string(
            temp.path()
                .join("wiki")
                .join(workspace_id.to_string())
                .join(project_id.to_string())
                .join("_meta.md"),
        )
        .unwrap();
        assert!(manifest.contains("project: acme-api"), "{manifest}");
    }

    #[tokio::test]
    async fn managed_run_rejects_mutually_exclusive_selection_before_promotion() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (project_id, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                ai_memory_core::repository_identity::RepositoryIdentity {
                    identity: "github.com/acme/api".into(),
                    source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
                },
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "api",
                None,
                None,
                None,
            )
            .await
            .unwrap();

        let response = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "acme-api".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: Some("existing".into()),
                new_workstream: Some("new".into()),
                force_unlock: false,
                lease_owner: "launcher".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, project_id)
                .await
                .unwrap()
                .as_deref(),
            Some("api")
        );
    }

    #[tokio::test]
    async fn managed_run_discloses_manifest_failure_after_committed_promotion() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (project_id, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                ai_memory_core::repository_identity::RepositoryIdentity {
                    identity: "github.com/acme/api".into(),
                    source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
                },
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "api",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        state.wiki.backfill_scope_manifests().await.unwrap();
        let manifest = temp
            .path()
            .join("wiki")
            .join(workspace_id.to_string())
            .join(project_id.to_string())
            .join("_meta.md");
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();

        let response = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "acme-api".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "launcher".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert!(
            prepared
                .manifest_warning
                .as_deref()
                .is_some_and(|warning| warning.contains("committed"))
        );
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, project_id)
                .await
                .unwrap()
                .as_deref(),
            Some("acme-api")
        );
        assert!(manifest.is_dir());
        std::fs::remove_dir(&manifest).unwrap();
        let restarted = Wiki::new(temp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        assert_eq!(restarted.backfill_scope_manifests().await.unwrap(), 1);
        assert!(
            std::fs::read_to_string(&manifest)
                .unwrap()
                .contains("project: acme-api")
        );
        assert_eq!(restarted.backfill_scope_manifests().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn managed_run_store_failure_after_promotion_preserves_manifest_warning() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (project_id, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                ai_memory_core::repository_identity::RepositoryIdentity {
                    identity: "github.com/acme/api".into(),
                    source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
                },
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "api",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        state.wiki.backfill_scope_manifests().await.unwrap();
        let manifest = temp
            .path()
            .join("wiki")
            .join(workspace_id.to_string())
            .join(project_id.to_string())
            .join("_meta.md");
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_managed_run_insert BEFORE INSERT ON managed_runs \
             BEGIN SELECT RAISE(ABORT, 'injected managed-run failure'); END;",
        )
        .unwrap();

        let response = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "acme-api".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "launcher".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("injected managed-run failure")),
            "{body}"
        );
        assert!(
            body["manifest_warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("promotion committed")),
            "{body}"
        );
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, project_id)
                .await
                .unwrap()
                .as_deref(),
            Some("acme-api")
        );
    }

    #[tokio::test]
    async fn managed_run_downstream_failure_after_promotion_preserves_manifest_warning() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (project_id, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                ai_memory_core::repository_identity::RepositoryIdentity {
                    identity: "github.com/acme/api".into(),
                    source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
                },
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "api",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        state.wiki.backfill_scope_manifests().await.unwrap();
        let manifest = temp
            .path()
            .join("wiki")
            .join(workspace_id.to_string())
            .join(project_id.to_string())
            .join("_meta.md");
        std::fs::remove_file(&manifest).unwrap();
        std::fs::create_dir(&manifest).unwrap();

        let response = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "acme-api".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: Some("missing".into()),
                new_workstream: None,
                force_unlock: false,
                lease_owner: "launcher".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("managed workstream 'missing'")),
            "{body}"
        );
        assert!(
            body["manifest_warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("promotion committed")),
            "{body}"
        );
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, project_id)
                .await
                .unwrap()
                .as_deref(),
            Some("acme-api")
        );
    }

    #[tokio::test]
    async fn workstream_provenance_finish_scrubs_before_raw_segment_and_search() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let mut state = test_state(&store, temp.path());
        state.sanitizer = Sanitizer::new(&ai_memory_core::SanitizeConfig {
            extra_patterns: vec!["private-correlation".into()],
            allowlist: Vec::new(),
        })
        .unwrap();
        let (workspace_id, project_id) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Codex,
                "launcher",
            ))
            .await
            .unwrap();
        let response = finish_run(
            State(state.clone()), None, None, None, None, AxumPath(run.run_id.to_string()),
            Json(FinishManagedRunRequest {
                native_session_id: Some("native-1".into()), source_cursor: None,
                events: vec![NewWorkstreamEvent {
                    event_id: "event-1".into(), agent: AgentKind::Codex,
                    native_session_id: "native-1".into(),
                    source_record_id: Some("private-correlation".into()),
                    kind: WorkstreamEventKind::ToolResult, role: Some("tool".into()),
                    content: "portable provenance sentinel".into(), occurred_at: None,
                    metadata: serde_json::json!({"tool_call_id": "call-1", "parent_id": "private-correlation", "dump": "private payload"}),
                }],
                complete: false, checkpoint: Default::default(), exit_code: None, losses: Vec::new(),
            }),
        ).await;
        assert_eq!(response.status(), StatusCode::OK);
        let segment_dir = temp
            .path()
            .join("raw/workstreams")
            .join(run.workstream_id.to_string())
            .join("segments");
        let path = std::fs::read_dir(segment_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(
            !raw.contains("private-correlation"),
            "raw segment leaked source id"
        );
        assert!(!raw.contains("private payload"));
        for q in ["", "portable"] {
            let response = search_events(
                State(state.clone()),
                None,
                None,
                AxumPath(run.workstream_id.to_string()),
                Query(EventQuery {
                    q: q.into(),
                    ..Default::default()
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let events: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(events[0]["source_record_id"], "[REDACTED:custom]");
            assert_eq!(events[0]["metadata"]["parent_id"], "[REDACTED:custom]");
            assert_eq!(events[0]["metadata"]["tool_call_id"], "call-1");
            assert!(events[0]["metadata"].get("dump").is_none());
        }
        let original: NewWorkstreamEvent = serde_json::from_str(raw.trim()).unwrap();
        let mut next = original.clone();
        next.event_id = "event-2".into();
        next.source_record_id = Some("record-2".into());
        let mut collision = original.clone();
        collision.kind = WorkstreamEventKind::Message;
        let import = |events| {
            finish_run(
                State(state.clone()),
                None,
                None,
                None,
                None,
                AxumPath(run.run_id.to_string()),
                Json(FinishManagedRunRequest {
                    native_session_id: Some("native-1".into()),
                    source_cursor: None,
                    events,
                    complete: false,
                    checkpoint: Default::default(),
                    exit_code: None,
                    losses: Vec::new(),
                }),
            )
        };
        assert_eq!(
            import(vec![next.clone(), collision]).await.status(),
            StatusCode::BAD_REQUEST
        );
        let read = || {
            store.reader.search_workstream_events(
                run.workstream_id,
                String::new(),
                10,
                state.sanitizer.clone(),
            )
        };
        assert_eq!(
            read().await.unwrap().len(),
            1,
            "failed segment must not become indexed history"
        );
        let recovered = import(vec![next, original]).await;
        assert_eq!(recovered.status(), StatusCode::OK);
        let body = to_bytes(recovered.into_body(), 64 * 1024).await.unwrap();
        let recovered: FinishManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(recovered.imported_events, 1);
        assert_eq!(recovered.latest_sequence, 2);
        assert_eq!(read().await.unwrap().len(), 2);
        let original: NewWorkstreamEvent = serde_json::from_str(raw.trim()).unwrap();
        let complete = |events| {
            finish_run(
                State(state.clone()),
                None,
                None,
                None,
                None,
                AxumPath(run.run_id.to_string()),
                Json(FinishManagedRunRequest {
                    native_session_id: Some("native-1".into()),
                    source_cursor: None,
                    events,
                    complete: true,
                    checkpoint: Default::default(),
                    exit_code: None,
                    losses: Vec::new(),
                }),
            )
        };
        assert_eq!(
            complete(vec![original.clone()]).await.status(),
            StatusCode::OK
        );
        let mut changed = original.clone();
        changed.source_record_id = Some("different-record".into());
        let mut late = original.clone();
        late.event_id = "late-new-event".into();
        assert_eq!(complete(vec![changed, late]).await.status(), StatusCode::OK);
        assert_eq!(complete(vec![original]).await.status(), StatusCode::OK);
        let hits = read().await.unwrap();
        assert_eq!(hits.len(), 3);
        assert!(
            hits.iter()
                .any(|hit| hit.kind == WorkstreamEventKind::Checkpoint)
        );
        assert!(!hits.iter().any(|hit| hit.event_id == "late-new-event"));
    }

    #[tokio::test]
    async fn workstream_provenance_client_cannot_use_server_event_ids() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Codex,
                "launcher",
            ))
            .await
            .unwrap();
        let import = |id: String, complete| {
            finish_run(
                State(state.clone()),
                None,
                None,
                None,
                None,
                AxumPath(run.run_id.to_string()),
                Json(FinishManagedRunRequest {
                    native_session_id: Some("native-1".into()),
                    source_cursor: None,
                    events: vec![NewWorkstreamEvent {
                        event_id: id,
                        agent: AgentKind::Codex,
                        native_session_id: "native-1".into(),
                        source_record_id: Some("record-1".into()),
                        kind: WorkstreamEventKind::Message,
                        role: Some("assistant".into()),
                        content: "legitimate native event".into(),
                        occurred_at: None,
                        metadata: serde_json::Value::Null,
                    }],
                    complete,
                    checkpoint: Default::default(),
                    exit_code: Some(0),
                    losses: if complete {
                        vec!["missing native record".into()]
                    } else {
                        Vec::new()
                    },
                }),
            )
        };
        // Legitimate native ids work before and after the attempted forgery.
        assert_eq!(
            import("native-event".into(), false).await.status(),
            StatusCode::OK
        );
        let segment_dir = temp
            .path()
            .join("raw/workstreams")
            .join(run.workstream_id.to_string())
            .join("segments");
        for id in [
            format!("managed-run:{}:checkpoint", run.run_id),
            format!("managed-run:{}:losses", run.run_id),
            "managed-run:other-run:annotation".into(),
        ] {
            assert_eq!(import(id, false).await.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                std::fs::read_dir(&segment_dir).unwrap().count(),
                1,
                "reserved ids must be refused before writing raw segments"
            );
        }
        assert_eq!(
            import("native-event".into(), true).await.status(),
            StatusCode::OK
        );
        let hits = store
            .reader
            .search_workstream_events(
                run.workstream_id,
                String::new(),
                10,
                state.sanitizer.clone(),
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().any(|hit| hit.event_id == "native-event"));
        for (suffix, kind) in [
            ("checkpoint", WorkstreamEventKind::Checkpoint),
            ("losses", WorkstreamEventKind::Annotation),
        ] {
            assert!(hits.iter().any(|hit| hit.event_id
                == format!("managed-run:{}:{suffix}", run.run_id)
                && hit.kind == kind
                && hit.source_record_id.is_none()));
        }
        assert_eq!(
            import("managed-run:late:checkpoint".into(), false)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    /// A credential straddling the 64 KiB event cap must be scrubbed before
    /// the cap runs (#1113): truncating first leaves an `sk-` prefix too short
    /// to match any pattern, persisting it verbatim. The segment file is the
    /// durable artifact written from the sanitized events.
    #[tokio::test]
    async fn finish_run_scrubs_event_content_before_the_byte_cap() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Codex,
                "launcher",
            ))
            .await
            .unwrap();
        // The key starts 18 bytes before the cap: capping first would keep
        // `sk-` plus 15 characters, one short of the 16 the pattern needs.
        // The space before the tail matters — without it the greedy key
        // pattern would swallow the tail into the same match and the
        // scrubbed text would never reach the cap.
        let credential = format!("sk-{}", "A".repeat(40));
        let content = format!(
            "{}{credential} {}",
            "x".repeat(MAX_EVENT_CONTENT_BYTES - 18),
            "y".repeat(2 * 1024),
        );
        let response = finish_run(
            State(state.clone()),
            None,
            None,
            None,
            None,
            AxumPath(run.run_id.to_string()),
            Json(FinishManagedRunRequest {
                native_session_id: Some("native-1".into()),
                source_cursor: None,
                events: vec![
                    NewWorkstreamEvent {
                        event_id: "straddling".into(),
                        agent: AgentKind::Codex,
                        native_session_id: "native-1".into(),
                        source_record_id: Some("record-1".into()),
                        kind: WorkstreamEventKind::Message,
                        role: Some("assistant".into()),
                        content,
                        occurred_at: None,
                        metadata: serde_json::Value::Null,
                    },
                    NewWorkstreamEvent {
                        event_id: "control".into(),
                        agent: AgentKind::Codex,
                        native_session_id: "native-1".into(),
                        source_record_id: Some("record-2".into()),
                        kind: WorkstreamEventKind::Message,
                        role: Some("assistant".into()),
                        content: "plain ledger note".into(),
                        occurred_at: None,
                        metadata: serde_json::Value::Null,
                    },
                ],
                complete: false,
                checkpoint: Default::default(),
                exit_code: None,
                losses: Vec::new(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let segment_dir = temp
            .path()
            .join("raw/workstreams")
            .join(run.workstream_id.to_string())
            .join("segments");
        let path = std::fs::read_dir(segment_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let raw = std::fs::read_to_string(path).unwrap();
        let events: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let straddling = events
            .iter()
            .find(|event| event["event_id"] == "straddling")
            .unwrap();
        let stored = straddling["content"].as_str().unwrap();
        assert!(
            stored.contains("[REDACTED:api_key]"),
            "straddling credential must be redacted before the cap: {stored:?}"
        );
        assert!(
            !stored.contains("sk-"),
            "cap-straddling credential prefix leaked: {stored:?}"
        );
        assert!(
            stored.ends_with("[truncated by ai-memory]"),
            "len {} tail {:?}",
            stored.len(),
            &stored[stored.len().saturating_sub(80)..]
        );
        assert!(
            stored.len() <= MAX_EVENT_CONTENT_BYTES + "\n[truncated by ai-memory]".len(),
            "stored content exceeds the cap plus its marker: {} bytes",
            stored.len()
        );
        let control = events
            .iter()
            .find(|event| event["event_id"] == "control")
            .unwrap();
        assert_eq!(control["content"], "plain ledger note");
        let hits = store
            .reader
            .search_workstream_events(
                run.workstream_id,
                String::new(),
                10,
                state.sanitizer.clone(),
            )
            .await
            .unwrap();
        assert!(
            hits.iter().all(|hit| !hit.content.contains("sk-")),
            "indexed content leaked the credential prefix"
        );
    }

    /// The server-generated checkpoint and losses events get the same
    /// scrub-before-cap ordering (#1113).
    #[tokio::test]
    async fn finish_run_scrubs_boundary_events_before_the_byte_cap() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Codex,
                "launcher",
            ))
            .await
            .unwrap();
        let credential = || format!("sk-{}", "A".repeat(40));
        // "Changed paths:\n" (15) + "- {filler}\n" (2 + 65498 + 1) + "- " (2)
        // places the key 18 bytes before the cap.
        let checkpoint = ai_memory_core::WorkstreamCheckpoint {
            changed_paths: vec!["x".repeat(65_498), credential(), "y".repeat(2 * 1024)],
            ..Default::default()
        };
        // "Transcript extraction losses:\n" (30) + "- {filler}\n" + "- "
        // places the key at the same offset.
        let losses = vec!["x".repeat(65_483), credential(), "y".repeat(2 * 1024)];
        let response = finish_run(
            State(state.clone()),
            None,
            None,
            None,
            None,
            AxumPath(run.run_id.to_string()),
            Json(FinishManagedRunRequest {
                native_session_id: Some("native-1".into()),
                source_cursor: None,
                events: Vec::new(),
                complete: true,
                checkpoint,
                exit_code: Some(0),
                losses,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let segment_dir = temp
            .path()
            .join("raw/workstreams")
            .join(run.workstream_id.to_string())
            .join("segments");
        let path = std::fs::read_dir(segment_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let raw = std::fs::read_to_string(path).unwrap();
        for suffix in ["checkpoint", "losses"] {
            let event: serde_json::Value = raw
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .find(|event: &serde_json::Value| {
                    event["event_id"] == format!("managed-run:{}:{suffix}", run.run_id)
                })
                .unwrap();
            let stored = event["content"].as_str().unwrap();
            assert!(
                stored.contains("[REDACTED:api_key]"),
                "{suffix} must be redacted before the cap: {stored:?}"
            );
            assert!(
                !stored.contains("sk-"),
                "{suffix} leaked the cap-straddling credential prefix"
            );
            assert!(stored.ends_with("[truncated by ai-memory]"));
            assert!(
                stored.len() <= MAX_EVENT_CONTENT_BYTES + "\n[truncated by ai-memory]".len(),
                "{suffix} exceeds the cap plus its marker: {} bytes",
                stored.len()
            );
        }
    }

    /// The launcher reads whether the run's child linked a session from the
    /// run status, so the route must carry it.
    #[tokio::test]
    async fn run_status_reports_a_session_linked_during_the_run() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let prepared = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Codex,
                "launcher",
            ))
            .await
            .unwrap();
        let status = async || {
            let response = run_status(
                State(state.clone()),
                None,
                None,
                AxumPath(prepared.run_id.to_string()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            serde_json::from_slice::<ManagedRunStatus>(&body).unwrap()
        };
        assert!(!status().await.native_session_linked);
        assert!(
            store
                .writer
                .link_managed_run_session(prepared.run_id, AgentKind::Codex, "native-1")
                .await
                .unwrap()
        );
        let linked = status().await;
        assert!(linked.native_session_linked);
        assert_eq!(linked.native_session_id.as_deref(), Some("native-1"));
    }

    #[tokio::test]
    async fn rename_endpoint_is_scoped_and_reports_selector_misuse() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let prepared = store
            .writer
            .prepare_workstream_run(PrepareWorkstreamRun {
                selection: WorkstreamSelection::New("typo-nmae".into()),
                ..prepare_input(workspace_id, project_id, AgentKind::OpenCode, "launcher")
            })
            .await
            .unwrap();

        fn request(from: Option<&str>, to: &str) -> RenameManagedWorkstreamRequest {
            RenameManagedWorkstreamRequest {
                workspace: "default".into(),
                project: "managed".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                from: from.map(str::to_owned),
                workstream_id: None,
                to: to.into(),
            }
        }

        let ok = rename_workstream(
            State(state.clone()),
            None,
            None,
            Json(request(Some("typo-nmae"), "refactor-db")),
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let body = to_bytes(ok.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["from"], "typo-nmae");
        assert_eq!(json["to"], "refactor-db");
        assert_eq!(json["workstream_id"], prepared.workstream_id.to_string());
        // The rename response is metadata like its sibling read: no checkout
        // path, and no native session id.
        let encoded = String::from_utf8(body.to_vec()).unwrap();
        assert!(!encoded.contains("/repo"));

        // A name that exists, but in another worktree, must not be reachable.
        let mut wrong_worktree = request(Some("refactor-db"), "stolen");
        wrong_worktree.worktree_fingerprint = "other-worktree".into();
        let response =
            rename_workstream(State(state.clone()), None, None, Json(wrong_worktree)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Unknown scopes stay 404 rather than being created by the write.
        let mut missing = request(Some("refactor-db"), "stolen");
        missing.workspace = "missing".into();
        let response = rename_workstream(State(state.clone()), None, None, Json(missing)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // Neither selector, and both selectors, are caller errors the server
        // rejects on its own rather than trusting clap to have done it.
        let neither =
            rename_workstream(State(state.clone()), None, None, Json(request(None, "x"))).await;
        assert_eq!(neither.status(), StatusCode::BAD_REQUEST);
        let mut both = request(Some("refactor-db"), "x");
        both.workstream_id = Some(prepared.workstream_id);
        let both = rename_workstream(State(state.clone()), None, None, Json(both)).await;
        assert_eq!(both.status(), StatusCode::BAD_REQUEST);

        // An invalid destination name is a 400, not a 500.
        let invalid = rename_workstream(
            State(state.clone()),
            None,
            None,
            Json(request(Some("refactor-db"), "a/b")),
        )
        .await;
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        // A collision is a 409 naming the taken name.
        store
            .writer
            .prepare_workstream_run(PrepareWorkstreamRun {
                selection: WorkstreamSelection::New("taken".into()),
                ..prepare_input(workspace_id, project_id, AgentKind::OpenCode, "other")
            })
            .await
            .unwrap();
        let conflict = rename_workstream(
            State(state),
            None,
            None,
            Json(request(Some("refactor-db"), "taken")),
        )
        .await;
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn recent_workstream_endpoint_is_checkout_scoped_and_read_only() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let prepared = store
            .writer
            .prepare_workstream_run(PrepareWorkstreamRun {
                selection: WorkstreamSelection::New("decide-after-death".into()),
                ..prepare_input(workspace_id, project_id, AgentKind::OpenCode, "launcher")
            })
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: prepared.run_id,
                    native_session_id: Some("private-native-id".into()),
                    source_cursor: None,
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();

        let response = list_recent_workstreams(
            State(state.clone()),
            None,
            None,
            Json(ListManagedWorkstreamsRequest {
                workspace: "default".into(),
                project: "managed".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                limit: 20,
                offset: 0,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json[0]["name"], "decide-after-death");
        assert_eq!(
            json[0]["linked_harnesses"],
            serde_json::json!(["open-code"])
        );
        assert_eq!(json[0]["current"], true);
        let encoded = String::from_utf8(body.to_vec()).unwrap();
        assert!(!encoded.contains("private-native-id"));
        assert!(!encoded.contains("/repo"));

        let other_checkout = list_recent_workstreams(
            State(state.clone()),
            None,
            None,
            Json(ListManagedWorkstreamsRequest {
                workspace: "default".into(),
                project: "managed".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "other-worktree".into(),
                limit: 20,
                offset: 0,
            }),
        )
        .await;
        let body = to_bytes(other_checkout.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!([])
        );

        let missing = list_recent_workstreams(
            State(state),
            None,
            None,
            Json(ListManagedWorkstreamsRequest {
                workspace: "missing".into(),
                project: "managed".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                limit: 20,
                offset: 0,
            }),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn recent_workstream_pages_reach_older_rows_without_crossing_checkout_or_scope() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        for index in 0..105 {
            store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
                    selection: WorkstreamSelection::New(format!("local-{index:03}")),
                    ..prepare_input(workspace_id, project_id, AgentKind::Codex, "launcher")
                })
                .await
                .unwrap();
        }
        let other_workspace = store.writer.get_or_create_workspace("other").await.unwrap();
        let sibling_project = store
            .writer
            .get_or_create_project(workspace_id, "sibling", None)
            .await
            .unwrap();
        let other_project = store
            .writer
            .get_or_create_project(other_workspace, "managed", None)
            .await
            .unwrap();
        for (workspace, project, repo, worktree, name) in [
            (
                workspace_id,
                sibling_project,
                "repo",
                "worktree",
                "foreign-project",
            ),
            (
                workspace_id,
                project_id,
                "other-repo",
                "worktree",
                "foreign-repo",
            ),
            (
                workspace_id,
                project_id,
                "repo",
                "other-worktree",
                "foreign-worktree",
            ),
            (
                other_workspace,
                other_project,
                "repo",
                "worktree",
                "foreign-workspace",
            ),
        ] {
            store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
                    selection: WorkstreamSelection::New(name.into()),
                    repo_fingerprint: repo.into(),
                    worktree_fingerprint: worktree.into(),
                    ..prepare_input(workspace, project, AgentKind::Codex, "foreign")
                })
                .await
                .unwrap();
        }
        let mut rows = Vec::<ManagedWorkstreamSummary>::new();
        for (offset, expected) in [(0, 100), (100, 5), (105, 0), (usize::MAX, 0)] {
            let response = list_recent_workstreams(
                State(state.clone()),
                None,
                None,
                Json(ListManagedWorkstreamsRequest {
                    workspace: "default".into(),
                    project: "managed".into(),
                    repo_fingerprint: "repo".into(),
                    worktree_fingerprint: "worktree".into(),
                    limit: 100,
                    offset,
                }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
            let page: Vec<ManagedWorkstreamSummary> = serde_json::from_slice(&body).unwrap();
            assert_eq!(page.len(), expected);
            assert!(page.iter().all(|row| row.name.starts_with("local-")));
            rows.extend(page);
        }
        let ids = rows
            .iter()
            .map(|row| row.workstream_id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 105);
        assert_eq!(rows[0].name, "local-104");
        assert!(rows[0].current);
        assert_eq!(rows.iter().filter(|row| row.current).count(), 1);
        assert!(rows.iter().any(|row| row.name == "local-000"));
    }

    #[tokio::test]
    async fn automatic_prepare_returns_the_established_available_harness() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let claude = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::ClaudeCode,
                "claude",
            ))
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: claude.run_id,
                    native_session_id: Some("claude-current".into()),
                    source_cursor: Some("cursor".into()),
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();

        let response = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Codex,
                automatic_harness: true,
                available_agents: vec![AgentKind::Codex, AgentKind::ClaudeCode],
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "automatic".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(AgentKind::ClaudeCode));
        assert_eq!(
            prepared.native_session_id.as_deref(),
            Some("claude-current")
        );
        assert!(!prepared.may_adopt_existing_session);
    }

    #[tokio::test]
    async fn kimi_code_is_accepted_as_an_explicit_and_automatic_harness() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());

        let explicit = prepare_run(
            State(state.clone()),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::KimiCode,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "explicit".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(explicit.status(), StatusCode::OK);
        let body = to_bytes(explicit.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(AgentKind::KimiCode));
        // Close the explicit run so the automatic prepare can take the lease.
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: prepared.run_id,
                    native_session_id: Some("session_abc".into()),
                    source_cursor: None,
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();

        let automatic = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::KimiCode,
                automatic_harness: true,
                available_agents: vec![AgentKind::KimiCode, AgentKind::ClaudeCode],
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "automatic".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(automatic.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn prepare_stamps_the_operator_bucket_used_by_session_start_recovery() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        store
            .writer
            .create_human_user(
                ai_memory_core::NewUser {
                    username: "alice".into(),
                    name: None,
                    email: None,
                },
                ai_memory_core::UserRole::User,
                None,
                false,
            )
            .await
            .unwrap();

        let response = prepare_run(
            State(state.clone()),
            None,
            Some(Extension(ai_memory_core::ActorContext {
                user: Some("alice".into()),
                ..ai_memory_core::ActorContext::default()
            })),
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed-owner".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "alice-launcher".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        let (workspace_id, project_id) = store
            .reader
            .managed_run_scope(prepared.run_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            store
                .writer
                .link_or_adopt_managed_run_session(ai_memory_store::LinkOrAdoptManagedRunSession {
                    supplied_run_id: prepared.run_id,
                    workspace_id,
                    project_id,
                    cwd: "/repo".into(),
                    agent: AgentKind::Codex,
                    native_session_id: "native-bob".into(),
                    owner_user: Some(
                        ai_memory_core::IdentityKey::User("bob".into()).storage_key(),
                    ),
                })
                .await
                .unwrap(),
            ai_memory_store::ManagedRunSessionLink::Refused
        );
        assert_eq!(
            store
                .writer
                .link_or_adopt_managed_run_session(ai_memory_store::LinkOrAdoptManagedRunSession {
                    supplied_run_id: prepared.run_id,
                    workspace_id,
                    project_id,
                    cwd: "/repo".into(),
                    agent: AgentKind::Codex,
                    native_session_id: "native-alice".into(),
                    owner_user: Some(
                        ai_memory_core::IdentityKey::User("alice".into()).storage_key(),
                    ),
                })
                .await
                .unwrap(),
            ai_memory_store::ManagedRunSessionLink::Exact(prepared.run_id)
        );
    }

    /// #1075: every route that renews, rebinds, accepts for or releases a run
    /// applies finish's owner rule, so none is a side door around it. Another
    /// operator — even one who may write the project, and root acting under a
    /// different identity — is refused on all four, and the run stays intact;
    /// the owner is admitted (legitimate control).
    #[tokio::test]
    async fn run_mutation_routes_refuse_another_operator() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        for name in ["alice", "bob"] {
            store
                .writer
                .create_human_user(
                    ai_memory_core::NewUser {
                        username: name.into(),
                        name: None,
                        email: None,
                    },
                    ai_memory_core::UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap();
        }
        let actor = |name: &str| {
            Some(Extension(ai_memory_core::ActorContext {
                user: Some(name.into()),
                ..ai_memory_core::ActorContext::default()
            }))
        };
        let user = || Some(Extension(AuthLevel::User));
        let prepared = prepare_run(
            State(state.clone()),
            None,
            actor("alice"),
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed-owner-routes".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Grok,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "alice:1".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(prepared.status(), StatusCode::OK);
        let body = to_bytes(prepared.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        let run = || AxumPath(prepared.run_id.to_string());
        let link = || {
            Json(LinkManagedRunRequest {
                native_session_id: "native-link".into(),
            })
        };

        for (who, level) in [("bob", user()), ("admin", Some(Extension(AuthLevel::Root)))] {
            let statuses = [
                heartbeat_run(State(state.clone()), level, None, actor(who), None, run())
                    .await
                    .status(),
                link_run(
                    State(state.clone()),
                    level,
                    None,
                    actor(who),
                    None,
                    run(),
                    link(),
                )
                .await
                .status(),
                accept_run_context(State(state.clone()), level, None, actor(who), None, run())
                    .await
                    .status(),
                cancel_run(State(state.clone()), level, None, actor(who), None, run())
                    .await
                    .status(),
            ];
            assert_eq!(statuses, [StatusCode::FORBIDDEN; 4], "{who}");
        }
        let status = store
            .reader
            .managed_run_status(prepared.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.state, "active",
            "refused callers must not end the run"
        );
        assert!(!status.context_delivered);
        assert_eq!(status.native_session_id, None);

        let statuses = [
            heartbeat_run(
                State(state.clone()),
                user(),
                None,
                actor("alice"),
                None,
                run(),
            )
            .await
            .status(),
            link_run(
                State(state.clone()),
                user(),
                None,
                actor("alice"),
                None,
                run(),
                link(),
            )
            .await
            .status(),
            accept_run_context(
                State(state.clone()),
                user(),
                None,
                actor("alice"),
                None,
                run(),
            )
            .await
            .status(),
            cancel_run(
                State(state.clone()),
                user(),
                None,
                actor("alice"),
                None,
                run(),
            )
            .await
            .status(),
        ];
        assert_eq!(statuses, [StatusCode::NO_CONTENT; 4]);
    }

    #[tokio::test]
    async fn force_unlock_cannot_evict_another_operator() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        for name in ["alice", "bob"] {
            store
                .writer
                .create_human_user(
                    ai_memory_core::NewUser {
                        username: name.into(),
                        name: None,
                        email: None,
                    },
                    ai_memory_core::UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap();
        }
        let actor = |name: &str| {
            Some(Extension(ai_memory_core::ActorContext {
                user: Some(name.into()),
                ..ai_memory_core::ActorContext::default()
            }))
        };
        let request = |force_unlock: bool, lease_owner: &str| PrepareManagedRunRequest {
            workspace: "default".into(),
            project: "managed-force-unlock".into(),
            cwd: "/repo".into(),
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            workstream: None,
            new_workstream: None,
            force_unlock,
            lease_owner: lease_owner.into(),
            repository_identity: None,
        };

        let first = prepare_run(
            State(state.clone()),
            None,
            actor("alice"),
            None,
            Json(request(false, "alice:1")),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let body = to_bytes(first.into_body(), 64 * 1024).await.unwrap();
        let first: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();

        let refused = prepare_run(
            State(state.clone()),
            None,
            actor("bob"),
            None,
            Json(request(true, "bob:2")),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        assert!(
            store
                .writer
                .heartbeat_managed_run(first.run_id)
                .await
                .unwrap(),
            "the refused caller must not disturb Alice's lease"
        );

        let replacement = prepare_run(
            State(state),
            None,
            actor("alice"),
            None,
            Json(request(true, "alice:3")),
        )
        .await;
        assert_eq!(replacement.status(), StatusCode::OK);
        let body = to_bytes(replacement.into_body(), 64 * 1024).await.unwrap();
        let replacement: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_ne!(replacement.run_id, first.run_id);
        assert!(
            !store
                .writer
                .heartbeat_managed_run(first.run_id)
                .await
                .unwrap()
        );
        assert!(
            store
                .writer
                .heartbeat_managed_run(replacement.run_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn kiro_is_accepted_as_an_explicit_and_automatic_harness() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());

        let explicit = prepare_run(
            State(state.clone()),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::KiroCli,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "explicit".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(explicit.status(), StatusCode::OK);
        let body = to_bytes(explicit.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(AgentKind::KiroCli));
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: prepared.run_id,
                    native_session_id: Some("7c1d5698-204a-4c0f-ae9c-43db7fc4e41d".into()),
                    source_cursor: None,
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();

        let automatic = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::KiroCli,
                automatic_harness: true,
                available_agents: vec![AgentKind::KiroCli],
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "automatic".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(automatic.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn command_code_is_accepted_as_an_explicit_and_automatic_harness() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());

        let explicit = prepare_run(
            State(state.clone()),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::CommandCode,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "explicit".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(explicit.status(), StatusCode::OK);
        let body = to_bytes(explicit.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(AgentKind::CommandCode));
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: prepared.run_id,
                    native_session_id: Some("2cce5126-f57d-4ddd-8f66-e5bb409f60db".into()),
                    source_cursor: None,
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();

        let automatic = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::CommandCode,
                automatic_harness: true,
                available_agents: vec![AgentKind::CommandCode, AgentKind::ClaudeCode],
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "automatic".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(automatic.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn grok_is_accepted_explicitly_but_not_in_the_automatic_pool() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());

        let explicit = prepare_run(
            State(state.clone()),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Grok,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "explicit".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(explicit.status(), StatusCode::OK);
        let body = to_bytes(explicit.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(AgentKind::Grok));
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: prepared.run_id,
                    native_session_id: Some("019f-session".into()),
                    source_cursor: None,
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();

        let automatic = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::Grok,
                automatic_harness: true,
                available_agents: vec![AgentKind::Grok],
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "automatic".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(automatic.status(), StatusCode::BAD_REQUEST);
    }

    /// The launcher offers `agy` explicitly, so the server has to accept it —
    /// a missing arm here rejects the launch with "requires a supported
    /// command-line harness" after the user already picked the harness.
    #[tokio::test]
    async fn antigravity_is_accepted_explicitly_but_not_in_the_automatic_pool() {
        assert_explicit_only_managed_harness(
            AgentKind::AntigravityCli,
            "a0d5ac62-2501-4780-b783-76d159c56cb3",
        )
        .await;
    }

    #[tokio::test]
    async fn copilot_cli_is_accepted_explicitly_but_not_in_the_automatic_pool() {
        assert_explicit_only_managed_harness(
            AgentKind::CopilotCli,
            "318db77d-e19b-4750-82df-192a6b931437",
        )
        .await;
    }

    async fn assert_explicit_only_managed_harness(agent: AgentKind, native_session_id: &str) {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());

        let explicit = prepare_run(
            State(state.clone()),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "explicit".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(explicit.status(), StatusCode::OK);
        let body = to_bytes(explicit.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(prepared.resolved_agent, Some(agent));
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: prepared.run_id,
                    native_session_id: Some(native_session_id.into()),
                    source_cursor: None,
                    events: Vec::new(),
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();

        let automatic = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "managed".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent,
                automatic_harness: true,
                available_agents: vec![agent],
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "automatic".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(automatic.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn grok_run_context_uses_the_direct_delivery_gate() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let input = prepare_input(workspace_id, project_id, AgentKind::Grok, "launcher");
        let prepared = store.writer.prepare_workstream_run(input).await.unwrap();

        let context = run_context(
            State(state.clone()),
            None,
            None,
            AxumPath(prepared.run_id.to_string()),
        )
        .await;
        assert_eq!(context.status(), StatusCode::OK);

        let accept = accept_run_context(
            State(state.clone()),
            None,
            None,
            None,
            None,
            AxumPath(prepared.run_id.to_string()),
        )
        .await;
        assert_eq!(accept.status(), StatusCode::NO_CONTENT);

        // Delivered context is not re-rendered.
        let redelivered = run_context(
            State(state),
            None,
            None,
            AxumPath(prepared.run_id.to_string()),
        )
        .await
        .into_body();
        let body = to_bytes(redelivered, 64 * 1024).await.unwrap();
        let response: ai_memory_core::ManagedRunContextResponse =
            serde_json::from_slice(&body).unwrap();
        assert!(response.context.is_none());
    }

    #[tokio::test]
    async fn cancel_endpoint_is_idempotent_and_releases_the_workstream() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let input = prepare_input(workspace_id, project_id, AgentKind::Codex, "launcher");
        let prepared = store
            .writer
            .prepare_workstream_run(input.clone())
            .await
            .unwrap();

        for _ in 0..2 {
            let response = cancel_run(
                State(state.clone()),
                None,
                None,
                None,
                None,
                AxumPath(prepared.run_id.to_string()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        store.writer.prepare_workstream_run(input).await.unwrap();
    }

    #[tokio::test]
    async fn crush_context_fetch_is_repeatable_until_explicit_accept() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (workspace_id, project_id) = seed_scope(&store).await;
        let codex = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Codex,
                "codex",
            ))
            .await
            .unwrap();
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: ai_memory_core::Sanitizer::default(),
                    run_id: codex.run_id,
                    native_session_id: Some("codex-native".into()),
                    source_cursor: None,
                    events: vec![NewWorkstreamEvent {
                        event_id: "codex:assistant:1".into(),
                        agent: AgentKind::Codex,
                        native_session_id: "codex-native".into(),
                        source_record_id: None,
                        kind: WorkstreamEventKind::Message,
                        role: Some("assistant".into()),
                        content: "AMWS-CODEX-SENTINEL".into(),
                        occurred_at: None,
                        metadata: serde_json::json!({}),
                    }],
                    complete: true,
                    segment_path: None,
                    exit_code: Some(0),
                },
            )
            .await
            .unwrap();
        let crush = store
            .writer
            .prepare_workstream_run(prepare_input(
                workspace_id,
                project_id,
                AgentKind::Crush,
                "crush",
            ))
            .await
            .unwrap();

        let fetch = || {
            run_context(
                State(state.clone()),
                None,
                None,
                AxumPath(crush.run_id.to_string()),
            )
        };
        for _ in 0..2 {
            let response = fetch().await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let packet: ManagedRunContextResponse = serde_json::from_slice(&body).unwrap();
            assert!(
                packet
                    .context
                    .as_deref()
                    .is_some_and(|context| context.contains("AMWS-CODEX-SENTINEL"))
            );
            assert!(
                !store
                    .reader
                    .managed_run_status(crush.run_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .context_delivered
            );
        }

        let accepted = accept_run_context(
            State(state.clone()),
            None,
            None,
            None,
            None,
            AxumPath(crush.run_id.to_string()),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
        assert!(
            store
                .reader
                .managed_run_status(crush.run_id)
                .await
                .unwrap()
                .unwrap()
                .context_delivered
        );

        let response = fetch().await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let packet: ManagedRunContextResponse = serde_json::from_slice(&body).unwrap();
        assert!(packet.context.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn segment_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().unwrap();
        let workstream_id = WorkstreamId::new();
        let run_id = ManagedRunId::new();
        let relative = write_segment(
            temp.path(),
            workstream_id,
            run_id,
            &[NewWorkstreamEvent {
                event_id: "event-1".into(),
                agent: AgentKind::Codex,
                native_session_id: "session-1".into(),
                source_record_id: None,
                kind: WorkstreamEventKind::Message,
                role: None,
                content: "sensitive transcript".into(),
                occurred_at: None,
                metadata: serde_json::json!({}),
            }],
        )
        .unwrap();
        let segment_dir = temp
            .path()
            .join("raw/workstreams")
            .join(workstream_id.to_string())
            .join("segments");
        for path in [
            temp.path().join("raw"),
            temp.path().join("raw/workstreams"),
            segment_dir.clone(),
        ] {
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                path.display()
            );
        }
        assert_eq!(
            std::fs::metadata(temp.path().join("raw").join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// The run and workstream routes take an id in the URL, so they never
    /// passed through scope resolution (#708): anyone holding a run id could
    /// read its status, heartbeat or cancel it, and anyone holding a
    /// workstream id could read that repository's event history. With
    /// authorization on, each now answers only someone who may reach the
    /// run's repository at the level the operation needs.
    #[tokio::test]
    async fn run_and_workstream_ids_only_answer_someone_who_may_reach_the_repository() {
        use ai_memory_core::{AuthorizedViewer, NewUser, UserRole};
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        // Grants only decide anything in a restricted project.
        store
            .writer
            .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();

        let prepared = prepare_run(
            State(state.clone()),
            None,
            None,
            None,
            Json(PrepareManagedRunRequest {
                workspace: "default".into(),
                project: "alice-client-work".into(),
                cwd: "/repo".into(),
                repo_fingerprint: "repo".into(),
                worktree_fingerprint: "worktree".into(),
                agent: AgentKind::ClaudeCode,
                automatic_harness: false,
                available_agents: Vec::new(),
                workstream: None,
                new_workstream: None,
                force_unlock: false,
                lease_owner: "alice".into(),
                repository_identity: None,
            }),
        )
        .await;
        assert_eq!(prepared.status(), StatusCode::OK);
        let body = to_bytes(prepared.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        let (_, repository) = store
            .reader
            .managed_run_scope(prepared.run_id)
            .await
            .unwrap()
            .expect("the run was just prepared");
        store
            .writer
            .finish_workstream_run(
                ai_memory_store::ManagedRunAuthority::from_auth(
                    AuthLevel::Anonymous,
                    None,
                    None,
                    &ai_memory_core::ActorContext::anonymous(),
                    false,
                ),
                FinishWorkstreamRun {
                    sanitizer: state.sanitizer.clone(),
                    run_id: prepared.run_id,
                    native_session_id: Some("claude-native".into()),
                    source_cursor: None,
                    events: vec![NewWorkstreamEvent {
                        event_id: "scope-provenance".into(),
                        agent: AgentKind::ClaudeCode,
                        native_session_id: "claude-native".into(),
                        source_record_id: Some("record-1".into()),
                        kind: WorkstreamEventKind::ToolResult,
                        role: Some("tool".into()),
                        content: "scope provenance sentinel".into(),
                        occurred_at: None,
                        metadata: serde_json::json!({"tool_use_id": "call-1"}),
                    }],
                    complete: false,
                    segment_path: None,
                    exit_code: None,
                },
            )
            .await
            .unwrap();

        let human = |name: &'static str| {
            let writer = store.writer.clone();
            async move {
                writer
                    .create_human_user(
                        NewUser {
                            username: name.into(),
                            name: None,
                            email: None,
                        },
                        UserRole::User,
                        None,
                        false,
                    )
                    .await
                    .unwrap()
            }
        };
        let alice = human("alice").await;
        let bob = human("bob").await;
        let carol = human("carol").await;
        store
            .writer
            .grant_memory(alice, repository, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
        // Carol may read the repository but not act on its runs.
        store
            .writer
            .grant_memory(carol, repository, ai_memory_store::GrantLevel::Read, None)
            .await
            .unwrap();
        let as_viewer = |user| Some(Extension(AuthorizedViewer(user)));
        let as_user = || Some(Extension(AuthLevel::User));
        let run = || AxumPath(prepared.run_id.to_string());

        // Status is a read: bob (nothing) is refused, carol (reader) is not.
        let refused = run_status(State(state.clone()), None, as_viewer(bob), run()).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let refusal = to_bytes(refused.into_body(), 64 * 1024).await.unwrap();
        assert!(
            String::from_utf8_lossy(&refusal).contains("not authorized for alice-client-work"),
            "{}",
            String::from_utf8_lossy(&refusal)
        );
        assert_eq!(
            run_status(State(state.clone()), None, as_viewer(carol), run())
                .await
                .status(),
            StatusCode::OK
        );

        // Acting on the run needs write: carol's read is not enough.
        for viewer in [bob, carol] {
            assert_eq!(
                heartbeat_run(
                    State(state.clone()),
                    as_user(),
                    as_viewer(viewer),
                    None,
                    None,
                    run()
                )
                .await
                .status(),
                StatusCode::FORBIDDEN,
                "heartbeat"
            );
            assert_eq!(
                cancel_run(
                    State(state.clone()),
                    as_user(),
                    as_viewer(viewer),
                    None,
                    None,
                    run()
                )
                .await
                .status(),
                StatusCode::FORBIDDEN,
                "cancel"
            );
        }
        assert_eq!(
            heartbeat_run(
                State(state.clone()),
                as_user(),
                as_viewer(alice),
                None,
                None,
                run()
            )
            .await
            .status(),
            StatusCode::NO_CONTENT,
            "alice's own run keeps working"
        );

        // The event history behind a workstream id.
        let events = |viewer| {
            search_events(
                State(state.clone()),
                None,
                viewer,
                AxumPath(prepared.workstream_id.to_string()),
                Query(EventQuery::default()),
            )
        };
        let refused = events(as_viewer(bob)).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(refused.into_body(), 64 * 1024).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("record-1"));
        let allowed = events(as_viewer(carol)).await;
        assert_eq!(allowed.status(), StatusCode::OK);
        let body = to_bytes(allowed.into_body(), 64 * 1024).await.unwrap();
        let provenance: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(provenance[0]["source_record_id"], "record-1");
        assert_eq!(provenance[0]["metadata"]["tool_use_id"], "call-1");

        // No viewer — an install with no database users, or root — is unchanged.
        assert_eq!(
            run_status(State(state.clone()), None, None, run())
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(events(None).await.status(), StatusCode::OK);

        // An id that matches nothing is still "not found", not a refusal.
        let unknown = run_status(
            State(state.clone()),
            None,
            as_viewer(bob),
            AxumPath(ManagedRunId::new().to_string()),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    async fn finish_sql_snapshot(store: &Store) -> Vec<Vec<String>> {
        store
            .reader
            .with_conn(|conn| {
                let mut result = Vec::new();
                for table in [
                    "workspaces",
                    "projects",
                    "managed_runs",
                    "workstreams",
                    "workstream_events",
                    "workstream_native_sessions",
                    "workstream_events_fts_data",
                    "workstream_events_fts_idx",
                    "workstream_events_fts_docsize",
                    "sessions",
                ] {
                    let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))?;
                    let columns = stmt.column_count();
                    result.extend(
                        stmt.query_map([], |row| {
                            (0..columns)
                                .map(|i| row.get_ref(i).map(|v| format!("{v:?}")))
                                .collect()
                        })?
                        .collect::<Result<Vec<Vec<String>>, _>>()?,
                    );
                }
                Ok(result)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn owner_finish_revocation_between_preflight_and_both_writer_paths() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let (ws, project) = seed_scope(&store).await;
        let alice = store
            .writer
            .create_human_user(
                ai_memory_core::NewUser {
                    username: "alice".into(),
                    name: None,
                    email: None,
                },
                ai_memory_core::UserRole::User,
                None,
                false,
            )
            .await
            .unwrap();
        store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        let actor = ai_memory_core::ActorContext {
            user: Some("alice".into()),
            ..ai_memory_core::ActorContext::anonymous()
        };
        for finished in [false, true] {
            store
                .writer
                .grant_memory(alice, project, ai_memory_store::GrantLevel::Write, None)
                .await
                .unwrap();
            let run = store
                .writer
                .prepare_workstream_run_owned(
                    PrepareWorkstreamRun {
                        selection: WorkstreamSelection::New(format!("revoke-{finished}")),
                        ..prepare_input(ws, project, AgentKind::Codex, "label")
                    },
                    Some("user:alice".into()),
                )
                .await
                .unwrap();
            if finished {
                store
                    .writer
                    .finish_workstream_run(
                        ai_memory_store::ManagedRunAuthority::from_auth(
                            AuthLevel::User,
                            Some(ai_memory_core::AuthorizedViewer(alice)),
                            Some(alice),
                            &actor,
                            false,
                        ),
                        FinishWorkstreamRun {
                            sanitizer: ai_memory_core::Sanitizer::default(),
                            run_id: run.run_id,
                            native_session_id: None,
                            source_cursor: None,
                            events: Vec::new(),
                            complete: true,
                            segment_path: None,
                            exit_code: None,
                        },
                    )
                    .await
                    .unwrap();
            }
            let barrier =
                std::sync::Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
            let mut state = test_state(&store, temp.path());
            state.finish_barrier = Some(barrier.clone());
            let request = serde_json::from_value(serde_json::json!({
                "native_session_id": "native-revoke", "source_cursor": "cursor-revoke", "checkpoint": {},
                "complete": true, "events": [{"event_id": "revoked-event", "agent": "codex",
                    "native_session_id": "native-revoke", "kind": "message", "role": "assistant",
                    "content": "revocation race evidence", "metadata": {}}]
            })).unwrap();
            let (response, before) = tokio::join!(
                finish_run(
                    State(state),
                    Some(Extension(AuthLevel::User)),
                    Some(Extension(ai_memory_core::AuthorizedViewer(alice))),
                    Some(Extension(actor.clone())),
                    Some(Extension(alice)),
                    AxumPath(run.run_id.to_string()),
                    Json(request)
                ),
                async {
                    barrier.0.notified().await;
                    assert!(
                        store
                            .writer
                            .revoke_memory(alice, project, None)
                            .await
                            .unwrap()
                    );
                    let before = finish_sql_snapshot(&store).await;
                    barrier.1.notify_one();
                    before
                }
            );
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "both writer paths must recheck Write after preflight"
            );
            assert_eq!(
                finish_sql_snapshot(&store).await,
                before,
                "writer refusal must change no SQL run, event, index, link or cursor"
            );
            let raw = temp
                .path()
                .join("raw/workstreams")
                .join(run.workstream_id.to_string());
            assert_eq!(
                raw.exists(),
                !finished,
                "normal revocation race can leave an unindexed raw segment; retries write none"
            );
            store
                .writer
                .grant_memory(alice, project, ai_memory_store::GrantLevel::Write, None)
                .await
                .unwrap();
            let response = finish_run(
                State(test_state(&store, temp.path())),
                Some(Extension(AuthLevel::User)),
                Some(Extension(ai_memory_core::AuthorizedViewer(alice))),
                Some(Extension(actor.clone())),
                Some(Extension(alice)),
                AxumPath(run.run_id.to_string()),
                Json(
                    serde_json::from_value(serde_json::json!({
                        "checkpoint": {}, "events": [], "complete": true
                    }))
                    .unwrap(),
                ),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "restored Write legitimately finishes or retries the same run"
            );
        }
    }

    #[tokio::test]
    async fn native_identity_http_link_refuses_before_status_or_sql() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let mut state = test_state(&store, temp.path());
        let mut privacy = ai_memory_core::SanitizeConfig::default();
        privacy.extra_patterns.push("private-vendor".into());
        state.sanitizer = Sanitizer::new(&privacy).unwrap();
        let (ws, project) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(ws, project, AgentKind::Codex, "launcher"))
            .await
            .unwrap();
        for id in [
            "private-vendor".to_owned(),
            "native\0tail".into(),
            "native\u{202e}tail".into(),
            "x".repeat(513),
        ] {
            let before = finish_sql_snapshot(&store).await;
            let response = link_run(
                State(state.clone()),
                None,
                None,
                None,
                None,
                AxumPath(run.run_id.to_string()),
                Json(LinkManagedRunRequest {
                    native_session_id: id,
                }),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "HTTP must refuse dirty original native identity"
            );
            assert_eq!(finish_sql_snapshot(&store).await, before);
            assert!(!temp.path().join("raw/workstreams").exists());
        }
        let response = link_run(
            State(state),
            None,
            None,
            None,
            None,
            AxumPath(run.run_id.to_string()),
            Json(LinkManagedRunRequest {
                native_session_id: "vendor-界-01".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            store
                .reader
                .managed_run_status(run.run_id)
                .await
                .unwrap()
                .unwrap()
                .native_session_id
                .as_deref(),
            Some("vendor-界-01")
        );
    }

    #[tokio::test]
    async fn native_identity_http_finish_refuses_before_segment_or_sql() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let (ws, project) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(ws, project, AgentKind::Codex, "launcher"))
            .await
            .unwrap();
        let request = |id: &str| FinishManagedRunRequest {
            native_session_id: Some(id.into()),
            source_cursor: Some("cursor-attack".into()),
            events: vec![NewWorkstreamEvent {
                event_id: "event-attack".into(),
                agent: AgentKind::Codex,
                native_session_id: id.into(),
                source_record_id: None,
                kind: WorkstreamEventKind::Message,
                role: None,
                content: "legitimate content".into(),
                occurred_at: None,
                metadata: serde_json::json!({}),
            }],
            complete: true,
            checkpoint: Default::default(),
            losses: Vec::new(),
            exit_code: Some(0),
        };
        for id in [
            "sk-abcdefghijklmnopqrstuvwx",
            "native\u{202e}tail",
            "native\u{200b}tail",
        ] {
            let before = finish_sql_snapshot(&store).await;
            let response = finish_run(
                State(state.clone()),
                None,
                None,
                None,
                None,
                AxumPath(run.run_id.to_string()),
                Json(request(id)),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "finish must refuse before raw segment"
            );
            let body = to_bytes(response.into_body(), 65536).await.unwrap();
            assert!(!String::from_utf8_lossy(&body).contains(id));
            assert!(!temp.path().join("raw/workstreams").exists());
            assert_eq!(finish_sql_snapshot(&store).await, before);
        }
        let response = finish_run(
            State(state.clone()),
            None,
            None,
            None,
            None,
            AxumPath(run.run_id.to_string()),
            Json(request("vendor-界-01")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let events = store
            .reader
            .search_workstream_events(run.workstream_id, "".into(), 10, Sanitizer::default())
            .await
            .unwrap();
        assert!(events.iter().all(|e| e.native_session_id == "vendor-界-01"));
    }
    #[tokio::test]
    async fn native_identity_http_event_guard_and_configured_history_projection() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let mut state = test_state(&store, temp.path());
        let (ws, project) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(ws, project, AgentKind::Codex, "launcher"))
            .await
            .unwrap();
        let request = |event_native: &str| FinishManagedRunRequest {
            native_session_id: Some("vendor-valid".into()),
            source_cursor: Some("cursor".into()),
            events: vec![NewWorkstreamEvent {
                event_id: "event".into(),
                agent: AgentKind::Codex,
                native_session_id: event_native.into(),
                source_record_id: None,
                kind: WorkstreamEventKind::Message,
                role: None,
                content: "shared history".into(),
                occurred_at: None,
                metadata: serde_json::json!({}),
            }],
            complete: true,
            checkpoint: Default::default(),
            losses: Vec::new(),
            exit_code: Some(0),
        };
        let before = finish_sql_snapshot(&store).await;
        let response = finish_run(
            State(state.clone()),
            None,
            None,
            None,
            None,
            AxumPath(run.run_id.to_string()),
            Json(request("sk-abcdefghijklmnopqrstuvwx")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("native session identity is UNKNOWN"),
            "event identity must be checked before mismatch/segment handling"
        );
        assert_eq!(finish_sql_snapshot(&store).await, before);
        assert!(!temp.path().join("raw/workstreams").exists());
        assert_eq!(
            finish_run(
                State(state.clone()),
                None,
                None,
                None,
                None,
                AxumPath(run.run_id.to_string()),
                Json(request("vendor-valid"))
            )
            .await
            .status(),
            StatusCode::OK
        );
        let privacy = ai_memory_core::SanitizeConfig {
            extra_patterns: vec!["vendor-valid".into()],
            ..Default::default()
        };
        state.sanitizer = Sanitizer::new(&privacy).unwrap();
        let before = finish_sql_snapshot(&store).await;
        let response = search_events(
            State(state),
            None,
            None,
            AxumPath(run.workstream_id.to_string()),
            Query(EventQuery::default()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 65536).await.unwrap();
        let events: Vec<ai_memory_core::WorkstreamEvent> = serde_json::from_slice(&body).unwrap();
        assert!(
            events.iter().all(|e| e.native_session_id.is_empty()),
            "HTTP configured privacy must project UNKNOWN"
        );
        assert_eq!(finish_sql_snapshot(&store).await, before);
    }

    #[tokio::test]
    async fn expired_run_recovery_is_owner_only_and_refuses_superseded_runs() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let mut state = test_state(&store, temp.path());
        state.trusted_proxy_identity = true;
        let (ws, project) = seed_scope(&store).await;
        let prepare = |name: &str| PrepareWorkstreamRun {
            workspace_id: ws,
            project_id: project,
            repo_fingerprint: name.into(),
            worktree_fingerprint: name.into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::New(name.into()),
            lease_owner: "launcher".into(),
        };
        let alice_actor = ai_memory_core::ActorContext {
            user: Some("alice".into()),
            ..ai_memory_core::ActorContext::anonymous()
        };
        let bob_actor = ai_memory_core::ActorContext {
            user: Some("bob".into()),
            ..ai_memory_core::ActorContext::anonymous()
        };
        let recoverable = store
            .writer
            .prepare_workstream_run_owned(prepare("recoverable"), Some("user:alice".into()))
            .await
            .unwrap();
        store
            .writer
            .cancel_managed_run(recoverable.run_id)
            .await
            .unwrap();
        let request = || FinishManagedRunRequest {
            native_session_id: Some("native-recovery".into()),
            source_cursor: None,
            events: vec![NewWorkstreamEvent {
                event_id: "recovered-event".into(),
                agent: AgentKind::Codex,
                native_session_id: "native-recovery".into(),
                source_record_id: None,
                kind: WorkstreamEventKind::Message,
                role: Some("user".into()),
                content: "recovered".into(),
                occurred_at: None,
                metadata: serde_json::Value::Null,
            }],
            complete: true,
            checkpoint: Default::default(),
            losses: Vec::new(),
            exit_code: Some(0),
        };
        let refused = recover_finish_run(
            State(state.clone()),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(bob_actor)),
            None,
            AxumPath(recoverable.run_id.to_string()),
            Json(request()),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        let accepted = recover_finish_run(
            State(state.clone()),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(alice_actor.clone())),
            None,
            AxumPath(recoverable.run_id.to_string()),
            Json(request()),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::OK);
        let exact_retry = recover_finish_run(
            State(state.clone()),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(alice_actor.clone())),
            None,
            AxumPath(recoverable.run_id.to_string()),
            Json(request()),
        )
        .await;
        let exact_status = exact_retry.status();
        let exact_body = to_bytes(exact_retry.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            exact_status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&exact_body)
        );
        let changed_checkpoint = recover_finish_run(
            State(state.clone()),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(alice_actor.clone())),
            None,
            AxumPath(recoverable.run_id.to_string()),
            Json(FinishManagedRunRequest {
                checkpoint: ai_memory_core::WorkstreamCheckpoint {
                    head: Some("repository-changed-after-finish".into()),
                    ..Default::default()
                },
                ..request()
            }),
        )
        .await;
        assert_eq!(changed_checkpoint.status(), StatusCode::OK);
        assert_eq!(
            store
                .reader
                .search_workstream_events(
                    recoverable.workstream_id,
                    "".into(),
                    100,
                    Sanitizer::default(),
                )
                .await
                .unwrap()
                .len(),
            2,
            "a finished retry must not append a regenerated checkpoint"
        );
        for changed in [
            FinishManagedRunRequest {
                native_session_id: Some("changed-native".into()),
                ..request()
            },
            FinishManagedRunRequest {
                events: {
                    let mut events = request().events;
                    events[0].event_id = "losing-event".into();
                    events
                },
                ..request()
            },
            FinishManagedRunRequest {
                source_cursor: Some("uncommitted-cursor".into()),
                ..request()
            },
            FinishManagedRunRequest {
                exit_code: Some(9),
                ..request()
            },
        ] {
            let changed_finished = recover_finish_run(
                State(state.clone()),
                Some(Extension(AuthLevel::User)),
                None,
                Some(Extension(alice_actor.clone())),
                None,
                AxumPath(recoverable.run_id.to_string()),
                Json(changed),
            )
            .await;
            assert_eq!(changed_finished.status(), StatusCode::BAD_REQUEST);
        }

        let old = store
            .writer
            .prepare_workstream_run_owned(prepare("superseded"), Some("user:alice".into()))
            .await
            .unwrap();
        store.writer.cancel_managed_run(old.run_id).await.unwrap();
        store
            .writer
            .prepare_workstream_run_owned(
                PrepareWorkstreamRun {
                    repo_fingerprint: "superseded".into(),
                    worktree_fingerprint: "superseded".into(),
                    selection: WorkstreamSelection::Named("superseded".into()),
                    ..prepare("replacement")
                },
                Some("user:alice".into()),
            )
            .await
            .unwrap();
        let superseded = recover_finish_run(
            State(state),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(alice_actor)),
            None,
            AxumPath(old.run_id.to_string()),
            Json(request()),
        )
        .await;
        assert_eq!(superseded.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn native_identity_http_stored_identity_refuses_before_raw_segment() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let mut state = test_state(&store, temp.path());
        let (ws, project) = seed_scope(&store).await;
        let run = store
            .writer
            .prepare_workstream_run(prepare_input(ws, project, AgentKind::Codex, "launcher"))
            .await
            .unwrap();
        store
            .writer
            .link_managed_run_session(run.run_id, AgentKind::Codex, "private-existing")
            .await
            .unwrap();
        state.sanitizer = Sanitizer::new(&ai_memory_core::SanitizeConfig {
            extra_patterns: vec!["private-existing".into()],
            ..Default::default()
        })
        .unwrap();
        let request = || FinishManagedRunRequest {
            native_session_id: None,
            source_cursor: Some("must-not-advance".into()),
            events: Vec::new(),
            complete: true,
            checkpoint: Default::default(),
            losses: Vec::new(),
            exit_code: Some(0),
        };
        let before = finish_sql_snapshot(&store).await;
        let response = finish_run(
            State(state.clone()),
            None,
            None,
            None,
            None,
            AxumPath(run.run_id.to_string()),
            Json(request()),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "stored identity must be validated before segment"
        );
        assert!(!temp.path().join("raw/workstreams").exists());
        assert_eq!(finish_sql_snapshot(&store).await, before);
        state.sanitizer = Sanitizer::builtin();
        assert_eq!(
            finish_run(
                State(state),
                None,
                None,
                None,
                None,
                AxumPath(run.run_id.to_string()),
                Json(request())
            )
            .await
            .status(),
            StatusCode::OK
        );
    }

    /// A checkout's run request as `ai-memory run` sends it for a repository
    /// whose folder name differs from its remote's derived name.
    fn identity_run_request(project: &str, identity: Option<&str>) -> PrepareManagedRunRequest {
        PrepareManagedRunRequest {
            workspace: "default".into(),
            project: project.into(),
            cwd: "/repo/new-space-game".into(),
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            workstream: None,
            new_workstream: None,
            force_unlock: false,
            lease_owner: "launcher".into(),
            repository_identity: identity.map(str::to_owned),
        }
    }

    fn unknown_system() -> ai_memory_core::repository_identity::RepositoryIdentity {
        ai_memory_core::repository_identity::RepositoryIdentity {
            identity: "github.com/victorcesc/unknown-system".into(),
            source: ai_memory_core::repository_identity::IdentitySource::GitRemote,
        }
    }

    async fn run_project(
        store: &Store,
        response: Response,
    ) -> (ai_memory_core::WorkspaceId, ai_memory_core::ProjectId) {
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let prepared: PrepareManagedRunResponse = serde_json::from_slice(&body).unwrap();
        store
            .reader
            .managed_run_scope(prepared.run_id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn project_names(store: &Store) -> Vec<String> {
        let mut names: Vec<String> = store
            .reader
            .list_projects_with_stats_for_workspace("default".into(), None)
            .await
            .unwrap()
            .into_iter()
            .map(|summary| summary.project_name)
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn managed_run_by_remote_identity_claims_the_folder_named_project() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        // Created from the folder name before identities were recorded.
        let legacy = store
            .writer
            .get_or_create_project(
                workspace_id,
                "new-space-game",
                Some("/repo/new-space-game".into()),
            )
            .await
            .unwrap();

        let response = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(identity_run_request(
                "victorcesc-unknown-system",
                Some("github.com/victorcesc/unknown-system"),
            )),
        )
        .await;

        assert_eq!(run_project(&store, response).await, (workspace_id, legacy));
        assert_eq!(project_names(&store).await, ["victorcesc-unknown-system"]);
    }

    #[tokio::test]
    async fn managed_run_by_remote_identity_routes_past_an_identityless_twin() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (holder, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                unknown_system(),
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "new-space-game",
                Some("/repo/new-space-game".into()),
                None,
                None,
            )
            .await
            .unwrap();
        // The twin an earlier name-only run created under the derived name.
        store
            .writer
            .get_or_create_project(workspace_id, "victorcesc-unknown-system", None)
            .await
            .unwrap();

        // By name alone the derived name names both projects.
        let by_name = prepare_run(
            State(state.clone()),
            None,
            None,
            None,
            Json(identity_run_request("victorcesc-unknown-system", None)),
        )
        .await;
        assert_eq!(by_name.status(), StatusCode::BAD_REQUEST);

        let by_identity = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(identity_run_request(
                "victorcesc-unknown-system",
                Some("github.com/victorcesc/unknown-system"),
            )),
        )
        .await;
        assert_eq!(
            run_project(&store, by_identity).await,
            (workspace_id, holder)
        );
        assert_eq!(
            project_names(&store).await,
            ["new-space-game", "victorcesc-unknown-system"]
        );
    }

    #[tokio::test]
    async fn managed_run_identity_never_redirects_another_project_name() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (holder, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                unknown_system(),
                ai_memory_core::repository_identity::IdentityStyle::Path,
                "new-space-game",
                None,
                None,
                None,
            )
            .await
            .unwrap();

        // An identity that does not derive the requested name is ignored: the
        // run opens the project it names, never the identity's holder.
        let response = prepare_run(
            State(state),
            None,
            None,
            None,
            Json(identity_run_request(
                "scratchpad",
                Some("github.com/victorcesc/unknown-system"),
            )),
        )
        .await;
        let (_, opened) = run_project(&store, response).await;
        assert_ne!(opened, holder);
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, opened)
                .await
                .unwrap()
                .as_deref(),
            Some("scratchpad")
        );
    }

    #[tokio::test]
    async fn managed_run_by_remote_identity_refuses_a_viewer_without_write() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (holder, _) = store
            .writer
            .resolve_project_by_identity(
                workspace_id,
                unknown_system(),
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "new-space-game",
                Some("/repo/new-space-game".into()),
                None,
                None,
            )
            .await
            .unwrap();
        store
            .writer
            .set_access_mode(holder, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        let user = |name: &str, seed: u8| {
            store.writer.create_user(
                ai_memory_core::NewUser {
                    username: name.into(),
                    name: None,
                    email: None,
                },
                [seed; ai_memory_store::TOKEN_HASH_LEN],
            )
        };
        let outsider = user("outsider", 3).await.unwrap();
        let member = user("member", 4).await.unwrap();
        store
            .writer
            .grant_memory(member, holder, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
        let request = || {
            Json(identity_run_request(
                "victorcesc-unknown-system",
                Some("github.com/victorcesc/unknown-system"),
            ))
        };

        let refused = prepare_run(
            State(state.clone()),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(ai_memory_core::AuthorizedViewer(outsider))),
            request(),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        // Neither a run nor a project to hold one appeared for the outsider.
        assert_eq!(project_names(&store).await, ["new-space-game"]);

        let admitted = prepare_run(
            State(state),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(ai_memory_core::AuthorizedViewer(member))),
            request(),
        )
        .await;
        assert_eq!(run_project(&store, admitted).await, (workspace_id, holder));
    }

    /// The claim branch: a legacy project with no recorded identity whose
    /// `repo_path` contains the run cwd. An outsider who may not write to it
    /// must neither stamp the identity on it nor get a twin under the derived
    /// name.
    #[tokio::test]
    async fn managed_run_identity_never_claims_a_restricted_legacy_project() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let legacy = store
            .writer
            .get_or_create_project(
                workspace_id,
                "new-space-game",
                Some("/repo/new-space-game".into()),
            )
            .await
            .unwrap();
        store
            .writer
            .set_access_mode(legacy, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        let user = |name: &str, seed: u8| {
            store.writer.create_user(
                ai_memory_core::NewUser {
                    username: name.into(),
                    name: None,
                    email: None,
                },
                [seed; ai_memory_store::TOKEN_HASH_LEN],
            )
        };
        let outsider = user("outsider", 3).await.unwrap();
        let member = user("member", 4).await.unwrap();
        store
            .writer
            .grant_memory(member, legacy, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
        let request = || {
            Json(identity_run_request(
                "victorcesc-unknown-system",
                Some("github.com/victorcesc/unknown-system"),
            ))
        };
        let legacy_identity = || async {
            store
                .reader
                .list_all_scopes()
                .await
                .unwrap()
                .into_iter()
                .find(|scope| scope.project_id == legacy)
                .unwrap()
                .identity
        };

        let refused = prepare_run(
            State(state.clone()),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(ai_memory_core::AuthorizedViewer(outsider))),
            request(),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_eq!(legacy_identity().await, None);
        assert_eq!(project_names(&store).await, ["new-space-game"]);

        let admitted = prepare_run(
            State(state),
            Some(Extension(AuthLevel::User)),
            None,
            Some(Extension(ai_memory_core::AuthorizedViewer(member))),
            request(),
        )
        .await;
        assert_eq!(run_project(&store, admitted).await, (workspace_id, legacy));
        assert_eq!(
            legacy_identity().await.as_deref(),
            Some("github.com/victorcesc/unknown-system")
        );
        assert_eq!(project_names(&store).await, ["victorcesc-unknown-system"]);
    }

    #[tokio::test]
    async fn managed_run_identity_routing_stays_inside_the_requested_workspace() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let default_ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let (holder, _) = store
            .writer
            .resolve_project_by_identity(
                default_ws,
                unknown_system(),
                ai_memory_core::repository_identity::IdentityStyle::HostPath,
                "new-space-game",
                Some("/repo/new-space-game".into()),
                None,
                None,
            )
            .await
            .unwrap();

        let mut request = identity_run_request(
            "victorcesc-unknown-system",
            Some("github.com/victorcesc/unknown-system"),
        );
        request.workspace = "client-work".into();
        let response = prepare_run(State(state), None, None, None, Json(request)).await;

        // Another workspace's identity holder is never the target: the run
        // gets that workspace's own project for the repository.
        let (workspace_id, opened) = run_project(&store, response).await;
        assert_ne!(workspace_id, default_ws);
        assert_ne!(opened, holder);
        assert_eq!(
            store
                .reader
                .project_name_by_id(workspace_id, opened)
                .await
                .unwrap()
                .as_deref(),
            Some("victorcesc-unknown-system")
        );
        assert_eq!(project_names(&store).await, ["new-space-game"]);
    }

    #[tokio::test]
    async fn concurrent_identity_runs_on_a_legacy_project_converge_on_it() {
        let temp = TempDir::new().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let state = test_state(&store, temp.path());
        let workspace_id = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let legacy = store
            .writer
            .get_or_create_project(
                workspace_id,
                "new-space-game",
                Some("/repo/new-space-game".into()),
            )
            .await
            .unwrap();
        let request = |workstream: &str| {
            let mut request = identity_run_request(
                "victorcesc-unknown-system",
                Some("github.com/victorcesc/unknown-system"),
            );
            request.new_workstream = Some(workstream.into());
            Json(request)
        };

        // Two launches in one checkout race the first claim of the row.
        let (first, second) = tokio::join!(
            prepare_run(State(state.clone()), None, None, None, request("market")),
            prepare_run(State(state), None, None, None, request("mine-meshs")),
        );

        assert_eq!(run_project(&store, first).await, (workspace_id, legacy));
        assert_eq!(run_project(&store, second).await, (workspace_id, legacy));
        assert_eq!(project_names(&store).await, ["victorcesc-unknown-system"]);
    }
}
