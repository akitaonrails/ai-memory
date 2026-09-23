//! Route module — assembles the public axum router.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use crate::html_auth::HtmlAuthRedirectConfig;
use crate::state::WebState;

mod api;
mod index;
mod login;
mod page;
mod project;
mod search;
mod statics;

/// Build the read-only wiki page router (no static assets — those are public).
pub(crate) fn build(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index::handler))
        .route("/w/{workspace}/{project}", get(project::handler))
        .route("/w/{workspace}/{project}/p/{*path}", get(page::handler))
        .route("/search", get(search::handler))
        .with_state(state)
}

/// Public builtin surface: login, change-password, and static assets.
///
/// Statics stay public so the login form can load CSS/logo without a
/// session; wiki pages remain behind dual-auth on the protected router.
pub(crate) fn build_public_auth(cfg: Arc<HtmlAuthRedirectConfig>) -> Router {
    Router::new()
        .route("/login", get(login::login))
        .route("/change-password", get(login::change_password))
        .route("/static/tailwind.css", get(statics::tailwind_css))
        .route("/static/logo.png", get(statics::logo))
        .with_state(cfg)
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
