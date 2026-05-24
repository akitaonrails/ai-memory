//! `ai-memory-web` — read-only HTTP browser for the wiki.
//!
//! Mounted under `/web` on the same axum server that hosts the MCP
//! endpoint, so a single port + single auth posture covers both. The
//! crate is deliberately read-only in v1: no editing, no POST routes,
//! no agent-write APIs. The wiki is already markdown-on-disk; this
//! surface just makes it browsable from a phone, a tablet, or a
//! teammate's machine without `docker exec cat …`.
//!
//! Routes (all under whatever prefix the host nests this router at):
//! - `GET /`                              → project list (cards)
//! - `GET /w/:workspace/:project`         → page tree + recent activity
//! - `GET /w/:workspace/:project/p/*path` → rendered markdown + metadata
//! - `GET /search?q=…`                    → FTS5 hit list
//! - `GET /static/*`                      → embedded CSS + logos
//!
//! The companion `api_router` exposes the same read-only data as JSON
//! for custom frontends. It intentionally does not expose write/admin
//! operations.
//!
//! Theme follows `prefers-color-scheme` via the included Tailwind
//! stylesheet; no JS toggle, no cookie.

use std::sync::Arc;

use ai_memory_store::ReaderPool;
use ai_memory_wiki::Wiki;
use axum::Router;

mod markdown;
mod routes;
mod state;
mod templates;

pub use state::WebState;

/// Build the read-only web router. Call once at server startup and
/// `nest("/web", router)` it onto the existing axum app, OR mount at
/// `/` if the web UI is the only HTTP surface.
pub fn router(reader: ReaderPool, wiki: Wiki) -> Router {
    let state = Arc::new(WebState::new(reader, wiki));
    routes::build(state)
}

/// Build the read-only JSON API router for third-party web UIs.
///
/// The host should `nest("/api/v1", api_router(...))` alongside `/web`
/// so custom frontends can browse memory without reading SQLite or wiki
/// files directly.
pub fn api_router(reader: ReaderPool, wiki: Wiki) -> Router {
    let state = Arc::new(WebState::new(reader, wiki));
    routes::build_api(state)
}
