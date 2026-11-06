//! Server-side idle auto-end, tick level: what one `run_idle_end_tick` pass
//! does on top of the store's guarded batch. The store-level guarantees
//! (CAS guard, watermark math, cross-scope refusal) live in
//! `ai-memory-store::tests::idle_session_end`; here the tick's own decisions
//! are exercised: substantive vs lifecycle-only classification, the
//! conditional page write that never supersedes a real SessionEnd,
//! agent-page retention, managed-run exclusion, the batch bound, and the
//! convergence of a late real SessionEnd (via the real `/hook` route).
//!
//! Coverage matrix: M5 (attribution to the session owner), M6 (late real
//! SessionEnd converges, no double handoff), M9 (no overwrite of a
//! concurrently ended session), invariant #16 (pages shared / batons owned —
//! the idle end never creates a handoff and never filters the page).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::{
    ActorContext, AgentKind, IdentityKey, IngestMetrics, NewObservation, NewSession,
    ObservationKind, PagePath, ProjectId, Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_store::{SessionEndDisposition, Store, WorkstreamSelection};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::idle_end::{IDLE_END_BATCH, IdleEndTickInput, run_idle_end_tick};
use crate::router::{
    CwdProbes, DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, HookState, IngestGates, IngestRateLimiter,
    ProjectCacheStore, SessionAgents, SubagentSessionSet, hook_router,
};

const WORKSPACE: &str = "default";
const PROJECT: &str = "idle";
const CWD: &str = "/repo";

/// One hour in microseconds.
const HOUR_US: i64 = 3_600_000_000;

async fn harness() -> (TempDir, Store, Wiki, HookState) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace(WORKSPACE)
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, PROJECT, None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let state = HookState {
        ingest_metrics: Arc::new(IngestMetrics::default()),
        workspace_id: ws,
        project_id: proj,
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: wiki.clone(),
        consolidator: None,
        sanitizer: Sanitizer::default(),
        project_cache: Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default())),
        active_project: ai_memory_core::ActiveProject::new(),
        consolidate_on_session_end: true,
        session_consolidation_notify: None,
        profile_notify: None,
        capture_assistant_enabled: false,
        claim_handoff_on_session_start: true,
        create_handoff_on_session_end: true,
        subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
        ingest_rate: Arc::new(tokio::sync::Mutex::new(IngestRateLimiter::disabled())),
        home_dir: None,
        trusted_proxy_identity: false,
        ingest_semaphore: Arc::new(tokio::sync::Semaphore::new(
            DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
        )),
        ingest_gates: IngestGates::default(),
        session_agents: SessionAgents::default(),
        cwd_probes: CwdProbes::default(),
        per_user_slots: false,
        mid_session_routing: ai_memory_core::MidSessionRouting::default(),
        profile: ai_memory_core::profile::ProfileSettings::default(),
        ingest_drop_list: crate::drop_list::IngestDropList::default(),
        record_tool_names: false,
        skip_wiki_checkpoints: false,
        index_file_paths: false,
    };
    (tmp, store, wiki, state)
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

fn owner(user: &str) -> String {
    IdentityKey::User(user.to_string()).storage_key()
}

/// Begin a session, back-dated to `at` (and optionally owned by `owner`).
async fn begin(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    at: i64,
    actor_user: Option<String>,
) -> SessionId {
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: Some(CWD.into()),
            actor_user,
            occurred_at: Some(at),
        })
        .await
        .unwrap();
    sid
}

async fn observe(state: &HookState, sid: SessionId, kind: ObservationKind, at: i64, body: &str) {
    state
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: sid,
                workspace_id: state.workspace_id,
                project_id: state.project_id,
                kind,
                extension: None,
                source_event: None,
                title: "idle tick test event".into(),
                body: body.into(),
                importance: 5,
                occurred_at: Some(at),
                file_path: None,
            },
            &state.sanitizer,
        ))
        .await
        .unwrap();
}

async fn disposition(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    sid: SessionId,
) -> SessionEndDisposition {
    store
        .reader
        .session_end_disposition(sid, ws, proj, AgentKind::Codex)
        .await
        .unwrap()
}

/// The page path the tick and the hook share for one session.
fn page_path(sid: SessionId) -> String {
    format!("sessions/{}.md", sid.to_string().to_lowercase())
}

async fn page_total(
    reader: &ai_memory_store::ReaderPool,
    ws: WorkspaceId,
    proj: ProjectId,
    sid: SessionId,
) -> u64 {
    reader
        .page_versions(ws, proj, page_path(sid), 10, 0)
        .await
        .unwrap()
        .total
}

async fn open_handoffs(store: &Store, ws: WorkspaceId, proj: ProjectId) -> usize {
    store
        .reader
        .open_handoffs_for_project(ws, proj, 100)
        .await
        .unwrap()
        .len()
}

async fn idle_end_audit(store: &Store) -> Vec<ai_memory_store::AuditEvent> {
    store
        .reader
        .list_audit_events(ai_memory_store::AuditLogFilter {
            workspace: Some(WORKSPACE.to_string()),
            project: Some(PROJECT.to_string()),
            op: Some("idle_session_end".to_string()),
            before_id: None,
            limit: 5,
        })
        .await
        .unwrap()
}

/// POST a real `/hook` SessionEnd for `sid`, the same shape a lifecycle
/// hook sends. Returns the HTTP status of the acceptance.
async fn post_session_end(state: &HookState, sid: SessionId) -> StatusCode {
    let app = hook_router(state.clone());
    let body = serde_json::json!({
        "event": "session-end",
        "session_id": sid.to_string(),
    });
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/hook?event=session-end&agent=codex&cwd={CWD}&workspace={WORKSPACE}&project={PROJECT}"
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

/// Poll an async condition (bounded) until it holds; returns whether it did.
///
/// The hook route returns 202 as soon as the effect task is SPAWNED (the
/// hook path is fire-and-forget: no completion barrier is offered), so a
/// test that must observe the effect's store state has no deterministic
/// hand-off point to wait on. Bounded polling against a monotone condition
/// (a row appears / a count settles) is the faithful substitute; a sleep
/// before the assertion would be a race fudge, this is not.
async fn until<'a, F>(mut cond: F) -> bool
where
    F: FnMut() -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>,
{
    for _ in 0..200 {
        if cond().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond().await
}

#[tokio::test]
async fn idle_tick_ends_idle_substantive_session_with_page_job_and_no_handoff() {
    let (_tmp, store, wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();
    // Owned by operator "alice": last activity five hours ago.
    let sid = begin(&store, ws, proj, now - 5 * HOUR_US, Some(owner("alice"))).await;
    observe(
        &state,
        sid,
        ObservationKind::UserPrompt,
        now - 5 * HOUR_US,
        "frobnicate the idle watermark sentinel",
    )
    .await;

    // Selection, exactly as `serve` does: idle cutoff two hours.
    let sessions = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, sid);
    assert_eq!(
        sessions[0].actor_user.as_deref(),
        Some(owner("alice").as_str())
    );

    let outcome = run_idle_end_tick(&IdleEndTickInput {
        writer: &store.writer,
        reader: &store.reader,
        wiki: &wiki,
        sessions,
        now_us: jiff::Timestamp::now().as_microsecond(),
        consolidate_on_session_end: true,
        has_consolidator: true,
    })
    .await
    .unwrap();
    assert_eq!(
        (
            outcome.ended,
            outcome.lifecycle_only,
            outcome.skipped,
            outcome.pages_written
        ),
        (1, 0, 0, 1)
    );

    // The session is ended, watermark = last activity, audited with a page.
    assert!(matches!(
        disposition(&store, ws, proj, sid).await,
        SessionEndDisposition::AlreadyEnded
    ));
    let audit = idle_end_audit(&store).await;
    assert_eq!(audit.len(), 1);
    assert!(
        audit[0].at >= now && audit[0].at < now + HOUR_US,
        "ended_at is the tick's clock (the discovery is ordered by ended_at), not the last activity: {}",
        audit[0].at
    );
    assert!(
        audit[0].detail.contains(&(now - 5 * HOUR_US).to_string()),
        "the last activity survives in the audit detail: {}",
        audit[0].detail
    );
    assert_eq!(audit[0].page_path.as_deref(), Some(page_path(sid).as_str()));

    // Page exists and is attributed to the session owner (M5): the frontmatter
    // carries the owner's username, not the anonymous server identity.
    let path = PagePath::new(page_path(sid)).unwrap();
    let doc = wiki.read_page(ws, proj, &path).unwrap();
    assert_eq!(
        doc.frontmatter["last_modified_by"]["username"], "alice",
        "page must carry the session owner, got: {}",
        doc.frontmatter
    );
    // Invariant #16: pages are shared — a second operator reads the page the
    // operator's harness left idle-ended, with the owner's attribution intact
    // (`author_id` never becomes a read filter).
    use ai_memory_core::{NewUser, UserRole};
    let bob = store
        .writer
        .create_human_user(
            NewUser {
                username: "bob".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let hits = store
        .reader
        .search_pages("frobnicate sentinel idle".to_string(), 5, Some(bob))
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.path.to_string() == page_path(sid)),
        "a second operator must read the owner's idle-ended page; hits: {:?}",
        hits.iter().map(|h| h.path.to_string()).collect::<Vec<_>>()
    );

    // Consolidation job enqueued (claim with a fresh clock: the enqueue's
    // `requested_at` is the tick's wall time, after this test's `now`).
    let now_after = now_us();
    let job = store
        .writer
        .claim_session_consolidation(now_after + 1_000, now_after - 1_000)
        .await
        .unwrap();
    assert_eq!(job.map(|j| j.session_id()), Some(sid));

    // No handoff: the idle end is never a baton hand-off.
    assert_eq!(open_handoffs(&store, ws, proj).await, 0);

    // A second tick sees nothing: the ended session is outside the scan.
    let again = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert!(again.is_empty());
}

#[tokio::test]
async fn idle_tick_lifecycle_only_ends_without_page_or_job_and_creates_no_handoff() {
    let (_tmp, store, _wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();
    // Session whose only content is a Stop (no substantive observation).
    let sid = begin(&store, ws, proj, now - 4 * HOUR_US, None).await;
    observe(
        &state,
        sid,
        ObservationKind::Stop,
        now - 4 * HOUR_US,
        "done",
    )
    .await;

    let sessions = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);

    let outcome = run_idle_end_tick(&IdleEndTickInput {
        writer: &store.writer,
        reader: &store.reader,
        wiki: &state.wiki,
        sessions,
        now_us: jiff::Timestamp::now().as_microsecond(),
        consolidate_on_session_end: true,
        has_consolidator: true,
    })
    .await
    .unwrap();
    assert_eq!(
        (
            outcome.ended,
            outcome.lifecycle_only,
            outcome.skipped,
            outcome.pages_written
        ),
        (0, 1, 0, 0)
    );

    assert!(matches!(
        disposition(&store, ws, proj, sid).await,
        SessionEndDisposition::AlreadyEnded
    ));
    assert!(
        store
            .reader
            .latest_page_id_by_ids(ws, proj, page_path(sid))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .writer
            .claim_session_consolidation(now + 1_000, now - 1_000)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(open_handoffs(&store, ws, proj).await, 0);
}

#[tokio::test]
async fn idle_tick_skips_active_sessions_and_respects_the_batch() {
    let (_tmp, store, wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();

    // 27 idle sessions (oldest first) plus one active session.
    for i in 0..27 {
        // 5h..5h13m ago — all well past the one-hour cutoff.
        let at = now - (5 * 3600 + i * 30) * 1_000_000;
        let sid = begin(&store, ws, proj, at, None).await;
        observe(&state, sid, ObservationKind::UserPrompt, at, "prompt").await;
    }
    let active = begin(&store, ws, proj, now - 10 * 60 * 1_000_000, None).await;
    observe(
        &state,
        active,
        ObservationKind::UserPrompt,
        now - 10 * 60 * 1_000_000,
        "still working",
    )
    .await;

    let tick = |sessions: Vec<_>| {
        let (store, wiki) = (&store, &wiki);
        async move {
            run_idle_end_tick(&IdleEndTickInput {
                writer: &store.writer,
                reader: &store.reader,
                wiki,
                sessions,
                now_us: jiff::Timestamp::now().as_microsecond(),
                consolidate_on_session_end: true,
                has_consolidator: true,
            })
            .await
        }
    };

    // Pass 1 ends exactly the batch bound — oldest first, never the active.
    let selected = store
        .reader
        .idle_open_sessions(now - HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(selected.len(), IDLE_END_BATCH);
    assert!(!selected.iter().any(|s| s.session_id == active));
    let outcome = tick(selected).await.unwrap();
    assert_eq!(outcome.ended, IDLE_END_BATCH);

    // Pass 2 ends the two remaining idle sessions.
    let selected = store
        .reader
        .idle_open_sessions(now - HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(selected.len(), 2);
    let outcome = tick(selected).await.unwrap();
    assert_eq!(outcome.ended, 2);

    // Pass 3: only the active session remains — it is never selected.
    let selected = store
        .reader
        .idle_open_sessions(now - HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert!(selected.is_empty());
    assert!(matches!(
        disposition(&store, ws, proj, active).await,
        SessionEndDisposition::Open
    ));
}

#[tokio::test]
async fn idle_tick_excludes_active_managed_run_sessions() {
    let (_tmp, store, wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();
    // Idle on paper: last activity five hours ago…
    let sid = begin(&store, ws, proj, now - 5 * HOUR_US, None).await;
    observe(
        &state,
        sid,
        ObservationKind::UserPrompt,
        now - 5 * HOUR_US,
        "old prompt",
    )
    .await;
    // …but a managed run launched from it is still active.
    let run = store
        .writer
        .prepare_workstream_run(ai_memory_store::PrepareWorkstreamRun {
            scope_by_id: false,
            workspace_id: ws,
            project_id: proj,
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            cwd: CWD.into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: "test:idle".into(),
        })
        .await
        .unwrap();
    store
        .writer
        .link_managed_run_session(run.run_id, AgentKind::Codex, sid.to_string())
        .await
        .unwrap();

    let run_tick = |sessions| {
        let (store, wiki) = (&store, &wiki);
        async move {
            run_idle_end_tick(&IdleEndTickInput {
                writer: &store.writer,
                reader: &store.reader,
                wiki,
                sessions,
                now_us: jiff::Timestamp::now().as_microsecond(),
                consolidate_on_session_end: true,
                has_consolidator: true,
            })
            .await
        }
    };

    let sessions = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    let outcome = run_tick(sessions).await.unwrap();
    assert_eq!((outcome.ended, outcome.skipped), (0, 1));
    assert!(matches!(
        disposition(&store, ws, proj, sid).await,
        SessionEndDisposition::Open
    ));

    // The run finishes: the next pass ends the session normally.
    store.writer.cancel_managed_run(run.run_id).await.unwrap();
    let sessions = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    let outcome = run_tick(sessions).await.unwrap();
    assert_eq!((outcome.ended, outcome.skipped), (1, 0));
    assert!(matches!(
        disposition(&store, ws, proj, sid).await,
        SessionEndDisposition::AlreadyEnded
    ));
}

#[tokio::test]
async fn idle_tick_excluded_backlog_cannot_starve_the_batch() {
    let (_tmp, store, wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();

    // 25 idle sessions each bound to an active managed run (the excluded
    // backlog, the OLDEST), plus one eligible session BEHIND them.
    let mut excluded = Vec::new();
    for i in 0..25u32 {
        let at = now - (26 - i as i64) * HOUR_US;
        let sid = begin(&store, ws, proj, at, None).await;
        observe(
            &state,
            sid,
            ObservationKind::UserPrompt,
            at,
            "backlog prompt",
        )
        .await;
        let run = store
            .writer
            .prepare_workstream_run(ai_memory_store::PrepareWorkstreamRun {
                scope_by_id: false,
                workspace_id: ws,
                project_id: proj,
                repo_fingerprint: format!("repo-{i}"),
                worktree_fingerprint: format!("worktree-{i}"),
                cwd: CWD.into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                selection: WorkstreamSelection::Current,
                lease_owner: format!("test:idle-backlog-{i}"),
            })
            .await
            .unwrap();
        store
            .writer
            .link_managed_run_session(run.run_id, AgentKind::Codex, sid.to_string())
            .await
            .unwrap();
        excluded.push((sid, run.run_id));
    }
    let eligible = begin(&store, ws, proj, now - HOUR_US, None).await;
    observe(
        &state,
        eligible,
        ObservationKind::UserPrompt,
        now - HOUR_US,
        "eligible prompt",
    )
    .await;

    // The serve loop over-fetches the selection past the excluded sessions
    // (batch + active-run count): the tick must not starve the eligible
    // session behind a 25-deep excluded backlog.
    let sessions = store
        .reader
        .idle_open_sessions(now - 30 * 60_000_000, IDLE_END_BATCH.saturating_add(25))
        .await
        .unwrap();
    assert_eq!(sessions.len(), 26);

    let run_tick = |sessions| {
        let (store, wiki) = (&store, &wiki);
        async move {
            run_idle_end_tick(&IdleEndTickInput {
                writer: &store.writer,
                reader: &store.reader,
                wiki,
                sessions,
                now_us: jiff::Timestamp::now().as_microsecond(),
                consolidate_on_session_end: true,
                has_consolidator: true,
            })
            .await
        }
    };

    let outcome = run_tick(sessions).await.unwrap();
    assert_eq!(
        (outcome.ended, outcome.skipped),
        (1, 25),
        "the eligible session ends on the FIRST pass despite 25 excluded sessions ahead of it"
    );
    assert!(matches!(
        disposition(&store, ws, proj, eligible).await,
        SessionEndDisposition::AlreadyEnded
    ));

    // A second pass: the eligible one is gone; the excluded backlog stays
    // open (their lifecycle belongs to the runs).
    let sessions = store
        .reader
        .idle_open_sessions(now - 30 * 60_000_000, IDLE_END_BATCH.saturating_add(25))
        .await
        .unwrap();
    assert_eq!(sessions.len(), 25);
    let outcome = run_tick(sessions).await.unwrap();
    assert_eq!(
        (outcome.ended, outcome.skipped),
        (0, 25),
        "the excluded sessions stay open over the second pass"
    );
    for (sid, _run) in &excluded {
        assert!(matches!(
            disposition(&store, ws, proj, *sid).await,
            SessionEndDisposition::Open
        ));
    }
}

#[tokio::test]
async fn idle_tick_keeps_a_session_page_written_by_the_agent() {
    let (_tmp, store, wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();
    let sid = begin(&store, ws, proj, now - 5 * HOUR_US, None).await;
    observe(
        &state,
        sid,
        ObservationKind::UserPrompt,
        now - 5 * HOUR_US,
        "agent wrote its own summary",
    )
    .await;

    // The agent already wrote the session page (e.g. via `memory_write_page`).
    let path = PagePath::new(page_path(sid)).unwrap();
    wiki.write_page(ai_memory_wiki::WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: path.clone(),
        frontmatter: serde_json::json!({ "consolidated_by": "agent" }),
        body: "Agent-authored summary that must survive the idle end.".into(),
        tier: Tier::Episodic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::default(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();
    let current = store
        .reader
        .latest_page_id_by_ids(ws, proj, page_path(sid))
        .await
        .unwrap()
        .expect("agent page must be indexed");

    let sessions = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].seen_page_id,
        Some(current),
        "selection must carry the current page id"
    );

    let outcome = run_idle_end_tick(&IdleEndTickInput {
        writer: &store.writer,
        reader: &store.reader,
        wiki: &wiki,
        sessions,
        now_us: jiff::Timestamp::now().as_microsecond(),
        consolidate_on_session_end: true,
        has_consolidator: true,
    })
    .await
    .unwrap();
    assert_eq!(
        (outcome.ended, outcome.pages_written),
        (1, 0),
        "an agent-written page is kept, not re-synthesized"
    );
    assert!(matches!(
        disposition(&store, ws, proj, sid).await,
        SessionEndDisposition::AlreadyEnded
    ));
    // Exactly one page version: the agent's, untouched.
    assert_eq!(page_total(&store.reader, ws, proj, sid).await, 1);
    let doc = wiki.read_page(ws, proj, &path).unwrap();
    assert!(doc.body.contains("Agent-authored summary"));
}

#[tokio::test]
async fn idle_tick_skips_a_session_ended_by_a_real_session_end_between_selection_and_write() {
    let (_tmp, store, _wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();
    let sid = begin(&store, ws, proj, now - 5 * HOUR_US, None).await;
    observe(
        &state,
        sid,
        ObservationKind::UserPrompt,
        now - 5 * HOUR_US,
        "real end wins the race",
    )
    .await;

    // The tick selects the session…
    let sessions = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);

    // …a real `/hook` SessionEnd lands and ends the session first. The
    // route 202s after SPAWNING its effect (no completion barrier), so the
    // bounded poll below waits on the effect's store state, not a sleep.
    assert_eq!(post_session_end(&state, sid).await, StatusCode::ACCEPTED);
    let settled = until(|| {
        let store = &store;
        Box::pin(async move {
            matches!(
                disposition(store, ws, proj, sid).await,
                SessionEndDisposition::AlreadyEnded
            )
        })
    })
    .await;
    assert!(settled, "the real SessionEnd must end the session");
    let hook_page = store
        .reader
        .latest_page_id_by_ids(ws, proj, page_path(sid))
        .await
        .unwrap()
        .expect("hook page must exist");

    // The tick then runs with its stale selection: the page precondition
    // (Absent) no longer holds and the CAS guard (ended_at IS NULL) no
    // longer holds either — it must skip, not overwrite (M9).
    let outcome = run_idle_end_tick(&IdleEndTickInput {
        writer: &store.writer,
        reader: &store.reader,
        wiki: &state.wiki,
        sessions,
        now_us: jiff::Timestamp::now().as_microsecond(),
        consolidate_on_session_end: true,
        has_consolidator: true,
    })
    .await
    .unwrap();
    assert_eq!(
        (outcome.ended, outcome.skipped, outcome.pages_written),
        (0, 1, 0)
    );
    // The hook's page is the only version — the tick superseded nothing.
    assert_eq!(
        store
            .reader
            .latest_page_id_by_ids(ws, proj, page_path(sid))
            .await
            .unwrap(),
        Some(hook_page)
    );
    assert_eq!(page_total(&store.reader, ws, proj, sid).await, 1);
    // The hook created its handoff; the tick added none.
    assert_eq!(open_handoffs(&store, ws, proj).await, 1);
    // No idle-end audit row: the tick ended nothing.
    assert!(idle_end_audit(&store).await.is_empty());
}

#[tokio::test]
async fn idle_tick_late_real_session_end_with_new_work_reends_with_handoff() {
    let (_tmp, store, _wiki, state) = harness().await;
    let ws = state.workspace_id;
    let proj = state.project_id;
    let now = now_us();
    let sid = begin(&store, ws, proj, now - 5 * HOUR_US, None).await;
    observe(
        &state,
        sid,
        ObservationKind::UserPrompt,
        now - 5 * HOUR_US,
        "first work",
    )
    .await;

    // The tick ends the session first (watermark at the old activity).
    let sessions = store
        .reader
        .idle_open_sessions(now - 2 * HOUR_US, IDLE_END_BATCH)
        .await
        .unwrap();
    let outcome = run_idle_end_tick(&IdleEndTickInput {
        writer: &store.writer,
        reader: &store.reader,
        wiki: &state.wiki,
        sessions,
        now_us: jiff::Timestamp::now().as_microsecond(),
        consolidate_on_session_end: true,
        has_consolidator: true,
    })
    .await
    .unwrap();
    assert_eq!(outcome.ended, 1);

    // The session was not really done: new work arrives, then the real
    // SessionEnd. The hook must re-end (new watermark, new page version,
    // and the one handoff a real SessionEnd carries).
    observe(
        &state,
        sid,
        ObservationKind::UserPrompt,
        now - 1_000,
        "late work after the idle end",
    )
    .await;
    assert_eq!(post_session_end(&state, sid).await, StatusCode::ACCEPTED);
    let settled = until(|| {
        let store = &store;
        Box::pin(async move { open_handoffs(store, ws, proj).await == 1 })
    })
    .await;
    assert!(
        settled,
        "the real SessionEnd must claim exactly one handoff"
    );
    assert_eq!(open_handoffs(&store, ws, proj).await, 1);
    assert_eq!(page_total(&store.reader, ws, proj, sid).await, 2);
    assert!(matches!(
        disposition(&store, ws, proj, sid).await,
        SessionEndDisposition::AlreadyEnded
    ));
    // No second idle-end audit row: the hook's re-end carries its own audit
    // op, and the tick's single end stands unchallenged.
    assert_eq!(idle_end_audit(&store).await.len(), 1);
}
