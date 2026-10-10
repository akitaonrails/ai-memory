//! `ai-memory hook` — emit a single lifecycle event natively.
//!
//! Reads the event payload from stdin. Instead of POSTing synchronously on the
//! agent's hot path (which would block every tool call on the network and drop
//! events against a slow/remote server), the event is **spooled** locally — an
//! instant write. `session-start` performs a short, lock-aware synchronous
//! cleanup pass before fetching handoff context. `session-end` returns quickly:
//! after enqueue it spawns a detached `hook-drain` process, whose stdout/stderr
//! are redirected away from the agent, and that process drains under an
//! exclusive spool lock with a longer bounded budget. Zero, whose hook sandbox
//! kills detached children, drains in-process within a short fixed budget.
//!
//! See `docs/windows.md#native-hook-command-claude-code-on-windows`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_memory_core::{AgentKind, ManagedRunId, SessionId};
use ai_memory_hooks::capture_policy::metadata_only_body;
use ai_memory_hooks::{
    CaptureDisposition, CaptureMode, HookEvent, PolicyState, absolute_file_tool_paths,
    repository_admits_capture,
};
use ai_memory_llm::OidcToken;

use crate::cli::HookArgs;
use crate::config::{Config, RuntimeEnv};
use crate::server_profiles::{self, ProfileName, Rejection, ResolvedServer};

use sha2::{Digest as _, Sha256};

use super::hook_capture::{
    build_client, canonical_context, capture_policy, extract_cwd, get_handoff, hook_scope,
    marker_query_suffix, marker_query_suffix_without_briefing, marker_requests_briefing,
    resolve_cwd_with_fallbacks, url_encode,
};
use super::hook_drain_process;
use super::hook_spool;
use super::path_util::{home_dir, strip_windows_verbatim_prefix};

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
const CAPTURE_OWNER_ENV: &str = "AI_MEMORY_CAPTURE_OWNER";
/// Backlog size at which `post-tool-use` does a mid-session catch-up drain, so a
/// light session pays only a `read_dir`. Override via the env var above.
const DEFAULT_INCREMENTAL_THRESHOLD: usize = 32;
const MAX_BRIEFED_MARKERS: usize = 512;
/// Total budget AND per-event timeout for the mid-session catch-up drain — kept
/// well under a second so a `post-tool-use` hook never stalls a tool call (one
/// in-flight POST against a slow server is bounded by this too).
const INCREMENTAL_DRAIN_BUDGET: Duration = Duration::from_millis(250);
/// Total budget, lock wait included, for a `session-end` drained in-process
/// ([`session_end_drains_in_process`]). Fixed rather than env-overridable:
/// together with [`IN_PROCESS_SESSION_END_DEADLINE`] it has to stay inside
/// Zero's 30-second hook timeout.
const IN_PROCESS_SESSION_END_DRAIN_BUDGET: Duration = Duration::from_secs(10);
/// Hard stop for that drain, the request in flight at the budget included.
const IN_PROCESS_SESSION_END_DEADLINE: Duration = Duration::from_secs(20);
// Zero kills a hook after 30 seconds; the hard stop must land first.
const _: () = assert!(
    IN_PROCESS_SESSION_END_DRAIN_BUDGET.as_secs() < IN_PROCESS_SESSION_END_DEADLINE.as_secs()
        && IN_PROCESS_SESSION_END_DEADLINE.as_secs() < 30
);

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

fn spawn_background_drainer(data_dir: &Path, live_token: Option<&str>) -> std::io::Result<()> {
    hook_drain_process::spawn(data_dir, live_token)
}

fn should_spawn_background_drainer(event: &str) -> bool {
    matches!(event, "session-end" | "stop" | "pre-compact")
}

/// Zero runs hooks in a sandbox started with `--die-with-parent`, which kills
/// a detached drainer the moment the hook exits, so its `session-end` drains
/// in-process within [`IN_PROCESS_SESSION_END_DRAIN_BUDGET`] instead (#1172).
fn session_end_drains_in_process(agent: AgentKind, event: &str) -> bool {
    agent == AgentKind::Zero && event == "session-end"
}

/// Drain the spool from the hook process itself, for a harness that kills the
/// detached drainer. Undelivered events stay spooled for the next boundary.
async fn drain_session_end_in_process(
    spool: &Path,
    data_dir: &Path,
    max_attempts: hook_spool::MaxAttempts,
) {
    let drain = hook_spool::drain_exclusive_within_budget(
        spool,
        data_dir,
        IN_PROCESS_SESSION_END_DRAIN_BUDGET,
        drain_event_timeout().min(IN_PROCESS_SESSION_END_DRAIN_BUDGET),
        max_attempts,
    );
    // The budget stops new requests; this also stops one already in flight
    // (an old server's per-event fallback, a token refresh). Cancelling only
    // at an await point leaves every spool file whole: at worst an accepted
    // event is re-sent and the server's ingest key skips it.
    let _ = tokio::time::timeout(IN_PROCESS_SESSION_END_DEADLINE, drain).await;
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

pub(crate) fn clear_session_id(data_dir: &Path, agent: AgentKind) {
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

/// The once-per-session profile-digest marker beside a brief marker: same
/// key, `profile-` prefix, so the two gates never share a file. The shell and
/// PowerShell hooks name it the same way.
fn profile_digest_marker_path(brief_marker: PathBuf) -> PathBuf {
    let name = brief_marker
        .file_name()
        .map(|name| format!("profile-{}", name.to_string_lossy()))
        .unwrap_or_else(|| "profile-".to_owned());
    brief_marker.with_file_name(name)
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

/// Agents whose hook payloads do not reliably carry a session id. Devin's
/// payloads may omit it; ZCode's do too (and ZCode fires `Stop` per turn with
/// no SessionEnd, so without a persisted id each turn would fragment into a
/// fresh server-side session). For these agents the hook maintains a
/// `<data_dir>/hook-state/<agent>-session-id` file so a whole agent session
/// shares one stable id. Agents whose payloads always carry an id never touch
/// this path.
fn agent_needs_session_id_state(agent_kind: AgentKind) -> bool {
    matches!(agent_kind, AgentKind::Devin | AgentKind::Zcode)
}

fn session_id_query_suffix(
    data_dir: &Path,
    agent: &str,
    event: &str,
    raw: &serde_json::Value,
) -> String {
    let agent_kind = AgentKind::from_wire(agent);
    if !agent_needs_session_id_state(agent_kind) || payload_has_session_id(raw) {
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

fn resolve_hook_cwd_with(
    agent: &str,
    raw: &serde_json::Value,
    env_lookup: impl FnMut(&str) -> Option<String>,
    current_dir: impl FnOnce() -> Option<PathBuf>,
) -> Option<String> {
    let agent_kind = AgentKind::from_wire(agent);
    let (canonical_cwd, _) = canonical_context(raw);
    if canonical_cwd.is_some() {
        canonical_cwd
    } else if agent_kind == AgentKind::Devin {
        resolve_cwd_with_fallbacks(raw, env_lookup, current_dir)
    } else {
        extract_cwd(raw).filter(|s| !s.trim().is_empty())
    }
}

fn cwd_query_suffix(
    agent: &str,
    raw: &serde_json::Value,
    default_strategy: Option<&str>,
) -> String {
    resolve_hook_cwd_with(agent, raw, env_lookup, || std::env::current_dir().ok())
        .map(|cwd| marker_query_suffix(&cwd, default_strategy))
        .unwrap_or_default()
}

fn after_background_drain_event_enqueue(
    data_dir: &Path,
    live_token: Option<&str>,
    spawn: impl FnOnce(&Path, Option<&str>) -> std::io::Result<()>,
) -> std::io::Result<()> {
    spawn(data_dir, live_token)
}

/// Hidden drain-only fast path. Reads no stdin and writes no stdout.
pub async fn run_drain(data_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let runtime_env = RuntimeEnv::from_process();
    let dd = resolve_data_dir_with(data_dir.as_deref(), &runtime_env);
    // The hook path never runs a full `Config::load` (see the fast-path
    // contract in `lib.rs`): an invalid config section elsewhere must not
    // fail the drainer. Only the spool retry policy is needed, loaded
    // tolerantly — any load error falls back to defaults.
    let config =
        Config::load_with_runtime_env(Some(&dd.join("config.toml")), Some(dd.clone()), runtime_env)
            .unwrap_or_default();
    let max_attempts = hook_spool::MaxAttempts::new(config.hook_spool.max_attempts);
    let spool = hook_spool::spool_dir(&dd);
    match hook_spool::drain_until_quiescent(
        &spool,
        &dd,
        background_drain_budget(),
        drain_event_timeout(),
        hook_spool::DrainLockWait::Bounded(Duration::from_secs(30)),
        max_attempts,
    )
    .await
    {
        Ok(outcome) => {
            let _ = write_drain_report(&mut std::io::stderr(), &outcome);
        }
        Err(err) => eprintln!("ai-memory hook-drain warning: failed to acquire drain lock: {err}"),
    }
    Ok(())
}

/// Report a drain pass on stderr when it ended with events undelivered.
///
/// Silent on a clean pass (nothing queued, or everything delivered) so
/// `hook-drain.log` stays warning-only, matching the rotation budget in
/// [`hook_drain_process`]. stderr, never stdout: the drainer's stdout is
/// contractually empty and the detached helper redirects stderr into the log.
///
/// Without this the drain discarded its own [`hook_spool::DrainResult`], so a
/// pass that dropped every queued event without sending it was byte-identical,
/// on both streams and in the exit code, to one that delivered them all — and
/// so was a pass that did nothing because another drainer held the lock (#493).
fn write_drain_report<W: std::io::Write>(
    w: &mut W,
    outcome: &hook_spool::LockedDrainResult,
) -> std::io::Result<()> {
    match outcome {
        // Says what this pass did, not what the spool holds: `LockBusy` is
        // returned before `drain_until_quiescent` lists anything, so the count
        // of queued events is unknown here. Asserting one would repeat, inside
        // the fix, the confusion the fix is for.
        hook_spool::LockedDrainResult::LockBusy => writeln!(
            w,
            "ai-memory hook-drain warning: another drainer holds the spool lock; \
             this pass attempted no delivery and left the spool untouched"
        ),
        hook_spool::LockedDrainResult::Drained(result) => {
            if result.remaining == 0 && result.dropped == 0 {
                return Ok(());
            }
            // "acknowledged", not "delivered": `sent` counts what the server
            // ACKed, and an ack also covers an item the server accepted and then
            // dropped by capture policy (`accepted_indices` includes protocol
            // drops). Reporting those as delivered would assert storage the
            // drain cannot observe — the precise confusion #493 was written in.
            writeln!(
                w,
                "ai-memory hook-drain warning: {} event(s) acknowledged by the server, \
                 {} still queued, {} DROPPED undelivered (past the retry cap or \
                 older than the spool TTL)",
                result.sent, result.remaining, result.dropped
            )
        }
    }
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

fn should_process_hook_event(agent: AgentKind, event: HookEvent, raw: &serde_json::Value) -> bool {
    if agent == AgentKind::AntigravityCli && event == HookEvent::SessionStart {
        return raw.get("invocationNum").and_then(serde_json::Value::as_u64) == Some(0);
    }
    true
}

/// A `--agent claude-code` hook invoked by Cursor (it runs Claude Code's
/// settings too) is a duplicate when Cursor's own ai-memory hooks are also
/// installed: the native `--agent cursor` hook already delivers the same
/// event. Dropping it keeps one observation per Cursor event (#721). The
/// installed check runs last so ordinary Claude Code events never touch disk.
fn is_redundant_cursor_copy(
    agent: AgentKind,
    raw: &serde_json::Value,
    cursor_hooks_installed: impl FnOnce() -> bool,
) -> bool {
    agent == AgentKind::ClaudeCode
        && ai_memory_hooks::agent_from_payload(raw) == Some(AgentKind::Cursor)
        && cursor_hooks_installed()
}

fn write_success_response<W: std::io::Write>(
    stdout: &mut W,
    agent: AgentKind,
    event: HookEvent,
) -> std::io::Result<()> {
    if agent == AgentKind::KiroCli {
        // Kiro adds successful SessionStart/UserPromptSubmit stdout to model
        // context, and v2/v3 Stop parse stdout for a block decision. Capture-only
        // hooks therefore stay silent unless SessionStart has real context.
        Ok(())
    } else if event == HookEvent::UserPrompt && agent.user_prompt_injects_handoff() {
        // Kimi Code injects any non-empty UserPromptSubmit stdout into the turn
        // verbatim, so a `{}` here would become user-visible text on every
        // prompt a gate dropped (allowlist, capture policy, refused profile).
        Ok(())
    } else if agent == AgentKind::AntigravityCli && event == HookEvent::PreToolUse {
        writeln!(stdout, r#"{{"decision": "allow"}}"#)
    } else {
        writeln!(stdout, "{{}}")
    }
}

const GROK_ADDITIONAL_CONTEXT_CHARS: usize = 10_000;

fn grok_post_tool_handoff_envelope(handoff: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": clip_chars(handoff, GROK_ADDITIONAL_CONTEXT_CHARS),
        }
    })
}

fn payload_is_subagent(raw: &serde_json::Value) -> bool {
    [
        "subagentType",
        "subagent_type",
        "agent_type",
        "agent_id",
        "parentSessionId",
    ]
    .iter()
    .any(|key| {
        raw.get(*key)
            .and_then(|value| value.as_str())
            .is_some_and(|text| !text.trim().is_empty())
    })
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(12);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("\n[truncated]");
    out
}

fn handoff_shown_path(
    data_dir: &Path,
    agent: &str,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> PathBuf {
    let key = briefed_marker_path(data_dir, agent, session_id, cwd);
    data_dir
        .join("handoff-shown")
        .join(key.file_name().unwrap_or_default())
}

fn session_start_handoff_envelope(agent: AgentKind, handoff: String) -> serde_json::Value {
    if agent == AgentKind::AntigravityCli {
        serde_json::json!({
            "injectSteps": [{
                "ephemeralMessage": handoff,
            }]
        })
    } else if agent == AgentKind::CopilotCli {
        // Copilot CLI requires the SessionStart output envelope itself, rather
        // than Claude Code's nested hookSpecificOutput shape.
        serde_json::json!({
            "additionalContext": handoff,
        })
    } else {
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": handoff,
            }
        })
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

/// [`run`] with the payload and output sink injected, so tests drive the hook
/// implementation directly without stdin/stdout.
///
/// Hooks deliberately bypass the full `Config::load` (the fast-path contract
/// in `lib.rs`): an unrelated invalid config section must never fail the hook
/// or add stdout noise. The spool retry policy is the only config needed, and
/// it is loaded tolerantly — a malformed config falls back to defaults rather
/// than erroring. The process environment is captured once, here, and
/// threaded down as data.
async fn run_with_payload<W, S>(
    data_dir: Option<PathBuf>,
    args: HookArgs,
    payload: String,
    stdout: &mut W,
    spawn_background_drainer: S,
) -> anyhow::Result<()>
where
    W: std::io::Write,
    S: FnOnce(&Path, Option<&str>) -> std::io::Result<()>,
{
    let runtime_env = RuntimeEnv::from_process();
    let dd = resolve_data_dir_with(data_dir.as_deref(), &runtime_env);
    let spool_policy =
        Config::load_with_runtime_env(Some(&dd.join("config.toml")), Some(dd), runtime_env.clone())
            .unwrap_or_default();
    let max_attempts = hook_spool::MaxAttempts::new(spool_policy.hook_spool.max_attempts);
    let agent_kind = AgentKind::from_wire(&args.agent);
    let hook_event = HookEvent::parse(&args.event);
    // This is inherited execution context, like AI_MEMORY_RUN_ID, rather
    // than server configuration. Hooks deliberately bypass Config::load.
    let external_capture = env_lookup(CAPTURE_OWNER_ENV).is_some_and(|v| !v.trim().is_empty());
    let delivers_context = (hook_event == HookEvent::SessionStart
        && agent_kind.session_start_injects_handoff())
        || (hook_event == HookEvent::UserPrompt && agent_kind.user_prompt_injects_handoff())
        // Grok delivers the handoff on the first PostToolUse (its SessionStart /
        // UserPromptSubmit stdout is discarded); that path is context delivery
        // too, so external capture must not suppress it.
        || (hook_event == HookEvent::PostToolUse && agent_kind.post_tool_injects_handoff());
    if external_capture && !args.check_capture && !delivers_context {
        // Retiring the fallback session ID is lifecycle housekeeping, not
        // capture. Preserve it even when no event is enqueued.
        if agent_kind == AgentKind::Devin && hook_event == HookEvent::SessionEnd {
            clear_session_id(
                &resolve_data_dir_with(data_dir.as_deref(), &runtime_env),
                agent_kind,
            );
        }
        write_success_response(stdout, agent_kind, hook_event)?;
        return Ok(());
    }
    let (mut payload, mut json) = match parse_hook_payload(payload) {
        Ok(parsed) => parsed,
        Err(_) => {
            eprintln!(
                "ai-memory hook warning: could not parse event payload as JSON; nothing was captured"
            );
            write_success_response(stdout, agent_kind, hook_event)?;
            return Ok(());
        }
    };
    // Antigravity exposes PreInvocation rather than a true SessionStart. It
    // fires before every model call; invocation zero is the only startup
    // boundary. Fail closed when the documented counter is absent so a later
    // invocation can never consume a handoff intended for the next session.
    let event_admits_capture = should_process_hook_event(agent_kind, hook_event, &json)
        && !is_redundant_cursor_copy(
            agent_kind,
            &json,
            super::install_hooks::cursor_native_hooks_installed,
        );
    if !event_admits_capture && !args.check_capture {
        write_success_response(stdout, agent_kind, hook_event)?;
        return Ok(());
    }
    // Assistant/Stop capture (#196). On an opted-in install
    // (`install-hooks --capture-assistant`), extract the assistant message,
    // sanitize + cap it, and splice the versioned `_ai_memory_assistant` marker
    // into the body. Otherwise just strip any raw assistant field defensively —
    // the field is never persisted until the server-gated opt-in accepts it.
    // Reserialize only when the JSON actually changed, so unrelated events keep
    // byte-exact spool bodies (see `native_hook_accepts_plain_and_bom_prefixed_json`).
    let capture_assistant = if args.capture_assistant {
        let transform = ai_memory_hooks::transform_for_client(&mut json, agent_kind, hook_event);
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
    if ai_memory_hooks::cap_lifecycle_body_for_client(&mut json, hook_event) {
        payload = serde_json::to_string(&json)?;
    }
    if agent_kind == AgentKind::AntigravityCli
        && ai_memory_hooks::enrich_antigravity_step_output(
            &mut json,
            hook_event,
            home_dir().as_deref(),
        )
    {
        payload = serde_json::to_string(&json)?;
    }
    let (payload_cwd, canonical_session_id) = hook_context(&args.agent, &json);
    let tool_event = is_tool_event(&args.event);
    // Some harnesses keep a subagent's parent cwd in every payload even when
    // a file tool operates in another checkout (#932). A recognized absolute
    // target may select that checkout, but only when every path proves the
    // same repository/marker boundary. This must precede capture policy and
    // server-profile resolution so the destination's privacy and credentials
    // remain authoritative.
    let file_routing_cwd = payload_cwd.as_deref().and_then(|cwd| {
        tool_event
            .then(|| file_tool_routing_cwd(agent_kind, &json, cwd))
            .flatten()
    });
    if let Some(cwd) = file_routing_cwd.as_deref()
        && let Some(object) = json.as_object_mut()
    {
        // `HookEnvelope` deliberately gives the native body cwd precedence
        // over the query fallback. Stamp the proven destination into that
        // canonical field too, or a URL-only reroute would be cosmetic.
        object.insert("cwd".into(), cwd.into());
        payload = serde_json::to_string(&json)?;
    }
    let policy_cwd = file_routing_cwd.or(payload_cwd);
    let inspection_cwd = policy_cwd.as_deref().map(lexical_capture_cwd);
    let policy = policy_cwd.as_deref().map(capture_policy);
    let decision = policy.as_ref().filter(|_| tool_event).map(|policy| {
        policy.inspect(
            AgentKind::from_wire(&args.agent),
            &json,
            inspection_cwd.as_deref().unwrap_or(""),
        )
    });
    let dd = resolve_data_dir_with(data_dir.as_deref(), &runtime_env);
    // Precedence: an explicit flag (tests, one-off runs) wins; otherwise the
    // persisted per-install mode; otherwise the historical default.
    let capture_mode = args.capture_mode.map_or_else(
        || persisted_capture_mode(&dd),
        crate::cli::CaptureModeArg::mode,
    );
    let route = resolve_hook_route(&dd, policy_cwd.as_deref(), || std::env::current_dir().ok());
    // Marker presence is the opt-in signal under allowlist mode. Resolved from
    // the same upward walk the policy uses, so opting in needs no new file.
    let marker_present = policy_cwd
        .as_deref()
        .is_some_and(|cwd| crate::marker::find_marker(cwd).is_some());
    let routing_valid = policy_cwd
        .as_deref()
        .is_none_or(|cwd| crate::marker::routing_selection(cwd).is_ok());
    let admits_capture = repository_admits_capture(capture_mode, marker_present);
    if args.check_capture {
        let protocol = decision.as_ref().map(|decision| decision.protocol());
        let scope = policy_cwd.as_deref().map(|cwd| {
            hook_scope(
                cwd,
                args.project_strategy
                    .and_then(crate::cli::ProjectStrategyArg::baked),
            )
        });
        let scope_resolution = match scope.as_ref() {
            None => "unavailable",
            Some(scope) if !scope.is_inspectable() => "unavailable",
            Some(scope) if scope.is_partial() => "partial",
            Some(scope) if scope.is_explicit() => "explicit",
            Some(_) => "server-derived",
        };
        let policy_admits_capture = admits_capture
            && routing_valid
            && event_admits_capture
            && !matches!(route, HookRoute::Rejected { .. })
            && scope_resolution != "partial"
            && scope_resolution != "unavailable"
            && protocol.is_none_or(|p| p.disposition() != CaptureDisposition::Drop);
        let mut output = serde_json::json!({
            "capture_mode": capture_mode,
            "marker_present": marker_present,
            "admits_capture": admits_capture && !external_capture,
            "policy_admits_capture": policy_admits_capture,
            "event_admits_capture": event_admits_capture,
            "scope": scope,
            "scope_resolution": scope_resolution,
            "version": protocol.map_or(1, |protocol| protocol.version()),
            "policy_state": protocol.map_or(PolicyState::Inactive, |protocol| protocol.policy_state()),
            "tool_family": protocol.map_or(ai_memory_hooks::ToolFamily::Unknown, |protocol| protocol.tool_family()),
            "path_count": protocol.map_or(0, |protocol| protocol.path_count()),
            "disposition": protocol.map_or(CaptureDisposition::Keep, |protocol| protocol.disposition()),
            "extraction_state": protocol.map_or(ai_memory_hooks::ExtractionState::NotApplicable, |protocol| protocol.extraction_state()),
            "server_profile": route.profile_name().map(ProfileName::as_str),
            "server_resolution": route.resolution(),
        });
        if external_capture {
            output["external_capture"] = true.into();
        }
        writeln!(stdout, "{output}")?;
        return Ok(());
    }
    // #992: a marker that selects a server profile which does not resolve
    // emits nothing — not the event, not a handoff fetch, not a backfill.
    // Delivering to the install default instead would hand one team's
    // capture to another team's server.
    let profile = match route {
        HookRoute::InstallDefault => None,
        HookRoute::Profile(resolved) => Some(resolved),
        HookRoute::Rejected { name, reason } => {
            eprintln!(
                "ai-memory hook warning: this repository's marker selects server profile {}, \
                 which was refused ({}); the event was dropped",
                name.as_ref()
                    .map_or_else(|| "<invalid>".to_owned(), |n| format!("`{n}`")),
                reason.as_str()
            );
            write_success_response(stdout, agent_kind, hook_event)?;
            return Ok(());
        }
    };
    // #446: under allowlist mode a repository that never opted in emits
    // nothing. This sits outside the `tool_event` path on purpose — `decision`
    // is `None` for UserPromptSubmit, SessionStart/End and Stop, so gating via
    // `CaptureDisposition` alone would still spool prompt text from an
    // opted-out repository while reporting it as excluded.
    if !admits_capture || !routing_valid {
        write_success_response(stdout, agent_kind, hook_event)?;
        return Ok(());
    }
    if let Some(decision) = decision {
        match decision.protocol().disposition() {
            CaptureDisposition::Drop => {
                write_success_response(stdout, agent_kind, hook_event)?;
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

    let qs = policy_cwd.as_deref().map_or_else(
        || {
            cwd_query_suffix(
                &args.agent,
                &json,
                args.project_strategy.and_then(|s| s.baked()),
            )
        },
        |cwd| marker_query_suffix(cwd, args.project_strategy.and_then(|s| s.baked())),
    );
    let spool = hook_spool::spool_dir(&dd);
    let session_qs = session_id_query_suffix(&dd, &args.agent, &args.event, &json);
    let managed_qs = managed_run_query_suffix();
    let hook_qs = format!("{qs}{session_qs}{managed_qs}");

    // Spool THIS event — an instant local write, never the network. The auth
    // mode is decided without a round-trip: an explicit `--auth-token` is
    // stored inline; otherwise a present OIDC token marks the event `oidc`
    // (resolved + refreshed at drain time); otherwise anonymous.
    // #552: the bearer is no longer rendered onto the hook's command line, so
    // fall back to the copy `install-hooks --apply` persisted under the data
    // dir. An explicit `--auth-token` still wins, which keeps every config
    // written before this change working exactly as it did.
    let persisted_token = crate::config::read_hook_auth_token(&dd);
    let install_token = args.auth_token.as_deref().or(persisted_token.as_deref());
    // A profile carries its own URL and token and never falls back to the
    // install's credentials or to OIDC: `auth.json` belongs to the install
    // default, and presenting it to a profile's server would leak it across
    // servers.
    let (base, effective_token, profile_name) = match &profile {
        Some(profile) => (
            profile.url.as_str(),
            Some(profile.token.as_str()),
            Some(profile.name.as_str()),
        ),
        None => (args.server_url.trim_end_matches('/'), install_token, None),
    };

    let oidc_present = effective_token.is_none()
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
    if !external_capture {
        let ingest_key = uuid::Uuid::new_v4().simple().to_string();
        let event_url = format!(
            "{base}/hook?event={}&agent={}{}{}&ingest_key={ingest_key}",
            args.event, args.agent, hook_qs, capture_qs
        );
        let entry =
            hook_spool::entry_for(event_url, payload.clone(), effective_token, oidc_present)
                .routed_to(profile_name);
        if hook_spool::enqueue_with_config(&spool, &entry, Some(&spool_policy)).is_err() {
            eprintln!(
                "ai-memory hook warning: failed to spool lifecycle event; capture for this event was skipped"
            );
        }
        // ZCode is intentionally absent here: it has no SessionEnd and fires Stop
        // per turn, so its stored id is cleared by `finalize-session` (or
        // overwritten by the next session-start), never by a hook event.
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
                max_attempts,
            )
            .await;
        }

        // session-start: if this checkout has never had the one-time boot backfill
        // attempted, spawn it detached. It self-gates (config opt-out, empty-store
        // check) and records the attempt, so this is at most one extra process the
        // first time a project is opened after installing hooks — never inline, so
        // the SessionStart budget is untouched. Best-effort; a spawn failure must
        // not affect session start.
        if args.event == "session-start"
            && let Ok(trigger_cwd) = std::env::current_dir()
            && !super::backfill::sentinel_path(&dd, &trigger_cwd).exists()
        {
            let target = match profile_name {
                Some(name) => hook_drain_process::BackfillTarget::Profile(name),
                None => hook_drain_process::BackfillTarget::ServerUrl(base),
            };
            let _ = hook_drain_process::spawn_backfill(&dd, target);
        }
    }

    // session-start: drain any backlog (e.g. from a previous session that ended
    // abruptly), then fetch + inject the pending handoff for the resuming agent.
    if args.event == "session-start" {
        if !external_capture {
            let _ = hook_spool::drain_exclusive_within_budget(
                &spool,
                &dd,
                start_drain_budget(),
                drain_event_timeout(),
                max_attempts,
            )
            .await;
        }
        // Only fetch the handoff for agents that inject the session-start
        // hook's stdout as context. Grok ignores it, so fetching here would
        // consume the handoff server-side (the GET is destructive) and then
        // discard the result — silently losing it. Those agents recover the
        // handoff on demand via the MCP `memory_handoff_accept` tool.
        if agent_kind.session_start_injects_handoff() {
            let client = build_client();
            let bearer = hook_spool::resolve_bearer(&client, &dd, effective_token).await;
            let native_session_qs = canonical_session_id.as_deref().map_or_else(
                || session_qs.clone(),
                |session_id| format!("&session_id={}", url_encode(session_id)),
            );
            let handoff_url = format!(
                "{base}/handoff?agent={}{qs}{managed_qs}{native_session_qs}",
                args.agent
            );
            if let Some(handoff) =
                get_handoff(&client, &handoff_url, bearer.as_deref(), handoff_timeout()).await
            {
                if agent_kind == AgentKind::KiroCli {
                    // Kiro v2/v3 consume SessionStart stdout verbatim and define
                    // no wrapper envelope.
                    writeln!(stdout, "{handoff}")?;
                } else {
                    let envelope = session_start_handoff_envelope(agent_kind, handoff);
                    writeln!(stdout, "{envelope}")?;
                }
                return Ok(());
            }
        }
    }

    // user-prompt: agents whose SessionStart stdout is discarded AND whose
    // UserPromptSubmit stdout is injected (Kimi Code) receive the handoff
    // here. Grok discards both, so it must not take this path: the GET
    // accepts the handoff. The payload carries the native session id when
    // available, so the destructive GET can also link the managed run to the
    // native session, same as session-start does.
    // The installed kimi hook passes the script stem (`user-prompt-submit`)
    // while the legacy shell path posts `user-prompt`; HookEvent::parse
    // canonicalizes both (and the snake/native spellings) to UserPrompt.
    if HookEvent::parse(&args.event) == HookEvent::UserPrompt
        && AgentKind::from_wire(&args.agent).user_prompt_injects_handoff()
    {
        let client = build_client();
        let bearer = hook_spool::resolve_bearer(&client, &dd, effective_token).await;
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
        // The profile digest rides the first prompt the same way, gated on its
        // own marker because it is on by default rather than an opt-in.
        let profile_path = profile_digest_marker_path(briefed_marker_path(
            &dd,
            &args.agent,
            canonical_session_id.as_deref(),
            policy_cwd.as_deref(),
        ));
        let profile_qs = if profile_path.is_file() {
            "&profile_digest=0"
        } else {
            ""
        };
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
            "{base}/handoff?agent={}{handoff_qs}{managed_qs}{native_session_qs}{profile_qs}",
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
        mark_briefed(&profile_path);
        if let Some(handoff) = handoff {
            writeln!(stdout, "{handoff}")?;
        }
        // Never print the usual `{}` here: kimi injects any non-empty stdout
        // into the turn verbatim, so an envelope would become user-visible
        // text. Empty handoff or any fetch error means print nothing at all
        // (kimi ignores empty stdout; warnings go to stderr).
        return Ok(());
    }

    // post-tool-use: Grok shows hookSpecificOutput.additionalContext to the
    // model after the tool result. SessionStart and UserPromptSubmit stdout
    // are discarded, so this is the event that can carry a handoff without
    // burning it. One fetch per session. A session that never calls a tool
    // leaves the handoff open for memory_handoff_accept.
    if HookEvent::parse(&args.event) == HookEvent::PostToolUse
        && AgentKind::from_wire(&args.agent).post_tool_injects_handoff()
    {
        let shown = handoff_shown_path(
            &dd,
            &args.agent,
            canonical_session_id.as_deref(),
            policy_cwd.as_deref(),
        );
        if !shown.is_file() && !payload_is_subagent(&json) {
            let client = build_client();
            let bearer = hook_spool::resolve_bearer(&client, &dd, effective_token).await;
            let native_session_qs = canonical_session_id
                .as_deref()
                .map_or_else(String::new, |session_id| {
                    format!("&session_id={}", url_encode(session_id))
                });
            let handoff_url = format!(
                "{base}/handoff?agent={}{qs}{managed_qs}{native_session_qs}",
                args.agent
            );
            let handoff =
                get_handoff(&client, &handoff_url, bearer.as_deref(), handoff_timeout()).await;
            // Only a real body consumes this session's one chance. An empty
            // or failed claim must retry on the next parent tool; a child
            // session that errors must not burn the baton for the parent.
            if let Some(handoff) = handoff {
                mark_briefed(&shown);
                let envelope = grok_post_tool_handoff_envelope(&handoff);
                writeln!(stdout, "{envelope}")?;
                return Ok(());
            }
        }
    }

    // Boundary drain trigger: enqueue first, then ask a detached native drainer
    // to flush the shared spool. `session-end` remains the primary close path,
    // but `stop` and `pre-compact` also trigger the helper so delivery does not
    // rely on the single hook most likely to be cancelled during agent shutdown.
    if !external_capture && session_end_drains_in_process(agent_kind, &args.event) {
        drain_session_end_in_process(&spool, &dd, max_attempts).await;
    } else if !external_capture
        && should_spawn_background_drainer(&args.event)
        && let Err(err) = after_background_drain_event_enqueue(
            &dd,
            // The install's own token, resolved the same way an install-default
            // event authenticates: `--auth-token`, else the copy
            // `install-hooks --apply` persisted under the data dir (#552).
            // Current by construction either way. The drain uses it only to
            // retry an install-default entry the server has already rejected
            // with 401 (#542); a profile entry retries with its own profile's
            // token, so a profile's token is never handed over here.
            install_token,
            spawn_background_drainer,
        )
    {
        eprintln!(
            "ai-memory hook warning: failed to start background spool drainer; event remains queued: {err}"
        );
    }

    write_success_response(stdout, agent_kind, hook_event)?;
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

#[derive(Debug, PartialEq, Eq)]
struct FileRouteAnchor {
    marker: Option<PathBuf>,
    repository: Option<PathBuf>,
}

/// Find the existing directory that can establish routing for a file target.
/// New files may name one or more not-yet-created parent directories.
fn existing_target_dir(path: &Path) -> Option<PathBuf> {
    let mut candidate = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        if candidate.is_dir() {
            return Some(candidate);
        }
        candidate = candidate.parent()?.to_path_buf();
    }
}

fn file_route_anchor(cwd: &Path) -> FileRouteAnchor {
    let cwd_text = cwd.to_string_lossy();
    FileRouteAnchor {
        marker: crate::marker::find_marker(&cwd_text),
        repository: ai_memory_consolidate::discover_main_repo_root(cwd).ok(),
    }
}

/// Pick a destination cwd only when absolute file-tool paths prove a single
/// repository/marker boundary different from the payload cwd.
fn file_tool_routing_cwd(
    agent: AgentKind,
    raw: &serde_json::Value,
    payload_cwd: &str,
) -> Option<String> {
    let paths = absolute_file_tool_paths(agent, raw, payload_cwd)?;
    let source_dir = existing_target_dir(Path::new(payload_cwd))?;
    let source_anchor = file_route_anchor(&source_dir);
    let mut selected: Option<(FileRouteAnchor, PathBuf)> = None;
    for path in paths {
        let dir = existing_target_dir(Path::new(&path))?;
        let anchor = file_route_anchor(&dir);
        // An arbitrary absolute path is not a project and must not mint a new
        // scope merely because a file tool happened to inspect it.
        if anchor.marker.is_none() && anchor.repository.is_none() {
            return None;
        }
        match &selected {
            Some((expected, _)) if *expected != anchor => return None,
            Some(_) => {}
            None => selected = Some((anchor, dir)),
        }
    }
    let (target_anchor, dir) = selected?;
    (target_anchor != source_anchor).then(|| dir.to_string_lossy().into_owned())
}

/// Where one event is delivered (#992).
#[derive(Debug)]
enum HookRoute {
    /// No marker selects a server: the install-time `--server-url`, exactly
    /// as before profiles existed.
    InstallDefault,
    /// The marker's profile resolved; its URL and token replace the install's.
    Profile(ResolvedServer),
    /// The marker selects a profile that did not resolve. Nothing is emitted.
    Rejected {
        /// The selected name, when it is a valid one (safe to print).
        name: Option<ProfileName>,
        reason: Rejection,
    },
}

impl HookRoute {
    fn profile_name(&self) -> Option<&ProfileName> {
        match self {
            Self::InstallDefault => None,
            Self::Profile(resolved) => Some(&resolved.name),
            Self::Rejected { name, .. } => name.as_ref(),
        }
    }

    /// Label for `--check-capture`. Never a URL, path, or token.
    fn resolution(&self) -> &'static str {
        match self {
            Self::InstallDefault => "install-default",
            Self::Profile(_) => "marker",
            Self::Rejected { reason, .. } => reason.as_str(),
        }
    }
}

/// Resolve the route for one event.
///
/// The payload's cwd decides; when the payload carries none, the hook
/// process's own cwd does (the agent launches hooks in its workspace). An
/// event without a payload cwd must not escape a profile-routed tree to the
/// install default merely because the harness omitted the field.
fn resolve_hook_route(
    data_dir: &Path,
    policy_cwd: Option<&str>,
    process_cwd: impl FnOnce() -> Option<PathBuf>,
) -> HookRoute {
    let home = super::path_util::home_dir();
    let cwd = policy_cwd
        .map(str::to_owned)
        .or_else(|| process_cwd().map(|dir| dir.to_string_lossy().into_owned()));
    let Some(selection) = cwd
        .as_deref()
        .and_then(|cwd| crate::marker::find_server_selection(cwd, home.as_deref()))
    else {
        return HookRoute::InstallDefault;
    };
    let Some(raw_name) = selection.name else {
        return HookRoute::Rejected {
            name: None,
            reason: Rejection::UnreadableMarker,
        };
    };
    match server_profiles::resolve(data_dir, &raw_name, &selection.marker_dir, home.as_deref()) {
        Ok(resolved) => HookRoute::Profile(resolved),
        Err(reason) => HookRoute::Rejected {
            name: ProfileName::parse(&raw_name),
            reason,
        },
    }
}

/// Normalize the raw hook `cwd` into the base `policy.inspect` joins a tool
/// event's relative candidate path onto — lexically, via
/// `marker::absolute_normalized`, NOT `fs::canonicalize` (#671).
///
/// `capture_policy(policy_cwd)` resolves `[capture] ignore_paths` against a
/// marker directory that is itself lexically normalized (never symlink- or
/// verbatim-prefix-resolved). Canonicalizing only this side used to resolve
/// a symlinked cwd to its real target while the marker directory stayed at
/// the symlinked path, so a relative candidate (joined onto the
/// canonicalized cwd) and the marker's `ignore_paths` directory_base ended
/// up in two different path namespaces and never matched
/// (`capture_drop_handles_symlinked_cwd` regressed this way). Normalizing
/// both sides with the same lexical function — instead of resolving either
/// against the filesystem — keeps them in one namespace on every platform,
/// including for a candidate file that doesn't exist on disk (canonicalize
/// would `Err` on that and silently fall back to the raw, un-normalized
/// cwd).
fn lexical_capture_cwd(cwd: &str) -> String {
    crate::marker::absolute_normalized(Path::new(cwd))
        .into_os_string()
        .into_string()
        .unwrap_or_else(|_| cwd.to_owned())
}

/// File under the data dir holding the per-install capture mode (#446).
///
/// Deliberately a standalone file rather than a flag baked into each agent's
/// hook command: the mode then covers every agent and every render path at
/// once, and `install-hooks --apply` cannot regenerate it away. The reporter's
/// binding requirement was that the protection survive an upgrade — here it
/// does so by construction rather than by preservation logic that a new render
/// path could forget.
pub(crate) const CAPTURE_MODE_FILE: &str = "capture-mode";

/// Read the persisted capture mode. Anything unreadable, absent, or
/// unrecognised means the historical default: this file can only ever tighten
/// capture, never silently loosen it below what the operator already had.
fn persisted_capture_mode(data_dir: &Path) -> CaptureMode {
    match std::fs::read_to_string(data_dir.join(CAPTURE_MODE_FILE)) {
        Ok(text) if text.trim().eq_ignore_ascii_case("allowlist") => CaptureMode::Allowlist,
        _ => CaptureMode::Denylist,
    }
}

/// Whether this install's capture mode admits a checkout at `cwd`: the same
/// gate a live hook applies, for callers that replay events outside a hook
/// (`finalize-session`).
pub(crate) fn admits_capture_at(data_dir: &Path, cwd: &Path) -> bool {
    let marker_present = crate::marker::find_marker(&cwd.to_string_lossy()).is_some();
    repository_admits_capture(persisted_capture_mode(data_dir), marker_present)
}

fn is_tool_event(event: &str) -> bool {
    matches!(
        event.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
        "pretooluse" | "posttooluse" | "tooluse"
    )
}

/// Resolve the data dir for tests that exercise the hook implementation
/// directly: explicit `--data-dir`, then the captured runtime environment, then
/// the platform local-data directory.
#[cfg(test)]
fn resolve_data_dir(data_dir: Option<&Path>) -> PathBuf {
    resolve_data_dir_with(data_dir, &RuntimeEnv::from_process())
}

fn resolve_data_dir_with(data_dir: Option<&Path>, runtime_env: &RuntimeEnv) -> PathBuf {
    let dir = data_dir
        .map(Path::to_path_buf)
        .or_else(|| runtime_env.data_dir().map(Path::to_path_buf))
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

    /// The regression this whole change exists for. `inspect` runs only for
    /// tool events, so a gate expressed through `CaptureDisposition` would
    /// leave prompt bodies spooling from a repository that never opted in —
    /// reporting it as excluded while writing the text to disk. Pin the fact
    /// that the gate is event-independent.
    #[test]
    fn allowlist_gate_covers_events_that_never_reach_capture_policy() {
        for event in ["user-prompt-submit", "session-start", "session-end", "stop"] {
            assert!(
                !is_tool_event(event),
                "{event} must not be a tool event, or this test proves nothing"
            );
        }
        // With no marker present, allowlist mode admits none of them.
        assert!(!repository_admits_capture(CaptureMode::Allowlist, false));
    }

    #[test]
    fn persisted_mode_defaults_to_denylist_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            persisted_capture_mode(tmp.path()),
            CaptureMode::Denylist,
            "a missing file must not change existing installs"
        );
    }

    #[test]
    fn persisted_mode_reads_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(CAPTURE_MODE_FILE), "allowlist\n").unwrap();
        assert_eq!(persisted_capture_mode(tmp.path()), CaptureMode::Allowlist);
    }

    #[test]
    fn unreadable_or_unknown_mode_falls_back_to_denylist_not_allowlist() {
        // Failing "closed" here would be a denial of service: a corrupt file
        // would silently stop all capture. Tightening is the operator's
        // explicit choice, so garbage must read as the historical default.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(CAPTURE_MODE_FILE), "\u{0}not-a-mode").unwrap();
        assert_eq!(persisted_capture_mode(tmp.path()), CaptureMode::Denylist);
    }

    #[test]
    fn admits_capture_at_applies_the_allowlist_to_unmarked_checkouts() {
        let data_dir = tempfile::tempdir().unwrap();
        let checkout = tempfile::tempdir().unwrap();
        assert!(
            admits_capture_at(data_dir.path(), checkout.path()),
            "denylist mode (the default) admits any checkout"
        );
        std::fs::write(data_dir.path().join(CAPTURE_MODE_FILE), "allowlist\n").unwrap();
        assert!(
            !admits_capture_at(data_dir.path(), checkout.path()),
            "allowlist mode admits no checkout without a marker"
        );
        std::fs::write(
            checkout.path().join(".ai-memory.toml"),
            "workspace = \"ws\"\nproject = \"proj\"\n",
        )
        .unwrap();
        assert!(admits_capture_at(data_dir.path(), checkout.path()));
    }

    /// "The server is down": a loopback endpoint that accepts and immediately
    /// closes every connection. A closed port would do, but Windows takes ~2s
    /// to report a refused loopback connect, which made every test that posts
    /// to a dead server cost 2s per request.
    fn dead_server_url() -> String {
        ai_memory_test_support::dead_http_endpoint()
    }

    fn devin_hook_args(event: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "devin".into(),
            server_url: dead_server_url(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
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
    fn only_zero_session_end_drains_in_process() {
        assert!(session_end_drains_in_process(
            AgentKind::Zero,
            "session-end"
        ));
        for event in ["session-start", "post-tool-use", "subagent-stop"] {
            assert!(
                !session_end_drains_in_process(AgentKind::Zero, event),
                "{event}"
            );
        }
        for agent in [AgentKind::ClaudeCode, AgentKind::Codex, AgentKind::Zcode] {
            assert!(
                !session_end_drains_in_process(agent, "session-end"),
                "{agent:?}"
            );
        }
    }

    #[test]
    fn antigravity_preinvocation_only_maps_invocation_zero_to_session_start() {
        for (raw, expected) in [
            (serde_json::json!({"invocationNum": 0}), true),
            (serde_json::json!({"invocationNum": 1}), false),
            (serde_json::json!({"invocationNum": 0.5}), false),
            (serde_json::json!({"invocationNum": "0"}), false),
            (serde_json::json!({}), false),
        ] {
            assert_eq!(
                should_process_hook_event(AgentKind::AntigravityCli, HookEvent::SessionStart, &raw,),
                expected,
                "{raw}"
            );
        }
        assert!(should_process_hook_event(
            AgentKind::ClaudeCode,
            HookEvent::SessionStart,
            &serde_json::json!({})
        ));
        assert!(should_process_hook_event(
            AgentKind::AntigravityCli,
            HookEvent::PostToolUse,
            &serde_json::json!({})
        ));
    }

    #[test]
    fn hook_success_response_is_specific_to_antigravity_pre_tool_use() {
        for (agent, event, expected) in [
            (
                AgentKind::AntigravityCli,
                HookEvent::PreToolUse,
                b"{\"decision\": \"allow\"}\n".as_slice(),
            ),
            (
                AgentKind::AntigravityCli,
                HookEvent::PostToolUse,
                b"{}\n".as_slice(),
            ),
            (
                AgentKind::ClaudeCode,
                HookEvent::PreToolUse,
                b"{}\n".as_slice(),
            ),
            (AgentKind::KiroCli, HookEvent::PreToolUse, b"".as_slice()),
            (AgentKind::KiroCli, HookEvent::SessionStart, b"".as_slice()),
            (AgentKind::KiroCli, HookEvent::UserPrompt, b"".as_slice()),
            (AgentKind::KiroCli, HookEvent::PostToolUse, b"".as_slice()),
            (AgentKind::KiroCli, HookEvent::Stop, b"".as_slice()),
        ] {
            let mut output = Vec::new();
            write_success_response(&mut output, agent, event).unwrap();
            assert_eq!(output, expected, "{agent:?} {event:?}");
        }
    }

    fn drain_report(outcome: &hook_spool::LockedDrainResult) -> String {
        let mut out = Vec::new();
        write_drain_report(&mut out, outcome).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// A pass with nothing left behind says nothing, so `hook-drain.log` keeps
    /// receiving only warnings and a healthy instance never grows it.
    #[test]
    fn drain_report_is_silent_on_a_clean_pass() {
        for result in [
            hook_spool::DrainResult::default(),
            hook_spool::DrainResult {
                sent: 12,
                durable: 12,
                remaining: 0,
                dropped: 0,
            },
        ] {
            let report = drain_report(&hook_spool::LockedDrainResult::Drained(result));
            assert!(report.is_empty(), "clean pass emitted: {report}");
        }
    }

    /// The regression #493 exists for. Three passes that mean opposite things —
    /// everything delivered, everything discarded undelivered, and nothing even
    /// attempted because another drainer held the lock — used to produce the
    /// same empty output and the same exit 0, which is what left the reporter
    /// unable to tell a working drain from a silently lossy one. Pin that each
    /// one is now distinguishable from the other two.
    #[test]
    fn drain_report_separates_loss_and_contention_from_a_clean_pass() {
        let clean = drain_report(&hook_spool::LockedDrainResult::Drained(
            hook_spool::DrainResult {
                sent: 5,
                durable: 5,
                remaining: 0,
                dropped: 0,
            },
        ));
        let lossy = drain_report(&hook_spool::LockedDrainResult::Drained(
            hook_spool::DrainResult {
                sent: 0,
                durable: 0,
                remaining: 0,
                dropped: 7,
            },
        ));
        let busy = drain_report(&hook_spool::LockedDrainResult::LockBusy);

        assert!(clean.is_empty());
        assert!(
            lossy.contains("DROPPED") && lossy.contains('7'),
            "a pass that discarded 7 undelivered events must name the loss: {lossy}"
        );
        assert!(
            !busy.contains("queued"),
            "LockBusy is returned before the spool is listed, so the report must \
             not claim anything about queued events: {busy}"
        );
        assert!(
            busy.contains("lock"),
            "a contended pass must say it delivered nothing: {busy}"
        );
        assert_ne!(lossy, busy);
        assert_ne!(lossy, clean);
        assert_ne!(busy, clean);
    }

    /// Events merely still queued (server down, or the budget ran out) are a
    /// warning too — they are the state that precedes the drop — but they must
    /// not be reported as lost.
    #[test]
    fn drain_report_distinguishes_still_queued_from_dropped() {
        let report = drain_report(&hook_spool::LockedDrainResult::Drained(
            hook_spool::DrainResult {
                sent: 1,
                durable: 1,
                remaining: 4,
                dropped: 0,
            },
        ));
        assert!(report.contains("4 still queued"), "{report}");
        assert!(report.contains("0 DROPPED"), "{report}");
    }

    #[test]
    fn kiro_v3_live_lifecycle_fixtures_share_native_context() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/kiro-v3-hook-payloads.json"
        ))
        .unwrap();
        let events = fixture["events"].as_array().unwrap();
        assert_eq!(events.len(), 5);
        for event in events {
            let payload = &event["payload"];
            let (cwd, session_id) = hook_context("kiro-cli", payload);
            assert_eq!(cwd.as_deref(), Some("/workspace/project"));
            assert_eq!(session_id.as_deref(), Some("kiro-v3-session"));
        }
        assert_eq!(events[0]["payload"]["hook_event_name"], "SessionStart");
        assert_eq!(events[1]["payload"]["hook_event_name"], "UserPromptSubmit");
        assert_eq!(events[2]["payload"]["hook_event_name"], "PreToolUse");
        assert_eq!(events[3]["payload"]["hook_event_name"], "PostToolUse");
        assert_eq!(events[4]["payload"]["hook_event_name"], "Stop");
        assert_eq!(events[2]["payload"]["tool_name"], "read_file");
        assert_eq!(
            events[2]["payload"]["tool_input"]["path"],
            "/workspace/project/sample.txt"
        );
    }

    #[test]
    fn cursor_copy_via_claude_code_hooks_is_dropped_only_when_cursor_hooks_exist() {
        let cursor = serde_json::json!({
            "cursor_version": "2026.09.02-c22c1a3",
            "conversation_id": "c1",
        });
        let claude = serde_json::json!({"session_id": "s1"});
        assert!(is_redundant_cursor_copy(
            AgentKind::ClaudeCode,
            &cursor,
            || true
        ));
        // Without Cursor's own hooks the Claude Code path is the only capture.
        assert!(!is_redundant_cursor_copy(
            AgentKind::ClaudeCode,
            &cursor,
            || false
        ));
        // The native Cursor hook itself is never dropped.
        assert!(!is_redundant_cursor_copy(
            AgentKind::Cursor,
            &cursor,
            || true
        ));
        // A real Claude Code payload never consults the filesystem.
        assert!(!is_redundant_cursor_copy(
            AgentKind::ClaudeCode,
            &claude,
            || { panic!("must not check Cursor hooks for a Claude Code payload") }
        ));
    }

    #[tokio::test]
    async fn antigravity_native_pre_tool_use_allows_and_spools_valid_input() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            antigravity_hook_args("pre-tool-use", &dead_server_url()),
            serde_json::json!({
                "conversationId": "agy-session",
                "workspacePaths": [tmp.path()],
                "toolCall": {"name": "view_file", "args": {"AbsolutePath": "README.md"}}
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{\"decision\": \"allow\"}\n");
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 1);
    }

    #[tokio::test]
    async fn antigravity_native_pre_tool_use_fails_open_on_malformed_input() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            antigravity_hook_args("pre-tool-use", &dead_server_url()),
            "not-json".into(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{\"decision\": \"allow\"}\n");
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
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
            |_, _| Ok(()),
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
    async fn devin_generated_session_id_reaches_startup_handoff_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (base, mut requests) = serve_requests("200 OK", "DEVIN-HANDOFF").await;
        let mut args = devin_hook_args("session-start");
        args.server_url = base;
        let mut stdout = Vec::new();

        run_with_payload(
            Some(data_dir.clone()),
            args,
            serde_json::json!({
                "hook_event_name": "SessionStart",
                "source": "startup"
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        let mut recorded = Vec::new();
        while let Some(request) = first_request(&mut requests).await {
            recorded.push(request);
        }
        assert!(
            recorded
                .iter()
                .any(|request| request.starts_with("POST /hook")),
            "session start must be posted: {recorded:?}"
        );
        let get = recorded
            .iter()
            .find(|request| request.starts_with("GET /handoff?"))
            .expect("startup handoff must be fetched");
        let request_param = |request: &str| {
            request
                .split_whitespace()
                .nth(1)
                .and_then(|target| query_param(target, "session_id"))
                .map(str::to_owned)
        };
        assert_eq!(
            request_param(get).as_deref(),
            stored_session_id(&data_dir, AgentKind::Devin).as_deref(),
            "the destructive startup claim must use the generated receiver id: {recorded:?}"
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
    fn session_id_query_suffix_is_opt_in_per_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = serde_json::json!({"hook_event_name": "PostToolUse"});

        let suffix = session_id_query_suffix(tmp.path(), "claude-code", "post-tool-use", &raw);

        assert!(suffix.is_empty());
        assert!(stored_session_id(tmp.path(), AgentKind::ClaudeCode).is_none());
    }

    #[test]
    fn zcode_query_session_id_is_stable_across_turns_without_native_id() {
        // ZCode fires Stop at the end of every turn and has no SessionEnd; the
        // stored id must survive Stop so one agent session maps to one
        // server-side session.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let bare = serde_json::json!({"hook_event_name": "SessionStart"});

        let first = session_id_query_suffix(data_dir, "zcode", "session-start", &bare);
        let after_stop = session_id_query_suffix(data_dir, "zcode", "stop", &bare);
        let next_turn = session_id_query_suffix(data_dir, "zcode", "pre-tool-use", &bare);

        assert!(first.starts_with("&session_id="), "{first}");
        assert_eq!(after_stop, first);
        assert_eq!(next_turn, first);
        assert_eq!(
            stored_session_id(data_dir, AgentKind::Zcode).as_deref(),
            first.strip_prefix("&session_id=")
        );
    }

    #[test]
    fn zcode_query_session_id_does_not_override_native_payload_id() {
        let tmp = tempfile::tempdir().unwrap();
        let with_session = serde_json::json!({
            "session_id": "zcode-native-session",
            "hook_event_name": "PostToolUse"
        });

        let suffix = session_id_query_suffix(tmp.path(), "zcode", "post-tool-use", &with_session);

        assert!(suffix.is_empty());
        assert!(stored_session_id(tmp.path(), AgentKind::Zcode).is_none());
    }

    #[test]
    fn zcode_stop_event_does_not_clear_stored_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        store_session_id(data_dir, AgentKind::Zcode, "stable-zcode-session");

        // The only hook-side clearing is Devin's session-end; ZCode's stored
        // id is cleared by `finalize-session` (or overwritten by the next
        // session-start), never by Stop.
        let bare = serde_json::json!({"hook_event_name": "Stop"});
        let _ = session_id_query_suffix(data_dir, "zcode", "stop", &bare);

        assert_eq!(
            stored_session_id(data_dir, AgentKind::Zcode).as_deref(),
            Some("stable-zcode-session")
        );
    }

    #[test]
    fn devin_missing_cwd_uses_devin_project_dir_before_process_cwd() {
        let raw = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": "startup"
        });

        let cwd = resolve_hook_cwd_with(
            "devin",
            &raw,
            |name| (name == "DEVIN_PROJECT_DIR").then(|| "env-project".into()),
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(cwd.as_deref(), Some("env-project"));
    }

    #[test]
    fn devin_missing_cwd_uses_process_cwd_when_env_is_missing() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec"
        });

        let cwd = resolve_hook_cwd_with(
            "devin",
            &raw,
            |_| None,
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(cwd.as_deref(), Some("process-project"));
    }

    #[test]
    fn devin_missing_cwd_uses_env_or_process_cwd() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "exec"
        });

        let from_env = resolve_hook_cwd_with(
            "devin",
            &raw,
            |name| (name == "DEVIN_PROJECT_DIR").then(|| "env-project".into()),
            || Some(PathBuf::from("process-project")),
        );
        let from_process = resolve_hook_cwd_with(
            "devin",
            &raw,
            |_| None,
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(from_env.as_deref(), Some("env-project"));
        assert_eq!(from_process.as_deref(), Some("process-project"));
    }

    #[test]
    fn devin_payload_cwd_wins_over_fallbacks() {
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "cwd": "payload-project"
        });

        let cwd = resolve_hook_cwd_with(
            "devin",
            &raw,
            |name| (name == "DEVIN_PROJECT_DIR").then(|| "env-project".into()),
            || Some(PathBuf::from("process-project")),
        );

        assert_eq!(cwd.as_deref(), Some("payload-project"));
    }

    #[test]
    fn missing_cwd_process_fallback_is_devin_only() {
        let raw = serde_json::json!({"hook_event_name": "PostToolUse"});

        let cwd = resolve_hook_cwd_with(
            "claude-code",
            &raw,
            |_| Some("env-project".into()),
            || Some(PathBuf::from("process-project")),
        );

        assert!(cwd.is_none());
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            server_url: dead_server_url(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        };

        run_with_payload(
            Some(data_dir.clone()),
            args,
            r#"{"session_id":"s","cwd":"/tmp"}"#.into(),
            &mut stdout,
            |path, _token| {
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
                server_url: dead_server_url(),
                auth_token: None,
                project_strategy: None,
                check_capture: false,
                capture_assistant: false,
                capture_mode: None,
            };

            run_with_payload(
                Some(data_dir.clone()),
                args,
                r#"{"session_id":"s","cwd":"/tmp"}"#.into(),
                &mut stdout,
                |path, _token| {
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

    fn zero_hook_args(event: &str, server_url: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "zero".into(),
            server_url: server_url.into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        }
    }

    /// Zero's sandbox kills the detached drainer with the hook (#1172), so its
    /// session-end must deliver before the hook returns.
    #[tokio::test]
    async fn zero_session_end_delivers_in_process_without_a_detached_drainer() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let (base, mut requests) = serve_requests("200 OK", r#"{"accepted":1}"#).await;
        let mut stdout = Vec::new();

        run_with_payload(
            Some(data_dir.clone()),
            zero_hook_args("session-end", &base),
            r#"{"sessionId":"s","cwd":"/tmp"}"#.into(),
            &mut stdout,
            |_, _| panic!("zero's session-end must not spawn a detached drainer"),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        let request = first_request(&mut requests)
            .await
            .expect("drained in-process");
        assert!(request.starts_with("POST /hook/batch"), "{request}");
        assert_eq!(
            hook_spool::spool_len(&spool),
            0,
            "delivered event left the spool"
        );
    }

    /// With the sandbox's network denied the in-process drain fails fast and
    /// the event stays spooled for `hook-drain` or the next session.
    #[tokio::test]
    async fn zero_session_end_without_network_keeps_the_event_spooled() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let spool = hook_spool::spool_dir(&data_dir);
        let refused = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let mut stdout = Vec::new();

        run_with_payload(
            Some(data_dir.clone()),
            zero_hook_args("session-end", &refused),
            r#"{"sessionId":"s","cwd":"/tmp"}"#.into(),
            &mut stdout,
            |_, _| panic!("zero's session-end must not spawn a detached drainer"),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        let entries = read_spooled_entries(&spool);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].attempts, 0,
            "an unreachable server costs no retry"
        );
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
            server_url: dead_server_url(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        };

        run_with_payload(
            Some(data_dir),
            args,
            "{}".into(),
            &mut stdout,
            |_path, _token| Err(std::io::Error::other("spawn failed")),
        )
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
            server_url: dead_server_url(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        };

        run_with_payload(
            Some(data_dir.clone()),
            args,
            r#"{"hook_event_name":"SessionEnd"}"#.into(),
            &mut stdout,
            |_, _| Ok(()),
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
        let err = after_background_drain_event_enqueue(tmp.path(), None, |_path, _token| {
            Err(std::io::Error::other("spawn failed"))
        })
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn background_drain_event_policy_spawns_without_inline_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let called = std::cell::Cell::new(false);

        after_background_drain_event_enqueue(tmp.path(), None, |path, _token| {
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
        args.server_url = dead_server_url();
        run_with_payload(Some(data_dir.clone()), args, serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"secret/SENTINEL"}}).to_string(), &mut stdout, |_, _| { called.set(true); Ok(()) }).await.unwrap();
        assert_eq!(stdout, b"{}\n");
        assert!(!called.get());
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    #[tokio::test]
    async fn absolute_file_target_uses_destination_scope_and_capture_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join(".ai-memory.toml"),
            "workspace = \"shared\"\nproject = \"source\"\n",
        )
        .unwrap();
        std::fs::write(
            destination.join(".ai-memory.toml"),
            "workspace = \"shared\"\nproject = \"destination\"\n[capture]\nignore_paths = [\"secret/**\"]\n",
        )
        .unwrap();
        let data_dir = tmp.path().join("data");

        let mut args = devin_hook_args("post-tool-use");
        args.agent = "claude-code".into();
        let excluded = serde_json::json!({
            "session_id": "cross-project",
            "cwd": source,
            "tool_name": "Edit",
            "tool_input": {"file_path": destination.join("secret/private.txt")},
            "tool_response": {"content": "SENTINEL_MUST_NOT_BE_SPOOLED"}
        });
        run_with_payload(
            Some(data_dir.clone()),
            args,
            excluded.to_string(),
            &mut Vec::new(),
            |_, _| panic!("a dropped tool event must not start a drainer"),
        )
        .await
        .unwrap();
        let spool = hook_spool::spool_dir(&data_dir);
        assert_eq!(hook_spool::spool_len(&spool), 0);

        let mut args = devin_hook_args("post-tool-use");
        args.agent = "claude-code".into();
        let public = serde_json::json!({
            "session_id": "cross-project",
            "cwd": source,
            "tool_name": "Edit",
            "tool_input": {"file_path": destination.join("public.txt")},
            "tool_response": {"content": "public"}
        });
        run_with_payload(
            Some(data_dir),
            args,
            public.to_string(),
            &mut Vec::new(),
            |_, _| panic!("a tool event below the threshold must only spool"),
        )
        .await
        .unwrap();
        let entries = read_spooled_entries(&spool);
        assert_eq!(entries.len(), 1);
        assert_eq!(query_param(&entries[0].url, "workspace"), Some("shared"));
        assert_eq!(query_param(&entries[0].url, "project"), Some("destination"));
        assert_eq!(
            query_param(&entries[0].url, "cwd"),
            Some(url_encode(destination.to_str().unwrap()).as_str())
        );
        let body: serde_json::Value = serde_json::from_str(&entries[0].body).unwrap();
        assert_eq!(body["cwd"], destination.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn absolute_file_target_cannot_escape_a_destination_server_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(
            source.join(".ai-memory.toml"),
            "workspace = \"shared\"\nproject = \"source\"\n",
        )
        .unwrap();
        std::fs::write(
            destination.join(".ai-memory.toml"),
            "server = \"unregistered\"\nworkspace = \"shared\"\nproject = \"destination\"\n",
        )
        .unwrap();
        let data_dir = tmp.path().join("data");
        let mut args = devin_hook_args("post-tool-use");
        args.agent = "claude-code".into();
        let raw = serde_json::json!({
            "session_id": "cross-profile",
            "cwd": source,
            "tool_name": "Edit",
            "tool_input": {"file_path": destination.join("public.txt")}
        });

        run_with_payload(
            Some(data_dir.clone()),
            args,
            raw.to_string(),
            &mut Vec::new(),
            |_, _| panic!("a refused destination route must not start a drainer"),
        )
        .await
        .unwrap();

        assert_eq!(
            hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)),
            0,
            "the event must not fall back to the source repository's server"
        );
    }

    #[test]
    fn file_target_routing_refuses_relative_external_and_mixed_project_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let destination = tmp.path().join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(source.join(".ai-memory.toml"), "project = \"source\"\n").unwrap();
        std::fs::write(
            destination.join(".ai-memory.toml"),
            "project = \"destination\"\n",
        )
        .unwrap();
        let cwd = source.to_str().unwrap();
        let relative = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": "../destination/file.txt"}
        });
        assert_eq!(
            file_tool_routing_cwd(AgentKind::ClaudeCode, &relative, cwd),
            None,
            "a relative path cannot independently prove a destination"
        );

        let external_dir = tmp.path().join("not-a-project");
        std::fs::create_dir_all(&external_dir).unwrap();
        let external = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": external_dir.join("file.txt")}
        });
        assert_eq!(
            file_tool_routing_cwd(AgentKind::ClaudeCode, &external, cwd),
            None,
            "an arbitrary absolute path must not mint a project"
        );

        let mixed = serde_json::json!({
            "tool_name": "read",
            "tool_input": {"paths": [
                source.join("one.txt"),
                destination.join("two.txt")
            ]}
        });
        assert_eq!(
            file_tool_routing_cwd(AgentKind::ClaudeCode, &mixed, cwd),
            None,
            "one event cannot be attributed to two projects"
        );
    }

    #[tokio::test]
    async fn shell_command_reading_an_ignored_path_is_dropped_before_spool() {
        for (command, spooled) in [("cat ./secret/*.md | head", 0), ("cat public/readme.md", 1)] {
            for event in ["pre-tool-use", "post-tool-use"] {
                let tmp = tempfile::tempdir().unwrap();
                std::fs::write(
                    tmp.path().join(".ai-memory.toml"),
                    "[capture]\nignore_paths = [\"secret/**\"]\n",
                )
                .unwrap();
                let data_dir = tmp.path().join("data");
                let mut args = devin_hook_args(event);
                args.agent = "claude-code".into();
                let raw = serde_json::json!({
                    "session_id": "shell", "cwd": tmp.path(),
                    "tool_name": "Bash", "tool_input": {"command": command},
                    "tool_response": {"stdout": "SENTINEL_CONTENT"},
                });
                let mut stdout = Vec::new();
                run_with_payload(
                    Some(data_dir.clone()),
                    args,
                    raw.to_string(),
                    &mut stdout,
                    |_, _| panic!("tool events below the threshold must only spool"),
                )
                .await
                .unwrap();
                assert_eq!(
                    hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)),
                    spooled,
                    "{event}: {command}"
                );
            }
        }
    }

    #[tokio::test]
    async fn codex_native_capture_policy_runs_before_spool_and_preserves_identity() {
        for (tool, input, disposition) in [
            (
                "read_file",
                serde_json::json!({"path": "secret/private.txt"}),
                CaptureDisposition::Drop,
            ),
            (
                "apply_patch",
                serde_json::json!({"command": "*** Begin Patch\n*** Add File: secret/private.txt\n+PRIVATE_CONTENT\n*** End Patch"}),
                CaptureDisposition::MetadataOnly,
            ),
            (
                "apply_patch",
                serde_json::json!(null),
                CaptureDisposition::MetadataOnly,
            ),
        ] {
            for event in ["pre-tool-use", "post-tool-use"] {
                let tmp = tempfile::tempdir().unwrap();
                std::fs::write(
                    tmp.path().join(".ai-memory.toml"),
                    "workspace = \"native\"\nproject = \"codex\"\n[capture]\nignore_paths = [\"secret/**\"]\n",
                ).unwrap();
                let data_dir = tmp.path().join("data");
                let mut args = devin_hook_args(event);
                args.agent = "codex".into();
                let raw = serde_json::json!({
                    "session_id": "native-codex", "cwd": tmp.path(), "turn_id": "turn-1",
                    "hook_event_name": if event == "pre-tool-use" { "PreToolUse" } else { "PostToolUse" },
                    "tool_name": tool, "tool_input": input, "tool_use_id": "call-native-1",
                    "tool_response": {"content": [{"type": "text", "text": "PRIVATE_CONTENT"}]},
                });
                let mut stdout = Vec::new();
                run_with_payload(
                    Some(data_dir.clone()),
                    args,
                    raw.to_string(),
                    &mut stdout,
                    |_, _| {
                        panic!("tool events below the threshold must only spool");
                    },
                )
                .await
                .unwrap();
                assert_eq!(stdout, b"{}\n");
                let spool = hook_spool::spool_dir(&data_dir);
                if disposition == CaptureDisposition::Drop {
                    assert!(!spool.exists());
                } else {
                    let entries = read_spooled_entries(&spool);
                    assert_eq!(entries.len(), 1);
                    let entry = &entries[0];
                    assert_eq!(query_param(&entry.url, "agent"), Some("codex"));
                    assert_eq!(query_param(&entry.url, "workspace"), Some("native"));
                    assert_eq!(query_param(&entry.url, "project"), Some("codex"));
                    assert!(query_param(&entry.url, "ingest_key").is_some());
                    assert!(!entry.body.contains("PRIVATE_CONTENT"));
                    assert!(!entry.body.contains("private.txt"));
                    let body: serde_json::Value = serde_json::from_str(&entry.body).unwrap();
                    assert_eq!(body["session_id"], "native-codex");
                    assert_eq!(body["tool_call_id"], "call-native-1");
                    assert_eq!(body["_ai_memory_capture"]["disposition"], "metadata-only");
                    assert_eq!(
                        body["_ai_memory_capture"]["extraction_state"],
                        "missing-or-malformed"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn hermes_file_exclusion_drops_before_spool_or_drain() {
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
        args.agent = "hermes".into();
        let raw = serde_json::json!({
            "hook_event_name": "post_tool_call",
            "tool_name": "write_file",
            "tool_input": {
                "path": "secret/token.txt",
                "content": "SENTINEL_MUST_NOT_BE_SPOOLED"
            },
            "session_id": "hermes-session",
            "cwd": tmp.path(),
            "extra": {"tool_call_id": "call-42", "status": "ok"}
        });
        run_with_payload(
            Some(data_dir.clone()),
            args,
            raw.to_string(),
            &mut stdout,
            |_, _| {
                called.set(true);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert!(!called.get());
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    #[tokio::test]
    async fn pool_file_exclusion_drops_before_spool_or_drain() {
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
        args.agent = "pool".into();
        let raw = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "write",
            "tool_input": {
                "path": "secret/token.txt",
                "content": "SENTINEL_MUST_NOT_BE_SPOOLED"
            },
            "session_id": "pool-session",
            "cwd": tmp.path()
        });
        run_with_payload(
            Some(data_dir.clone()),
            args,
            raw.to_string(),
            &mut stdout,
            |_, _| {
                called.set(true);
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert!(!called.get());
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capture_drop_handles_symlinked_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(
            project.join(".ai-memory.toml"),
            "[capture]\nignore_paths = [\"secret/**\"]\n",
        )
        .unwrap();
        let alias = tmp.path().join("project-alias");
        std::os::unix::fs::symlink(&project, &alias).unwrap();

        let data_dir = tmp.path().join("data");
        let mut stdout = Vec::new();
        let called = std::cell::Cell::new(false);
        run_with_payload(
            Some(data_dir.clone()),
            devin_hook_args("post-tool-use"),
            serde_json::json!({
                "cwd": alias,
                "tool_name": "Edit",
                "tool_input": {"path": "secret/SENTINEL"}
            })
            .to_string(),
            &mut stdout,
            |_, _| {
                called.set(true);
                Ok(())
            },
        )
        .await
        .unwrap();

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
        run_with_payload(Some(data_dir.clone()), devin_hook_args("post-tool-use"), serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"SENTINEL_PATH","args":"SENTINEL_ARGS"},"output":"SENTINEL_OUTPUT","error":"SENTINEL_ERROR","nested":{"raw":"SENTINEL_NESTED"}}).to_string(), &mut stdout, |_, _| Ok(())).await.unwrap();
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
        run_with_payload(Some(data_dir.clone()), args, serde_json::json!({"cwd":tmp.path(),"tool_name":"Edit","tool_input":{"path":"SENTINEL"}}).to_string(), &mut stdout, |_, _| Err(std::io::Error::other("must not spawn"))).await.unwrap();
        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        // Pin the exact key set, not just its size: `--check-capture` is how an
        // operator verifies an opt-out, so a field silently appearing or
        // vanishing here is a defect in its own right.
        let mut keys: Vec<&str> = output
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "admits_capture",
                "capture_mode",
                "disposition",
                "event_admits_capture",
                "extraction_state",
                "marker_present",
                "path_count",
                "policy_admits_capture",
                "policy_state",
                "scope",
                "scope_resolution",
                "server_profile",
                "server_resolution",
                "tool_family",
                "version",
            ]
        );
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
        assert!(!String::from_utf8(stdout).unwrap().contains("SENTINEL"));
    }

    #[tokio::test]
    async fn check_capture_refuses_partial_scope_and_excluded_content() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let marker = tmp.path().join(".ai-memory.toml");
        for (declaration, path, admits, resolution) in [
            ("workspace = \"team\"\n", "public.txt", false, "partial"),
            (
                "workspace = \"team\"\nproject = \"app\"\n",
                "secret/token.txt",
                false,
                "explicit",
            ),
            (
                "workspace = \"team\"\nproject = \"app\"\n",
                "public.txt",
                true,
                "explicit",
            ),
        ] {
            std::fs::write(
                &marker,
                format!("{declaration}[capture]\nignore_paths = [\"secret/**\"]\n"),
            )
            .unwrap();
            let mut args = devin_hook_args("post-tool-use");
            args.check_capture = true;
            let mut stdout = Vec::new();
            run_with_payload(
                Some(data_dir.clone()), args,
                serde_json::json!({"cwd":tmp.path(), "tool_name":"Edit", "tool_input":{"path":path,"content":"PRIVATE_CONTENT_SENTINEL"}}).to_string(),
                &mut stdout,
                |_, _| Err(std::io::Error::other("inspection must not spawn")),
            ).await.unwrap();
            let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(report["policy_admits_capture"], admits, "{report}");
            assert_eq!(report["scope_resolution"], resolution);
            assert_eq!(report["scope"]["workspace"], "team");
            let printed = String::from_utf8(stdout).unwrap();
            assert!(!printed.contains("PRIVATE_CONTENT_SENTINEL"));
            assert!(!printed.contains("token.txt"));
            assert!(!data_dir.exists());
        }
    }

    #[tokio::test]
    async fn check_capture_bounds_marker_hints_and_refuses_oversized_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        for (workspace, identity, admits) in [
            ("x".repeat(513), "app".to_owned(), false),
            ("team".to_owned(), "y".repeat(513), false),
            ("team".to_owned(), "y".repeat(512), true),
        ] {
            std::fs::write(
                tmp.path().join(".ai-memory.toml"),
                format!(
                    "workspace = \"{workspace}\"\nproject = \"app\"\nidentity = \"{identity}\"\n"
                ),
            )
            .unwrap();
            let mut args = devin_hook_args("post-tool-use");
            args.check_capture = true;
            let mut stdout = Vec::new();
            run_with_payload(
                Some(data_dir.clone()), args,
                serde_json::json!({"cwd":tmp.path(), "tool_name":"Edit", "tool_input":{"path":"public.txt"}}).to_string(),
                &mut stdout,
                |_, _| Err(std::io::Error::other("inspection must not spawn")),
            ).await.unwrap();
            let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(report["policy_admits_capture"], admits, "{report}");
            assert_eq!(
                report["scope_resolution"],
                if admits { "explicit" } else { "unavailable" }
            );
            assert_eq!(report["scope"]["project"], "app");
            let printed = String::from_utf8(stdout).unwrap();
            for (field, value) in [("workspace", &workspace), ("identity", &identity)] {
                if value.len() > 512 {
                    assert!(report["scope"][field].is_null(), "{report}");
                    assert!(!printed.contains(value));
                } else {
                    assert_eq!(report["scope"][field], value.as_str());
                }
            }
            assert!(!data_dir.exists());
            // Inspection bounds do not change the native routing contract.
            let suffix = marker_query_suffix(tmp.path().to_str().unwrap(), None);
            assert!(suffix.contains(&workspace));
            assert!(suffix.contains(&format!("&identity={identity}")));
        }
    }

    #[tokio::test]
    async fn check_capture_reports_rejected_lifecycle_without_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let mut args = devin_hook_args("session-start");
        args.agent = "antigravity-cli".into();
        args.check_capture = true;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            args,
            serde_json::json!({"cwd":tmp.path(),"invocationNum":4}).to_string(),
            &mut stdout,
            |_, _| Err(std::io::Error::other("inspection must not spawn")),
        )
        .await
        .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(report["event_admits_capture"], false);
        assert_eq!(report["policy_admits_capture"], false);
        assert!(!data_dir.exists());
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
    async fn antigravity_pre_tool_capture_drop_still_allows_the_tool() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".ai-memory.toml"),
            "workspace = \"canonical\"\n[capture]\nignore_paths = [\"secret/**\"]\n",
        )
        .unwrap();
        let data_dir = tmp.path().join("data");
        let mut args = devin_hook_args("pre-tool-use");
        args.agent = "antigravity-cli".into();
        let mut stdout = Vec::new();
        let raw = serde_json::json!({"workspacePaths":[tmp.path()],"toolCall":{"name":"Edit","args":{"path":"secret/a"}}});
        assert!(cwd_query_suffix("antigravity-cli", &raw, None).contains("workspace=canonical"));
        run_with_payload(
            Some(data_dir.clone()),
            args,
            raw.to_string(),
            &mut stdout,
            |_, _| Err(std::io::Error::other("must not spawn")),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"{\"decision\": \"allow\"}\n");
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
                |_, _| Ok(()),
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

    fn grok_hook_args(event: &str, server_url: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "grok".into(),
            server_url: server_url.into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        }
    }

    #[tokio::test]
    async fn antigravity_post_tool_use_spools_enriched_output() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact_dir = tmp.path().join("brain").join("conv-123");
        let step_dir = artifact_dir
            .join(".system_generated")
            .join("steps")
            .join("7");
        std::fs::create_dir_all(&step_dir).unwrap();
        std::fs::write(
            step_dir.join("output.txt"),
            "npm test passed with 0 errors\n",
        )
        .unwrap();

        let data_dir = tmp.path().join("data");
        let raw = serde_json::json!({
            "conversationId": "conv-123",
            "workspacePaths": [tmp.path().to_str().unwrap()],
            "toolCall": {
                "name": "run_command",
                "args": {
                    "CommandLine": "npm test"
                }
            },
            "stepIdx": 7,
            "artifactDirectoryPath": artifact_dir.to_str().unwrap()
        });

        let mut stdout = Vec::new();
        let args = HookArgs {
            event: "post-tool-use".into(),
            agent: "antigravity-cli".into(),
            server_url: "http://127.0.0.1:41399".into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        };

        run_with_payload(
            Some(data_dir.clone()),
            args,
            raw.to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        let spooled = read_spooled_entries(&hook_spool::spool_dir(&data_dir));
        assert_eq!(spooled.len(), 1);
        let body: serde_json::Value = serde_json::from_str(&spooled[0].body).unwrap();
        assert_eq!(
            body.get("tool_response")
                .and_then(serde_json::Value::as_str),
            Some("npm test passed with 0 errors")
        );
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
            capture_mode: None,
        }
    }

    fn kiro_hook_args(event: &str, server_url: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "kiro-cli".into(),
            server_url: server_url.into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        }
    }

    fn copilot_cli_hook_args(event: &str, server_url: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "copilot-cli".into(),
            server_url: server_url.into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
        }
    }

    fn antigravity_hook_args(event: &str, server_url: &str) -> HookArgs {
        HookArgs {
            event: event.into(),
            agent: "antigravity-cli".into(),
            server_url: server_url.into(),
            auth_token: None,
            project_strategy: None,
            check_capture: false,
            capture_assistant: false,
            capture_mode: None,
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
    async fn antigravity_initial_invocation_fetches_with_native_output_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "AGY-HANDOFF").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().join("data")),
            antigravity_hook_args("session-start", &base),
            serde_json::json!({
                "invocationNum": 0,
                "initialNumSteps": 0,
                "conversationId": "agy-conversation",
                "workspacePaths": [tmp.path()]
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(
            output,
            serde_json::json!({
                "injectSteps": [{"ephemeralMessage": "AGY-HANDOFF"}]
            })
        );
        let mut recorded = Vec::new();
        while let Some(request) = first_request(&mut requests).await {
            recorded.push(request);
        }
        assert!(
            recorded
                .iter()
                .any(|request| request.starts_with("POST /hook")),
            "{recorded:?}"
        );
        assert!(
            recorded.iter().any(|request| {
                request.starts_with("GET /handoff?")
                    && request.contains("session_id=agy-conversation")
            }),
            "{recorded:?}"
        );
    }

    #[tokio::test]
    async fn antigravity_later_invocation_has_no_capture_or_handoff_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (base, mut requests) = serve_requests("200 OK", "MUST-NOT-BE-CONSUMED").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            antigravity_hook_args("session-start", &base),
            serde_json::json!({
                "invocationNum": 4,
                "initialNumSteps": 12,
                "conversationId": "agy-conversation",
                "workspacePaths": [tmp.path()]
            })
            .to_string(),
            &mut stdout,
            |_, _| Err(std::io::Error::other("must not spawn")),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        assert!(first_request(&mut requests).await.is_none());
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    #[tokio::test]
    async fn kiro_session_start_prints_handoff_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "KIRO-HANDOFF").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().join("data")),
            kiro_hook_args("session-start", &base),
            serde_json::json!({
                "session_id": "kiro-session",
                "cwd": tmp.path()
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"KIRO-HANDOFF\n");
        let mut recorded = Vec::new();
        while let Some(request) = first_request(&mut requests).await {
            recorded.push(request);
        }
        assert!(
            recorded.iter().any(|request| {
                request.starts_with("GET /handoff?")
                    && request.contains("agent=kiro-cli")
                    && request.contains("session_id=kiro-session")
            }),
            "{recorded:?}"
        );
    }

    #[tokio::test]
    async fn copilot_cli_session_start_prints_top_level_handoff_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "COPILOT-HANDOFF").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().join("data")),
            copilot_cli_hook_args("session-start", &base),
            serde_json::json!({
                "session_id": "copilot-session",
                "cwd": tmp.path()
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        let envelope: serde_json::Value = serde_json::from_slice(stdout.trim_ascii()).unwrap();
        assert_eq!(
            envelope,
            serde_json::json!({"additionalContext": "COPILOT-HANDOFF"})
        );
        assert!(
            envelope.get("hookSpecificOutput").is_none(),
            "Copilot reads the top-level SessionStart envelope"
        );
        let mut recorded = Vec::new();
        while let Some(request) = first_request(&mut requests).await {
            recorded.push(request);
        }
        assert!(
            recorded.iter().any(|request| {
                request.starts_with("GET /handoff?")
                    && request.contains("agent=copilot-cli")
                    && request.contains("session_id=copilot-session")
            }),
            "{recorded:?}"
        );
    }

    // ── #992: per-repository server profiles ─────────────────────────────

    /// Register `name` in `data_dir` with a token and the given roots.
    fn register_profile(
        data_dir: &Path,
        name: &str,
        url: &str,
        roots: &[&Path],
        token: Option<&str>,
    ) {
        let roots: Vec<String> = roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        crate::server_profiles::add(
            data_dir,
            &ProfileName::parse(name).unwrap(),
            url,
            &roots,
            token,
        )
        .unwrap();
    }

    fn repo_with_marker(parent: &Path, name: &str, marker: &str) -> PathBuf {
        let repo = parent.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".ai-memory.toml"), marker).unwrap();
        repo
    }

    fn claude_args(event: &str, server_url: &str) -> HookArgs {
        let mut args = devin_hook_args(event);
        args.agent = "claude-code".into();
        args.server_url = server_url.into();
        args
    }

    async fn run_prompt(data_dir: &Path, args: HookArgs, cwd: &Path) -> Vec<u8> {
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.to_path_buf()),
            args,
            serde_json::json!({"session_id": "s", "cwd": cwd, "prompt": "hello"}).to_string(),
            &mut stdout,
            |_, _| panic!("a prompt must only spool"),
        )
        .await
        .unwrap();
        stdout
    }

    /// Two repositories on one install deliver to two servers, each with its
    /// own token, and neither with the install's persisted one.
    #[tokio::test]
    async fn each_repository_spools_to_its_own_profile_with_its_own_token() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let work = tmp.path().join("work");
        let repo_a = repo_with_marker(&work, "a", "server = \"team-a\"\n");
        let repo_b = repo_with_marker(&work, "b", "workspace = \"b\"\nserver = \"team-b\"\n");
        register_profile(
            &data_dir,
            "team-a",
            "https://a.example",
            &[&repo_a],
            Some("tok-a"),
        );
        register_profile(
            &data_dir,
            "team-b",
            "https://b.example/",
            &[&repo_b],
            Some("tok-b"),
        );
        crate::config::store_hook_auth_token(&data_dir, "INSTALL-TOKEN").unwrap();

        run_prompt(
            &data_dir,
            claude_args("user-prompt-submit", "https://default.example"),
            &repo_a,
        )
        .await;
        run_prompt(
            &data_dir,
            claude_args("user-prompt-submit", "https://default.example"),
            &repo_b,
        )
        .await;

        let entries = read_spooled_entries(&hook_spool::spool_dir(&data_dir));
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].url.starts_with("https://a.example/hook?"),
            "{}",
            entries[0].url
        );
        assert_eq!(entries[0].token.as_deref(), Some("tok-a"));
        assert_eq!(entries[0].profile.as_deref(), Some("team-a"));
        assert!(
            entries[1].url.starts_with("https://b.example/hook?"),
            "{}",
            entries[1].url
        );
        assert_eq!(entries[1].token.as_deref(), Some("tok-b"));
        assert_eq!(entries[1].profile.as_deref(), Some("team-b"));
    }

    /// Backward compatibility: without a `server` key the install default is
    /// used exactly as before, and the spooled entry carries no profile.
    #[tokio::test]
    async fn a_repository_without_a_server_key_keeps_the_install_default() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let repo = repo_with_marker(tmp.path(), "repo", "workspace = \"w\"\n");
        register_profile(&data_dir, "team-a", "https://a.example", &[], Some("tok-a"));
        crate::config::store_hook_auth_token(&data_dir, "INSTALL-TOKEN").unwrap();

        run_prompt(
            &data_dir,
            claude_args("user-prompt-submit", "https://default.example"),
            &repo,
        )
        .await;

        let spool = hook_spool::spool_dir(&data_dir);
        let entries = read_spooled_entries(&spool);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].url.starts_with("https://default.example/hook?"));
        assert_eq!(entries[0].token.as_deref(), Some("INSTALL-TOKEN"));
        let raw = std::fs::read_dir(&spool)
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .next()
            .unwrap();
        assert!(!raw.contains("\"profile\""), "{raw}");
    }

    /// Every refusal emits nothing — no spool entry, no drainer — and
    /// `--check-capture` names the reason without a URL, path, or token.
    #[tokio::test]
    async fn a_selection_that_does_not_resolve_emits_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let inside = repo_with_marker(tmp.path(), "inside", "server = \"rooted\"\n");
        let outside = repo_with_marker(tmp.path(), "outside", "server = \"rooted\"\n");
        let unknown = repo_with_marker(tmp.path(), "unknown", "server = \"nobody\"\n");
        let tokenless = repo_with_marker(tmp.path(), "tokenless", "server = \"tokenless\"\n");
        let unrooted = repo_with_marker(tmp.path(), "unrooted", "server = \"unrooted\"\n");
        let bare = repo_with_marker(tmp.path(), "bare", "server = Bad/Name\n");
        register_profile(
            &data_dir,
            "rooted",
            "https://r.example",
            &[&inside],
            Some("tok-r"),
        );
        register_profile(
            &data_dir,
            "tokenless",
            "https://t.example",
            &[&tokenless],
            None,
        );
        register_profile(
            &data_dir,
            "unrooted",
            "https://u.example",
            &[],
            Some("tok-u"),
        );
        crate::config::store_hook_auth_token(&data_dir, "INSTALL-TOKEN").unwrap();

        for (cwd, resolution) in [
            (&outside, "rejected-outside-roots"),
            (&unknown, "rejected-unknown-profile"),
            (&tokenless, "rejected-no-token"),
            (&unrooted, "rejected-roots-required"),
            (&bare, "rejected-invalid-profile-name"),
            (&inside, "marker"),
        ] {
            let mut args = claude_args("user-prompt-submit", "https://default.example");
            args.check_capture = true;
            let stdout = run_prompt(&data_dir, args, cwd).await;
            let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(output["server_resolution"], resolution, "{}", cwd.display());
            assert_eq!(output["policy_admits_capture"], resolution == "marker");
            let printed = String::from_utf8(stdout).unwrap();
            for secret in ["tok-", "https://", &*tmp.path().to_string_lossy()] {
                assert!(!printed.contains(secret), "{printed}");
            }

            let spool = hook_spool::spool_dir(&data_dir);
            let before = hook_spool::spool_len(&spool);
            let mut stdout = Vec::new();
            run_with_payload(
                Some(data_dir.clone()),
                claude_args("session-end", "https://default.example"),
                serde_json::json!({"session_id": "s", "cwd": cwd}).to_string(),
                &mut stdout,
                |_, token| {
                    assert!(
                        resolution == "marker",
                        "a refused route must not spawn a drainer"
                    );
                    assert_eq!(
                        token,
                        Some("INSTALL-TOKEN"),
                        "the drain gets the install token for install-default retries, \
                         never the profile's"
                    );
                    Ok(())
                },
            )
            .await
            .unwrap();
            let spooled = hook_spool::spool_len(&spool) - before;
            assert_eq!(spooled, usize::from(resolution == "marker"), "{resolution}");
        }
    }

    /// An event whose payload has no cwd is routed by the hook process's own
    /// cwd, so a harness omitting the field cannot escape a routed tree.
    #[test]
    fn an_event_without_a_payload_cwd_routes_by_the_process_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let repo = repo_with_marker(tmp.path(), "repo", "server = \"team-b\"\n");
        register_profile(
            &data_dir,
            "team-b",
            "https://b.example",
            &[&repo],
            Some("tok-b"),
        );

        let routed = resolve_hook_route(&data_dir, None, || Some(repo.clone()));
        assert!(matches!(routed, HookRoute::Profile(ref p) if p.url == "https://b.example"));
        let refused = resolve_hook_route(&data_dir, None, || Some(repo.join("..")));
        assert!(matches!(refused, HookRoute::InstallDefault), "{refused:?}");
        let unknown = resolve_hook_route(&data_dir, None, || None);
        assert!(matches!(unknown, HookRoute::InstallDefault));
        let payload_wins =
            resolve_hook_route(&data_dir, Some(&tmp.path().to_string_lossy()), || {
                Some(repo.clone())
            });
        assert!(matches!(payload_wins, HookRoute::InstallDefault));
    }

    /// Kimi injects any UserPromptSubmit stdout into the turn verbatim, so a
    /// prompt dropped by allowlist mode must print nothing — not even `{}`.
    #[tokio::test]
    async fn an_allowlist_drop_prints_nothing_for_kimi_user_prompts() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let mut args = kimi_hook_args("user-prompt", &dead_server_url());
        args.capture_mode = Some(crate::cli::CaptureModeArg::Allowlist);
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            args,
            serde_json::json!({"session_id": "k", "cwd": tmp.path(), "prompt": "hi"}).to_string(),
            &mut stdout,
            |_, _| panic!("a dropped prompt spawns nothing"),
        )
        .await
        .unwrap();
        assert!(stdout.is_empty(), "{:?}", String::from_utf8_lossy(&stdout));
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    /// The same for a prompt dropped because its profile was refused.
    #[tokio::test]
    async fn a_refused_route_prints_nothing_for_kimi_user_prompts() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let repo = repo_with_marker(tmp.path(), "repo", "server = \"nobody\"\n");
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            kimi_hook_args("user-prompt", &dead_server_url()),
            serde_json::json!({"session_id": "k", "cwd": repo, "prompt": "hi"}).to_string(),
            &mut stdout,
            |_, _| panic!("a refused route spawns nothing"),
        )
        .await
        .unwrap();
        assert!(stdout.is_empty(), "{:?}", String::from_utf8_lossy(&stdout));
        assert_eq!(hook_spool::spool_len(&hook_spool::spool_dir(&data_dir)), 0);
    }

    /// Session start fetches the handoff from the profile's server with the
    /// profile's token; the install-default server sees nothing at all.
    #[tokio::test]
    async fn session_start_handoff_comes_from_the_profile_server_only() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (profile_base, mut profile_requests) = serve_requests("200 OK", "B-HANDOFF").await;
        let (default_base, mut default_requests) = serve_requests("200 OK", "A-HANDOFF").await;
        let repo = repo_with_marker(tmp.path(), "repo", "server = \"team-b\"\n");
        register_profile(&data_dir, "team-b", &profile_base, &[&repo], Some("tok-b"));
        crate::config::store_hook_auth_token(&data_dir, "INSTALL-TOKEN").unwrap();

        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            kiro_hook_args("session-start", &default_base),
            serde_json::json!({"session_id": "kiro-session", "cwd": repo}).to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"B-HANDOFF\n");
        let mut recorded = Vec::new();
        while let Some(request) = first_request(&mut profile_requests).await {
            recorded.push(request);
        }
        assert!(
            recorded.iter().any(|r| r.starts_with("GET /handoff?")
                && r.lines()
                    .any(|l| l.eq_ignore_ascii_case("authorization: Bearer tok-b"))),
            "{recorded:?}"
        );
        assert!(
            recorded.iter().all(|r| !r.contains("INSTALL-TOKEN")),
            "{recorded:?}"
        );
        assert_eq!(first_request(&mut default_requests).await, None);
    }

    /// Recording stub that accepts exactly one bearer. An authorised
    /// `/hook/batch` answers `{"accepted": n}`, an authorised per-event POST
    /// `202`, anything else `401`. Every request is recorded whole (head and
    /// body), so a test can see both who authenticated and what was delivered.
    async fn serve_token_gated(
        accepted_token: &'static str,
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
                // Read until the head is complete and the body has arrived:
                // a batch body can land in a second segment.
                let mut raw = Vec::new();
                let mut chunk = [0_u8; 8192];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&raw);
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let expected = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= expected {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&raw).into_owned();
                let _ = tx.send(request.clone());
                let authorized = request.lines().any(|l| {
                    l.trim()
                        .eq_ignore_ascii_case(&format!("authorization: Bearer {accepted_token}"))
                });
                let is_batch = request
                    .lines()
                    .next()
                    .is_some_and(|l| l.contains("/hook/batch"));
                let (status, body) = if !authorized {
                    ("401 Unauthorized", "{\"error\":\"bad token\"}".to_string())
                } else if is_batch {
                    let payload = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    let accepted = serde_json::from_str::<serde_json::Value>(payload)
                        .ok()
                        .and_then(|v| v.as_array().map(Vec::len))
                        .unwrap_or(0);
                    ("200 OK", format!("{{\"accepted\":{accepted}}}"))
                } else {
                    ("202 Accepted", "queued".to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), rx)
    }

    /// Everything a stub recorded, once no more requests arrive.
    async fn recorded(requests: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut all = Vec::new();
        while let Ok(Some(request)) =
            tokio::time::timeout(Duration::from_millis(250), requests.recv()).await
        {
            all.push(request);
        }
        all
    }

    /// The issue's first acceptance criterion at the wire: two repositories
    /// on one install, one routed to a profile and one not, spool into the
    /// same queue and are drained by one pass, and each server receives
    /// exactly its own event with exactly its own bearer. Server A accepts
    /// only the install token and server B only the profile's, so a
    /// misdelivered event or a swapped credential would be refused and
    /// counted as undelivered.
    #[tokio::test]
    async fn a_mixed_spool_drains_each_event_only_to_its_own_server() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let work = tmp.path().join("work");
        let (base_a, mut requests_a) = serve_token_gated("INSTALL-TOKEN").await;
        let (base_b, mut requests_b) = serve_token_gated("tok-b").await;
        let repo_a = repo_with_marker(&work, "a", "workspace = \"a\"\n");
        let repo_b = repo_with_marker(&work, "b", "workspace = \"b\"\nserver = \"team-b\"\n");
        register_profile(&data_dir, "team-b", &base_b, &[&repo_b], Some("tok-b"));
        crate::config::store_hook_auth_token(&data_dir, "INSTALL-TOKEN").unwrap();

        for (repo, sentinel) in [(&repo_a, "SENTINEL-A"), (&repo_b, "SENTINEL-B")] {
            let mut stdout = Vec::new();
            run_with_payload(
                Some(data_dir.clone()),
                claude_args("user-prompt-submit", &base_a),
                serde_json::json!({"session_id": "s", "cwd": repo, "prompt": sentinel}).to_string(),
                &mut stdout,
                |_, _| panic!("a prompt only spools"),
            )
            .await
            .unwrap();
        }
        let spool = hook_spool::spool_dir(&data_dir);
        assert_eq!(hook_spool::spool_len(&spool), 2);

        let result = hook_spool::drain_with_live_token(
            &spool,
            &data_dir,
            Duration::from_secs(5),
            Duration::from_millis(500),
            Some("INSTALL-TOKEN"),
            hook_spool::MaxAttempts::new(crate::config::DEFAULT_HOOK_SPOOL_MAX_ATTEMPTS),
        )
        .await;
        assert_eq!(result.sent, 2, "{result:?}");
        assert_eq!(hook_spool::spool_len(&spool), 0, "nothing left queued");

        let got_a = recorded(&mut requests_a).await;
        let got_b = recorded(&mut requests_b).await;
        assert_eq!(got_a.len(), 1, "server A saw {got_a:?}");
        assert_eq!(got_b.len(), 1, "server B saw {got_b:?}");
        let (a, b) = (&got_a[0], &got_b[0]);
        assert!(a.contains("SENTINEL-A") && !a.contains("SENTINEL-B"), "{a}");
        assert!(b.contains("SENTINEL-B") && !b.contains("SENTINEL-A"), "{b}");
        assert!(
            a.lines()
                .any(|l| l.eq_ignore_ascii_case("authorization: Bearer INSTALL-TOKEN"))
                && !a.contains("tok-b"),
            "{a}"
        );
        assert!(
            b.lines()
                .any(|l| l.eq_ignore_ascii_case("authorization: Bearer tok-b"))
                && !b.contains("INSTALL-TOKEN"),
            "{b}"
        );
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
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
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        let request = first_request(&mut requests).await.unwrap();
        assert!(request.starts_with("GET /handoff?"), "{request}");
        assert!(!request.contains("briefing"), "{request}");
        // Only the profile digest's own gate is written; no brief marker.
        let markers: Vec<String> = std::fs::read_dir(data_dir.join("briefed"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(markers, vec!["profile-session_abc".to_owned()]);
    }

    /// Kimi re-fetches on every prompt, so the profile digest rides the first
    /// prompt only: later prompts of the session send `profile_digest=0`.
    #[tokio::test]
    async fn kimi_delivers_the_profile_digest_on_the_first_prompt_only() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        let (base, mut requests) = serve_requests("404 Not Found", "").await;
        let prompt = || {
            serde_json::json!({ "session_id": "session_k", "cwd": cwd, "prompt": "hi" }).to_string()
        };
        for _ in 0..2 {
            let mut stdout = Vec::new();
            run_with_payload(
                Some(data_dir.clone()),
                kimi_hook_args("user-prompt-submit", &base),
                prompt(),
                &mut stdout,
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        }
        let first = first_request(&mut requests).await.unwrap();
        let second = first_request(&mut requests).await.unwrap();
        assert!(!first.contains("profile_digest"), "{first}");
        assert!(second.contains("&profile_digest=0"), "{second}");
    }

    #[tokio::test]
    async fn grok_session_start_never_fetches_the_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            grok_hook_args("session-start", &base),
            serde_json::json!({"session_id": "grok-session", "cwd": tmp.path()}).to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        while let Some(request) = first_request(&mut requests).await {
            assert!(!request.starts_with("GET /handoff"), "{request}");
        }
    }

    #[tokio::test]
    async fn grok_user_prompt_does_not_fetch_the_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().to_path_buf()),
            grok_hook_args("user-prompt", &base),
            serde_json::json!({
                "session_id": "grok-session",
                "cwd": tmp.path(),
                "prompt": "hello"
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        // Grok discards allowing UserPromptSubmit stdout. Fetching would
        // accept the handoff and then throw the body away.
        assert_eq!(stdout, b"{}\n");
        while let Some(request) = first_request(&mut requests).await {
            assert!(
                !request.starts_with("GET /handoff"),
                "grok user-prompt must not accept the handoff: {request}"
            );
        }
    }

    #[tokio::test]
    async fn grok_user_prompt_submit_stem_does_not_fetch_the_handoff() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        write_briefing_marker(&cwd);
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir),
            grok_hook_args("user-prompt-submit", &base),
            serde_json::json!({
                "session_id": "grok-session",
                "cwd": cwd,
                "prompt": "hi"
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(stdout, b"{}\n");
        while let Some(request) = first_request(&mut requests).await {
            assert!(
                !request.starts_with("GET /handoff"),
                "briefing opt-in must not make grok fetch /handoff: {request}"
            );
        }
    }

    #[tokio::test]
    async fn grok_post_tool_prints_additional_context_once() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let payload = serde_json::json!({
            "session_id": "grok-session",
            "cwd": tmp.path(),
            "tool_name": "read_file"
        })
        .to_string();

        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir.clone()),
            grok_hook_args("post-tool-use", &base),
            payload.clone(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(stdout.trim_ascii()).unwrap();
        assert_eq!(
            envelope["hookSpecificOutput"]["hookEventName"],
            "PostToolUse"
        );
        assert_eq!(
            envelope["hookSpecificOutput"]["additionalContext"],
            "AMWS-HANDOFF-DELTA"
        );
        let first = first_request(&mut requests).await.unwrap();
        assert!(first.starts_with("GET /handoff?"), "{first}");
        assert!(first.contains("agent=grok"), "{first}");
        assert!(first.contains("session_id=grok-session"), "{first}");
        assert!(
            data_dir
                .join("handoff-shown")
                .join("grok-session")
                .is_file(),
            "shown marker missing"
        );

        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir),
            grok_hook_args("post-tool-use", &base),
            payload,
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"{}\n");
        assert!(
            first_request(&mut requests).await.is_none(),
            "second post-tool must not fetch again"
        );
    }

    #[tokio::test]
    async fn grok_post_tool_includes_briefing_when_the_marker_opts_in() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir(&cwd).unwrap();
        write_briefing_marker(&cwd);
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(data_dir),
            grok_hook_args("post-tool-use", &base),
            serde_json::json!({
                "session_id": "grok-session",
                "cwd": cwd,
                "tool_name": "read_file"
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        let request = first_request(&mut requests).await.unwrap();
        assert!(request.contains("&briefing=true"), "{request}");
        assert!(request.contains("&briefing_budget=6000"), "{request}");
        let envelope: serde_json::Value = serde_json::from_slice(stdout.trim_ascii()).unwrap();
        assert_eq!(
            envelope["hookSpecificOutput"]["additionalContext"],
            "AMWS-HANDOFF-DELTA"
        );
    }

    #[tokio::test]
    async fn grok_post_tool_skips_a_subagent_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("200 OK", "AMWS-HANDOFF-DELTA").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().join("data")),
            grok_hook_args("post-tool-use", &base),
            serde_json::json!({
                "session_id": "child-session",
                "cwd": tmp.path(),
                "subagentType": "goal-plan-writer",
                "tool_name": "read_file"
            })
            .to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"{}\n");
        while let Some(request) = first_request(&mut requests).await {
            assert!(
                !request.starts_with("GET /handoff"),
                "a child session must not accept the parent handoff: {request}"
            );
        }
    }

    #[tokio::test]
    async fn grok_post_tool_without_handoff_prints_empty_object() {
        let tmp = tempfile::tempdir().unwrap();
        let (base, mut requests) = serve_requests("404 Not Found", "").await;
        let mut stdout = Vec::new();
        run_with_payload(
            Some(tmp.path().join("data")),
            grok_hook_args("post-tool-use", &base),
            serde_json::json!({"session_id": "grok-session", "cwd": tmp.path()}).to_string(),
            &mut stdout,
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        assert_eq!(stdout, b"{}\n");
        let request = first_request(&mut requests).await.unwrap();
        assert!(request.starts_with("GET /handoff?"), "{request}");
    }

    #[test]
    fn grok_additional_context_is_clipped_to_the_model_cap() {
        let long = "x".repeat(GROK_ADDITIONAL_CONTEXT_CHARS + 50);
        let envelope = grok_post_tool_handoff_envelope(&long);
        let note = envelope["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(note.ends_with("\n[truncated]"));
        assert!(note.chars().count() <= GROK_ADDITIONAL_CONTEXT_CHARS);
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
