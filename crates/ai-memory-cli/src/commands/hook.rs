//! `ai-memory hook` — emit a single lifecycle event natively.
//!
//! Reads the event payload from stdin. Instead of POSTing synchronously on the
//! agent's hot path (which would block every tool call on the network and drop
//! events against a slow/remote server), the event is **spooled** locally — an
//! instant write. `session-start` performs a short, lock-aware synchronous
//! cleanup pass before fetching handoff context. `session-end` returns quickly:
//! after enqueue it spawns a detached `hook-drain` process, whose stdout/stderr
//! are redirected away from the agent, and that process drains under an
//! exclusive spool lock with a longer bounded budget.
//!
//! See `docs/windows.md#native-hook-command-claude-code-on-windows`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_memory_core::{AgentKind, ManagedRunId, SessionId};
use ai_memory_hooks::capture_policy::metadata_only_body;
use ai_memory_hooks::{CaptureDisposition, HookEvent, PolicyState};
use ai_memory_llm::OidcToken;

use crate::cli::HookArgs;

use sha2::{Digest as _, Sha256};

use super::hook_capture::{
    build_client, canonical_context, capture_policy, extract_cwd, get_handoff, marker_query_suffix,
    marker_query_suffix_without_briefing, marker_requests_briefing, resolve_cwd_with_fallbacks,
    url_encode,
};
use super::hook_drain_process;
use super::hook_spool;
use super::path_util::strip_windows_verbatim_prefix;

// All drain/handoff timings default to the current short values and can be
// overridden by whole-minute env vars for very high-latency or large-backlog
// instances. Two kinds: per-request timeouts cap each individual POST / handoff
// GET; session-boundary budgets cap how long a boundary spends draining (so a
// boundary never hangs unbounded).
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_HANDOFF_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_START_BUDGET: Duration = Duration::from_secs(3);
const DEFAULT_BACKGROUND_DRAIN_BUDGET: Duration = Duration::from_secs(5 * 60);
const MAX_OVERRIDE_MINUTES: u64 = 60;

const DRAIN_TIMEOUT_ENV: &str = "AI_MEMORY_HOOK_DRAIN_TIMEOUT_MINUTES";
const HANDOFF_TIMEOUT_ENV: &str = "AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MINUTES";
const START_BUDGET_ENV: &str = "AI_MEMORY_HOOK_START_BUDGET_MINUTES";
const BACKGROUND_DRAIN_BUDGET_ENV: &str = "AI_MEMORY_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES";

const INCREMENTAL_THRESHOLD_ENV: &str = "AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD";
const MANAGED_RUN_ENV: &str = "AI_MEMORY_RUN_ID";
/// Backlog size at which `post-tool-use` does a mid-session catch-up drain, so a
/// light session pays only a `read_dir`. Override via the env var above.
const DEFAULT_INCREMENTAL_THRESHOLD: usize = 32;
const MAX_BRIEFED_MARKERS: usize = 512;
/// Total budget AND per-event timeout for the mid-session catch-up drain — kept
/// well under a second so a `post-tool-use` hook never stalls a tool call (one
/// in-flight POST against a slow server is bounded by this too).
const INCREMENTAL_DRAIN_BUDGET: Duration = Duration::from_millis(250);

/// Per-event POST timeout during a drain. Env: `AI_MEMORY_HOOK_DRAIN_TIMEOUT_MINUTES`.
fn drain_event_timeout() -> Duration {
    drain_event_timeout_from(env_lookup)
}
/// Synchronous handoff GET timeout. Env: `AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MINUTES`.
fn handoff_timeout() -> Duration {
    handoff_timeout_from(env_lookup)
}
/// Total budget for the `session-start` cleanup drain (kept tight so session
/// start stays snappy even when the server is down — leftovers wait). Env:
/// `AI_MEMORY_HOOK_START_BUDGET_MINUTES`.
fn start_drain_budget() -> Duration {
    start_drain_budget_from(env_lookup)
}
/// Total budget for detached background drains. Env:
/// `AI_MEMORY_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES`.
fn background_drain_budget() -> Duration {
    background_drain_budget_from(env_lookup)
}

fn drain_event_timeout_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(DRAIN_TIMEOUT_ENV, DEFAULT_DRAIN_TIMEOUT, lookup)
}

fn handoff_timeout_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(HANDOFF_TIMEOUT_ENV, DEFAULT_HANDOFF_TIMEOUT, lookup)
}

fn start_drain_budget_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(START_BUDGET_ENV, DEFAULT_START_BUDGET, lookup)
}

fn background_drain_budget_from(lookup: impl FnMut(&str) -> Option<String>) -> Duration {
    env_minutes(
        BACKGROUND_DRAIN_BUDGET_ENV,
        DEFAULT_BACKGROUND_DRAIN_BUDGET,
        lookup,
    )
}

/// Backlog size at which `post-tool-use` triggers a mid-session catch-up drain.
/// Env: `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD` (positive integer).
fn incremental_drain_threshold() -> usize {
    incremental_drain_threshold_from(env_lookup)
}

fn incremental_drain_threshold_from(mut lookup: impl FnMut(&str) -> Option<String>) -> usize {
    lookup(INCREMENTAL_THRESHOLD_ENV)
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_INCREMENTAL_THRESHOLD)
}

/// Whether to run a mid-session catch-up drain for this event: only
/// `post-tool-use` (the highest-frequency event) and only once the spool backlog
/// has crossed `threshold`. Boundaries run their own cleanup/background drains,
/// so a light session never drains mid-session.
fn should_incremental_drain(event: &str, spool_len: usize, threshold: usize) -> bool {
    event == "post-tool-use" && spool_len >= threshold
}

fn spawn_background_drainer(data_dir: &Path) -> std::io::Result<()> {
    hook_drain_process::spawn(data_dir)
}

fn should_spawn_background_drainer(event: &str) -> bool {
    matches!(event, "session-end" | "stop" | "pre-compact")
}

fn session_id_state_path(data_dir: &Path, agent: AgentKind) -> PathBuf {
    data_dir
        .join("hook-state")
        .join(format!("{}-session-id", agent.as_str()))
}

fn stored_session_id(data_dir: &Path, agent: AgentKind) -> Option<String> {
    let path = session_id_state_path(data_dir, agent);
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn store_session_id(data_dir: &Path, agent: AgentKind, session_id: &str) {
    let path = session_id_state_path(data_dir, agent);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, session_id);
}

fn clear_session_id(data_dir: &Path, agent: AgentKind) {
    let _ = fs::remove_file(session_id_state_path(data_dir, agent));
}

/// `<data_dir>/briefed/<key>` — records that the compiled project brief
/// (`[briefing] inject_on_session_start`) was already delivered for this
/// session by the user-prompt handoff path. kimi-code discards SessionStart
/// hook stdout, so the brief rides the FIRST user prompt of the session
/// (parity with Claude's once-per-SessionStart brief); the marker keeps
/// later prompts from re-requesting it. Keyed by the payload's canonical
/// session id when Kimi supplies one; payloads without one fall back to a
/// stable hash of agent+cwd so a session-less agent still briefs once per
/// checkout. The key is sanitized to a safe file name.
fn briefed_marker_path(
    data_dir: &Path,
    agent: &str,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> PathBuf {
    let key = session_id.map_or_else(
        || {
            format!(
                "{:x}",
                Sha256::digest(format!("{agent}\n{}", cwd.unwrap_or_default()).as_bytes())
            )
        },
        sanitize_briefed_key,
    );
    data_dir.join("briefed").join(key)
}

fn sanitize_briefed_key(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Best-effort marker write with bounded retention: on failure the worst case
/// is a re-brief on the next prompt, which is acceptable.
fn mark_briefed(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() || fs::write(path, b"").is_err() {
        return;
    }

    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let mut stale_candidates = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then_some((
                metadata.modified().ok(),
                entry.file_name(),
                entry.path(),
            ))
        })
        .filter(|(_, _, candidate)| candidate != path)
        .collect::<Vec<_>>();
    stale_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let keep_others = MAX_BRIEFED_MARKERS.saturating_sub(1);
    for (_, _, stale) in stale_candidates.into_iter().skip(keep_others) {
        let _ = fs::remove_file(stale);
    }
}

fn fresh_session_id(data_dir: &Path, agent: AgentKind) -> String {
    let session_id = SessionId::new().to_string();
    store_session_id(data_dir, agent, &session_id);
    session_id
}

fn payload_has_session_id(raw: &serde_json::Value) -> bool {
    [
        "session_id",
        "sessionId",
        "sessionID",
        "session",
        "conversationId",
    ]
    .iter()
    .any(|key| {
        raw.get(*key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

fn session_id_query_suffix(
    data_dir: &Path,
    agent: &str,
    event: &str,
    raw: &serde_json::Value,
) -> String {
    let agent_kind = AgentKind::from_wire(agent);
    if agent_kind != AgentKind::Devin || payload_has_session_id(raw) {
        return String::new();
    }

    let session_id = if event == "session-start" {
        fresh_session_id(data_dir, agent_kind)
    } else {
        stored_session_id(data_dir, agent_kind)
            .unwrap_or_else(|| fresh_session_id(data_dir, agent_kind))
    };
    format!("&session_id={}", url_encode(&session_id))
}

fn cwd_query_suffix_with(
    agent: &str,
    raw: &serde_json::Value,
    default_strategy: Option<&str>,
    env_lookup: impl FnMut(&str) -> Option<String>,
    current_dir: impl FnOnce() -> Option<PathBuf>,
) -> String {
    let agent_kind = AgentKind::from_wire(agent);
    let (canonical_cwd, _) = canonical_context(raw);
    let cwd = if canonical_cwd.is_some() {
        canonical_cwd
    } else if agent_kind == AgentKind::Devin {
        resolve_cwd_with_fallbacks(raw, env_lookup, current_dir)
    } else {
        extract_cwd(raw).filter(|s| !s.trim().is_empty())
    };
    cwd.map(|cwd| marker_query_suffix(&cwd, default_strategy))
        .unwrap_or_default()
}

fn cwd_query_suffix(
    agent: &str,
    raw: &serde_json::Value,
    default_strategy: Option<&str>,
) -> String {
    cwd_query_suffix_with(agent, raw, default_strategy, env_lookup, || {
        std::env::current_dir().ok()
    })
}

fn after_background_drain_event_enqueue(
    data_dir: &Path,
    spawn: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    spawn(data_dir)
}

/// Hidden drain-only fast path. Reads no stdin and writes no stdout.
pub async fn run_drain(data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let dd = resolve_data_dir(data_dir.as_deref());
    let spool = hook_spool::spool_dir(&dd);
    match hook_spool::drain_until_quiescent(
        &spool,
        &dd,
        background_drain_budget(),
        drain_event_timeout(),
        hook_spool::DrainLockWait::Bounded(Duration::from_secs(30)),
    )
    .await
    {
        Ok(hook_spool::LockedDrainResult::Drained(_))
        | Ok(hook_spool::LockedDrainResult::LockBusy) => {}
        Err(err) => eprintln!("ai-memory hook-drain warning: failed to acquire drain lock: {err}"),
    }
    Ok(())
}

fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn managed_run_query_suffix_with(mut env_lookup: impl FnMut(&str) -> Option<String>) -> String {
    env_lookup(MANAGED_RUN_ENV)
        .filter(|value| value.parse::<ManagedRunId>().is_ok())
        .map_or_else(String::new, |value| {
            format!("&managed_run={}", url_encode(&value))
        })
}

fn managed_run_query_suffix() -> String {
    managed_run_query_suffix_with(env_lookup)
}

/// Read a positive-integer minute override from `name`, falling back to the
/// built-in short default for missing / empty / non-numeric / zero values. Clamp
/// large values so a typo cannot block a hook boundary for hours or days.
fn env_minutes(
    name: &str,
    default: Duration,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Duration {
    parse_minutes(lookup(name), default)
}

fn parse_minutes(raw: Option<String>, default: Duration) -> Duration {
    let minutes = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(|n| n.min(MAX_OVERRIDE_MINUTES));
    match minutes {
        Some(n) => Duration::from_secs(n * 60),
        None => default,
    }
}

/// Run a single hook end-to-end. Always returns Ok and always writes a JSON
/// object to stdout — a hook must never fail the agent.
///
/// `data_dir` is the resolved global `--data-dir` (if any); used to locate the
/// spool and the stored OIDC token.
pub async fn run(data_dir: Option<PathBuf>, args: HookArgs) -> anyhow::Result<()> {
    let mut payload = String::new();
    std::io::stdin().read_to_string(&mut payload).ok();
    let mut stdout = std::io::stdout();
    run_with_payload(
        data_dir,
        args,
        payload,
        &mut stdout,
        spawn_background_drainer,
    )
    .await
}

async fn run_with_payload<W, S>(
    data_dir: Option<PathBuf>,
    args: HookArgs,
    payload: String,
    stdout: &mut W,
    spawn_background_drainer: S,
) -> anyhow::Result<()>
where
    W: std::io::Write,
    S: FnOnce(&Path) -> std::io::Result<()>,
{
    let (mut payload, mut json) = match parse_hook_payload(payload) {
        Ok(parsed) => parsed,
        Err(_) => {
            eprintln!(
                "ai-memory hook warning: could not parse event payload as JSON; nothing was captured"
            );
            writeln!(stdout, "{{}}")?;
            return Ok(());
        }
    };
    // Assistant/Stop capture (#196). On an opted-in install
    // (`install-hooks --capture-assistant`), extract the assistant message,
    // sanitize + cap it, and splice the versioned `_ai_memory_assistant` marker
    // into the body. Otherwise just strip any raw assistant field defensively —
    // the field is never persisted until the server-gated opt-in accepts it.
    // Reserialize only when the JSON actually changed, so unrelated events keep
    // byte-exact spool bodies (see `native_hook_accepts_plain_and_bom_prefixed_json`).
    let capture_assistant = if args.capture_assistant {
        let transform = ai_memory_hooks::transform_for_client(
            &mut json,
            AgentKind::from_wire(&args.agent),
            ai_memory_hooks::HookEvent::parse(&args.event),
        );
        if transform.changed {
            payload = serde_json::to_string(&json)?;
        }
        transform.captured
    } else {
        if ai_memory_hooks::strip_assistant_message_raw(&mut json) {
            payload = serde_json::to_string(&json)?;
        }
        false
    };
    let (policy_cwd, canonical_session_id) = hook_context(&args.agent, &json);
    let policy = policy_cwd.as_deref().map(capture_policy);
    let tool_event = is_tool_event(&args.event);
    let decision = policy.as_ref().filter(|_| tool_event).map(|policy| {
        policy.inspect(
            AgentKind::from_wire(&args.agent),
            &json,
            policy_cwd.as_deref().unwrap_or(""),
        )
    });
    if args.check_capture {
        let protocol = decision.as_ref().map(|decision| decision.protocol());
        let output = serde_json::json!({
            "version": protocol.map_or(1, |protocol| protocol.version()),
            "policy_state": protocol.map_or(PolicyState::Inactive, |protocol| protocol.policy_state()),
            "tool_family": protocol.map_or(ai_memory_hooks::ToolFamily::Unknown, |protocol| protocol.tool_family()),
            "path_count": protocol.map_or(0, |protocol| protocol.path_count()),
            "disposition": protocol.map_or(CaptureDisposition::Keep, |protocol| protocol.disposition()),
            "extraction_state": protocol.map_or(ai_memory_hooks::ExtractionState::NotApplicable, |protocol| protocol.extraction_state()),
        });
        writeln!(stdout, "{output}")?;
        return Ok(());
    }
    if let Some(decision) = decision {
        match decision.protocol().disposition() {
            CaptureDisposition::Drop => {
                writeln!(stdout, "{{}}")?;
                return Ok(());
            }
            CaptureDisposition::MetadataOnly => {
                json = metadata_only_body(
                    canonical_session_id.as_deref(),
                    policy_cwd.as_deref(),
                    &decision,
                );
                payload = serde_json::to_string(&json)?;
            }
            CaptureDisposition::Keep
                if decision.protocol().policy_state() != PolicyState::Inactive =>
            {
                if let Some(object) = json.as_object_mut() {
                    object.insert(
                        "_ai_memory_capture".into(),
                        serde_json::to_value(decision.protocol())?,
                    );
                    payload = serde_json::to_string(&json)?;
                }
            }
            CaptureDisposition::Keep => {}
        }
    }

    let qs = cwd_query_suffix(
        &args.agent,
        &json,
        args.project_strategy.and_then(|s| s.baked()),
    );
    let base = args.server_url.trim_end_matches('/');
    let dd = resolve_data_dir(data_dir.as_deref());
    let spool = hook_spool::spool_dir(&dd);
    let session_qs = session_id_query_suffix(&dd, &args.agent, &args.event, &json);
    let managed_qs = managed_run_query_suffix();
    let hook_qs = format!("{qs}{session_qs}{managed_qs}");

    // Spool THIS event — an instant local write, never the network. The auth
    // mode is decided without a round-trip: an explicit `--auth-token` is
    // stored inline; otherwise a present OIDC token marks the event `oidc`
    // (resolved + refreshed at drain time); otherwise anonymous.
    let oidc_present = args.auth_token.is_none()
        && OidcToken::load(&dd.join("auth.json"))
            .ok()
            .flatten()
            .is_some();
    // Append the opt-in capture flag outside the marker query so the server can
    // gate on it (#196). Only present when a valid protocol was actually spliced
    // into this event's body, so the flag and the marker always travel together.
    let capture_qs = if capture_assistant {
        "&capture_assistant=1"
    } else {
        ""
    };
    // Idempotency key: minted ONCE here and baked into the spooled URL, so
    // every retry of this entry re-sends the same key. The server claims it
    // atomically with the observation row and skips replays whose previous
    // delivery succeeded but whose response was lost — closing the
    // conservative-retry duplication vector. Older servers ignore the param.
    let ingest_key = uuid::Uuid::new_v4().simple().to_string();
    let event_url = format!(
        "{base}/hook?event={}&agent={}{}{}&ingest_key={ingest_key}",
        args.event, args.agent, hook_qs, capture_qs
    );
    let entry = hook_spool::entry_for(
        event_url,
        payload.clone(),
        args.auth_token.as_deref(),
        oidc_present,
    );
    if hook_spool::enqueue(&spool, &entry).is_err() {
        eprintln!(
            "ai-memory hook warning: failed to spool lifecycle event; capture for this event was skipped"
        );
    }
    if AgentKind::from_wire(&args.agent) == AgentKind::Devin && args.event == "session-end" {
        clear_session_id(&dd, AgentKind::Devin);
    }

    // Mid-session catch-up: per-event hooks only enqueue, so a heavy session
    // outpaces the boundary-only drain and the spool grows until the next
    // boundary. On `post-tool-use`, once the backlog crosses the threshold, do a
    // tightly time-boxed drain (budget == per-event timeout, sub-second) so the
    // spool stays flat without ever stalling a tool call.
    if should_incremental_drain(
        &args.event,
        hook_spool::spool_len(&spool),
        incremental_drain_threshold(),
    ) {
        let _ = hook_spool::drain_exclusive(
            &spool,
            &dd,
            INCREMENTAL_DRAIN_BUDGET,
            INCREMENTAL_DRAIN_BUDGET,
            hook_spool::DrainLockWait::NoWait,
        )
        .await;
    }

    // session-start: drain any backlog (e.g. from a previous session that ended
    // abruptly), then fetch + inject the pending handoff for the resuming agent.
    if args.event == "session-start" {
        let _ = hook_spool::drain_exclusive_within_budget(
            &spool,
            &dd,
            start_drain_budget(),
            drain_event_timeout(),
        )
        .await;
        // Only fetch the handoff for agents that inject the session-start
        // hook's stdout as context. Grok ignores it, so fetching here would
        // consume the handoff server-side (the GET is destructive) and then
        // discard the result — silently losing it. Those agents recover the
        // handoff on demand via the MCP `memory_handoff_accept` tool.
        if AgentKind::from_wire(&args.agent).session_start_injects_handoff() {
            let client = build_client();
            let bearer = hook_spool::resolve_bearer(&client, &dd, args.auth_token.as_deref()).await;
            let native_session_qs = canonical_session_id
                .as_deref()
                .map_or_else(String::new, |session_id| {
                    format!("&session_id={}", url_encode(session_id))
                });
            let handoff_url = format!(
                "{base}/handoff?agent={}{qs}{managed_qs}{native_session_qs}",
                args.agent
            );
            if let Some(handoff) =
                get_handoff(&client, &handoff_url, bearer.as_deref(), handoff_timeout()).await
            {
                let envelope = serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "SessionStart",
                        "additionalContext": handoff,
                    }
                });
                writeln!(stdout, "{envelope}")?;
                return Ok(());
            }
        }
    }

    // user-prompt: agents whose SessionStart stdout is discarded (Kimi Code)
    // receive the handoff here instead — kimi injects UserPromptSubmit stdout
    // into the turn verbatim as a `hook_result` user message. The payload
    // carries the native session id when available, so the destructive GET
    // can also link the managed run to the native session, same as
    // session-start does.
    // The installed kimi hook passes the script stem (`user-prompt-submit`)
    // while the legacy shell path posts `user-prompt`; HookEvent::parse
    // canonicalizes both (and the snake/native spellings) to UserPrompt.
    if HookEvent::parse(&args.event) == HookEvent::UserPrompt
        && AgentKind::from_wire(&args.agent).user_prompt_injects_handoff()
    {
        let client = build_client();
        let bearer = hook_spool::resolve_bearer(&client, &dd, args.auth_token.as_deref()).await;
        let native_session_qs = canonical_session_id
            .as_deref()
            .map_or_else(String::new, |session_id| {
                format!("&session_id={}", url_encode(session_id))
            });
        // Gate the `[briefing]` params to the FIRST user prompt of the
        // session: kimi has no working SessionStart injection, so the brief
        // is delivered here exactly once (parity with Claude); afterwards
        // the handoff fetch continues on every prompt — it is cheap and
        // self-limiting (empty body when nothing is pending) — but without
        // `&briefing`/`&briefing_budget`, so the server does not recompose
        // the brief per prompt. The marker survives `/clear`, so re-briefing
        // after a context clear is not supported in v1.
        let briefed_path = policy_cwd
            .as_deref()
            .filter(|cwd| marker_requests_briefing(cwd))
            .map(|_| {
                briefed_marker_path(
                    &dd,
                    &args.agent,
                    canonical_session_id.as_deref(),
                    policy_cwd.as_deref(),
                )
            });
        let handoff_qs = if briefed_path.as_ref().is_some_and(|path| path.is_file()) {
            policy_cwd
                .as_deref()
                .map(|cwd| {
                    marker_query_suffix_without_briefing(
                        cwd,
                        args.project_strategy.and_then(|s| s.baked()),
                    )
                })
                .unwrap_or_default()
        } else {
            qs.clone()
        };
        let handoff_url = format!(
            "{base}/handoff?agent={}{handoff_qs}{managed_qs}{native_session_qs}",
            args.agent
        );
        let handoff =
            get_handoff(&client, &handoff_url, bearer.as_deref(), handoff_timeout()).await;
        // Mark the session as briefed only AFTER the GET completed — success
        // OR error. Fail-open on purpose: with the server down, repeating
        // the brief-flagged request on every prompt would not deliver
        // anything anyway, and the one lost brief is recovered on the next
        // session.
        if let Some(path) = briefed_path.as_deref() {
            mark_briefed(path);
        }
        if let Some(handoff) = handoff {
            writeln!(stdout, "{handoff}")?;
        }
        // Never print the usual `{}` here: kimi injects any non-empty stdout
        // into the turn verbatim, so an envelope would become user-visible
        // text. Empty handoff or any fetch error means print nothing at all
        // (kimi ignores empty stdout; warnings go to stderr).
        return Ok(());
    }

    // Boundary drain trigger: enqueue first, then ask a detached native drainer
    // to flush the shared spool. `session-end` remains the primary close path,
    // but `stop` and `pre-compact` also trigger the helper so delivery does not
    // rely on the single hook most likely to be cancelled during agent shutdown.
    if should_spawn_background_drainer(&args.event)
        && let Err(err) = after_background_drain_event_enqueue(&dd, spawn_background_drainer)
    {
        eprintln!(
            "ai-memory hook warning: failed to start background spool drainer; event remains queued: {err}"
        );
    }

    writeln!(stdout, "{{}}")?;
    Ok(())
}

fn parse_hook_payload(mut payload: String) -> serde_json::Result<(String, serde_json::Value)> {
    if payload.starts_with('\u{feff}') {
        payload.drain(..'\u{feff}'.len_utf8());
    }
    let json = serde_json::from_str(&payload)?;
    Ok((payload, json))
}

fn hook_context(agent: &str, raw: &serde_json::Value) -> (Option<String>, Option<String>) {
    let (cwd, session_id) = canonical_context(raw);
    if cwd.is_some() {
        return (cwd, session_id);
    }
    if AgentKind::from_wire(agent) == AgentKind::Devin {
        (
            resolve_cwd_with_fallbacks(raw, env_lookup, || std::env::current_dir().ok()),
            session_id,
        )
    } else {
        (
            extract_cwd(raw).filter(|cwd| !cwd.trim().is_empty()),
            session_id,
        )
    }
}

fn is_tool_event(event: &str) -> bool {
    matches!(
        event.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
        "pretooluse" | "posttooluse" | "tooluse"
    )
}

/// Resolve the data dir cheaply, without loading the full config (the hook
/// fast-path skips config for latency). Mirrors `config.rs`: explicit
/// `--data-dir`, else `AI_MEMORY_DATA_DIR`, else the platform local-data dir.
fn resolve_data_dir(data_dir: Option<&Path>) -> PathBuf {
    let dir = data_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("AI_MEMORY_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ai-memory")
        });
    // Recover already-installed hooks that baked a safe verbatim data-dir form.
    match dir.to_str() {
        Some(s) if s.starts_with(r"\\?\") => {
            PathBuf::from(strip_windows_verbatim_prefix(s).into_owned())
        }
        _ => dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devin_hook_args(event: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "devin".into(),
            server_url: "http://127.0.0.1:1".into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
        }
    }

    fn read_spooled_entries(spool: &Path) -> Vec<hook_spool::SpoolEntry> {
        let mut entries = std::fs::read_dir(spool)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect::<Vec<_>>();
        entries.sort();
        entries
            .into_iter()
            .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap())
            .collect()
    }

    fn query_param<'a>(url: &'a str, key: &str) -> Option<&'a str> {
        url.split('?')
            .nth(1)?
            .split('&')
            .filter_map(|part| part.split_once('='))
            .find_map(|(name, value)| (name == key).then_some(value))
    }

    #[test]
    fn resolve_data_dir_strips_verbatim_prefix_from_baked_arg() {
        // Recover safe verbatim data dirs baked by older installs (#116).
        let resolved =
            resolve_data_dir(Some(Path::new(r"\\?\C:\Users\me\AppData\Local\ai-memory")));
        assert_eq!(
            resolved,
            PathBuf::from(r"C:\Users\me\AppData\Local\ai-memory")
        );
    }

    #[test]
    fn resolve_data_dir_leaves_plain_path_untouched() {
        let resolved = resolve_data_dir(Some(Path::new(r"C:\Users\me\ai-memory")));
        assert_eq!(resolved, PathBuf::from(r"C:\Users\me\ai-memory"));
    }

    #[test]
    fn should_incremental_drain_only_post_tool_use_over_threshold() {
        assert!(should_incremental_drain("post-tool-use", 32, 32));
        assert!(should_incremental_drain("post-tool-use", 100, 32));
        // below threshold: a light session never drains mid-session
        assert!(!should_incremental_drain("post-tool-use", 31, 32));
        // other events only enqueue; boundaries do the real flush
        assert!(!should_incremental_drain("pre-tool-use", 999, 32));
        assert!(!should_incremental_drain("session-start", 999, 32));
        assert!(!should_incremental_drain("session-end", 999, 32));
        assert!(!should_incremental_drain("stop", 999, 32));
    }

    #[test]
    fn boundary_events_trigger_background_drainer() {
        assert!(should_spawn_background_drainer("session-end"));
        assert!(should_spawn_background_drainer("stop"));
        assert!(should_spawn_background_drainer("pre-compact"));

        assert!(!should_spawn_background_drainer("session-start"));
        assert!(!should_spawn_background_drainer("post-tool-use"));
        assert!(!should_spawn_background_drainer("pre-tool-use"));
        assert!(!should_spawn_background_drainer("user-prompt"));
    }

    #[test]
    fn incremental_threshold_parses_and_falls_back() {
        assert_eq!(incremental_drain_threshold_from(|_| Some("64".into())), 64);
        assert_eq!(
            incremental_drain_threshold_from(|_| None),
            DEFAULT_INCREMENTAL_THRESHOLD
        );
        // zero / non-numeric fall back to the default (a 0 threshold would drain
        // on every post-tool-use)
        assert_eq!(
            incremental_drain_threshold_from(|_| Some("0".into())),
            DEFAULT_INCREMENTAL_THRESHOLD
        );
        assert_eq!(
            incremental_drain_threshold_from(|_| Some("abc".into())),
            DEFAULT_INCREMENTAL_THRESHOLD
        );
    }

    #[test]
    fn parse_minutes_falls_back_on_invalid() {
        assert_eq!(
            parse_minutes(None, DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
        assert_eq!(
            parse_minutes(Some(String::new()), DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
        assert_eq!(
            parse_minutes(Some("abc".into()), DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
        // Zero is rejected (a 0-minute timeout would drop every request).
        assert_eq!(
            parse_minutes(Some("0".into()), DEFAULT_DRAIN_TIMEOUT),
            DEFAULT_DRAIN_TIMEOUT
        );
    }

    #[test]
    fn parse_minutes_honours_valid_override() {
        assert_eq!(
            parse_minutes(Some("2".into()), DEFAULT_DRAIN_TIMEOUT),
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_minutes(Some("  3 ".into()), DEFAULT_DRAIN_TIMEOUT),
            Duration::from_secs(180)
        );
    }

    #[test]
    fn parse_minutes_clamps_large_values() {
        assert_eq!(
            parse_minutes(Some("999".into()), DEFAULT_DRAIN_TIMEOUT),
            Duration::from_secs(MAX_OVERRIDE_MINUTES * 60)
        );
    }

    #[test]
    fn background_drain_budget_defaults_and_clamps() {
        assert_eq!(
            background_drain_budget_from(|_| None),
            DEFAULT_BACKGROUND_DRAIN_BUDGET
        );
        assert_eq!(
            background_drain_budget_from(|_| Some("1".into())),
            Duration::from_secs(60)
        );
        assert_eq!(
            background_drain_budget_from(|_| Some("999".into())),
            Duration::from_secs(60 * 60)
        );
    }

    #[test]
    fn devin_query_session_id_is_stable_across_payloads_without_native_id() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let session_start = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": "startup"
        });
        let post_tool_use = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec",
            "tool_input": {"command": "ls"},
            "tool_use_id": "call_c101a272288d400b831e1498",
            "tool_response": {"success": true, "output": "ok", "error": null}
        });

        let first = session_id_query_suffix(data_dir, "devin", "session-start", &session_start);
        let second = session_id_query_suffix(data_dir, "devin", "post-tool-use", &post_tool_use);

        assert!(first.starts_with("&session_id="), "{first}");
        assert_eq!(second, first);
        assert_eq!(
            stored_session_id(data_dir, AgentKind::Devin).as_deref(),
            first.strip_prefix("&session_id=")
        );
    }

    #[tokio::test]
    async fn devin_session_start_real_fixture_without_session_id_or_cwd_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let mut stdout = Vec::new();
        let payload = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": "startup"
        });

        run_with_payload(
            Some(data_dir),
            devin_hook_args("session-start"),
            payload.to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        let entries = read_spooled_entries(&spool);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].url.contains("event=session-start"));
        assert!(entries[0].url.contains("agent=devin"));
        assert!(
            query_param(&entries[0].url, "session_id").is_some(),
            "{}",
            entries[0].url
        );
        assert!(
            query_param(&entries[0].url, "cwd").is_some(),
            "{}",
            entries[0].url
        );
    }

    #[tokio::test]
    async fn devin_post_tool_use_real_fixture_without_session_id_or_cwd_is_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let mut stdout = Vec::new();
        let payload = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec",
            "tool_input": {"command": "ls"},
            "tool_use_id": "call_c101a272288d400b831e1498",
            "tool_response": {"success": true, "output": "ok", "error": null}
        });

        run_with_payload(
            Some(data_dir),
            devin_hook_args("post-tool-use"),
            payload.to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        let entries = read_spooled_entries(&spool);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].url.contains("event=post-tool-use"));
        assert!(entries[0].url.contains("agent=devin"));
        assert!(
            query_param(&entries[0].url, "session_id").is_some(),
            "{}",
            entries[0].url
        );
        assert!(
            query_param(&entries[0].url, "cwd").is_some(),
            "{}",
            entries[0].url
        );
    }

    #[tokio::test]
    async fn devin_events_share_session_id_when_payload_omits_it() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let mut stdout = Vec::new();

        run_with_payload(
            Some(data_dir.clone()),
            devin_hook_args("session-start"),
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "source": "startup"
            })
            .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        run_with_payload(
            Some(data_dir),
            devin_hook_args("post-tool-use"),
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "exec",
                "tool_input": {"command": "ls"},
                "tool_use_id": "call_c101a272288d400b831e1498",
                "tool_response": {"success": true, "output": "ok", "error": null}
            })
            .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        let entries = read_spooled_entries(&spool);
        assert_eq!(entries.len(), 2);
        let first = query_param(&entries[0].url, "session_id").unwrap();
        let second = query_param(&entries[1].url, "session_id").unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn devin_query_session_id_does_not_override_native_payload_id() {
        let tmp = tempfile::tempdir().unwrap();
        let with_session = serde_json::json!({
            "session_id": "native-session",
            "hook_event_name": "PostToolUse"
        });

        let suffix = session_id_query_suffix(tmp.path(), "devin", "post-tool-use", &with_session);

        assert!(suffix.is_empty());
        assert!(stored_session_id(tmp.path(), AgentKind::Devin).is_none());
    }

    #[test]
    fn session_id_query_suffix_is_devin_only() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = serde_json::json!({"hook_event_name": "PostToolUse"});

        let suffix = session_id_query_suffix(tmp.path(), "claude-code", "post-tool-use", &raw);

        assert!(suffix.is_empty());
        assert!(stored_session_id(tmp.path(), AgentKind::ClaudeCode).is_none());
    }

    #[test]
    fn devin_missing_cwd_uses_devin_project_dir_before_process_cwd() {
        let raw = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": "startup"
        });

        let suffix = cwd_query_suffix_with(
            "devin",
            &raw,
            None,
            |name| (name == "DEVIN_PROJECT_DIR").then(|| "env-project".into()),
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(suffix, "&cwd=env-project");
    }

    #[test]
    fn devin_missing_cwd_uses_process_cwd_when_env_is_missing() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec"
        });

        let suffix = cwd_query_suffix_with(
            "devin",
            &raw,
            None,
            |_| None,
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(suffix, "&cwd=process-project");
    }

    #[test]
    fn devin_missing_cwd_uses_env_or_process_cwd() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec"
        });

        let from_env = cwd_query_suffix_with(
            "devin",
            &raw,
            None,
            |name| (name == "DEVIN_PROJECT_DIR").then(|| "env-project".into()),
            || Some(PathBuf::from("process-project")),
        );
        let from_process = cwd_query_suffix_with(
            "devin",
            &raw,
            None,
            |_| None,
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(from_env, "&cwd=env-project");
        assert_eq!(from_process, "&cwd=process-project");
    }

    #[test]
    fn devin_payload_cwd_wins_over_fallbacks() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "cwd": "payload-project"
        });

        let suffix = cwd_query_suffix_with(
            "devin",
            &raw,
            None,
            |name| (name == "DEVIN_PROJECT_DIR").then(|| "env-project".into()),
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(suffix, "&cwd=payload-project");
    }

    #[test]
    fn missing_cwd_process_fallback_is_devin_only() {
        let raw = serde_json::json!({"hook_event_name": "PostToolUse"});

        let suffix = cwd_query_suffix_with(
            "claude-code",
            &raw,
            None,
            |_| Some("env-project".into()),
            || Some(PathBuf::from("process-project")),
        );

        assert!(suffix.is_empty());
    }

    #[tokio::test]
    async fn devin_post_compaction_summary_without_payload_cwd_uses_same_session() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let summary = "Context compacted: 15000/20000 tokens used";
        let mut stdout = Vec::new();

        run_with_payload(
            Some(data_dir.clone()),
            devin_hook_args("session-start"),
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "source": "startup"
            })
            .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        run_with_payload(
            Some(data_dir),
            devin_hook_args("post-compaction"),
            serde_json::json!({
                "hook_event_name": "PostCompaction",
                "summary": summary
            })
            .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        let entries = read_spooled_entries(&spool);
        assert_eq!(entries.len(), 2);
        let first = query_param(&entries[0].url, "session_id").unwrap();
        let second = query_param(&entries[1].url, "session_id").unwrap();
        assert_eq!(first, second);
        assert!(query_param(&entries[1].url, "cwd").is_some());
        assert!(entries[1].body.contains(summary));
    }

    #[tokio::test]
    async fn session_end_run_enqueues_outputs_empty_json_and_spawns_after_enqueue() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let called = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let args = HookArgs {
            event: "session-end".into(),
            agent: "claude-code".into(),
            server_url: "http://127.0.0.1:1".into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
        };

        run_with_payload(
            Some(data_dir.clone()),
            args,
            r#"{"session_id":"s","cwd":"/tmp"}"#.into(),
            &mut stdout,
            |path| {
                assert_eq!(path, data_dir.as_path());
                assert_eq!(hook_spool::spool_len(&spool), 1, "spawn runs after enqueue");
                called.set(called.get() + 1);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert_eq!(called.get(), 1);
        assert_eq!(
            hook_spool::spool_len(&spool),
            1,
            "session-end must not drain inline"
        );
    }

    #[tokio::test]
    async fn stop_and_pre_compact_spawn_background_drainer_after_enqueue() {
        for event in ["stop", "pre-compact"] {
            let tmp = tempfile::tempdir().unwrap();
            let data_dir = tmp.path().to_path_buf();
            let spool = hook_spool::spool_dir(&data_dir);
            let called = std::cell::Cell::new(0);
            let mut stdout = Vec::new();
            let args = HookArgs {
                event: event.into(),
                agent: "claude-code".into(),
                server_url: "http://127.0.0.1:1".into(),
                auth_token: None,
                project_strategy: None,
                check_capture: false,
                capture_assistant: false,
            };

            run_with_payload(
                Some(data_dir.clone()),
                args,
                r#"{"session_id":"s","cwd":"/tmp"}"#.into(),
                &mut stdout,
                |path| {
                    assert_eq!(path, data_dir.as_path());
                    assert_eq!(hook_spool::spool_len(&spool), 1, "spawn runs after enqueue");
                    called.set(called.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap();

            assert_eq!(stdout, b"{}\n", "{event} should keep hook stdout clean");
            assert_eq!(called.get(), 1, "{event} should start background drain");
            assert_eq!(
                hook_spool::spool_len(&spool),
                1,
                "{event} must not drain inline"
            );
        }
    }

    #[tokio::test]
    async fn session_end_run_spawn_failure_keeps_event_queued_and_stdout_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let mut stdout = Vec::new();
        let args = HookArgs {
            event: "session-end".into(),
            agent: "claude-code".into(),
            server_url: "http://127.0.0.1:1".into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
        };

        run_with_payload(Some(data_dir), args, "{}".into(), &mut stdout, |_path| {
            Err(std::io::Error::other("spawn failed"))
        })
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert_eq!(hook_spool::spool_len(&spool), 1);
    }

    #[tokio::test]
    async fn devin_session_end_spools_stored_session_id_then_clears_state() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        store_session_id(&data_dir, AgentKind::Devin, "stable-devin-session");
        let mut stdout = Vec::new();
        let args = HookArgs {
            event: "session-end".into(),
            agent: "devin".into(),
            server_url: "http://127.0.0.1:1".into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
        };

        run_with_payload(
            Some(data_dir.clone()),
            args,
            r#"{"hook_event_name":"SessionEnd"}"#.into(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert!(stored_session_id(&data_dir, AgentKind::Devin).is_none());
        let entries: Vec<_> = std::fs::read_dir(&spool)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        let entry: hook_spool::SpoolEntry =
            serde_json::from_slice(&std::fs::read(&entries[0]).unwrap()).unwrap();
        assert!(
            entry.url.contains("&session_id=stable-devin-session"),
            "{}",
            entry.url
        );
    }

    #[test]
    fn session_end_spawn_failure_is_returned_for_warning_only() {
        let tmp = tempfile::tempdir().unwrap();
        let err = after_background_drain_event_enqueue(tmp.path(), |_path| {
            Err(std::io::Error::other("spawn failed"))
        })
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn background_drain_event_policy_spawns_without_inline_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let called = std::cell::Cell::new(false);

        after_background_drain_event_enqueue(tmp.path(), |path| {
            assert_eq!(path, tmp.path());
            called.set(true);
            Ok(())
        })
        .unwrap();

        assert!(called.get());
    }

    #[test]
    fn timing_accessors_read_the_expected_env_vars() {
        fn one_minute_for(expected_name: &'static str) -> impl FnMut(&str) -> Option<String> {
            move |actual_name| {
                assert_eq!(actual_name, expected_name);
                Some("1".to_string())
            }
        }

        assert_eq!(
            drain_event_timeout_from(one_minute_for(DRAIN_TIMEOUT_ENV)),
            Duration::from_secs(60)
        );
        assert_eq!(
            handoff_timeout_from(one_minute_for(HANDOFF_TIMEOUT_ENV)),
            Duration::from_secs(60)
        );
        assert_eq!(
            start_drain_budget_from(one_minute_for(START_BUDGET_ENV)),
            Duration::from_secs(60)
        );
        assert_eq!(
            background_drain_budget_from(one_minute_for(BACKGROUND_DRAIN_BUDGET_ENV)),
            Duration::from_secs(60)
        );
    }

    #[tokio::test]
    async fn capture_drop_prevents_spool_and_drainer() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "[capture]\nignore_paths = [\"secret/**\"]\n",
        )
        .unwrap();
        let data_dir = tmp.path().join("data");
        let mut stdout = Vec::new();
        let called = std::cell::Cell::new(false);
        let mut args = devin_hook_args("post-tool-use");
        args.server_url = "http://127.0.0.1:1".into();
        run_with_payload(Some(data_dir.clone()), args, serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"secret/SENTINEL"}}).to_string(), &mut stdout, |_| { called.set(true); Ok(()) }).await.unwrap();
        assert_eq!(stdout, b"{}\n");
        assert!(!called.get());
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    #[tokio::test]
    async fn invalid_capture_marker_spools_only_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "[capture]\nunknown = 1\n",
        )
        .unwrap();
        let data_dir = tmp.path().join("data");
        let mut stdout = Vec::new();
        run_with_payload(Some(data_dir.clone()), devin_hook_args("post-tool-use"), serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"SENTINEL_PATH","args":"SENTINEL_ARGS"},"output":"SENTINEL_OUTPUT","error":"SENTINEL_ERROR","nested":{"raw":"SENTINEL_NESTED"}}).to_string(), &mut stdout, |_| Ok(())).await.unwrap();
        let entry = read_spooled_entries(&hook_spool::spool_dir(&data_dir))
            .pop()
            .unwrap();
        for sentinel in [
            "SENTINEL_PATH",
            "SENTINEL_ARGS",
            "SENTINEL_OUTPUT",
            "SENTINEL_ERROR",
            "SENTINEL_NESTED",
        ] {
            assert!(!entry.body.contains(sentinel));
        }
        assert!(entry.body.contains("_ai_memory_capture"));
    }

    #[tokio::test]
    async fn check_capture_has_no_spool_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let mut args = devin_hook_args("post-tool-use");
        args.check_capture = true;
        let mut stdout = Vec::new();
        run_with_payload(Some(data_dir.clone()), args, serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"SENTINEL"}}).to_string(), &mut stdout, |_| Err(std::io::Error::other("must not spawn"))).await.unwrap();
        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output.as_object().unwrap().len(), 6);
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
        assert!(!String::from_utf8(stdout).unwrap().contains("SENTINEL"));
    }

    #[tokio::test]
    async fn inactive_preserves_bytes_and_active_keep_adds_protocol() {
        let tmp = tempfile::tempdir().unwrap();
        let inactive_data = tmp.path().join("inactive-data");
        let inactive_payload =
            serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"public"}})
                .to_string();
        let mut stdout = Vec::new();
        run_with_payload(
            Some(inactive_data.clone()),
            devin_hook_args("post-tool-use"),
            inactive_payload.clone(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(
            read_spooled_entries(&hook_spool::spool_dir(&inactive_data))[0].body,
            inactive_payload
        );

        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "[capture]\nignore_paths = [\"secret/**\"]\n",
        )
        .unwrap();
        let active_data = tmp.path().join("active-data");
        run_with_payload(
            Some(active_data.clone()),
            devin_hook_args("post-tool-use"),
            serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"public"}})
                .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        let body: serde_json::Value = serde_json::from_str(
            &read_spooled_entries(&hook_spool::spool_dir(&active_data))[0].body,
        )
        .unwrap();
        assert_eq!(body["_ai_memory_capture"]["disposition"], "keep");
        assert_eq!(body["_ai_memory_capture"]["policy_state"], "active");
    }

    #[tokio::test]
    async fn antigravity_workspace_path_drops_before_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"canonical\"\n[capture]\nignore_paths = [\"secret/**\"]\n",
        )
        .unwrap();
        let data_dir = tmp.path().join("data");
        let mut args = devin_hook_args("post-tool-use");
        args.agent = "antigravity-cli".into();
        let mut stdout = Vec::new();
        let raw = serde_json::json!({"workspacePaths":[tmp.path()],"toolCall":{"name":"Edit","args":{"path":"secret/a"}}});
        assert!(cwd_query_suffix("antigravity-cli", &raw, None).contains("workspace=canonical"));
        run_with_payload(
            Some(data_dir.clone()),
            args,
            raw.to_string(),
            &mut stdout,
            |_| Err(std::io::Error::other("must not spawn")),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"{}\n");
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    #[tokio::test]
    async fn metadata_only_canonicalizes_supported_session_aliases() {
        for (key, value) in [
            ("session_id", serde_json::json!("one")),
            ("sessionId", serde_json::json!("two")),
            ("sessionID", serde_json::json!("three")),
            ("conversationId", serde_json::json!("four")),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join(".ai-memory.toml"),
                "[capture]\nunknown = true\n",
            )
            .unwrap();
            let data_dir = tmp.path().join("data");
            let mut raw = serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"secret"}});
            raw.as_object_mut()
                .unwrap()
                .insert(key.into(), value.clone());
            let mut stdout = Vec::new();
            run_with_payload(
                Some(data_dir.clone()),
                devin_hook_args("post-tool-use"),
                raw.to_string(),
                &mut stdout,
                |_| Ok(()),
            )
            .await
            .unwrap();
            let body: serde_json::Value = serde_json::from_str(
                &read_spooled_entries(&hook_spool::spool_dir(&data_dir))[0].body,
            )
            .unwrap();
            assert_eq!(body["session_id"], value, "{key}");
        }
    }

    fn kimi_hook_args(event: &str, server_url: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "kimi-code".into(),
            server_url: server_url.into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
        }
    }

    /// Recording HTTP stub: replies to every request with `status`/`body` and
    /// streams each request head back so tests can assert which endpoints the
    /// hook touched (session-start also drains the spool, so POSTs to `/hook`
    /// are legitimate traffic here — only `GET /handoff` is interesting).
    async fn serve_requests(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0_u8; 8192];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..read]).into_owned());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// Wait briefly for the first recorded request (if any).
    async fn first_request(
        requests: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    ) -> Option<String> {
        tokio::time::timeout(Duration::from_millis(500), requests.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn kimi_user_prompt_prints_the_handoff_body_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            kimi_hook_args("user-prompt", &base),
            serde_json::json!({
                "sessionId": "session_abc",
                "cwd": tmp.path(),
                "prompt": "hello"
            })
            .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        // Kimi injects UserPromptSubmit stdout verbatim into the turn, so the
        // body must be the bare handoff — never a JSON envelope.
        assert_eq!(stdout, b"AMWS-HANDOFF-DELTA\n");
        let request = first_request(&mut requests).await.unwrap();
        assert!(request.starts_with("GET /handoff?"), "{request}");
        assert!(request.contains("agent=kimi-code"), "{request}");
        // The native session id rides along so the destructive fetch can link
        // the managed run to the kimi session.
        assert!(request.contains("session_id=session_abc"), "{request}");
    }

    #[tokio::test]
    async fn kimi_user_prompt_submit_stem_also_delivers_the_handoff() {
        // The default PosixNative/WindowsNative installs pass the script stem
        // (`--event user-prompt-submit`), not the legacy `user-prompt` token;
        // both must trigger delivery or the production path would print `{}`
        // and kimi would inject it as literal context.
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            kimi_hook_args("user-prompt-submit", &base),
            serde_json::json!({"sessionId": "session_abc", "cwd": tmp.path(), "prompt": "hi"})
                .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"AMWS-HANDOFF-DELTA\n");
        let request = first_request(&mut requests).await.unwrap();
        assert!(request.starts_with("GET /handoff?"), "{request}");

        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("404 Not Found", "").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            kimi_hook_args("user-prompt-submit", &base),
            serde_json::json!({"sessionId": "session_abc", "cwd": tmp.path(), "prompt": "hi"})
                .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        let request = first_request(&mut requests).await.unwrap();
        assert!(request.starts_with("GET /handoff?"), "{request}");
        assert_eq!(stdout, b"");
    }

    #[tokio::test]
    async fn kimi_user_prompt_prints_nothing_when_no_handoff_is_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("404 Not Found", "").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            kimi_hook_args("user-prompt", &base),
            serde_json::json!({"sessionId": "session_abc", "cwd": tmp.path(), "prompt": "hi"})
                .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        // The hook still attempted the fetch (a miss is indistinguishable
        // from an empty handoff server-side)...
        let request = first_request(&mut requests).await.unwrap();
        assert!(request.starts_with("GET /handoff?"), "{request}");
        // ...but stdout stays empty: kimi injects nothing, and a `{}`
        // envelope would show up as literal user-visible text.
        assert_eq!(stdout, b"");
    }

    #[tokio::test]
    async fn kimi_session_start_never_fetches_the_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        // A handoff IS pending; kimi's SessionStart stdout is discarded, so
        // the hook must not consume it here (it is delivered on user-prompt).
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            kimi_hook_args("session-start", &base),
            serde_json::json!({"sessionId": "session_abc", "cwd": tmp.path()}).to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        // The session-start backlog drain may POST the spooled event, but no
        // request may touch /handoff.
        while let Some(request) = first_request(&mut requests).await {
            assert!(!request.starts_with("GET /handoff"), "{request}");
        }
    }

    #[tokio::test]
    async fn claude_user_prompt_does_not_fetch_the_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut args = kimi_hook_args("user-prompt", &base);
        args.agent = "claude-code".into();
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            args,
            serde_json::json!({"session_id": "claude-session", "cwd": tmp.path()}).to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        // Claude keeps receiving the handoff on session-start; user-prompt
        // output stays the plain empty object and no fetch happens.
        assert_eq!(stdout, b"{}\n");
        while let Some(request) = first_request(&mut requests).await {
            assert!(!request.starts_with("GET /handoff"), "{request}");
        }
    }

    fn write_briefing_marker(dir: &Path) {
        std::fs::write(
            dir.join(".ai-memory.toml"),
            "[briefing]\ninject_on_session_start = true\nmax_chars = 6000\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn kimi_user_prompt_briefing_only_on_first_prompt_of_session() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        write_briefing_marker(&cwd);
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let payload = serde_json::json!({
            "sessionId": "session_abc",
            "cwd": cwd,
            "prompt": "hi"
        })
        .to_string();

        // First prompt of the session: the briefing params ride along so the
        // server appends the compiled project brief (kimi cannot receive it
        // on SessionStart — that hook's stdout is discarded).
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            kimi_hook_args("user-prompt-submit", &base),
            payload.clone(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"AMWS-HANDOFF-DELTA\n");
        let first = first_request(&mut requests).await.unwrap();
        assert!(first.starts_with("GET /handoff?"), "{first}");
        assert!(first.contains("&briefing=true"), "{first}");
        assert!(first.contains("&briefing_budget=6000"), "{first}");
        // ...and the session is marked as briefed.
        assert!(data_dir.join("briefed").join("session_abc").is_file());

        // Second prompt: the handoff is still fetched and printed (cheap and
        // self-limiting), but the briefing params are gone so the server
        // does not recompose the brief on every prompt.
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            kimi_hook_args("user-prompt-submit", &base),
            payload,
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"AMWS-HANDOFF-DELTA\n");
        let second = first_request(&mut requests).await.unwrap();
        assert!(second.starts_with("GET /handoff?"), "{second}");
        assert!(second.contains("agent=kimi-code"), "{second}");
        assert!(second.contains("session_id=session_abc"), "{second}");
        assert!(second.contains("&cwd="), "{second}");
        assert!(!second.contains("briefing"), "{second}");
    }

    #[tokio::test]
    async fn kimi_user_prompt_briefing_fallback_key_hashes_agent_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        write_briefing_marker(&cwd);
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        // No session id in the payload: the briefed marker is keyed by a
        // stable hash of agent+cwd, so a session-less payload still briefs
        // only once.
        let payload = serde_json::json!({"cwd": cwd, "prompt": "hi"}).to_string();

        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            kimi_hook_args("user-prompt", &base),
            payload.clone(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"AMWS-HANDOFF-DELTA\n");
        let first = first_request(&mut requests).await.unwrap();
        assert!(first.contains("&briefing=true"), "{first}");
        let expected_key = format!(
            "{:x}",
            Sha256::digest(format!("kimi-code\n{}", cwd.to_str().unwrap()).as_bytes())
        );
        assert!(data_dir.join("briefed").join(expected_key).is_file());

        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            kimi_hook_args("user-prompt", &base),
            payload,
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"AMWS-HANDOFF-DELTA\n");
        let second = first_request(&mut requests).await.unwrap();
        assert!(second.starts_with("GET /handoff?"), "{second}");
        assert!(!second.contains("briefing"), "{second}");
    }

    #[tokio::test]
    async fn kimi_user_prompt_without_briefing_creates_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let (base, mut requests) = serve_requests("404 Not Found", "").await;

        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            kimi_hook_args("user-prompt-submit", &base),
            serde_json::json!({
                "session_id": "session_abc",
                "cwd": cwd,
                "prompt": "hi"
            })
            .to_string(),
            &mut stdout,
            |_| Ok(()),
        )
        .await
        .unwrap();

        let request = first_request(&mut requests).await.unwrap();
        assert!(request.starts_with("GET /handoff?"), "{request}");
        assert!(!request.contains("briefing"), "{request}");
        assert!(!data_dir.join("briefed").exists());
    }

    #[test]
    fn briefed_markers_are_bounded_and_keep_current() {
        let tmp = tempfile::tempdir().unwrap();
        let marker_dir = tmp.path().join("briefed");
        let count = MAX_BRIEFED_MARKERS + 20;
        for index in 0..count {
            mark_briefed(&marker_dir.join(format!("session-{index:04}")));
        }

        let retained = std::fs::read_dir(&marker_dir).unwrap().count();
        assert_eq!(retained, MAX_BRIEFED_MARKERS);
        assert!(
            marker_dir
                .join(format!("session-{:04}", count - 1))
                .is_file()
        );
    }
}
