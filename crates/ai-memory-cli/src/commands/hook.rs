//! `ai-memory hook` — emit a single lifecycle event natively.
//!
//! Reads the event payload from stdin. Instead of POSTing synchronously on the
//! agent's hot path (which would block every tool call on the network and drop
//! events against a slow/remote server), the event is **spooled** locally — an
//! instant write — and the spool is drained to the server at session
//! boundaries (a cleanup pass on `session-start`, the main flush on
//! `session-end`). The one synchronous request is the `session-start` handoff
//! GET, whose result is injected back into the agent as context.
//!
//! See `docs/windows.md#native-hook-command-claude-code-on-windows`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_memory_llm::OidcToken;

use crate::cli::HookArgs;

use super::hook_capture::{build_client, extract_cwd, get_handoff, marker_query_suffix};
use super::hook_spool;

// All drain/handoff timings default to the values below and can be overridden
// (in milliseconds) by the matching env var, for very high-latency or
// large-backlog instances. Two kinds: per-request timeouts cap each individual
// POST / handoff GET; session-boundary budgets cap how long a boundary spends
// draining (so a boundary never hangs unbounded).
const DEFAULT_DRAIN_TIMEOUT_MS: u64 = 3000;
const DEFAULT_HANDOFF_TIMEOUT_MS: u64 = 3000;
const DEFAULT_START_BUDGET_MS: u64 = 3000;
const DEFAULT_END_BUDGET_MS: u64 = 10000;

/// Per-event POST timeout during a drain. Env: `AI_MEMORY_HOOK_DRAIN_TIMEOUT_MS`.
fn drain_event_timeout() -> Duration {
    env_ms("AI_MEMORY_HOOK_DRAIN_TIMEOUT_MS", DEFAULT_DRAIN_TIMEOUT_MS)
}
/// Synchronous handoff GET timeout. Env: `AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MS`.
fn handoff_timeout() -> Duration {
    env_ms(
        "AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MS",
        DEFAULT_HANDOFF_TIMEOUT_MS,
    )
}
/// Total budget for the `session-start` cleanup drain (kept tight so session
/// start stays snappy even when the server is down — leftovers wait). Env:
/// `AI_MEMORY_HOOK_START_BUDGET_MS`.
fn start_drain_budget() -> Duration {
    env_ms("AI_MEMORY_HOOK_START_BUDGET_MS", DEFAULT_START_BUDGET_MS)
}
/// Total budget for the `session-end` flush (the main delivery point; a session
/// boundary tolerates more). Env: `AI_MEMORY_HOOK_END_BUDGET_MS`.
fn end_drain_budget() -> Duration {
    env_ms("AI_MEMORY_HOOK_END_BUDGET_MS", DEFAULT_END_BUDGET_MS)
}

/// Read a positive-integer millisecond override from `name`, falling back to
/// `default_ms` for missing / empty / non-numeric / zero values.
fn env_ms(name: &str, default_ms: u64) -> Duration {
    parse_ms(std::env::var(name).ok(), default_ms)
}

fn parse_ms(raw: Option<String>, default_ms: u64) -> Duration {
    let ms = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}

/// Run a single hook end-to-end. Always returns Ok and always writes a JSON
/// object to stdout — a hook must never fail the agent.
///
/// `data_dir` is the resolved global `--data-dir` (if any); used to locate the
/// spool and the stored OIDC token.
pub async fn run(data_dir: Option<PathBuf>, args: HookArgs) -> anyhow::Result<()> {
    let mut payload = String::new();
    std::io::stdin().read_to_string(&mut payload).ok();
    let json: serde_json::Value = serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);

    let qs = extract_cwd(&json)
        .map(|cwd| marker_query_suffix(&cwd))
        .unwrap_or_default();
    let base = args.server_url.trim_end_matches('/');
    let dd = resolve_data_dir(data_dir.as_deref());
    let spool = hook_spool::spool_dir(&dd);

    // Spool THIS event — an instant local write, never the network. The auth
    // mode is decided without a round-trip: an explicit `--auth-token` is
    // stored inline; otherwise a present OIDC token marks the event `oidc`
    // (resolved + refreshed at drain time); otherwise anonymous.
    let oidc_present = args.auth_token.is_none()
        && OidcToken::load(&dd.join("auth.json"))
            .ok()
            .flatten()
            .is_some();
    let event_url = format!("{base}/hook?event={}&agent={}{qs}", args.event, args.agent);
    let entry = hook_spool::entry_for(
        event_url,
        payload.clone(),
        args.auth_token.as_deref(),
        oidc_present,
    );
    let _ = hook_spool::enqueue(&spool, &entry);

    // session-start: drain any backlog (e.g. from a previous session that ended
    // abruptly), then fetch + inject the pending handoff for the resuming agent.
    if args.event == "session-start" {
        let _ = hook_spool::drain(&spool, &dd, start_drain_budget(), drain_event_timeout()).await;
        let client = build_client();
        let bearer = hook_spool::resolve_bearer(&client, &dd, args.auth_token.as_deref()).await;
        let handoff_url = format!("{base}/handoff?agent={}{qs}", args.agent);
        if let Some(handoff) =
            get_handoff(&client, &handoff_url, bearer.as_deref(), handoff_timeout()).await
        {
            let envelope = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": handoff,
                }
            });
            println!("{envelope}");
            return Ok(());
        }
    }

    // session-end: the main delivery point — flush the session's spooled
    // observations (oldest-first) so the server has them before it consolidates.
    if args.event == "session-end" {
        let _ = hook_spool::drain(&spool, &dd, end_drain_budget(), drain_event_timeout()).await;
    }

    println!("{{}}");
    Ok(())
}

/// Resolve the data dir cheaply, without loading the full config (the hook
/// fast-path skips config for latency). Mirrors `config.rs`: explicit
/// `--data-dir`, else `AI_MEMORY_DATA_DIR`, else the platform local-data dir.
fn resolve_data_dir(data_dir: Option<&Path>) -> PathBuf {
    data_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("AI_MEMORY_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ai-memory")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ms_falls_back_on_invalid() {
        assert_eq!(parse_ms(None, 3000), Duration::from_millis(3000));
        assert_eq!(
            parse_ms(Some(String::new()), 3000),
            Duration::from_millis(3000)
        );
        assert_eq!(
            parse_ms(Some("abc".into()), 3000),
            Duration::from_millis(3000)
        );
        // Zero is rejected (a 0ms timeout would drop every request).
        assert_eq!(
            parse_ms(Some("0".into()), 3000),
            Duration::from_millis(3000)
        );
    }

    #[test]
    fn parse_ms_honours_valid_override() {
        assert_eq!(
            parse_ms(Some("8000".into()), 3000),
            Duration::from_millis(8000)
        );
        assert_eq!(
            parse_ms(Some("  6000 ".into()), 3000),
            Duration::from_millis(6000)
        );
    }
}
