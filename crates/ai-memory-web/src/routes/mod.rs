//! Route module — assembles the public axum router.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use crate::state::WebState;

mod api;
mod index;
mod page;
mod project;
mod search;
mod statics;

/// Build the read-only web router from a shared [`WebState`].
pub(crate) fn build(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/", get(index::handler))
        .route("/w/{workspace}/{project}", get(project::handler))
        .route("/w/{workspace}/{project}/p/{*path}", get(page::handler))
        .route("/search", get(search::handler))
        .route("/static/tailwind.css", get(statics::tailwind_css))
        .route("/static/logo.png", get(statics::logo_light))
        .route("/static/logo-dark.png", get(statics::logo_dark))
        .with_state(state)
}

/// Build the read-only JSON API router from a shared [`WebState`].
pub(crate) fn build_api(state: Arc<WebState>) -> Router {
    api::build(state)
}
