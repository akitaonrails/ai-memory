//! axum router exposing `POST /hook`.
//!
//! Returns 202 immediately unless the in-flight hook limit is saturated,
//! in which case it returns 429. Heavy work (DB writes, session-page
//! synthesis) happens *after* the response is sent — but we still `await`
//! the writer ack to honour the cross-cutting invariant that "indexes commit
//! in the same transaction as the data" (no background-task-indexing-after-return,
//! basic-memory #763). The agent never blocks on us thanks to the
//! fire-and-forget client side.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use ai_memory_consolidate::Consolidator;
use ai_memory_core::{
    ActiveProject, AgentKind, DEFAULT_WORKSPACE_NAME, Handoff, NewHandoff, NewObservation,
    NewSession, ObservationKind, ProjectId, Sanitized, Sanitizer, SessionId, WorkspaceId,
};
use ai_memory_store::WriterHandle;
use ai_memory_wiki::Wiki;
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use jiff::Timestamp;
use serde::Deserialize;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::log;
use crate::payload::{HookEnvelope, HookEvent, HookQuery, ProjectStrategy, parse_agent};
use crate::synth::synthesize_session_page;

/// Default maximum number of hook events allowed to be processing at once.
///
/// This matches the writer queue order of magnitude and prevents unbounded
/// background tasks during tool-heavy bursts. Saturated servers return 429 so
/// callers can drop or retry instead of growing memory without bound.
pub const DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT: usize = 1024;

/// Resolved-project cache: `(cwd, workspace_override, project_override, project_strategy)`
/// keyed map, shared behind a Tokio mutex so the router can be cloned
/// freely without losing the in-flight cache hits.
pub type ProjectCache =
    Arc<tokio::sync::Mutex<HashMap<(String, String, String, String), (WorkspaceId, ProjectId)>>>;

/// Shared state passed to the hook handler.
#[derive(Clone)]
pub struct HookState {
    /// Default workspace to use when a hook event lacks a `cwd` field.
    pub workspace_id: WorkspaceId,
    /// Default project to use when a hook event lacks a `cwd` field.
    pub project_id: ProjectId,
    /// Writer actor handle.
    pub writer: WriterHandle,
    /// Reader pool — needed for session-end synthesis.
    pub reader: ai_memory_store::ReaderPool,
    /// Wiki handle — used to write the session-summary page.
    pub wiki: Wiki,
    /// Optional LLM-driven consolidator. When set, PreCompact uses it
    /// to refresh `sessions/<id>.md` before the agent loses its
    /// working context. When `None`, falls back to the deterministic
    /// rule-based synth (still useful, just lower-signal).
    pub consolidator: Option<Arc<Consolidator>>,
    /// Privacy strip applied to every observation before it lands in
    /// the store. Same handle is also held by the wiki and consolidator
    /// so scrubbing happens at every write boundary.
    pub sanitizer: Sanitizer,
    /// Cache of `(cwd, workspace_override, project_override, project_strategy) → ids`.
    /// The composite key avoids poisoning between callers that resolve
    /// the same `cwd` with and without an override during a hook-script
    /// upgrade window. Each tuple element defaults to the empty string
    /// when absent so missing overrides collapse into a single slot.
    pub project_cache: ProjectCache,
    /// Pointer shared with the MCP server. Every cwd-resolved event
    /// publishes its project here so the read tools (which have no cwd
    /// of their own) default to the project the user is actually in
    /// rather than the server's static `--project` (issue #2).
    pub active_project: ActiveProject,
    /// In-flight hook processing limiter. Requests acquire one permit before
    /// spawning work and return 429 immediately when saturated.
    pub ingest_semaphore: Arc<tokio::sync::Semaphore>,
    /// Opt-in (`AI_MEMORY_CONSOLIDATE_ON_SESSION_END`): when true and a
    /// `consolidator` is present, SessionEnd also runs LLM consolidation on
    /// top of the always-written heuristic session page. Off by default so
    /// session close stays cheap; the LLM checkpoint otherwise happens on
    /// PreCompact and via manual `memory_consolidate`.
    pub consolidate_on_session_end: bool,
}

/// Build a router with `POST /hook` (event ingress) and `GET /handoff`
/// (synchronous handoff-fetch for session-start hooks).
pub fn hook_router(state: HookState) -> Router {
    Router::new()
        .route("/hook", post(handle_hook))
        .route("/handoff", get(handle_handoff))
        .with_state(Arc::new(state))
}

async fn handle_hook(
    State(state): State<Arc<HookState>>,
    Query(query): Query<HookQuery>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let env = HookEnvelope::from_query_and_body(query, body);
    let Ok(permit) = state.ingest_semaphore.clone().try_acquire_owned() else {
        warn!("hook ingest saturated; dropping event with 429");
        return (StatusCode::TOO_MANY_REQUESTS, "hook queue full");
    };
    tokio::spawn(async move {
        let _permit = permit;
        process_envelope(state, env).await;
    });
    (StatusCode::ACCEPTED, "queued")
}

/// Query params for `GET /handoff`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HandoffQuery {
    /// Identifier of the agent fetching the handoff. Used to mark the
    /// handoff as accepted-by; defaults to `Other` if unrecognised.
    pub agent: Option<String>,
    /// Optional cwd filter. When provided, only handoffs whose stored
    /// cwd matches this string are returned. Note: the cwd string is
    /// not canonicalized; symlinked paths must match byte-for-byte.
    pub cwd: Option<String>,
    /// Workspace override (mirror of `HookQuery.workspace`). Lets the
    /// `session-start` hook fetch the handoff for the same `(workspace,
    /// project)` pair the marker file declared, without depending on
    /// the MCP `active_project` cache (which only populates after the
    /// first hook event of the session).
    pub workspace: Option<String>,
    /// Project override (mirror of `HookQuery.project`).
    pub project: Option<String>,
    /// Project strategy (mirror of `HookQuery.project_strategy`).
    pub project_strategy: Option<String>,
}

/// Synchronous endpoint used by `session-start.sh` to discover any
/// pending handoff from a previous agent. Returns plain text Markdown
/// (or an empty body when no handoff is open) with a 1-second cap on
/// the server side so the agent never blocks measurably on startup.
///
/// Side effect: when a handoff is found, it is *marked accepted* before
/// the response is sent. Two agents starting in parallel therefore
/// race; whichever arrives first wins. That is intentional — handoffs
/// are 1:1, not broadcast.
async fn handle_handoff(
    State(state): State<Arc<HookState>>,
    Query(query): Query<HandoffQuery>,
) -> impl IntoResponse {
    match fetch_and_accept_handoff(&state, query).await {
        Ok(Some(markdown)) => (StatusCode::OK, markdown),
        Ok(None) => (StatusCode::OK, String::new()),
        Err(e) => {
            warn!(error = %e, "handoff fetch failed");
            (StatusCode::OK, String::new())
        }
    }
}

async fn fetch_and_accept_handoff(
    state: &HookState,
    query: HandoffQuery,
) -> anyhow::Result<Option<String>> {
    let agent = query.agent.as_deref().map_or(AgentKind::Other, parse_agent);
    let (ws, proj) = resolve_project_ids(
        state,
        query.cwd.as_deref(),
        query.workspace.as_deref(),
        query.project.as_deref(),
        ProjectStrategy::parse(query.project_strategy.as_deref()),
    )
    .await?;
    let handoff = state
        .reader
        .latest_open_handoff(ws, proj, query.cwd)
        .await?;
    let Some(h) = handoff else {
        return Ok(None);
    };
    state.writer.accept_handoff(h.id, agent, None).await?;
    Ok(Some(render_handoff_markdown(&h)))
}

fn render_handoff_markdown(h: &Handoff) -> String {
    // Layout goal: TUI-renderable + agent-friendly. The previous
    // shape put a paragraph-long `## Summary` first, which made the
    // hook output look like a wall of text in Codex's "completed"
    // block AND let the agent miss that this *is* the answer to
    // "where did we leave off" questions. The new layout leads
    // with the actionable bullets (open questions, next steps) and
    // pushes the prose summary to the bottom; the agent-facing
    // footer explicitly tells the model how to interpret a follow-up
    // memory_handoff_accept = null.
    let mut buf = String::with_capacity(512);
    buf.push_str("> 📥 **ai-memory: pending handoff from previous session**\n");
    buf.push_str(&format!(
        "> from `{from}` · created {ts}\n",
        from = h.from_agent.as_str(),
        ts = h.created_at,
    ));

    if !h.open_questions.is_empty() {
        buf.push_str("\n**Open questions**\n");
        for q in &h.open_questions {
            buf.push_str(&format!("- {q}\n"));
        }
    }
    if !h.next_steps.is_empty() {
        buf.push_str("\n**Next steps**\n");
        for s in &h.next_steps {
            buf.push_str(&format!("- {s}\n"));
        }
    }
    if !h.files_touched.is_empty() {
        buf.push_str("\n**Files touched**\n");
        for f in &h.files_touched {
            buf.push_str(&format!("- `{f}`\n"));
        }
    }

    // Summary last, as reference prose. Models reading top-down
    // see the action items first; the summary is detail.
    buf.push_str("\n**Summary**\n");
    buf.push_str(h.summary.trim());
    buf.push('\n');

    // Agent-facing reading instructions. This block is the
    // load-bearing UX fix — without it, agents call
    // memory_handoff_accept again, get `null` (single-use
    // already consumed by this hook), and conclude "no handoff"
    // *despite this content being right in their context*.
    buf.push_str(
        "\n---\n\
         _**To the receiving agent:** this content IS the pending \
         handoff — already consumed by the SessionStart hook. A \
         subsequent `memory_handoff_accept` call will return \
         `{ \"handoff\": null }` (single-use). When the user asks \
         \"where did we leave off?\" or \"any pending handoff?\", \
         answer from THIS content; do NOT re-call the tool. Call \
         `memory_query` / `memory_recent` only for additional \
         context beyond what's listed here._\n",
    );
    buf
}

/// Build the `project_cache` key from the resolved cwd, overrides, and
/// project strategy. Shared by `resolve_project_ids` (insert/lookup) and
/// `process` (eviction on the stale-cache retry) so the two always agree on
/// the slot.
fn cache_key_for(
    cwd_norm: Option<&str>,
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_strategy: ProjectStrategy,
) -> (String, String, String, String) {
    (
        cwd_norm.unwrap_or_default().to_string(),
        workspace_override.unwrap_or_default().to_string(),
        project_override.unwrap_or_default().to_string(),
        project_strategy.as_str().to_string(),
    )
}

/// Resolve the `(workspace_id, project_id)` pair for a hook event.
///
/// Precedence:
/// 1. `workspace_override` (typically declared by the agent's host-side
///    hook via a `.ai-memory.toml` walk-up) OR `DEFAULT_WORKSPACE_NAME`.
/// 2. `project_override` OR marker-selected project strategy OR
///    `basename(cwd)` OR fallback to `state.project_id` (when `cwd` is
///    also unavailable).
///
/// Cache key is `(cwd, workspace_override, project_override,
/// project_strategy)` so the same `cwd` resolved with and without an
/// override (e.g. during a hook-script upgrade window) doesn't poison each
/// other's slot.
async fn resolve_project_ids(
    state: &HookState,
    cwd: Option<&str>,
    workspace_override: Option<&str>,
    project_override: Option<&str>,
    project_strategy: ProjectStrategy,
) -> anyhow::Result<(WorkspaceId, ProjectId)> {
    let cwd_norm = cwd.filter(|s| !s.is_empty()).map(str::to_string);

    // Without cwd AND without a project override, there's nothing to
    // resolve — fall through to the server defaults.
    if cwd_norm.is_none() && project_override.is_none() {
        return Ok((state.workspace_id, state.project_id));
    }

    let cache_key = cache_key_for(
        cwd_norm.as_deref(),
        workspace_override,
        project_override,
        project_strategy,
    );

    {
        let cache = state.project_cache.lock().await;
        if let Some(ids) = cache.get(&cache_key) {
            // Republish on every hit: a cache hit still means the agent
            // is active in this project *now*, which is exactly what the
            // MCP read tools need as their default.
            state.active_project.set(ids.0, ids.1);
            return Ok(*ids);
        }
    }

    let workspace_name = workspace_override
        .unwrap_or(DEFAULT_WORKSPACE_NAME)
        .to_string();

    let (project_name, repo_path) = match (project_override, cwd_norm.as_deref()) {
        (Some(p), _) => (p.to_string(), cwd_norm.clone()),
        (None, Some(c)) => match derive_project_from_cwd(c, project_strategy) {
            Some(resolved) => resolved,
            None => {
                state
                    .active_project
                    .set(state.workspace_id, state.project_id);
                return Ok((state.workspace_id, state.project_id));
            }
        },
        (None, None) => {
            // The early-return at the top of the function guards
            // against this branch; the explicit fallback here keeps
            // the resolver panic-free if that guard ever moves or
            // gets refactored. Same effect as `unreachable!`, but
            // visible at compile time instead of inside the panic
            // message.
            state
                .active_project
                .set(state.workspace_id, state.project_id);
            return Ok((state.workspace_id, state.project_id));
        }
    };

    fn derive_project_from_cwd(
        cwd: &str,
        strategy: ProjectStrategy,
    ) -> Option<(String, Option<String>)> {
        let path = std::path::Path::new(cwd);
        if matches!(strategy, ProjectStrategy::RepoRoot)
            && let Ok(root) = ai_memory_consolidate::discover_main_repo_root(path)
            && let Some(name) = basename(&root)
        {
            return Some((name, Some(root.to_string_lossy().into_owned())));
        }
        basename(path).map(|name| (name, Some(cwd.to_string())))
    }

    fn basename(path: &std::path::Path) -> Option<String> {
        // Split on both `/` and `\` so Windows paths sent to a Linux
        // server still resolve to the final component.
        let s = path.to_str()?;
        let name = s.rsplit(['/', '\\']).find(|seg| !seg.is_empty())?;
        Some(name.to_string())
    }

    let ws = state
        .writer
        .get_or_create_workspace(workspace_name)
        .await
        .map_err(|e| anyhow::anyhow!("get_or_create_workspace: {e}"))?;
    let proj = state
        .writer
        .get_or_create_project(ws, project_name, repo_path)
        .await
        .map_err(|e| anyhow::anyhow!("get_or_create_project: {e}"))?;
    let ids = (ws, proj);
    state.project_cache.lock().await.insert(cache_key, ids);
    state.active_project.set(ws, proj);
    Ok(ids)
}

async fn process_envelope(state: Arc<HookState>, env: HookEnvelope) {
    if let Err(e) = process(&state, env).await {
        warn!(error = %e, "hook processing failed");
    }
}

async fn process(state: &HookState, env: HookEnvelope) -> anyhow::Result<()> {
    let session_id = resolve_session_id(&env)?;
    let (mut ws, mut proj) = resolve_project_ids(
        state,
        env.cwd.as_deref(),
        env.workspace_override.as_deref(),
        env.project_override.as_deref(),
        env.project_strategy,
    )
    .await?;

    // Hooks are fire-and-forget and may arrive out of order. Begin the
    // session idempotently before every observation so a resumed agent
    // session, or a prompt racing ahead of SessionStart, cannot trip the
    // observations.session_id foreign key.
    let new_session = NewSession {
        id: session_id,
        workspace_id: ws,
        project_id: proj,
        agent_kind: env.agent,
        cwd: env.cwd.as_ref().map(std::path::PathBuf::from),
    };
    if let Err(e) = state.writer.begin_session(new_session).await {
        // The cached (workspace, project) may have been deleted out from
        // under us — e.g. `purge-project` on a live server drops the row
        // but leaves this in-memory cache pointing at the old id, so
        // begin_session trips the project foreign key. Evict the stale
        // slot, re-resolve (which recreates the project), and retry once.
        warn!(error = %e, "begin_session failed; evicting stale project cache and retrying");
        let cwd_norm = env.cwd.as_deref().filter(|s| !s.is_empty());
        let key = cache_key_for(
            cwd_norm,
            env.workspace_override.as_deref(),
            env.project_override.as_deref(),
            env.project_strategy,
        );
        state.project_cache.lock().await.remove(&key);
        let refreshed = resolve_project_ids(
            state,
            env.cwd.as_deref(),
            env.workspace_override.as_deref(),
            env.project_override.as_deref(),
            env.project_strategy,
        )
        .await?;
        ws = refreshed.0;
        proj = refreshed.1;
        state
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: env.agent,
                cwd: env.cwd.as_ref().map(std::path::PathBuf::from),
            })
            .await?;
    }

    // Persist the observation row.
    let kind = env.event.to_observation_kind();
    let title = env
        .title_hint
        .clone()
        .unwrap_or_else(|| kind.as_str().to_string());
    let body = env.body_excerpt.clone().unwrap_or_default();
    let raw_obs = NewObservation {
        session_id,
        workspace_id: ws,
        project_id: proj,
        kind,
        extension: env.extension.clone(),
        source_event: env.source_event.clone(),
        title,
        body,
        importance: importance_for(env.event),
    };
    let sanitized = Sanitized::new(raw_obs, &state.sanitizer);
    let _ = state
        .writer
        .insert_observation(sanitized.inner().clone())
        .await?;

    // Append the log line to the per-project log.md.
    if let Err(e) = log::append_event(
        &state.wiki,
        ws,
        proj,
        Timestamp::now(),
        env.event,
        sanitized.inner().title.as_str(),
    ) {
        warn!(error = %e, "log.md append failed");
    }

    // On PreCompact, refresh `sessions/<id>.md` so the wiki captures
    // the working state before the agent's compaction throws it out
    // of context. Does NOT end the session and does NOT create a
    // handoff. The eventual SessionEnd supersedes this page.
    if matches!(env.event, HookEvent::PreCompact)
        && let Err(e) = consolidate_or_synth(state, session_id, ws, proj).await
    {
        warn!(error = %e, "PreCompact consolidation failed; continuing");
    }

    // On SessionEnd, synthesize the summary page, end the session, and
    // auto-create a handoff so the next agent can pick up.
    if matches!(env.event, HookEvent::SessionEnd) {
        let observations = state.reader.observations_for_session(session_id).await?;
        let new_page = synthesize_session_page(ws, proj, session_id, &observations);
        let page_id = state
            .wiki
            .write_page(ai_memory_wiki::WritePageRequest {
                workspace_id: new_page.workspace_id,
                project_id: new_page.project_id,
                path: new_page.path.clone(),
                frontmatter: new_page.frontmatter_json.clone(),
                body: new_page.body.clone(),
                tier: new_page.tier,
                pinned: new_page.pinned,
                title: None,
            })
            .await?;
        state.writer.end_session(session_id, Some(page_id)).await?;
        let handoff = build_auto_handoff(
            ws,
            proj,
            env.agent,
            session_id,
            env.cwd.clone(),
            &observations,
        );
        let handoff_id = state.writer.insert_handoff(handoff).await?;
        // Opt-in (AI_MEMORY_CONSOLIDATE_ON_SESSION_END): additionally run LLM
        // consolidation so the session's knowledge is compiled into topical
        // pages, not just the heuristic session record. The heuristic page
        // above is always written first, so an LLM failure here is non-fatal —
        // warn and keep the deterministic result. Runs before the commit so
        // the consolidated pages land in the same atomic snapshot.
        if state.consolidate_on_session_end
            && let Some(c) = state.consolidator.as_ref()
        {
            match c.consolidate_session(session_id, false).await {
                Ok(outcome) => info!(
                    session = %session_id,
                    path = %outcome.path,
                    "SessionEnd: LLM consolidation written (opt-in)",
                ),
                Err(e) => warn!(
                    error = %e,
                    "SessionEnd LLM consolidation failed; heuristic page already written",
                ),
            }
        }
        // Auto-commit the wiki tree so the session/handoff/log.md
        // changes land in git in one atomic snapshot.
        let commit_msg = format!(
            "session {}: {}",
            short_id(&session_id.to_string()),
            new_page.title.chars().take(60).collect::<String>(),
        );
        match state.wiki.commit_all(&commit_msg) {
            Ok(Some(oid)) => debug!(commit = %oid, "wiki auto-commit"),
            Ok(None) => debug!("wiki clean; no auto-commit"),
            Err(e) => warn!(error = %e, "auto-commit failed"),
        }
        info!(
            session = %session_id,
            page = %new_page.path,
            handoff = %handoff_id,
            "session ended; summary page + open handoff created",
        );
    }

    Ok(())
}

fn resolve_session_id(env: &HookEnvelope) -> anyhow::Result<SessionId> {
    if let Some(raw) = &env.session_id {
        // Accept either a UUID (canonical) or any string, hashing the
        // latter to a deterministic UUID v5 so each agent's session id
        // maps cleanly into our schema.
        if let Ok(id) = SessionId::from_str(raw) {
            return Ok(id);
        }
        let uuid = Uuid::new_v5(&Uuid::NAMESPACE_OID, raw.as_bytes());
        return Ok(SessionId(uuid));
    }
    if matches!(env.event, HookEvent::SessionStart) {
        return Ok(SessionId::new());
    }
    anyhow::bail!("hook payload missing session_id and event is not session-start")
}

fn build_auto_handoff(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    from_agent: AgentKind,
    session_id: SessionId,
    cwd: Option<String>,
    observations: &[ai_memory_core::Observation],
) -> NewHandoff {
    // Prefer obs.body (the full prompt) over obs.title (first-line +
    // truncated to 80 chars for log/list display). When body is
    // empty fall back to title so we never produce an empty entry.
    fn pick_text(obs: &ai_memory_core::Observation) -> &str {
        if !obs.body.is_empty() {
            obs.body.as_str()
        } else {
            obs.title.as_str()
        }
    }
    /// Cap so a single 10-page prompt doesn't blow up the handoff.
    /// The body is already scrubbed at insert time; this is just a
    /// length budget. 1500 chars ≈ 250 words ≈ a paragraph.
    fn cap(s: &str) -> String {
        const MAX: usize = 1500;
        if s.chars().count() <= MAX {
            s.to_string()
        } else {
            let truncated: String = s.chars().take(MAX).collect();
            format!("{truncated}…")
        }
    }
    let mut prompts: Vec<String> = Vec::new();
    let mut tools: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for obs in observations {
        match obs.kind {
            ObservationKind::UserPrompt => {
                let text = pick_text(obs);
                if !text.is_empty() {
                    prompts.push(text.to_string());
                }
            }
            ObservationKind::PostToolUse | ObservationKind::PreToolUse if !obs.title.is_empty() => {
                tools.insert(obs.title.as_str());
            }
            _ => {}
        }
    }
    let first_prompt = prompts.first().cloned();
    let last_prompt = prompts.last().cloned();
    let summary = match (&first_prompt, &last_prompt) {
        (Some(first), Some(last)) if first == last => format!("Session focused on: {}", cap(first)),
        (Some(first), Some(last)) => format!("Started: {}\n\nLast: {}", cap(first), cap(last),),
        (Some(first), None) => format!("Started: {}", cap(first)),
        _ => format!(
            "Session ended; {} observations recorded.",
            observations.len()
        ),
    };
    let open_questions = if let Some(last) = last_prompt {
        // Heuristic: last user prompt often *is* the open question.
        vec![format!("Continue from: {}", cap(&last))]
    } else {
        Vec::new()
    };
    let next_steps = if tools.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Tools used: {}",
            tools.into_iter().collect::<Vec<_>>().join(", ")
        )]
    };
    NewHandoff {
        workspace_id,
        project_id,
        from_session_id: Some(session_id),
        from_agent,
        to_agent: None,
        cwd: cwd.map(std::path::PathBuf::from),
        summary,
        open_questions,
        next_steps,
        files_touched: Vec::new(),
    }
}

/// Write a fresh `sessions/<id>.md` for the current session without
/// ending it. Used by the PreCompact branch to checkpoint state before
/// the agent's working context collapses.
async fn consolidate_or_synth(
    state: &HookState,
    session_id: SessionId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> anyhow::Result<()> {
    if let Some(c) = state.consolidator.as_ref() {
        let outcome = c.consolidate_session(session_id, false).await?;
        debug!(
            session = %session_id,
            path = %outcome.path,
            "PreCompact: LLM consolidation written",
        );
        let _ = state.wiki.commit_all(&format!(
            "pre-compact(session {}): checkpoint",
            short_id(&session_id.to_string()),
        ));
        return Ok(());
    }
    let observations = state.reader.observations_for_session(session_id).await?;
    if observations.is_empty() {
        return Ok(());
    }
    let new_page = synthesize_session_page(workspace_id, project_id, session_id, &observations);
    state
        .wiki
        .write_page(ai_memory_wiki::WritePageRequest {
            workspace_id: new_page.workspace_id,
            project_id: new_page.project_id,
            path: new_page.path,
            frontmatter: new_page.frontmatter_json,
            body: new_page.body,
            tier: new_page.tier,
            pinned: new_page.pinned,
            title: None,
        })
        .await?;
    let _ = state.wiki.commit_all(&format!(
        "pre-compact(session {}): checkpoint",
        short_id(&session_id.to_string()),
    ));
    debug!(session = %session_id, "PreCompact: rule-based checkpoint written");
    Ok(())
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

const fn importance_for(event: HookEvent) -> u8 {
    match event {
        HookEvent::SessionStart | HookEvent::SessionEnd => 7,
        HookEvent::UserPrompt => 8,
        HookEvent::PostToolUse | HookEvent::PreToolUse => 5,
        HookEvent::Stop | HookEvent::PreCompact => 6,
        HookEvent::Notification | HookEvent::Other => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use ai_memory_core::Sanitizer;
    use ai_memory_store::Store;
    use ai_memory_wiki::Wiki;
    use tempfile::TempDir;

    use super::*;
    use crate::payload::HookQuery;

    /// Build a minimal `HookState` backed by a real on-disk store.
    async fn make_state(tmp: &TempDir) -> HookState {
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let sanitizer = Sanitizer::default();
        HookState {
            workspace_id: ws,
            project_id: proj,
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            consolidator: None,
            sanitizer,
            project_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            active_project: ActiveProject::new(),
            consolidate_on_session_end: false,
            ingest_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
            )),
        }
    }

    fn init_repo_with_commit(path: &std::path::Path) -> git2::Repository {
        std::fs::create_dir_all(path).unwrap();
        let repo = git2::Repository::init(path).unwrap();
        let sig = repo
            .signature()
            .unwrap_or_else(|_| git2::Signature::now("test", "test@test.com").unwrap());
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap();
        }
        repo
    }

    /// Two hook events with distinct cwds must land in two distinct projects.
    #[tokio::test]
    async fn process_with_cwd_creates_new_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // Event from /home/user/project-alpha.
        let (ws_a, proj_a) = resolve_project_ids(
            &state,
            Some("/home/user/project-alpha"),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        // Event from /home/user/project-beta.
        let (ws_b, proj_b) = resolve_project_ids(
            &state,
            Some("/home/user/project-beta"),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        // Projects must be distinct; workspace is the same (`default`).
        assert_ne!(proj_a, proj_b, "different cwds → different projects");
        assert_eq!(ws_a, ws_b, "same default workspace");

        // Neither should match the server-default scratch project.
        assert_ne!(proj_a, state.project_id);
        assert_ne!(proj_b, state.project_id);

        // The MCP-shared pointer reflects the most recently resolved
        // project (issue #2) — here, project-beta.
        assert_eq!(state.active_project.get(), Some((ws_b, proj_b)));
    }

    #[tokio::test]
    async fn handle_hook_returns_429_when_ingest_saturated() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(0));

        let response = handle_hook(
            State(Arc::new(state)),
            Query(HookQuery {
                event: "session-start".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            }),
            Json(serde_json::json!({})),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// An event without a cwd must fall back to the server defaults.
    #[tokio::test]
    async fn process_with_missing_cwd_falls_back_to_state_defaults() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws, proj) = resolve_project_ids(&state, None, None, None, ProjectStrategy::Basename)
            .await
            .unwrap();
        assert_eq!(ws, state.workspace_id);
        assert_eq!(proj, state.project_id);

        // Likewise for an empty string.
        let (ws2, proj2) =
            resolve_project_ids(&state, Some(""), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        assert_eq!(ws2, state.workspace_id);
        assert_eq!(proj2, state.project_id);

        // A cwd-less event must NOT publish the scratch fallback as the
        // active project — that would re-introduce the issue #2 bug of
        // MCP reads defaulting to an empty scratch bucket.
        assert!(state.active_project.get().is_none());
    }

    #[tokio::test]
    async fn process_with_root_cwd_falls_back_to_state_defaults() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws, proj) =
            resolve_project_ids(&state, Some("/"), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        assert_eq!(ws, state.workspace_id);
        assert_eq!(proj, state.project_id);
        assert_eq!(state.active_project.get(), Some((ws, proj)));
    }

    #[test]
    fn resolve_session_id_hashes_agent_ids_deterministically() {
        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "post-tool-use".into(),
                agent: Some("opencode".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "opencode-session-123" }),
        );

        let first = resolve_session_id(&env).unwrap();
        let second = resolve_session_id(&env).unwrap();
        assert_eq!(first, second);
    }

    /// A second call for the same cwd must hit the in-memory cache — no
    /// additional `get_or_create_project` writes happen, proven by
    /// inspecting the cache after both calls.
    #[tokio::test]
    async fn project_cache_hits_on_second_event() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let cwd = "/home/user/cached-project";

        // First call — populates the cache.
        let (_, proj_first) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();

        // Inspect the cache: should have exactly one entry.
        {
            let cache = state.project_cache.lock().await;
            assert_eq!(cache.len(), 1, "cache has one entry after first call");
            let key = (
                cwd.to_string(),
                String::new(),
                String::new(),
                ProjectStrategy::Basename.as_str().to_string(),
            );
            assert!(
                cache.contains_key(&key),
                "cache keyed by (cwd, ws_override, proj_override, project_strategy)"
            );
        }

        // Second call — must return the same IDs from the cache.
        let (_, proj_second) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        assert_eq!(proj_first, proj_second, "cache must return identical IDs");

        // Cache must still have exactly one entry (no duplicate insert).
        {
            let cache = state.project_cache.lock().await;
            assert_eq!(cache.len(), 1, "no duplicate cache entries");
        }
    }

    /// If the cached project is deleted out from under the router (e.g.
    /// `purge-project` on a live server), the next event must self-heal:
    /// evict the stale slot, recreate the project, and ingest — instead of
    /// failing forever on the `sessions.project_id` foreign key.
    #[tokio::test]
    async fn process_self_heals_when_cached_project_purged() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/user/heal-project";

        // 1) First event creates + caches the project (and a session).
        let env1 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-sess-1" }),
        );
        process(&state, env1).await.unwrap();
        let (ws, proj) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();

        // 2) Purge the project — the DB row is gone but the cache still
        //    points at it (exactly the purge-on-live-server scenario).
        state
            .writer
            .purge_project(ws, proj, "default/heal-project")
            .await
            .unwrap();
        assert!(
            state
                .project_cache
                .lock()
                .await
                .values()
                .any(|ids| *ids == (ws, proj)),
            "cache still holds the now-deleted project id"
        );

        // 3) Next event with the same cwd must NOT error on the stale FK —
        //    the router evicts, recreates, and ingests.
        let env2 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-sess-2" }),
        );
        process(&state, env2)
            .await
            .expect("self-heal: stale cached project must be recreated, not FK-fail");

        // 4) The project was recreated (fresh id) and the event landed.
        let (_, proj_new) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        assert_ne!(
            proj_new, proj,
            "purged project must be replaced by a fresh one"
        );
        let counts = state.reader.status_counts().await.unwrap();
        assert!(counts.sessions >= 1, "recreated session must be persisted");
    }

    #[tokio::test]
    async fn process_self_heal_evicts_project_strategy_cache_slot() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let repo_dir = tmp.path().join("repo-root-project");
        init_repo_with_commit(&repo_dir);
        let app_dir = repo_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let cwd = app_dir.to_str().unwrap();

        let env1 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                project_strategy: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-repo-root-1" }),
        );
        process(&state, env1).await.unwrap();
        let (ws, proj) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::RepoRoot)
                .await
                .unwrap();

        state
            .writer
            .purge_project(ws, proj, "default/repo-root-project")
            .await
            .unwrap();

        let env2 = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("claude-code".into()),
                cwd: Some(cwd.into()),
                project_strategy: Some("repo-root".into()),
                ..Default::default()
            },
            serde_json::json!({ "sessionID": "heal-repo-root-2" }),
        );
        process(&state, env2)
            .await
            .expect("repo-root cache slot must be evicted and recreated");

        let (_, proj_new) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::RepoRoot)
                .await
                .unwrap();
        assert_ne!(proj_new, proj);
    }

    /// A hook event fires end-to-end through `process`. Validates that
    /// the session + observation rows land in the resolved project, not
    /// the server-default scratch project.
    #[tokio::test]
    async fn process_routes_observation_to_cwd_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "session-start".into(),
                agent: Some("claude-code".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "test-session-cwd-routing",
                "cwd": "/home/user/my-project",
            }),
        );

        process(&state, env).await.unwrap();

        // The observation must be in the project derived from the cwd,
        // not in the server-default `scratch` project.
        let (_, expected_proj) = resolve_project_ids(
            &state,
            Some("/home/user/my-project"),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        assert_ne!(
            expected_proj, state.project_id,
            "routing must not use server-default project"
        );
    }

    /// SessionEnd must always write the heuristic `sessions/<id>.md` page,
    /// even with `consolidate_on_session_end` enabled but no LLM provider:
    /// the opt-in LLM pass is additive and guarded by a present
    /// `consolidator`, so flag-on + no-provider degrades to today's
    /// deterministic behavior (issue #40 — no regression).
    #[tokio::test]
    async fn session_end_writes_heuristic_page_even_with_consolidate_flag_on() {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.consolidate_on_session_end = true; // flag on; consolidator stays None

        let sid = "11111111-1111-1111-1111-111111111111";
        for event in ["session-start", "session-end"] {
            let env = HookEnvelope::from_query_and_body(
                HookQuery {
                    event: event.into(),
                    agent: Some("claude-code".into()),
                    ..Default::default()
                },
                serde_json::json!({ "session_id": sid }),
            );
            process(&state, env).await.unwrap();
        }

        let pages = state
            .reader
            .recent_pages_for_project(state.workspace_id, state.project_id, 20)
            .await
            .unwrap();
        assert!(
            pages
                .iter()
                .any(|p| p.path.as_str().starts_with("sessions/")),
            "SessionEnd must write a heuristic sessions/<id>.md page regardless of the flag; got {:?}",
            pages.iter().map(|p| p.path.as_str()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn process_accepts_prompt_before_session_start() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "user-prompt".into(),
                agent: Some("opencode".into()),
                ..Default::default()
            },
            serde_json::json!({
                "sessionID": "opencode-resumed-session",
                "cwd": "/home/user/resumed-project",
                "prompt": "continue",
            }),
        );

        process(&state, env).await.unwrap();

        let counts = state.reader.status_counts().await.unwrap();
        assert_eq!(counts.sessions, 1);
        assert_eq!(counts.observations, 1);
    }

    #[tokio::test]
    async fn process_preserves_opt_in_extension_event_metadata() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                agent: Some("other".into()),
                extension: Some("fstech".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "fstech-custom-event",
                "cwd": "/home/user/crm",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );
        let session_id = resolve_session_id(&env).unwrap();

        process(&state, env).await.unwrap();

        let observations = state
            .reader
            .observations_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(observations.len(), 1);
        let obs = &observations[0];
        assert_eq!(obs.kind, ObservationKind::Other);
        assert_eq!(obs.extension.as_deref(), Some("fstech"));
        assert_eq!(obs.source_event.as_deref(), Some("lead.contact"));
        assert_eq!(obs.title, "Lead contacted");
        assert_eq!(obs.body, "Lead Maria requested a proposal");
        let hits = state
            .reader
            .search_observations_for_project(obs.workspace_id, obs.project_id, "maria".into(), 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "extension body should be searchable");
    }

    #[tokio::test]
    async fn process_unknown_event_without_extension_leaves_storage_clean() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let env = HookEnvelope::from_query_and_body(
            HookQuery {
                event: "lead.contact".into(),
                agent: Some("other".into()),
                ..Default::default()
            },
            serde_json::json!({
                "session_id": "plain-unknown-event",
                "cwd": "/home/user/crm",
                "title": "Lead contacted",
                "message": "Lead Maria requested a proposal"
            }),
        );
        let session_id = resolve_session_id(&env).unwrap();

        process(&state, env).await.unwrap();

        let observations = state
            .reader
            .observations_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(observations.len(), 1);
        let obs = &observations[0];
        assert_eq!(obs.kind, ObservationKind::Other);
        assert_eq!(obs.extension, None);
        assert_eq!(obs.source_event, None);
        assert_eq!(obs.title, "other");
        assert!(obs.body.is_empty());
        let hits = state
            .reader
            .search_observations_for_project(obs.workspace_id, obs.project_id, "maria".into(), 5)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "unknown events without extension must not leak custom payload into observation FTS"
        );
    }

    /// `.ai-memory.toml` walk-up declares `workspace = "movvia"`. The hook
    /// forwards it as a query param, so the same `cwd` ends up in a
    /// distinct workspace from the default-buckets resolver path.
    #[tokio::test]
    async fn workspace_override_yields_distinct_workspace() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws_default, _) = resolve_project_ids(
            &state,
            Some("/home/u/repo"),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        let (ws_movvia, _) = resolve_project_ids(
            &state,
            Some("/home/u/repo"),
            Some("movvia"),
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        assert_ne!(
            ws_default, ws_movvia,
            "marker-declared workspace must not collide with the default"
        );
    }

    #[tokio::test]
    async fn handoff_with_workspace_marker_and_cwd_uses_basename_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/u/repo";

        let (ws, proj) = resolve_project_ids(
            &state,
            Some(cwd),
            Some("acme"),
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        state
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: Some(std::path::PathBuf::from(cwd)),
                summary: "handoff summary".to_string(),
                open_questions: Vec::new(),
                next_steps: vec!["continue".to_string()],
                files_touched: Vec::new(),
            })
            .await
            .unwrap();

        let rendered = fetch_and_accept_handoff(
            &state,
            HandoffQuery {
                agent: Some("codex".into()),
                cwd: Some(cwd.into()),
                workspace: Some("acme".into()),
                project: None,
                project_strategy: None,
            },
        )
        .await
        .unwrap();

        assert!(
            rendered.as_deref().is_some_and(|s| s.contains("continue")),
            "workspace-only marker handoff lookup must resolve workspace + basename(cwd)"
        );
    }

    #[tokio::test]
    async fn handoff_with_no_marker_uses_cwd_basename_project() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/u/plain-repo";

        let (ws, proj) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        state
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: Some(std::path::PathBuf::from(cwd)),
                summary: "handoff summary".to_string(),
                open_questions: Vec::new(),
                next_steps: vec!["resume plain repo".to_string()],
                files_touched: Vec::new(),
            })
            .await
            .unwrap();

        let rendered = fetch_and_accept_handoff(
            &state,
            HandoffQuery {
                agent: Some("codex".into()),
                cwd: Some(cwd.into()),
                workspace: None,
                project: None,
                project_strategy: None,
            },
        )
        .await
        .unwrap();

        assert!(
            rendered
                .as_deref()
                .is_some_and(|s| s.contains("resume plain repo")),
            "no-marker handoff lookup must still resolve basename(cwd)"
        );
    }

    /// A marker file with `project = "pe-portais"` replaces the
    /// basename-derived project name for every descendant `cwd`.
    #[tokio::test]
    async fn project_override_replaces_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (_, proj_basename) = resolve_project_ids(
            &state,
            Some("/home/u/api"),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        let (_, proj_override) = resolve_project_ids(
            &state,
            Some("/home/u/api"),
            None,
            Some("pe-portais"),
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        assert_ne!(
            proj_basename, proj_override,
            "project override must produce a different ProjectId than basename(cwd)"
        );
    }

    /// Two events resolved with overrides land in the same `(ws, proj)`
    /// pair as long as the override names match — even if the `cwd`
    /// differs. Confirms the override is the source of truth.
    #[tokio::test]
    async fn matching_overrides_collapse_to_same_pair() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws_a, proj_a) = resolve_project_ids(
            &state,
            Some("/x"),
            Some("acme"),
            Some("api"),
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        let (ws_b, proj_b) = resolve_project_ids(
            &state,
            Some("/y"),
            Some("acme"),
            Some("api"),
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        assert_eq!(ws_a, ws_b);
        assert_eq!(proj_a, proj_b);
    }

    /// During a hook-script upgrade window, the same `cwd` may resolve
    /// with and without an override in the same process. The composite
    /// cache key keeps both rows isolated; otherwise the first one
    /// "wins" and the second silently inherits its `ProjectId`.
    #[tokio::test]
    async fn cache_does_not_poison_across_override_variants() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;
        let cwd = "/home/u/poison-test";

        let (ws_default, _) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        let (ws_movvia, _) = resolve_project_ids(
            &state,
            Some(cwd),
            Some("movvia"),
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        assert_ne!(
            ws_default, ws_movvia,
            "cache must distinguish override variants"
        );

        let cache = state.project_cache.lock().await;
        assert_eq!(
            cache.len(),
            2,
            "two distinct cache entries for same cwd with different overrides"
        );
    }

    /// With no `cwd` but with both overrides, the resolver still produces
    /// a real `(ws, proj)` pair — covers handoff fetches issued before
    /// any hook event has populated the cwd cache.
    #[tokio::test]
    async fn overrides_resolve_without_cwd() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (ws, proj) = resolve_project_ids(
            &state,
            None,
            Some("acme"),
            Some("api"),
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        assert_ne!(ws, state.workspace_id);
        assert_ne!(proj, state.project_id);
    }

    #[test]
    fn unknown_project_strategy_defaults_to_basename() {
        assert_eq!(
            ProjectStrategy::parse(Some("repo-root")),
            ProjectStrategy::RepoRoot
        );
        assert_eq!(
            ProjectStrategy::parse(Some("repo_root")),
            ProjectStrategy::RepoRoot
        );
        assert_eq!(
            ProjectStrategy::parse(Some("git-root")),
            ProjectStrategy::Basename
        );
    }

    #[tokio::test]
    async fn default_strategy_keeps_git_subdirs_as_basename_projects() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let main_dir = tmp.path().join("my-project");
        init_repo_with_commit(&main_dir);
        let app_dir = main_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cwd = app_dir.to_str().unwrap();

        let (_, proj_basename) =
            resolve_project_ids(&state, Some(app_cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        let (_, proj_explicit_app) = resolve_project_ids(
            &state,
            Some(main_dir.to_str().unwrap()),
            None,
            Some("app"),
            ProjectStrategy::RepoRoot,
        )
        .await
        .unwrap();
        let (_, proj_repo_root) =
            resolve_project_ids(&state, Some(app_cwd), None, None, ProjectStrategy::RepoRoot)
                .await
                .unwrap();

        assert_eq!(
            proj_basename, proj_explicit_app,
            "default strategy must keep project = basename(cwd) inside git repos"
        );
        assert_ne!(
            proj_basename, proj_repo_root,
            "repo-root strategy is opt-in and must not affect the basename default"
        );
    }

    #[tokio::test]
    async fn project_override_wins_over_repo_root_strategy() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let main_dir = tmp.path().join("repo");
        init_repo_with_commit(&main_dir);
        let app_dir = main_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cwd = app_dir.to_str().unwrap();

        let (_, proj_repo_root) =
            resolve_project_ids(&state, Some(app_cwd), None, None, ProjectStrategy::RepoRoot)
                .await
                .unwrap();
        let (_, proj_override_repo_root) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            Some("manual"),
            ProjectStrategy::RepoRoot,
        )
        .await
        .unwrap();
        let (_, proj_override_basename) = resolve_project_ids(
            &state,
            Some(app_cwd),
            None,
            Some("manual"),
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        assert_eq!(proj_override_repo_root, proj_override_basename);
        assert_ne!(
            proj_override_repo_root, proj_repo_root,
            "explicit project override must beat repo-root derivation"
        );
    }

    #[tokio::test]
    async fn cache_does_not_poison_across_project_strategies() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let main_dir = tmp.path().join("repo");
        init_repo_with_commit(&main_dir);
        let app_dir = main_dir.join("app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_cwd = app_dir.to_str().unwrap();

        let (_, proj_basename) =
            resolve_project_ids(&state, Some(app_cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        let (_, proj_repo_root) =
            resolve_project_ids(&state, Some(app_cwd), None, None, ProjectStrategy::RepoRoot)
                .await
                .unwrap();

        assert_ne!(proj_basename, proj_repo_root);
        let cache = state.project_cache.lock().await;
        assert_eq!(
            cache.len(),
            2,
            "same cwd must have isolated cache entries per project strategy"
        );
    }

    /// A git worktree must resolve to the same project as the main
    /// working directory only when the marker opts into repo-root identity.
    #[tokio::test]
    async fn worktree_resolves_to_same_project_as_main_repo() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // Create a real git repo inside the temp dir.
        let main_dir = tmp.path().join("my-project");
        let repo = init_repo_with_commit(&main_dir);

        // Create a worktree in a sibling directory.
        let wt_dir = tmp.path().join("my-project-feature-branch");
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        // Create a branch for the worktree to check out.
        let branch = repo.branch("feature-branch", &head, false).unwrap();
        repo.worktree(
            "feature-branch",
            &wt_dir,
            Some(git2::WorktreeAddOptions::new().reference(Some(&branch.into_reference()))),
        )
        .unwrap();

        let main_cwd = main_dir.to_str().unwrap();
        let wt_cwd = wt_dir.to_str().unwrap();

        let (ws_main, proj_main) = resolve_project_ids(
            &state,
            Some(main_cwd),
            None,
            None,
            ProjectStrategy::RepoRoot,
        )
        .await
        .unwrap();
        let (ws_wt, proj_wt) =
            resolve_project_ids(&state, Some(wt_cwd), None, None, ProjectStrategy::RepoRoot)
                .await
                .unwrap();

        assert_eq!(ws_main, ws_wt, "same workspace");
        assert_eq!(
            proj_main, proj_wt,
            "worktree must resolve to same project as main repo"
        );

        let (_, proj_wt_basename) =
            resolve_project_ids(&state, Some(wt_cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();
        assert_ne!(
            proj_main, proj_wt_basename,
            "default strategy must not collapse worktrees into the main repo project"
        );
    }

    /// A directory that is NOT inside a git repo must still resolve
    /// via basename(cwd), preserving the existing behaviour.
    #[tokio::test]
    async fn non_git_dir_falls_back_to_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        // Create a plain directory (no .git).
        let plain_dir = tmp.path().join("plain-project");
        std::fs::create_dir_all(&plain_dir).unwrap();
        let cwd = plain_dir.to_str().unwrap();

        let (_, proj) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();

        // Must NOT be the server-default scratch project.
        assert_ne!(proj, state.project_id);

        // Resolve a second time with a different basename to prove
        // they produce distinct projects (basename-based).
        let other_dir = tmp.path().join("other-project");
        std::fs::create_dir_all(&other_dir).unwrap();
        let (_, proj2) = resolve_project_ids(
            &state,
            Some(other_dir.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        assert_ne!(proj, proj2, "different basenames → different projects");
    }

    /// A bare repository must fall back to basename(cwd), not resolve
    /// to the grandparent directory via commondir().parent().
    #[tokio::test]
    async fn bare_repo_falls_back_to_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let bare_dir = tmp.path().join("my-bare-project.git");
        git2::Repository::init_bare(&bare_dir).unwrap();
        let cwd = bare_dir.to_str().unwrap();

        let (_, proj) =
            resolve_project_ids(&state, Some(cwd), None, None, ProjectStrategy::Basename)
                .await
                .unwrap();

        // Must NOT be the server-default scratch project — basename should work.
        assert_ne!(proj, state.project_id);

        // The project name should come from basename, not from the grandparent.
        // To verify: resolve with a different bare repo name and confirm different project.
        let bare_dir2 = tmp.path().join("other-bare.git");
        git2::Repository::init_bare(&bare_dir2).unwrap();
        let (_, proj2) = resolve_project_ids(
            &state,
            Some(bare_dir2.to_str().unwrap()),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();
        assert_ne!(
            proj, proj2,
            "different bare repo basenames → different projects"
        );
    }

    /// Windows-style backslash paths sent to a Linux server must
    /// still resolve to `basename(cwd)`, not the full path string.
    #[tokio::test]
    async fn windows_backslash_path_resolves_to_basename() {
        let tmp = TempDir::new().unwrap();
        let state = make_state(&tmp).await;

        let (_, proj_a) = resolve_project_ids(
            &state,
            Some(r"E:\source\ai-memory"),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        let (_, proj_b) = resolve_project_ids(
            &state,
            Some(r"C:\Users\dev\projects\ai-memory"),
            None,
            None,
            ProjectStrategy::Basename,
        )
        .await
        .unwrap();

        assert_eq!(
            proj_a, proj_b,
            "different Windows paths with same basename must resolve to same project"
        );
        assert_ne!(
            proj_a, state.project_id,
            "Windows path must not fall back to the server-default project"
        );
    }
}
