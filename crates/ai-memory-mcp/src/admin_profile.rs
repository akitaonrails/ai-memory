//! `/admin/profile/*`: operator view of the cross-project profile
//! (`docs/cross-project-profile.md`). `status`, `list`, `review` and `apply`
//! read (`apply` returns the lines `ai-memory profile apply` writes into a
//! repository's rules file); `rebuild` re-harvests every contributing project
//! and converges the profile. `ai-memory profile show` and `forget` reuse
//! `/admin/read-page` and `/admin/delete-page` against the scope `status`
//! reports. Root-only on a multi-user server, like every `/admin/*` route.

use std::collections::BTreeSet;
use std::sync::Arc;

use ai_memory_core::profile::{EffectiveProfileShare, ProfileSettings, render_digest};
use ai_memory_core::{AuthLevel, Capability};
use ai_memory_llm::LlmProvider;
use ai_memory_store::{ReaderPool, ResolvedScope, ScopeResolutionError, WriterHandle};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// State for the profile admin routes.
#[derive(Clone)]
pub struct ProfileAdminState {
    /// Reader pool.
    pub reader: ReaderPool,
    /// Writer actor (`rebuild` records candidates and marks).
    pub writer: WriterHandle,
    /// Wiki (`rebuild` writes profile entries through it).
    pub wiki: Wiki,
    /// The configured LLM provider, if any (`rebuild` classifies and merges
    /// with it when `[profile] llm` allows).
    pub llm: Option<Arc<dyn LlmProvider>>,
    /// `[profile]` settings.
    pub profile: ProfileSettings,
    /// `[auth].actor_proxy_bearer_token` is configured; see
    /// [`crate::admin::AdminState::trusted_proxy_identity`].
    pub trusted_proxy_identity: bool,
}

/// Build the `/admin/profile/*` router.
pub fn profile_admin_router(state: ProfileAdminState) -> Router {
    let state = Arc::new(state);
    Router::new()
        .route("/admin/profile/status", get(handle_status))
        .route("/admin/profile/list", get(handle_list))
        .route("/admin/profile/review", get(handle_review))
        .route("/admin/profile/apply", get(handle_apply))
        .route("/admin/profile/rebuild", post(handle_rebuild))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_root_for_multiuser_admin,
        ))
        .with_state(state)
}

/// Same gate as the rest of `/admin/*`: open on a single-operator server,
/// root-only once the deployment distinguishes operators.
async fn require_root_for_multiuser_admin(
    State(state): State<Arc<ProfileAdminState>>,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let level = req
        .extensions()
        .get::<AuthLevel>()
        .copied()
        .unwrap_or(AuthLevel::Anonymous);
    let distinguishes = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(distinguishes) => distinguishes,
        Err(error) => {
            tracing::error!(%error, "profile admin authorization could not read the operator topology");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "admin authorization unavailable" })),
            )
                .into_response();
        }
    };
    match level.authorize(Capability::Admin, distinguishes) {
        Ok(()) => next.run(req).await,
        Err(e) => {
            let status = if e.is_authentication_required() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            };
            (status, Json(serde_json::json!({ "error": e.message() }))).into_response()
        }
    }
}

/// Query of both routes.
#[derive(Debug, Deserialize)]
struct ProfileQuery {
    /// Workspace whose profile to inspect when `share = "workspace"`.
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Operator whose private profile to inspect when `share = "user"`.
    #[serde(default)]
    user: Option<String>,
}

fn default_workspace() -> String {
    ai_memory_core::DEFAULT_WORKSPACE_NAME.to_owned()
}

/// Names of the scope holding the inspected profile.
#[derive(Debug, Serialize)]
struct ScopeNames {
    workspace: String,
    project: String,
}

/// The project holding the profile for `query`, without creating anything.
/// `Ok(None)` when it does not exist yet or a private profile was asked for
/// without naming its operator.
async fn resolve_scope(
    reader: &ReaderPool,
    share: EffectiveProfileShare,
    query: &ProfileQuery,
) -> Result<Option<(ResolvedScope, ScopeNames)>, ScopeResolutionError> {
    let (workspace, project) = match share {
        EffectiveProfileShare::Global => (
            ai_memory_core::DEFAULT_WORKSPACE_NAME.to_owned(),
            ai_memory_core::GLOBAL_SCOPE_PROJECT.to_owned(),
        ),
        EffectiveProfileShare::Workspace => (
            query.workspace.clone(),
            ai_memory_core::profile::WORKSPACE_PROFILE_PROJECT.to_owned(),
        ),
        EffectiveProfileShare::User => {
            let Some(username) = query
                .user
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
            else {
                return Ok(None);
            };
            let Some(user) = reader.find_user_by_username(username.to_owned()).await? else {
                return Ok(None);
            };
            (
                ai_memory_core::DEFAULT_WORKSPACE_NAME.to_owned(),
                ai_memory_core::profile::user_profile_project(user.id),
            )
        }
    };
    match ai_memory_store::lookup_existing_scope(reader, &workspace, &project).await {
        Ok(scope) => Ok(Some((scope, ScopeNames { workspace, project }))),
        Err(
            ScopeResolutionError::WorkspaceNotFound { .. }
            | ScopeResolutionError::ProjectNotFoundInWorkspace { .. },
        ) => Ok(None),
        Err(e) => Err(e),
    }
}

fn internal(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// `GET /admin/profile/status`: effective mode, settings, the inspected
/// scope, its entry count and digest size, and the projects that opted out.
async fn handle_status(
    State(state): State<Arc<ProfileAdminState>>,
    Query(query): Query<ProfileQuery>,
) -> Response {
    let distinguishes = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(distinguishes) => distinguishes,
        Err(e) => return internal(e),
    };
    let settings = &state.profile;
    let share = settings.effective_share(distinguishes);
    let (scope, entries, digest_bytes) = match share {
        None => (None, 0, 0),
        Some(share) => match resolve_scope(&state.reader, share, &query).await {
            Ok(None) => (None, 0, 0),
            Ok(Some((scope, names))) => {
                let entries = match state
                    .reader
                    .profile_entries(scope.workspace_id, scope.project_id)
                    .await
                {
                    Ok(entries) => entries,
                    Err(e) => return internal(e),
                };
                let digest = render_digest(
                    &entries,
                    &BTreeSet::new(),
                    settings.digest_budget(),
                    false,
                    ai_memory_core::profile::is_team_profile(share, distinguishes),
                );
                (Some(names), entries.len(), digest.map_or(0, |d| d.len()))
            }
            Err(e) => return internal(e),
        },
    };
    let opt_outs = match state.reader.profile_contribute_opt_outs().await {
        Ok(rows) => rows,
        Err(e) => return internal(e),
    };
    Json(serde_json::json!({
        "enabled": settings.enabled.as_str(),
        "share": settings.share,
        "effective_share": share.map(EffectiveProfileShare::as_str),
        "distinguishes_operators": distinguishes,
        "min_projects": settings.min_projects,
        "inject_on_session_start": settings.inject_on_session_start,
        "digest_max_bytes": settings.digest_budget(),
        "baseline_max_bytes": settings.baseline_budget(),
        "apply_max_lines": settings.apply_max_lines,
        "llm": settings.llm,
        "scope": scope,
        "entries": entries,
        "digest_bytes": digest_bytes,
        "contribute_opt_outs": opt_outs,
    }))
    .into_response()
}

/// `GET /admin/profile/list`: the inspected profile's entries, by path.
async fn handle_list(
    State(state): State<Arc<ProfileAdminState>>,
    Query(query): Query<ProfileQuery>,
) -> Response {
    let distinguishes = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(distinguishes) => distinguishes,
        Err(e) => return internal(e),
    };
    let Some(share) = state.profile.effective_share(distinguishes) else {
        return Json(serde_json::json!({ "scope": null, "entries": [] })).into_response();
    };
    let (scope, names) = match resolve_scope(&state.reader, share, &query).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Json(serde_json::json!({ "scope": null, "entries": [] })).into_response();
        }
        Err(e) => return internal(e),
    };
    let entries = match state
        .reader
        .profile_entries(scope.workspace_id, scope.project_id)
        .await
    {
        Ok(entries) => entries,
        Err(e) => return internal(e),
    };
    let entries: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "path": entry.path,
                "category": entry.category(),
                "statement": entry.statement,
                "applies_to": entry.applies_to,
                "enforced_by": entry.enforced_by,
            })
        })
        .collect();
    Json(serde_json::json!({ "scope": names, "entries": entries })).into_response()
}

/// Query of `GET /admin/profile/apply`: the project whose rules file is being
/// written, and the operator whose private profile to use when it is per user.
#[derive(Debug, Deserialize)]
struct ApplyQuery {
    workspace: String,
    project: String,
    #[serde(default)]
    user: Option<String>,
}

/// `GET /admin/profile/apply`: the lines `ai-memory profile apply` writes for
/// one project. Selection matches the digest (stack scope, entries enforced
/// elsewhere skipped) and is capped at `apply_max_lines`. A project that set
/// `[profile] consume = false` receives nothing. A project the server has not
/// seen yet gets every entry, like the baseline digest of a new project.
async fn handle_apply(
    State(state): State<Arc<ProfileAdminState>>,
    Query(query): Query<ApplyQuery>,
) -> Response {
    let distinguishes = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(distinguishes) => distinguishes,
        Err(e) => return internal(e),
    };
    let Some(share) = state.profile.effective_share(distinguishes) else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "the profile is off on this server" })),
        )
            .into_response();
    };
    let profile_query = ProfileQuery {
        workspace: query.workspace.clone(),
        user: query.user.clone(),
    };
    let (profile_scope, names) = match resolve_scope(&state.reader, share, &profile_query).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Json(serde_json::json!({
                "scope": null, "consume": true, "lines": [], "omitted": 0,
            }))
            .into_response();
        }
        Err(e) => return internal(e),
    };
    let project = match ai_memory_store::lookup_existing_scope(
        &state.reader,
        &query.workspace,
        &query.project,
    )
    .await
    {
        Ok(scope) => Some(scope),
        Err(
            ScopeResolutionError::WorkspaceNotFound { .. }
            | ScopeResolutionError::ProjectNotFoundInWorkspace { .. },
        ) => None,
        Err(e) => return internal(e),
    };
    let (entries, tags) = match project {
        Some(project) => {
            match state
                .reader
                .project_profile_flags(project.workspace_id, project.project_id)
                .await
            {
                Ok(flags) if !flags.consume => {
                    return Json(serde_json::json!({
                        "scope": names, "consume": false, "lines": [], "omitted": 0,
                    }))
                    .into_response();
                }
                Ok(_) => {}
                Err(e) => return internal(e),
            }
            match state
                .reader
                .profile_digest_inputs(profile_scope.as_tuple(), project.as_tuple())
                .await
            {
                Ok(inputs) => (inputs.entries, inputs.project_tags),
                Err(e) => return internal(e),
            }
        }
        None => match state
            .reader
            .profile_entries(profile_scope.workspace_id, profile_scope.project_id)
            .await
        {
            Ok(entries) => (entries, BTreeSet::new()),
            Err(e) => return internal(e),
        },
    };
    let (lines, omitted) =
        ai_memory_core::profile::apply_lines(&entries, &tags, state.profile.apply_max_lines);
    Json(serde_json::json!({
        "scope": names, "consume": true, "lines": lines, "omitted": omitted,
    }))
    .into_response()
}

async fn pass_config(
    state: &ProfileAdminState,
) -> Result<ai_memory_consolidate::profile::ProfilePassConfig, Response> {
    let distinguishes = state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
        .map_err(internal)?;
    Ok(ai_memory_consolidate::profile::ProfilePassConfig {
        settings: state.profile.clone(),
        distinguishes_operators: distinguishes,
    })
}

/// `GET /admin/profile/review`: the inspected profile's entries with their
/// evidence, the groups still below the bar, and the entries a pass would
/// update, leaves alone (hand-edited) or keeps removed (forgotten).
async fn handle_review(
    State(state): State<Arc<ProfileAdminState>>,
    Query(query): Query<ProfileQuery>,
) -> Response {
    let config = match pass_config(&state).await {
        Ok(config) => config,
        Err(response) => return response,
    };
    let workspace_id = match state.reader.find_workspace(query.workspace.clone()).await {
        Ok(ws) => ws,
        Err(e) => return internal(e),
    };
    let user = match query
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        Some(name) => match state.reader.find_user_by_username(name.to_owned()).await {
            Ok(Some(user)) => Some(user.id),
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": format!("no user named {name}") })),
                )
                    .into_response();
            }
            Err(e) => return internal(e),
        },
        None => None,
    };
    match ai_memory_consolidate::profile::profile_review(&state.reader, &config, workspace_id, user)
        .await
    {
        Ok(review) => Json(review).into_response(),
        Err(e) => internal(e),
    }
}

/// `POST /admin/profile/rebuild`: forget the harvest marks, re-read every
/// contributing project from the start and converge every profile scope.
/// Recording a candidate twice is a no-op, so this is safe to repeat.
async fn handle_rebuild(State(state): State<Arc<ProfileAdminState>>) -> Response {
    let config = match pass_config(&state).await {
        Ok(config) => config,
        Err(response) => return response,
    };
    if config
        .settings
        .effective_share(config.distinguishes_operators)
        .is_none()
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "the profile is off on this server" })),
        )
            .into_response();
    }
    if let Err(e) = state.writer.clear_profile_harvest_marks().await {
        return internal(e);
    }
    match ai_memory_consolidate::profile::run_profile_pass(
        &state.reader,
        &state.writer,
        &state.wiki,
        state.llm.as_ref(),
        &config,
    )
    .await
    {
        Ok(report) => Json(report).into_response(),
        Err(e) => internal(e),
    }
}
