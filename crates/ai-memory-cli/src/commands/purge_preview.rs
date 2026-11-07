//! Shared best-effort dry-run preview machinery for `purge-project` and
//! `purge-session`.
//!
//! Both subcommands refuse without `--confirm`, but first ask the server for
//! a bounded preview (`"dry_run": true`, which wins over `confirm`
//! server-side) of what the confirmed run would do. The HTTP round trip, its
//! timeout, and the classification of the result into "print this" vs "stay
//! silent" are identical between the two commands — only the request body's
//! shape, the endpoint path, and the summary line's wording differ, so those
//! stay in `purge_project.rs` / `purge_session.rs` and this module holds only
//! what would otherwise be copy-pasted between them.

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::config::Config;
use crate::http_client::{ServerEndpoint, ServerResponseError, post_json};

/// How long the preview request (auth resolution plus the HTTP round trip)
/// is allowed to take before this falls back to the plain refusal. The
/// preview is optional information layered on top of a refusal that must
/// still happen either way, so it must never be the reason `--confirm` takes
/// noticeably longer than it used to.
pub const PREVIEW_TIMEOUT: Duration = Duration::from_secs(5);

/// What became of the best-effort preview request, reduced to what deciding
/// whether — and what — to print needs. Kept separate from the network call
/// itself so the printing decision is a pure function and testable without a
/// server.
pub enum PreviewOutcome {
    /// A 200 with `"dry_run": true`: the server understood the request and
    /// ran the preview.
    Previewed(serde_json::Value),
    /// A 200 without `dry_run` set: an old-enough server both predates the
    /// field AND happens to 200 an unrecognized shape. Treated the same as
    /// not getting a preview at all.
    Ignored,
    /// A non-2xx response with a body worth showing: the scope resolved to
    /// something the operator should know about before the refusal (a 404
    /// naming the missing project/session, a 409 naming a live managed run,
    /// a 403 naming the auth problem), or an unexpected status this command
    /// has no specific handling for.
    Refused { status: u16, body: String },
    /// The request predates `dry_run` support (400, the pre-existing
    /// "confirm=true" refusal body) — not worth repeating, since `run` below
    /// prints its own version of exactly that message next regardless.
    OlderServer,
    /// Timed out or never reached a server at all (DNS/connect failure,
    /// auth-refresh hang, etc). Indistinguishable from the operator's
    /// perspective, and neither is this command's business to diagnose.
    Unreachable,
}

/// Run the preview request under [`PREVIEW_TIMEOUT`] and classify the
/// result.
///
/// The preview goes to the server the confirmed purge would reach:
/// [`ServerEndpoint::for_checkout`] picks the server profile this checkout's
/// marker selects, with that profile's own token, and the install's server
/// otherwise (#2016). Auth resolution (which can itself refresh an OIDC token
/// over the network) runs inside the same timeout so a hung refresh cannot
/// silently make a `--confirm`-less purge command block far longer than the
/// rest of it ever has.
///
/// # Errors
/// A marker that selects a server profile which does not resolve. The
/// confirmed purge fails closed on it, and so does the preview: it is sent
/// nowhere rather than to the install's server with this checkout's names.
pub async fn run_preview<Req: Serialize>(
    config: &Config,
    path: &str,
    request: &Req,
) -> Result<PreviewOutcome> {
    let attempt = tokio::time::timeout(PREVIEW_TIMEOUT, async {
        let endpoint = ServerEndpoint::for_checkout(config).await?;
        Ok::<_, anyhow::Error>(post_json::<_, serde_json::Value>(&endpoint, path, request).await)
    })
    .await;

    let Ok(routed) = attempt else {
        return Ok(PreviewOutcome::Unreachable);
    };
    Ok(match routed? {
        Ok(report) if report["dry_run"].as_bool().unwrap_or(false) => {
            PreviewOutcome::Previewed(report)
        }
        Ok(_) => PreviewOutcome::Ignored,
        Err(e) => match e.downcast_ref::<ServerResponseError>() {
            Some(resp) if resp.status().as_u16() == 400 => PreviewOutcome::OlderServer,
            Some(resp) => PreviewOutcome::Refused {
                status: resp.status().as_u16(),
                body: resp.body().to_string(),
            },
            // Not an HTTP response at all: connect/DNS failure, request
            // timeout already handled above, or a body that failed to
            // deserialize as JSON.
            None => PreviewOutcome::Unreachable,
        },
    })
}

/// Format a `PreviewOutcome::Refused` body as the one line every caller
/// prints before its refusal: `Preview refused (<status>): <message>`. The
/// message is the server's own `{"error": ...}` string when the body parses
/// that way, and the raw body otherwise. Both are sanitized for terminal output.
pub fn refused_message(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.to_string());
    format!(
        "Preview refused ({status}): {}",
        super::show::terminal_text(&message)
    )
}
