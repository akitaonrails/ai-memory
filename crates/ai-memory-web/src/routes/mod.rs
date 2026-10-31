//! Route module — assembles the public axum router.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};

use crate::html_auth::HtmlAuthRedirectConfig;
use crate::state::WebState;

mod agents;
mod api;
mod documentation;
mod graph;
mod health;
mod history;
mod inbox;
mod index;
mod login;
mod notifications;
mod page;
mod pending;
mod project;
pub(crate) mod search;
mod session;
pub(crate) mod statics;
mod workstreams;

/// Whether the viewer may read `workspace/project` (#708).
///
/// For the routes that read a repository by name without resolving a scope
/// first — the page views and the project listing. Those went straight from a
/// URL to the page body, so the guard never saw them.
///
/// No viewer — an install with no database users, or root — returns
/// `Ok(None)` without a lookup, so these routes behave exactly as they did
/// before. Otherwise returns the scope the check resolved.
pub(crate) async fn authorize_read(
    state: &WebState,
    viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
    workspace: &str,
    project: &str,
) -> Result<Option<ai_memory_store::ResolvedScope>, ai_memory_store::ScopeResolutionError> {
    let Some(axum::Extension(viewer)) = viewer else {
        return Ok(None);
    };
    ai_memory_store::lookup_existing_scope_guarded(
        &state.reader,
        workspace,
        project,
        Some(viewer.user()),
        ai_memory_store::ProjectAccess::Read,
    )
    .await
    .map(Some)
}

/// Resolve `workspace/project` for a route that needs its ids, refusing a
/// viewer without read access (#708).
///
/// Unlike [`authorize_read`] it always resolves, viewer or not, because the
/// history and session views read by id. No-create and fail-closed, like
/// every read path.
pub(crate) async fn resolve_read_scope(
    state: &WebState,
    viewer: Option<ai_memory_core::UserId>,
    workspace: &str,
    project: &str,
) -> Result<ai_memory_store::ResolvedScope, ai_memory_store::ScopeResolutionError> {
    ai_memory_store::lookup_existing_scope_guarded(
        &state.reader,
        workspace,
        project,
        viewer,
        ai_memory_store::ProjectAccess::Read,
    )
    .await
}

/// Refuse a root-only page unless the caller may use `/admin`, with the same
/// `Capability::Admin` decision `require_root_for_multiuser_admin` makes:
/// anonymous on a multi-user server is 401, a non-root user 403.
pub(crate) async fn require_admin(
    i18n: &ai_memory_core::i18n::Localizer,
    state: &WebState,
    level: Option<axum::Extension<ai_memory_core::AuthLevel>>,
) -> Result<(), axum::response::Response> {
    use axum::http::StatusCode;
    use axum::response::{Html, IntoResponse};
    let level = level.map_or(
        ai_memory_core::AuthLevel::Anonymous,
        |axum::Extension(level)| level,
    );
    let distinguishes_operators = state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    match level.authorize(ai_memory_core::Capability::Admin, distinguishes_operators) {
        Ok(()) => Ok(()),
        Err(e) if e.is_authentication_required() => Err(StatusCode::UNAUTHORIZED.into_response()),
        Err(_) => {
            use askama::Template;
            let mut response =
                match (crate::templates::AdminRequiredView { i18n: i18n.clone() }).render() {
                    Ok(body) => (StatusCode::FORBIDDEN, Html(body)).into_response(),
                    Err(_) => StatusCode::FORBIDDEN.into_response(),
                };
            // A 403 here means "not root", not "change your password": keep
            // the HTML auth redirect from sending the user to that form.
            response
                .extensions_mut()
                .insert(crate::html_auth::AdminRequired);
            Err(response)
        }
    }
}

/// Build the read-only wiki page router (no static assets — those are public).
pub(crate) fn build(state: Arc<WebState>) -> Router {
    crate::localization::apply(
        Router::new()
            .route("/", get(index::handler))
            .route("/docs", get(documentation::index))
            .route("/docs/{slug}", get(documentation::page))
            .route("/inbox", get(inbox::handler))
            .route("/inbox/notifications", get(notifications::handler))
            .route("/messages", post(inbox::send))
            .route("/messages/{id}/edit", post(inbox::begin_edit))
            .route("/messages/{id}/save", post(inbox::save_edit))
            .route("/messages/{id}/release", post(inbox::release_edit))
            .route(
                "/w/{workspace}/{project}/agents/{agent}",
                get(agents::handler),
            )
            .route("/graph", get(graph::global_handler))
            .route("/w/{workspace}/{project}", get(project::handler))
            .route(
                "/w/{workspace}/{project}/graph",
                get(graph::project_handler),
            )
            .route("/w/{workspace}/{project}/p/{*path}", get(page::handler))
            .route("/w/{workspace}/{project}/h/{*path}", get(history::handler))
            .route(
                "/w/{workspace}/{project}/s/{session_id}",
                get(session::handler),
            )
            .route("/search", get(search::handler))
            .route("/pending", get(pending::handler))
            .route("/workstreams", get(workstreams::handler))
            .route("/health", get(health::handler))
            .with_state(state),
    )
}

/// Public builtin surface: login, change-password, and static assets.
///
/// Statics stay public so the login form can load CSS/logo without a
/// session; wiki pages remain behind dual-auth on the protected router.
pub(crate) fn build_public_auth(cfg: Arc<HtmlAuthRedirectConfig>) -> Router {
    crate::localization::apply(
        Router::new()
            .route("/login", get(login::login))
            .route("/change-password", get(login::change_password))
            .route("/static/tailwind.css", get(statics::tailwind_css))
            .route("/static/logo.png", get(statics::logo))
            .route("/static/{font}", get(statics::font))
            .with_state(cfg),
    )
}

/// Build the read-only JSON API router from a shared [`WebState`].
pub(crate) fn build_api(state: Arc<WebState>) -> Router {
    api::build(state)
}

/// Standalone `GET /favicon.ico` router. Mounted at the **host root**
/// by `serve`, OUTSIDE the `/web` nest and OUTSIDE the `--base-path`
/// prefix, because browsers auto-fetch `/favicon.ico` from the host
/// origin regardless of where the rest of the app is mounted. Putting
/// it inside the web router (as the original PR #79 did) made it
/// reachable only at `/web/favicon.ico` — never seen by the browser's
/// automatic fetch — so the in-page `<link rel="icon">` was the only
/// thing actually showing the icon.
pub(crate) fn build_favicon() -> Router {
    Router::new().route("/favicon.ico", get(statics::favicon))
}
