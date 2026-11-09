//! Server-side idle auto-end, store level: cutoff-scoped selection, the
//! tick-clock `ended_at` with the last activity preserved in the audit, the
//! guarded stamp (the `ended_at IS NULL` CAS re-verifying the in-scope
//! observation count and the latest page version a real `SessionEnd` that
//! wins the race is never re-stamped), the lifecycle-only audit + handoff
//! release, the late-real-`SessionEnd` convergence against the watermark,
//! and batched one-command ends.

use ai_memory_core::{
    AgentKind, HandoffAcceptance, HandoffId, HandoffState, NewHandoff, NewObservation, NewPage,
    NewSession, ObservationKind, OwnerFilter, PagePath, ProjectId, Sanitized, Sanitizer, SessionId,
    Tier, WorkspaceId,
};
use ai_memory_store::{
    AuditLogFilter, IdleEndAction, IdleEndCandidate, IdleEndResult, PrepareWorkstreamRun,
    SessionEndDisposition, Store, WorkstreamSelection,
};

const HOUR_US: i64 = 3_600_000_000;

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "idle", None)
        .await
        .unwrap();
    (ws, proj)
}

fn backdated(started_at_us: i64, actor_user: Option<String>) -> NewSession {
    NewSession {
        occurred_at: Some(started_at_us),
        id: SessionId::new(),
        workspace_id: WorkspaceId::new(),
        project_id: ProjectId::new(),
        agent_kind: AgentKind::Codex,
        cwd: Some("/repo".into()),
        actor_user,
    }
}

async fn begin(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    at_us: i64,
    actor: Option<String>,
) -> SessionId {
    let id = SessionId::new();
    begin_with_id(store, ws, proj, id, at_us, actor).await;
    id
}

async fn begin_with_id(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    id: SessionId,
    at_us: i64,
    actor: Option<String>,
) {
    store
        .writer
        .begin_session(NewSession {
            id,
            workspace_id: ws,
            project_id: proj,
            ..backdated(at_us, actor)
        })
        .await
        .unwrap();
}

fn observation(
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
    kind: ObservationKind,
    at_us: i64,
    body: &str,
) -> NewObservation {
    NewObservation {
        session_id: sid,
        workspace_id: ws,
        project_id: proj,
        kind,
        extension: None,
        source_event: None,
        title: "idle test event".into(),
        body: body.into(),
        importance: 5,
        occurred_at: Some(at_us),
        file_path: None,
    }
}

async fn insert(store: &Store, obs: NewObservation) {
    store
        .writer
        .insert_observation(Sanitized::new(obs, &Sanitizer::builtin()))
        .await
        .unwrap();
}

fn substantive(
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
    page: ai_memory_core::PageId,
    count: i64,
    now_us: i64,
    last_activity_us: i64,
) -> IdleEndCandidate {
    IdleEndCandidate {
        session_id: sid,
        workspace_id: ws,
        project_id: proj,
        agent_kind: AgentKind::Codex,
        action: IdleEndAction::Substantive {
            expected_page_id: page,
            observation_count: count,
            now_us,
            last_activity_us,
        },
    }
}

fn lifecycle_only(
    sid: SessionId,
    ws: WorkspaceId,
    proj: ProjectId,
    now_us: i64,
    last_activity_us: i64,
) -> IdleEndCandidate {
    IdleEndCandidate {
        session_id: sid,
        workspace_id: ws,
        project_id: proj,
        agent_kind: AgentKind::Codex,
        action: IdleEndAction::LifecycleOnly {
            now_us,
            last_activity_us,
        },
    }
}

async fn session_page(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    sid: SessionId,
) -> ai_memory_core::PageId {
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(format!("sessions/{sid}.md")).unwrap(),
            title: "summary".into(),
            body: "rule-based summary".into(),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap()
}

/// A new version of the session page with distinct body content: the
/// store's upsert is idempotent for identical content (same version id),
/// so a "moved latest page" needs a real content change.
async fn session_page_body(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    sid: SessionId,
    body: &str,
) -> ai_memory_core::PageId {
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(format!("sessions/{sid}.md")).unwrap(),
            title: "summary".into(),
            body: body.into(),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap()
}

fn idle_audit_filter() -> AuditLogFilter {
    AuditLogFilter {
        workspace: Some("default".into()),
        project: Some("idle".into()),
        op: Some("idle_session_end".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn idle_selection_is_cutoff_scoped_ordered_and_bounded() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();

    // Idle + substantive: started 10h ago, last activity 9h ago.
    let old = begin(&store, ws, proj, now - 10 * HOUR_US, None).await;
    insert(
        &store,
        observation(
            old,
            ws,
            proj,
            ObservationKind::UserPrompt,
            now - 9 * HOUR_US,
            "old prompt",
        ),
    )
    .await;
    // Idle + no observations: last activity falls back to started_at (5h ago).
    let bare = begin(&store, ws, proj, now - 5 * HOUR_US, None).await;
    // Active: last activity 1h ago — outside a 2h cutoff.
    let fresh = begin(&store, ws, proj, now - HOUR_US, None).await;
    insert(
        &store,
        observation(
            fresh,
            ws,
            proj,
            ObservationKind::UserPrompt,
            now - HOUR_US,
            "fresh prompt",
        ),
    )
    .await;
    // Already ended 7h ago: never a candidate.
    let ended = begin(&store, ws, proj, now - 8 * HOUR_US, None).await;
    insert(
        &store,
        observation(
            ended,
            ws,
            proj,
            ObservationKind::UserPrompt,
            now - 7 * HOUR_US,
            "ended prompt",
        ),
    )
    .await;
    store.writer.end_session(ended, None).await.unwrap();

    let cutoff = now - 2 * HOUR_US;
    let idle = store.reader.idle_open_sessions(cutoff, 25).await.unwrap();
    let ids: Vec<SessionId> = idle.iter().map(|s| s.session_id).collect();
    // Oldest last-activity first: the 9h-old session, then the 5h-old bare one.
    assert_eq!(ids, vec![old, bare]);
    assert_eq!(
        idle[0].last_activity_us,
        now - 9 * HOUR_US,
        "the watermark is MAX(observations.created_at), not started_at"
    );
    assert_eq!(
        idle[1].last_activity_us,
        now - 5 * HOUR_US,
        "a session without observations falls back to started_at"
    );
    assert!(idle.iter().all(|s| s.seen_page_id.is_none()));

    // Bounded: a limit of one keeps only the oldest.
    let first = store.reader.idle_open_sessions(cutoff, 1).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].session_id, old);

    // The selection also captures each session page's latest version (the
    // tick's write precondition) from the same statement: a session WITH a
    // page reports it, a pageless one stays `None`. This also proves the
    // SQL-rendered path (the id blob's hex with 8-4-4-4-12 hyphens) is
    // byte-for-byte the Rust `sessions/<id>.md` the page was written at.
    let page = session_page(&store, ws, proj, old).await;
    let idle = store.reader.idle_open_sessions(cutoff, 25).await.unwrap();
    let by_id = |sid: SessionId| idle.iter().find(|s| s.session_id == sid).unwrap();
    assert_eq!(by_id(old).seen_page_id, Some(page));
    assert!(by_id(bare).seen_page_id.is_none());
}

#[tokio::test]
async fn idle_end_stamps_watermark_audit_page_and_consolidation() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;

    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            true,
        )
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::EndedSubstantive]);

    // The audit row carries the tick's clock (the auto-improvement discovery
    // is ordered by `ended_at`, so auto-ended sessions are found in the
    // order they were ended) — and keeps the last activity next to it.
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].at, now, "ended_at is the tick's clock");
    assert!(audits[0].detail.contains(&sid.to_string()));
    assert!(
        audits[0].detail.contains(&watermark.to_string()),
        "the last activity survives in the audit detail: {}",
        audits[0].detail
    );

    // A late real SessionEnd with no new work converges against the watermark.
    let disposition = store
        .reader
        .session_end_disposition(sid, ws, proj, AgentKind::Codex)
        .await
        .unwrap();
    assert_eq!(disposition, SessionEndDisposition::AlreadyEnded);

    // The consolidation job is enqueued inside the same command. A fresh
    // clock: the job's `next_attempt_at` is the enqueue's own `now`.
    let claimed_now = jiff::Timestamp::now().as_microsecond();
    let job = store
        .writer
        .claim_session_consolidation(claimed_now, claimed_now - 1)
        .await
        .unwrap()
        .expect("the idle end enqueues its consolidation job");
    assert_eq!(job.session_id(), sid);

    // It is no longer selectable as idle.
    assert!(
        store
            .reader
            .idle_open_sessions(claimed_now - HOUR_US, 25)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn idle_end_loses_to_an_already_ended_session() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "worked on it",
        ),
    )
    .await;
    // The real SessionEnd lands first (its end is now, not the watermark).
    store.writer.end_session(sid, None).await.unwrap();

    let page = session_page(&store, ws, proj, sid).await;
    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            false,
        )
        .await
        .unwrap();
    // CAS lost: no restamp, no audit, no overwrite of the hook's end.
    assert_eq!(results, vec![IdleEndResult::Lost]);
    let audits = store
        .reader
        .list_audit_events(AuditLogFilter {
            op: Some("idle_session_end".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(audits.is_empty(), "a lost end writes no audit row");
    let disposition = store
        .reader
        .session_end_disposition(sid, ws, proj, AgentKind::Codex)
        .await
        .unwrap();
    assert_eq!(disposition, SessionEndDisposition::AlreadyEnded);
}

#[tokio::test]
async fn idle_end_reopens_the_handoff_a_lifecycle_only_session_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 4 * HOUR_US;

    // Boundary-only: a start and a stop, no real work.
    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::Stop,
            watermark,
            "turn finished",
        ),
    )
    .await;
    let handoff = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: Some("/repo".into()),
            summary: "pick up the build".into(),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            owner_user: None,
        })
        .await
        .unwrap();
    assert!(
        store
            .writer
            .accept_handoff(HandoffAcceptance {
                handoff_id: handoff,
                workspace_id: ws,
                project_id: proj,
                accepting_agent: AgentKind::Codex,
                accepting_session: Some(sid),
                accepting_user: None,
                owner_filter: OwnerFilter::Any,
                receiving_cwd: Some("/repo".into()),
            })
            .await
            .unwrap()
    );

    let results = store
        .writer
        .end_idle_sessions(vec![lifecycle_only(sid, ws, proj, now, watermark)], true)
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::EndedLifecycleOnly]);

    // The claimed baton goes back to the open pool for its owner.
    let reopened = store.reader.handoff_by_id(handoff).await.unwrap().unwrap();
    assert_eq!(reopened.lifecycle.state, HandoffState::Open);
    assert!(reopened.lifecycle.accepted_by_session.is_none());

    // No page, no consolidation job, no new handoff. Fresh clock so the
    // "no job" claim actually sees due jobs.
    assert!(
        store
            .reader
            .latest_page_id_by_ids(ws, proj, format!("sessions/{sid}.md"))
            .await
            .unwrap()
            .is_none()
    );
    let claimed_now = jiff::Timestamp::now().as_microsecond();
    assert!(
        store
            .writer
            .claim_session_consolidation(claimed_now, claimed_now - 1)
            .await
            .unwrap()
            .is_none()
    );
    // The auto-end audit is its OWN row, distinct from the handoff release's
    // `release_lifecycle_only_handoff` row: an operator looking for the
    // automatic end sees exactly one idle_session_end row, with the tick's
    // clock and the last activity in the detail.
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert_eq!(audits.len(), 1, "exactly one idle_session_end row");
    assert_eq!(audits[0].at, now);
    assert!(
        audits[0].detail.contains("lifecycle_only"),
        "the detail says which kind of idle end this was: {}",
        audits[0].detail
    );
    assert!(
        audits[0].detail.contains(&watermark.to_string()),
        "the last activity survives in the audit detail: {}",
        audits[0].detail
    );
    let release_audits = store
        .reader
        .list_audit_events(AuditLogFilter {
            op: Some("release_lifecycle_only_handoff".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        release_audits.len(),
        1,
        "the handoff release keeps its own audit row, distinct from the auto-end"
    );
}

#[tokio::test]
async fn idle_end_lifecycle_only_loses_to_a_substantive_race() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 4 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::Stop,
            watermark,
            "turn finished",
        ),
    )
    .await;
    // Substantive work races in before the atomic end re-check.
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            now,
            "late real work",
        ),
    )
    .await;

    let results = store
        .writer
        .end_idle_sessions(vec![lifecycle_only(sid, ws, proj, now, watermark)], true)
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![IdleEndResult::Lost],
        "the atomic re-check must not downgrade substantive work to a boundary-only end"
    );
    let disposition = store
        .reader
        .session_end_disposition(sid, ws, proj, AgentKind::Codex)
        .await
        .unwrap();
    assert_eq!(disposition, SessionEndDisposition::Open);
}

#[tokio::test]
async fn idle_end_watermark_converges_a_late_real_session_end() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;
    store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::AlreadyEnded,
        "no new work since the watermark: the real end is a no-op AlreadyEnded"
    );

    // New work arrives: the real end re-ends against the old watermark.
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            now,
            "late real work",
        ),
    )
    .await;
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::ReEndWithNewWork
    );
    store.writer.end_session(sid, None).await.unwrap();
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::AlreadyEnded,
        "the watermark moved with the re-end"
    );
}

#[tokio::test]
async fn idle_end_batch_stamps_every_candidate_in_one_command() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();

    let mut candidates = Vec::new();
    for i in 0..25u32 {
        let watermark = now - (i as i64 + 1) * HOUR_US;
        let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
        insert(
            &store,
            observation(
                sid,
                ws,
                proj,
                ObservationKind::UserPrompt,
                watermark,
                &format!("prompt {i}"),
            ),
        )
        .await;
        let page = session_page(&store, ws, proj, sid).await;
        // The 13th candidate is already ended: it must report Lost in batch.
        if i == 12 {
            store.writer.end_session(sid, None).await.unwrap();
        }
        candidates.push(substantive(sid, ws, proj, page, 1, now, watermark));
    }
    let results = store
        .writer
        .end_idle_sessions(candidates, false)
        .await
        .unwrap();
    assert_eq!(results.len(), 25);
    let ended = results
        .iter()
        .filter(|r| matches!(r, IdleEndResult::EndedSubstantive))
        .count();
    let lost = results
        .iter()
        .filter(|r| matches!(r, IdleEndResult::Lost))
        .count();
    assert_eq!(ended, 24);
    assert_eq!(lost, 1);
    assert_eq!(results[12], IdleEndResult::Lost);
    // enqueue=false: no consolidation job was created by any of the ends.
    let claimed_now = jiff::Timestamp::now().as_microsecond();
    assert!(
        store
            .writer
            .claim_session_consolidation(claimed_now, claimed_now - 1)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn idle_end_candidate_cannot_cross_the_project_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;
    // A candidate that names the wrong project must lose the CAS even though
    // the session id is otherwise valid (the stamp never trusts a caller-
    // provided id alone).
    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, other, page, 1, now, watermark)],
            false,
        )
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::Lost]);
    let disposition = store
        .reader
        .session_end_disposition(sid, ws, proj, AgentKind::Codex)
        .await
        .unwrap();
    assert_eq!(disposition, SessionEndDisposition::Open);
    // Same-scope control: the identical candidate named at the session's
    // real project ends it — the refusal above was the scope guard, not the
    // session itself.
    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            false,
        )
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::EndedSubstantive]);
    let disposition = store
        .reader
        .session_end_disposition(sid, ws, proj, AgentKind::Codex)
        .await
        .unwrap();
    assert_eq!(disposition, SessionEndDisposition::AlreadyEnded);
}

#[tokio::test]
async fn idle_end_cas_rejects_a_stale_observation_count() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;
    // The candidate was built from a digest that saw exactly one
    // observation. A non-end observation lands after that read (the window
    // between the tick's read and the stamp): the count guard must refuse
    // the stamp, or the watermark would cover work the page never saw.
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            now,
            "late work the page never saw",
        ),
    )
    .await;
    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![IdleEndResult::Lost],
        "an observation after the digest read must lose the CAS"
    );
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert!(audits.is_empty(), "a lost end writes no audit row");
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::Open,
        "the session stays open: the next tick re-reads the digest and the count"
    );
}

#[tokio::test]
async fn idle_end_cas_rejects_a_moved_latest_page() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page_v1 = session_page(&store, ws, proj, sid).await;
    // A checkpoint (or the real SessionEnd) lands a newer page version
    // after the tick wrote v1. Stamping v1 would point summary_page_id at a
    // page that no longer represents the session's work: the latest-version
    // guard must refuse.
    let page_v2 = session_page_body(&store, ws, proj, sid, "checkpoint v2, new content").await;
    assert_ne!(page_v1, page_v2);
    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page_v1, 1, now, watermark)],
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![IdleEndResult::Lost],
        "a newer page version must lose the CAS"
    );
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert!(audits.is_empty(), "a lost end writes no audit row");
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::Open,
        "the session stays open for the next tick"
    );
}

#[tokio::test]
async fn idle_end_consolidation_enqueue_shares_the_callers_transaction() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    store.writer.end_session(sid, None).await.unwrap();

    // The idle stamp enqueues its consolidation job INSIDE the stamp's
    // transaction: the enqueue must live or die with the caller's commit.
    // Prove the rollback side — a failing caller statement discards the
    // enqueue with everything else in the transaction.
    let mut conn = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
    let tx = conn.transaction().unwrap();
    ai_memory_store::session_consolidation::enqueue_in_tx(&tx, ws, proj, sid).unwrap();
    // The job is visible to the caller's transaction...
    let visible: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM session_consolidation_jobs WHERE session_id = ?1",
            rusqlite::params![sid.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(visible, 1);
    // ...and a failing statement rolls the whole transaction back.
    let failed = tx.execute("INSERT INTO no_such_table VALUES (1)", []);
    assert!(failed.is_err(), "the caller's failure");
    drop(tx);
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_consolidation_jobs WHERE session_id = ?1",
            rusqlite::params![sid.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 0,
        "a failed caller command discards the enqueue: the session stays \
         enqueuable, the next tick retries"
    );
}

#[tokio::test]
async fn idle_end_pre_stamp_non_end_observation_in_window_loses_the_cas() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;

    // The test-only seam races a non-end observation (a `Stop` marker) into
    // the window between the tick's digest read and the stamp: the count
    // guard must lose the CAS, or the watermark would cover work the page
    // never saw. Enqueue is ON: a lost candidate must still create no job.
    let results = store
        .writer
        .end_idle_sessions_pre_stamp(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            true,
            Box::new(move |conn| {                conn.execute(
                    "INSERT INTO observations \
                     (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
                     VALUES (?1, ?2, ?3, ?4, 'stop', 'window stop', 'a non-end observation in the window', 5, ?5)",
                    rusqlite::params![
                        uuid::Uuid::new_v4().as_bytes(),
                        sid.as_bytes(),
                        ws.as_bytes(),
                        proj.as_bytes(),
                        now
                    ],
                )
                .unwrap();
            }),
        )
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::Lost]);
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert!(audits.is_empty(), "a lost end writes no audit row");
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::Open,
        "the session stays open for the next tick"
    );
    // Lost with consolidation enabled: the enqueue is skipped, not half-done.
    let claimed_now = jiff::Timestamp::now().as_microsecond();
    assert!(
        store
            .writer
            .claim_session_consolidation(claimed_now, claimed_now - 1)
            .await
            .unwrap()
            .is_none(),
        "a lost candidate with enqueue on creates no job"
    );
}

#[tokio::test]
async fn idle_end_pre_stamp_checkpoint_page_in_window_loses_the_cas() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page_v1 = session_page(&store, ws, proj, sid).await;

    // The seam races a checkpoint page version (a new latest row supersedes
    // v1) into the window: the latest-version guard must lose the CAS, or
    // summary_page_id would point at a page that no longer represents the
    // session's work.
    let results = store
        .writer
        .end_idle_sessions_pre_stamp(
            vec![substantive(sid, ws, proj, page_v1, 1, now, watermark)],
            false,
            Box::new(move |conn| {
                let body = "checkpoint v2 landing in the window";
                use sha2::Digest;
                let mut hasher = sha2::Sha256::default();
                hasher.update(body.as_bytes());
                let sha: [u8; 32] = hasher.finalize().into();
                conn.execute(
                    "UPDATE pages SET is_latest = 0 WHERE id = ?1",
                    rusqlite::params![page_v1.as_bytes()],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO pages \
                     (id, workspace_id, project_id, path, path_search, title, tier, body, \
                      body_sha256, frontmatter_json, is_latest, supersedes, pinned, author_id, \
                      created_at, updated_at, expires_at, valid_from, compacted_at) \
                     VALUES (?1, ?2, ?3, ?4, ?4, 'summary', 'episodic', ?5, ?6, '{}', 1, ?7, 0, NULL, ?8, ?8, NULL, ?8, NULL)",
                    rusqlite::params![
                        uuid::Uuid::new_v4().as_bytes(),
                        ws.as_bytes(),
                        proj.as_bytes(),
                        format!("sessions/{sid}.md"),
                        body,
                        sha,
                        page_v1.as_bytes(),
                        now
                    ],
                )
                .unwrap();
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![IdleEndResult::Lost],
        "a newer page version landing in the window must lose the stamp"
    );
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert!(audits.is_empty(), "a lost end writes no audit row");
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::Open,
        "the session stays open: the next tick re-reads and the checkpoint wins"
    );
}

#[tokio::test]
async fn idle_end_pre_stamp_real_session_end_in_window_loses_the_cas() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;
    let hook_end_us = now - HOUR_US / 2;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;

    // The seam stamps the session exactly the way a real SessionEnd's hook
    // path does (ended_at + the live observation count): the `ended_at IS
    // NULL` guard must lose the CAS and the hook's end keeps the row.
    let results = store
        .writer
        .end_idle_sessions_pre_stamp(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            true,
            Box::new(move |conn| {
                conn.execute(
                    "UPDATE sessions SET ended_at = ?1, \
                     ended_observation_count = (SELECT COUNT(*) FROM observations WHERE session_id = ?2) \
                     WHERE id = ?2",
                    rusqlite::params![hook_end_us, sid.as_bytes()],
                )
                .unwrap();
            }),
        )
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::Lost]);
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert!(audits.is_empty(), "a lost end writes no audit row");
    // The hook's end is untouched: no idle restamp, no idle-enqueue job.
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::AlreadyEnded
    );
    let claimed_now = jiff::Timestamp::now().as_microsecond();
    assert!(
        store
            .writer
            .claim_session_consolidation(claimed_now, claimed_now - 1)
            .await
            .unwrap()
            .is_none(),
        "the lost candidate never enqueues; the hook path owns the job"
    );
}

#[tokio::test]
async fn idle_end_writer_recheck_refuses_runs_linked_after_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let substantive_native = "native-substantive-race";
    let substantive_sid = SessionId::from_native(substantive_native);
    begin_with_id(&store, ws, proj, substantive_sid, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            substantive_sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, substantive_sid).await;

    let lifecycle_native = "native-lifecycle-race";
    let lifecycle_sid = SessionId::from_native(lifecycle_native);
    begin_with_id(&store, ws, proj, lifecycle_sid, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            lifecycle_sid,
            ws,
            proj,
            ObservationKind::Stop,
            watermark,
            "turn finished",
        ),
    )
    .await;

    let prepare = |name: &str| PrepareWorkstreamRun {
        scope_by_id: false,
        workspace_id: ws,
        project_id: proj,
        repo_fingerprint: "repo".into(),
        worktree_fingerprint: "worktree".into(),
        cwd: "/repo".into(),
        agent: AgentKind::Codex,
        automatic_harness: false,
        available_agents: Vec::new(),
        selection: WorkstreamSelection::New(name.into()),
        lease_owner: format!("launcher-{name}"),
    };
    let substantive_run = store
        .writer
        .prepare_workstream_run(prepare("substantive-race"))
        .await
        .unwrap();
    let lifecycle_run = store
        .writer
        .prepare_workstream_run(prepare("lifecycle-race"))
        .await
        .unwrap();

    // Both runs are active but unlinked when the reader would select these
    // sessions. Link each one in the last writer-side seam before its stamp.
    // The decisive recheck must reject both action arms.
    let mut links = std::collections::VecDeque::from([
        (substantive_run.run_id, substantive_native),
        (lifecycle_run.run_id, lifecycle_native),
    ]);
    let results = store
        .writer
        .end_idle_sessions_pre_stamp(
            vec![
                substantive(substantive_sid, ws, proj, page, 1, now, watermark),
                lifecycle_only(lifecycle_sid, ws, proj, now, watermark),
            ],
            true,
            Box::new(move |conn| {
                let (run_id, native_session_id) = links.pop_front().unwrap();
                conn.execute(
                    "UPDATE managed_runs SET native_session_id = ?1 \
                     WHERE id = ?2 AND state = 'active'",
                    rusqlite::params![native_session_id, run_id.as_bytes()],
                )
                .unwrap();
            }),
        )
        .await
        .unwrap();

    assert_eq!(results, vec![IdleEndResult::Lost, IdleEndResult::Lost]);
    for session_id in [substantive_sid, lifecycle_sid] {
        assert_eq!(
            store
                .reader
                .session_end_disposition(session_id, ws, proj, AgentKind::Codex)
                .await
                .unwrap(),
            SessionEndDisposition::Open,
            "an active managed run keeps its session open"
        );
    }
    assert!(
        store
            .reader
            .list_audit_events(idle_audit_filter())
            .await
            .unwrap()
            .is_empty(),
        "the refused stamps write no audit rows"
    );
    let claim_at = now + 1;
    assert!(
        store
            .writer
            .claim_session_consolidation(claim_at, claim_at - 1)
            .await
            .unwrap()
            .is_none(),
        "the refused substantive stamp enqueues no consolidation job"
    );
    assert!(
        store
            .reader
            .page_versions(
                ws,
                proj,
                PagePath::new(format!("sessions/{lifecycle_sid}.md"))
                    .unwrap()
                    .to_string(),
                10,
                0,
            )
            .await
            .unwrap()
            .versions
            .is_empty(),
        "the refused lifecycle-only stamp creates no page"
    );
}

#[tokio::test]
async fn idle_end_consolidation_enqueue_failure_rolls_back_and_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "idle prompt",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;

    // F6: the consolidation enqueue runs INSIDE the stamp's transaction.
    // A test-only trigger aborts the job INSERT: the whole command — stamp,
    // audit, enqueue — must roll back and the session stay open.
    let failed = store
        .writer
        .end_idle_sessions_pre_stamp(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            true,
            Box::new(|conn| {
                conn.execute(
                    "CREATE TRIGGER idle_f6_abort \
                     BEFORE INSERT ON session_consolidation_jobs \
                     BEGIN SELECT RAISE(ABORT, 'f6 test trigger'); END",
                    [],
                )
                .unwrap();
            }),
        )
        .await;
    assert!(
        failed.is_err(),
        "the aborted enqueue must fail the whole command"
    );
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::Open,
        "the stamp rolled back with the enqueue: the session stays open"
    );
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert!(
        audits.is_empty(),
        "the audit row rolled back with the enqueue: {}",
        audits
            .iter()
            .map(|e| e.op.clone())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let claimed_now = jiff::Timestamp::now().as_microsecond();
    assert!(
        store
            .writer
            .claim_session_consolidation(claimed_now, claimed_now - 1)
            .await
            .unwrap()
            .is_none(),
        "no half-enqueued job survives the rollback"
    );

    // The next pass (trigger removed) ends the session and enqueues.
    let results = store
        .writer
        .end_idle_sessions_pre_stamp(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            true,
            Box::new(|conn| {
                conn.execute("DROP TRIGGER idle_f6_abort", []).unwrap();
            }),
        )
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::EndedSubstantive]);
    // Fresh wall clock: the retried pass's enqueue used its own .
    let fresh = jiff::Timestamp::now().as_microsecond();
    let job = store
        .writer
        .claim_session_consolidation(fresh + 1_000, fresh)
        .await
        .unwrap()
        .expect("the retried pass enqueues its consolidation job");
    assert_eq!(job.session_id(), sid);
}

#[tokio::test]
async fn idle_end_lifecycle_only_refuses_a_foreign_scope_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 4 * HOUR_US;

    // Boundary-only session in `proj`, with a handoff it claimed.
    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::Stop,
            watermark,
            "turn finished",
        ),
    )
    .await;
    let handoff = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::Codex,
            to_agent: None,
            cwd: Some("/repo".into()),
            summary: "pick up the build".into(),
            open_questions: Vec::new(),
            next_steps: Vec::new(),
            files_touched: Vec::new(),
            owner_user: None,
        })
        .await
        .unwrap();
    assert!(
        store
            .writer
            .accept_handoff(HandoffAcceptance {
                handoff_id: handoff,
                workspace_id: ws,
                project_id: proj,
                accepting_agent: AgentKind::Codex,
                accepting_session: Some(sid),
                accepting_user: None,
                owner_filter: OwnerFilter::Any,
                receiving_cwd: Some("/repo".into()),
            })
            .await
            .unwrap()
    );

    // A lifecycle-only candidate naming the WRONG project must lose before
    // any mutation or handoff release — even though the session id is
    // otherwise valid (the end ops key on the id alone; the guard is what
    // makes scope matter).
    let results = store
        .writer
        .end_idle_sessions(vec![lifecycle_only(sid, ws, other, now, watermark)], true)
        .await
        .unwrap();
    assert_eq!(results, vec![IdleEndResult::Lost]);
    assert_ne!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::AlreadyEnded,
        "the foreign-scope candidate ends nothing"
    );
    // The claimed handoff is untouched: the release belongs to the real
    // scope's end, not a foreign candidate's.
    let still = store.reader.handoff_by_id(handoff).await.unwrap().unwrap();
    assert_eq!(
        still.lifecycle.state,
        HandoffState::Accepted,
        "the handoff stays claimed for its real owner"
    );
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert!(audits.is_empty(), "a lost end writes no audit row");
}

/// R36b-1: the CAS count guard must count the SAME population as the digest
/// and `session_observation_count` — every observation of the validated
/// session, whatever project each row landed in. A session that carried work
/// into another project must end, not spin as `Lost` forever.
#[tokio::test]
async fn idle_end_counts_the_same_observation_population_as_the_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other = store
        .writer
        .get_or_create_project(ws, "other-count", None)
        .await
        .unwrap();
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "home scope",
        ),
    )
    .await;
    // The session keeps its id when its work lands in another project:
    // the second observation carries the other project's coordinates.
    insert(
        &store,
        observation(
            sid,
            ws,
            other,
            ObservationKind::PostToolUse,
            watermark,
            "moved scope",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;
    // The production counter sees the whole session population...
    let count = store
        .reader
        .session_observation_count(ws, proj, sid)
        .await
        .unwrap();
    assert_eq!(
        count, 2,
        "the counter counts every row of the validated session"
    );
    // ...so the stamp must accept the candidate without any race.
    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page, count, now, watermark)],
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![IdleEndResult::EndedSubstantive],
        "digest/counter and CAS count the same population; nothing raced"
    );
    // The watermark covers BOTH observations, not only the in-scope row.
    let conn = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
    let ended_count: i64 = conn
        .query_row(
            "SELECT COALESCE(ended_observation_count, -1) FROM sessions WHERE id = ?1",
            rusqlite::params![sid.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        ended_count, 2,
        "the watermark counts the whole session population"
    );
}

/// The guard counts the whole session on purpose. A late row in another
/// project moves the same generation the watermark covers, so it loses the
/// stamp and the next tick reads the session again. Scoping the guard to the
/// digest's project would let the watermark pass over a row nobody compared.
#[tokio::test]
async fn idle_end_cas_counts_a_late_row_in_another_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other = store
        .writer
        .get_or_create_project(ws, "late-row", None)
        .await
        .unwrap();
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "home scope",
        ),
    )
    .await;
    let page = session_page(&store, ws, proj, sid).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            other,
            ObservationKind::PostToolUse,
            now,
            "late row in another project",
        ),
    )
    .await;
    let results = store
        .writer
        .end_idle_sessions(
            vec![substantive(sid, ws, proj, page, 1, now, watermark)],
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![IdleEndResult::Lost],
        "a row after the digest read loses the stamp, whatever project it landed in"
    );
    assert_eq!(
        store
            .reader
            .session_end_disposition(sid, ws, proj, AgentKind::Codex)
            .await
            .unwrap(),
        SessionEndDisposition::Open,
        "the session stays open for the next tick"
    );
}

/// R36b-3: the raw-id reader refuses foreign workspaces, foreign projects
/// and missing session ids with a zero count — and returns the count for
/// the right tuple (legitimate control in the same fixture).
#[tokio::test]
async fn session_observation_count_refuses_foreign_and_missing_scopes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other = store
        .writer
        .get_or_create_project(ws, "other-count", None)
        .await
        .unwrap();
    let foreign_ws = store
        .writer
        .get_or_create_workspace("foreign".to_string())
        .await
        .unwrap();
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    for body in ["first", "second"] {
        insert(
            &store,
            observation(sid, ws, proj, ObservationKind::UserPrompt, watermark, body),
        )
        .await;
    }
    let reader = store.reader.clone();
    // Legitimate control: the right tuple returns the count.
    assert_eq!(
        reader
            .session_observation_count(ws, proj, sid)
            .await
            .unwrap(),
        2
    );
    // Violations: a foreign project, a foreign workspace, a missing id —
    // each counts nothing the session does not own in that scope.
    assert_eq!(
        reader
            .session_observation_count(ws, other, sid)
            .await
            .unwrap(),
        0,
        "a foreign project counts no rows of the session"
    );
    assert_eq!(
        reader
            .session_observation_count(foreign_ws, proj, sid)
            .await
            .unwrap(),
        0,
        "a foreign workspace counts no rows of the session"
    );
    assert_eq!(
        reader
            .session_observation_count(ws, proj, SessionId::new())
            .await
            .unwrap(),
        0,
        "a missing id counts nothing"
    );
}

/// R36b-3: `enqueue_in_tx` refuses a foreign tuple — it enqueues nothing
/// and leaves the foreign project's queue exactly as it was — while the
/// right tuple enqueues (legitimate control in the same fixture).
#[tokio::test]
async fn enqueue_in_tx_refuses_a_foreign_scope_and_leaves_the_foreign_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other = store
        .writer
        .get_or_create_project(ws, "other-count", None)
        .await
        .unwrap();
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 5 * HOUR_US;

    // The session the foreign candidate will name: ended, with work.
    let sid = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid,
            ws,
            proj,
            ObservationKind::UserPrompt,
            watermark,
            "work to consolidate",
        ),
    )
    .await;
    // The foreign project's own ended session and its job: the baseline the
    // foreign attempt must not disturb.
    let sid_other = begin(&store, ws, other, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid_other,
            ws,
            other,
            ObservationKind::UserPrompt,
            watermark,
            "foreign project work",
        ),
    )
    .await;
    let mut conn = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
    {
        let tx = conn.transaction().unwrap();
        // Both sessions end inside the baseline transaction FIRST: the job
        // INSERT needs `ended_at IS NOT NULL`.
        for id in [sid, sid_other] {
            tx.execute(
                "UPDATE sessions SET ended_at = ?1, \
                 ended_observation_count = (SELECT COUNT(*) FROM observations WHERE session_id = ?2) \
                 WHERE id = ?2",
                rusqlite::params![now, id.as_bytes()],
            )
            .unwrap();
        }
        ai_memory_store::session_consolidation::enqueue_in_tx(&tx, ws, other, sid_other).unwrap();
        tx.commit().unwrap();
    }
    let queue_of = |conn: &rusqlite::Connection, ws: WorkspaceId, proj: ProjectId| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM session_consolidation_jobs \
                 WHERE workspace_id = ?1 AND project_id = ?2",
            rusqlite::params![ws.as_bytes(), proj.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    let jobs_for = |conn: &rusqlite::Connection, id: SessionId| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM session_consolidation_jobs WHERE session_id = ?1",
            rusqlite::params![id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    let baseline_other = queue_of(&conn, ws, other);
    assert_eq!(baseline_other, 1, "the foreign project's baseline job");

    // Violation: the session's real scope is `proj`, but the candidate
    // names `other`. Nothing may be enqueued anywhere.
    {
        let tx = conn.transaction().unwrap();
        let enqueued =
            ai_memory_store::session_consolidation::enqueue_in_tx(&tx, ws, other, sid).unwrap();
        assert!(!enqueued, "the foreign tuple enqueues nothing");
        tx.commit().unwrap();
    }
    assert_eq!(
        jobs_for(&conn, sid),
        0,
        "no job appeared for the session in any project"
    );
    assert_eq!(
        queue_of(&conn, ws, other),
        baseline_other,
        "the foreign project's queue is unchanged"
    );
    assert_eq!(
        queue_of(&conn, ws, proj),
        0,
        "the real scope got no job either"
    );

    // Legitimate control: the right tuple enqueues.
    {
        let tx = conn.transaction().unwrap();
        let enqueued =
            ai_memory_store::session_consolidation::enqueue_in_tx(&tx, ws, proj, sid).unwrap();
        assert!(enqueued, "the right tuple enqueues");
        tx.commit().unwrap();
    }
    assert_eq!(jobs_for(&conn, sid), 1);
}

/// R36b-3 (lifecycle-only arm): a dedicated already-ended case returns
/// `Lost` with no audit and no handoff release — next to its own
/// legitimate control in the same fixture, which ends and releases.
#[tokio::test]
async fn idle_end_lifecycle_only_refuses_an_already_ended_session_with_control() {
    async fn claimed_handoff(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        sid: SessionId,
    ) -> HandoffId {
        let handoff = store
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: None,
                from_agent: AgentKind::Codex,
                to_agent: None,
                cwd: Some("/repo".into()),
                summary: "pick up the build".into(),
                open_questions: Vec::new(),
                next_steps: Vec::new(),
                files_touched: Vec::new(),
                owner_user: None,
            })
            .await
            .unwrap();
        assert!(
            store
                .writer
                .accept_handoff(HandoffAcceptance {
                    handoff_id: handoff,
                    workspace_id: ws,
                    project_id: proj,
                    accepting_agent: AgentKind::Codex,
                    accepting_session: Some(sid),
                    accepting_user: None,
                    owner_filter: OwnerFilter::Any,
                    receiving_cwd: Some("/repo".into()),
                })
                .await
                .unwrap()
        );
        handoff
    }

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let now = jiff::Timestamp::now().as_microsecond();
    let watermark = now - 4 * HOUR_US;

    // A: boundary-only, already ended by the hook path, handoff claimed.
    let sid_a = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid_a,
            ws,
            proj,
            ObservationKind::Stop,
            watermark,
            "A's turn finished",
        ),
    )
    .await;
    let handoff_a = claimed_handoff(&store, ws, proj, sid_a).await;
    // B: the open control, the same shape.
    let sid_b = begin(&store, ws, proj, watermark - HOUR_US, None).await;
    insert(
        &store,
        observation(
            sid_b,
            ws,
            proj,
            ObservationKind::Stop,
            watermark,
            "B's turn finished",
        ),
    )
    .await;
    let handoff_b = claimed_handoff(&store, ws, proj, sid_b).await;

    // The hook path ended A first (not the tick): ended_at + watermark.
    {
        let conn = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
        conn.execute(
            "UPDATE sessions SET ended_at = ?1, \
             ended_observation_count = (SELECT COUNT(*) FROM observations WHERE session_id = ?2) \
             WHERE id = ?2",
            rusqlite::params![now, sid_a.as_bytes()],
        )
        .unwrap();
    }

    // One tick command: A is refused by the scope/open guard, B ends.
    let results = store
        .writer
        .end_idle_sessions(
            vec![
                lifecycle_only(sid_a, ws, proj, now, watermark),
                lifecycle_only(sid_b, ws, proj, now, watermark),
            ],
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![IdleEndResult::Lost, IdleEndResult::EndedLifecycleOnly],
        "the already-ended case loses; the control ends"
    );
    // A's claimed handoff is untouched: the release belongs to a live end.
    let handoff_a = store
        .reader
        .handoff_by_id(handoff_a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handoff_a.lifecycle.state,
        HandoffState::Accepted,
        "the already-ended session's handoff stays claimed"
    );
    // B's handoff was released by its legitimate end.
    let handoff_b = store
        .reader
        .handoff_by_id(handoff_b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        handoff_b.lifecycle.state,
        HandoffState::Open,
        "the control's handoff is released by its end"
    );
    // Exactly one idle_session_end audit row: the control's, with the
    // tick's clock and the lifecycle-only action. A wrote nothing.
    let audits = store
        .reader
        .list_audit_events(idle_audit_filter())
        .await
        .unwrap();
    assert_eq!(audits.len(), 1, "only the control end audits");
    assert_eq!(audits[0].at, now, "the tick's clock");
    assert!(
        audits[0].detail.contains("lifecycle_only"),
        "the detail says which kind of idle end this was: {}",
        audits[0].detail
    );
    assert!(
        audits[0].detail.contains(&sid_b.to_string()),
        "the audit row is the control's: {}",
        audits[0].detail
    );
}
