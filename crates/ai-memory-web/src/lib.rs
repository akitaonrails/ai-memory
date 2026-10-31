//! `ai-memory-web` — read-only HTTP browser for the wiki.
//!
//! Mounted under `/web` on the same axum server that hosts the MCP
//! endpoint, so a single port + single auth posture covers both. The
//! wiki browsing remains read-only. The inbox also queues authorized messages
//! through sanitization, admission and the single writer; it never claims mail
//! or starts an agent. Hosts must mount the router behind normal dual-auth.
//!
//! Routes (all under whatever prefix the host nests this router at):
//! - `GET /`                              → project list (cards)
//! - `GET /docs` / `GET /docs/:id` / `GET /docs/:id.md` → bundled product guides
//! - `GET /inbox`                         → scoped received/sent history
//! - `GET /inbox/notifications`           → access-filtered pending counts
//! - `POST /messages`                     → authorized harness-addressed request
//! - `GET /w/:workspace/:project/agents/:agent` → agent sessions and usage
//! - `GET /graph`                         → cross-project link graph (SVG + table)
//! - `GET /w/:workspace/:project`         → page tree + recent activity
//! - `GET /w/:workspace/:project/graph`   → in-project link graph (SVG + table)
//! - `GET /w/:workspace/:project/p/*path` → rendered markdown + metadata
//! - `GET /w/:workspace/:project/h/*path` → version history + unified diff
//! - `GET /w/:workspace/:project/s/:session` → session timeline, paged
//! - `GET /search?q=…`                    → cross-project FTS5 hit list
//! - `GET /search?q=…&workspace=…&project=…&explain=1` → project search
//!   through `memory_query`'s hybrid ranker, with the recall trace
//! - `GET /pending`                       → root-only pending-writes triage;
//!   its buttons post to `/admin/pending-writes/*`, not to this crate
//! - `GET /workstreams`                   → root-only managed-run operations
//!   overview derived from trusted lifecycle fields
//! - `GET /health`                        → root-only server health panel
//! - `GET /login` / `GET /change-password` → public human-auth HTML forms
//! - `GET /static/*`                      → embedded CSS + logo (public)
//!
//! The companion `api_router` exposes the same read-only data as JSON
//! for custom frontends. It intentionally does not expose write/admin
//! operations. `clippy.toml` in this crate forbids the `Wiki` write and
//! destructive methods, keeping wiki mutations outside this surface.
//!
//! Themes support light, dark and system choices stored locally in the browser.

use std::sync::Arc;

use ai_memory_store::ReaderPool;
use ai_memory_wiki::Wiki;
use axum::Router;

mod diff;
mod documentation;
pub mod features;
pub mod health;
mod html_auth;
mod knowledge;
mod localization;
#[cfg(feature = "management-ui")]
pub mod management;
mod markdown;
pub mod mount;
mod operations;
pub mod recall;
mod routes;
mod state;
mod templates;
mod views;

pub use html_auth::{HtmlAuthRedirectConfig, html_auth_redirect_mw, sanitize_next};
pub use mount::{
    SplitWebRouters, WebMountSpec, inject_base_href, inject_base_path_meta, normalize_prefix,
    split_web_routers, split_web_routers_with_metrics, web_base_href,
};
pub use recall::Recall;
pub use state::WebState;

/// Build the web dashboard and authorized inbox composer. Call once at server startup and
/// `nest("/web", router)` it onto the existing axum app, OR mount at
/// `/` if the web UI is the only HTTP surface.
pub fn router(reader: ReaderPool, wiki: Wiki) -> Router {
    let state = Arc::new(WebState::new(reader, wiki));
    routes::build(state)
}

/// Build the read-only JSON API router for third-party web UIs.
///
/// The host should `nest("/api/v1", api_router(...))` behind its normal auth
/// boundary, with or without `/web`, so external clients can browse memory
/// without reading SQLite or wiki files directly.
pub fn api_router(reader: ReaderPool, wiki: Wiki) -> Router {
    let state = Arc::new(WebState::new(reader, wiki));
    routes::build_api(state)
}

/// Standalone `GET /favicon.ico` router. Merge at the host-root level
/// (NOT under `--base-path`, NOT nested under `/web`) so the browser's
/// automatic `/favicon.ico` fetch actually reaches it. The handler is
/// stateless — it returns the same embedded PNG as `/web/static/logo.png`.
pub fn favicon_router() -> Router {
    routes::build_favicon()
}

// Integration tests compile into this crate's test harness instead of a
// separate binary: every test binary is another link and, on macOS and
// Windows, another first-run malware scan. They still exercise only the
// public API; `extern crate self` lets them keep addressing it by crate name.
#[cfg(test)]
extern crate self as ai_memory_web;
// The fixtures seed pages through `Wiki::write_page`, which `clippy.toml`
// forbids in this crate's own code.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
#[path = "../tests/suite/mod.rs"]
mod integration;
