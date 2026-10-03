//! Per-request actor context helpers for MCP tool handlers.
//!
//! ## Why this exists
//!
//! The auth middleware injects canonical [`ActorContext`] and [`AuthLevel`]
//! values into request extensions. Tool handlers receive raw HTTP `Parts` via
//! rmcp's `Extension<Parts>` extractor (rmcp 1.7+) and read those extensions;
//! raw actor headers are intentionally not trusted in handlers.
//!
//! ## Why not `tokio::task_local`
//!
//! We tried that first. `rmcp::transport::streamable_http_server::tower`
//! dispatches each tool handler via `tokio::spawn` (see
//! `tower.rs:569+619+1183`), which **does not** inherit task-locals from
//! the outer axum middleware. The Extension extractor is the official
//! supported path — see
//! <https://docs.rs/rmcp/1.7/rmcp/transport/streamable_http_server/struct.StreamableHttpService.html#accessing-http-request-data-from-tool-handlers>.
//!
//! ## Skip-list header
//!
//! `X-Memory-Skip-Admission-Chain` is a loop-prevention escape hatch for
//! trusted webhook re-entry. It is honored only for root/auth-disabled requests
//! (or test/stdio calls with no auth extension), so a regular DB user cannot
//! bypass a reject-policy admission webhook by setting a client-controlled
//! header.

use ai_memory_core::{ActorContext, AuthLevel, UserId};
use axum::http::HeaderMap;
use axum::http::request::Parts;

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Build an [`ActorContext`] from `X-Memory-Actor-*` headers.
///
/// Header names are matched case-insensitively (per HTTP spec).
#[must_use]
pub fn actor_from_headers(headers: &HeaderMap) -> ActorContext {
    ActorContext {
        agent: header_str(headers, "x-memory-actor-agent"),
        user: header_str(headers, "x-memory-actor-user"),
        issuer: header_str(headers, "x-memory-actor-issuer"),
        sub: header_str(headers, "x-memory-actor-sub"),
        client: header_str(headers, "x-memory-actor-client"),
        session_id: header_str(headers, "x-memory-actor-session-id"),
        // `name`/`email` aren't carried by the `X-Memory-Actor-*` bridge;
        // the four-rung auth middleware fills those when it injects the
        // canonical `Extension<ActorContext>` directly.
        ..ActorContext::default()
    }
}

/// Return the authenticated actor injected by the auth middleware. Raw
/// `X-Memory-Actor-*` headers are deliberately ignored here; handlers should
/// only trust middleware-resolved identity.
#[must_use]
pub fn actor_from_parts(parts: &Parts) -> ActorContext {
    parts
        .extensions
        .get::<ActorContext>()
        .cloned()
        .unwrap_or_else(ActorContext::anonymous)
}

/// Return the authenticated DB user's id when multi-user auth resolved one.
#[must_use]
pub fn author_id_from_parts(parts: &Parts) -> Option<UserId> {
    parts.extensions.get::<UserId>().copied()
}

fn header_value(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(ai_memory_core::SKIP_ADMISSION_CHAIN_HEADER)
        .and_then(|v| v.to_str().ok())
}

/// Parse the admission-chain loop-prevention skip list from the
/// `X-Memory-Skip-Admission-Chain` request header (comma-separated
/// webhook names). A webhook that writes back into the engine (e.g. via
/// `memory_write_page`) sets this so the chain doesn't re-invoke it on
/// the recursive write — see [`ai_memory_wiki::AdmissionContext::skip_webhooks`].
///
/// Returns an empty `Vec` when the header is absent. Entries are trimmed
/// and empty tokens dropped, so `"a, ,b,"` → `["a", "b"]`.
#[must_use]
pub fn skip_webhooks_from_headers(headers: &HeaderMap) -> Vec<String> {
    ai_memory_core::parse_skip_admission_chain(header_value(headers))
}

/// Parse the skip-list header only for trusted re-entry contexts.
#[must_use]
pub fn skip_webhooks_from_parts(parts: &Parts) -> Vec<String> {
    let level = parts
        .extensions
        .get::<AuthLevel>()
        .copied()
        .unwrap_or(AuthLevel::Anonymous);
    ai_memory_core::skip_admission_chain_for(level, header_value(&parts.headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn full_header_set_maps_correctly() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-memory-actor-agent",
            HeaderValue::from_static("claude-code"),
        );
        h.insert("x-memory-actor-user", HeaderValue::from_static("djalmajr"));
        h.insert(
            "x-memory-actor-issuer",
            HeaderValue::from_static("https://idp.example"),
        );
        h.insert("x-memory-actor-sub", HeaderValue::from_static("8f3a-uuid"));
        h.insert(
            "x-memory-actor-client",
            HeaderValue::from_static("72836f52-uuid"),
        );
        h.insert(
            "x-memory-actor-session-id",
            HeaderValue::from_static("019e6d-session"),
        );
        let ctx = actor_from_headers(&h);
        assert_eq!(ctx.agent.as_deref(), Some("claude-code"));
        assert_eq!(ctx.user.as_deref(), Some("djalmajr"));
        assert_eq!(ctx.issuer.as_deref(), Some("https://idp.example"));
        assert_eq!(ctx.sub.as_deref(), Some("8f3a-uuid"));
        assert_eq!(ctx.client.as_deref(), Some("72836f52-uuid"));
        assert_eq!(ctx.session_id.as_deref(), Some("019e6d-session"));
        assert!(ctx.has_any());
    }

    #[test]
    fn missing_headers_leave_none() {
        let h = HeaderMap::new();
        let ctx = actor_from_headers(&h);
        assert!(ctx.agent.is_none());
        assert!(ctx.user.is_none());
        assert!(ctx.issuer.is_none());
        assert!(ctx.sub.is_none());
        assert!(ctx.client.is_none());
        assert!(ctx.session_id.is_none());
        assert!(!ctx.has_any());
    }

    #[test]
    fn empty_or_whitespace_header_treated_as_none() {
        let mut h = HeaderMap::new();
        h.insert("x-memory-actor-agent", HeaderValue::from_static("   "));
        h.insert("x-memory-actor-user", HeaderValue::from_static(""));
        let ctx = actor_from_headers(&h);
        assert!(ctx.agent.is_none(), "whitespace must trim to None");
        assert!(ctx.user.is_none(), "empty must be None");
        assert!(!ctx.has_any());
    }

    #[test]
    fn case_insensitive_header_lookup() {
        // HeaderMap normalises names to lowercase on insert; this verifies
        // that the canonical `X-Memory-Actor-Agent` form also resolves.
        let mut h = HeaderMap::new();
        h.insert("X-Memory-Actor-Agent", HeaderValue::from_static("codex"));
        let ctx = actor_from_headers(&h);
        assert_eq!(ctx.agent.as_deref(), Some("codex"));
    }

    #[test]
    fn skip_webhooks_parses_csv_trims_and_drops_empties() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-memory-skip-admission-chain",
            HeaderValue::from_static("contributors, ,git-mirror,"),
        );
        assert_eq!(
            skip_webhooks_from_headers(&h),
            vec!["contributors".to_string(), "git-mirror".to_string()]
        );
    }

    #[test]
    fn skip_webhooks_absent_header_is_empty() {
        let h = HeaderMap::new();
        assert!(skip_webhooks_from_headers(&h).is_empty());
    }

    #[test]
    fn actor_from_parts_prefers_authenticated_extension_over_headers() {
        let mut req = axum::http::Request::builder().uri("/mcp").body(()).unwrap();
        req.headers_mut().insert(
            "x-memory-actor-user",
            HeaderValue::from_static("spoofed-user"),
        );
        req.extensions_mut().insert(ActorContext {
            user: Some("real-user".into()),
            ..ActorContext::default()
        });
        let parts = req.into_parts().0;

        let actor = actor_from_parts(&parts);
        assert_eq!(actor.user.as_deref(), Some("real-user"));
    }

    #[test]
    fn skip_header_is_ignored_for_db_user_requests() {
        let mut req = axum::http::Request::builder().uri("/mcp").body(()).unwrap();
        req.headers_mut().insert(
            "x-memory-skip-admission-chain",
            HeaderValue::from_static("validator"),
        );
        req.extensions_mut().insert(AuthLevel::User);
        let parts = req.into_parts().0;

        assert!(skip_webhooks_from_parts(&parts).is_empty());
    }
}
