//! MCP server for ai-memory.
//!
//! Hosts an [`AiMemoryServer`] that wraps the storage layer and exposes a
//! deliberately narrow tool surface to coding agents. Read-only tools live
//! here; write tools (M3+) and consolidation tools (M7+) follow.
//!
//! Pin the MCP protocol version explicitly so we never fall into the
//! agentmemory #510 / #553 "negotiated-down to a version the client
//! discards tools for" failure mode.

pub mod actor;
pub mod admin;
pub mod auth;
pub mod human_auth;
mod server;

pub use actor::actor_from_headers;
pub use admin::{
    AdminState, ScopeInvalidation, ScopeInvalidator, admin_router, admin_router_with_decay_breadth,
    admin_router_with_sweep_tuning,
};
pub use human_auth::{
    HumanAuthRuntime, expire_legacy_cookie_mw, internal_auth_router, public_auth_router,
    require_dual_auth, session_auth_router,
};
pub use server::{AiMemoryServer, MEMORY_INSTRUCTIONS};
