//! What follows a session when `move_session` re-stamps it into another
//! project: the ingest keys that make a spool redelivery idempotent, the
//! profile evidence harvested from its prompts (which must answer to the
//! target project's profile governance rather than the source's), and the
//! moved session page's `page_feedback`, `query_page_signals`, and
//! `entity_page_links`.

use ai_memory_core::profile::{ProfileCandidateSource, ProfileGenerality};
use ai_memory_core::{
    AgentKind, FeedbackKind, NewObservation, NewPage, NewSession, ObservationKind, OwnerFilter,
    PagePath, ProjectId, Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_store::{
    AccessMode, Compaction, DecayParams, HookSessionAdmission, IngestObservationOutcome,
    NewProfileCandidate, PagesMode, ProfileHarvestMark, ProjectProfileFlags, PurgeMode, Store,
};
use rusqlite::{Connection, params};

async fn project(store: &Store, ws: WorkspaceId, name: &str) -> ProjectId {
    store
        .writer
        .get_or_create_project(ws, name.to_owned(), None)
        .await
        .unwrap()
}

fn hook_session(id: SessionId, ws: WorkspaceId, proj: ProjectId) -> NewSession {
    NewSession {
        occurred_at: None,
        id,
        workspace_id: ws,
        project_id: proj,
        agent_kind: AgentKind::Codex,
        cwd: Some("/repo/app".into()),
        actor_user: None,
    }
}

fn observation(session: &NewSession, kind: ObservationKind, body: &str) -> NewObservation {
    NewObservation {
        occurred_at: Some(1_700_000_000_000_000),
        session_id: session.id,
        workspace_id: session.workspace_id,
        project_id: session.project_id,
        kind,
        extension: None,
        source_event: None,
        title: kind.as_str().into(),
        body: body.into(),
        importance: 5,
        file_path: None,
    }
}

fn sanitized(obs: NewObservation) -> Sanitized<NewObservation> {
    Sanitized::new(obs, &Sanitizer::builtin())
}

fn session_end_count(store: &Store, sid: SessionId) -> i64 {
    let conn = Connection::open(store.db_path()).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM observations WHERE session_id = ?1 AND kind = ?2",
        params![sid.as_bytes(), ObservationKind::SessionEnd.as_str()],
        |r| r.get(0),
    )
    .unwrap()
}

/// A keyed recovery SessionEnd that already completed must stay complete
/// after its session moves. The spool redelivers the same envelope (still
/// naming the source project); admission rescopes it to the session row's new
/// project and looks the key up there. Before #1265 the key stayed behind in
/// the source project, the lookup missed, and the replay appended a second
/// SessionEnd to the moved session.
#[tokio::test]
async fn a_replayed_recovery_session_end_stays_complete_after_its_session_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let source = project(&store, ws, "source").await;
    let target = project(&store, ws, "target").await;

    let sid = SessionId::new();
    let session = hook_session(sid, ws, source);
    store.writer.begin_session(session.clone()).await.unwrap();
    store
        .writer
        .insert_observation(sanitized(observation(
            &session,
            ObservationKind::UserPrompt,
            "fix the parser",
        )))
        .await
        .unwrap();

    let key = format!("recovery_{}", "b".repeat(55));
    let end = observation(&session, ObservationKind::SessionEnd, "session ended");
    let admitted = match store
        .writer
        .admit_hook_session_event(
            session.clone(),
            sanitized(end.clone()),
            OwnerFilter::Any,
            Some(key.clone()),
            None,
        )
        .await
        .unwrap()
    {
        HookSessionAdmission::EndOpen {
            session,
            ingest: IngestObservationOutcome::Inserted(_),
        } => session,
        other => panic!("the first recovery SessionEnd must be admitted: {other:?}"),
    };
    store
        .writer
        .end_admitted_session(admitted, None, None)
        .await
        .unwrap();
    store
        .writer
        .complete_observation_ingest(source, key.clone())
        .await
        .unwrap();
    // A late delivery after the end, so a redelivered SessionEnd is not
    // short-circuited as `AlreadyEnded` and must be judged by its key.
    store
        .writer
        .admit_hook_session_event(
            session.clone(),
            sanitized(observation(
                &session,
                ObservationKind::PostToolUse,
                "late tool event",
            )),
            OwnerFilter::Any,
            None,
            None,
        )
        .await
        .unwrap();

    store
        .writer
        .move_session(sid, ws, target, PagesMode::Move, None, true)
        .await
        .unwrap();

    let replay = store
        .writer
        .admit_hook_session_event(session, sanitized(end), OwnerFilter::Any, Some(key), None)
        .await
        .unwrap();
    assert!(
        matches!(
            replay,
            HookSessionAdmission::ReEnd {
                ingest: IngestObservationOutcome::AlreadyComplete,
                ..
            }
        ),
        "the replayed recovery SessionEnd must be recognised as complete: {replay:?}"
    );
    assert_eq!(
        session_end_count(&store, sid),
        1,
        "a replay must not append a second SessionEnd"
    );
}

/// Adversarial (security-boundaries row 1h): profile evidence harvested from
/// a session follows the session, so moving it into a project that refuses to
/// contribute takes the candidate out of convergence, and moving it into a
/// restricted project marks it restricted so a team profile drops it. Control:
/// the same move into an open, contributing project keeps it counted, now
/// attributed to the target. Before #1265 the candidate stayed stamped with
/// the open source project and kept counting from there.
#[tokio::test]
async fn a_moved_sessions_profile_candidate_follows_the_target_projects_governance() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let source = project(&store, ws, "open-source").await;
    let opted_out = project(&store, ws, "client-work").await;
    store
        .writer
        .set_project_profile_flags(
            opted_out,
            ProjectProfileFlags {
                contribute: false,
                consume: true,
            },
        )
        .await
        .unwrap();
    let restricted = project(&store, ws, "private-repo").await;
    store
        .writer
        .set_access_mode(restricted, AccessMode::Restricted)
        .await
        .unwrap();
    let open = project(&store, ws, "open-target").await;

    let mut sessions = Vec::new();
    for statement in ["opted-out habit", "restricted habit", "open habit"] {
        let sid = SessionId::new();
        store
            .writer
            .begin_session(hook_session(sid, ws, source))
            .await
            .unwrap();
        store
            .writer
            .record_profile_harvest(
                ws,
                source,
                vec![NewProfileCandidate {
                    source: ProfileCandidateSource::Prompt,
                    source_ref: format!("session:{sid}"),
                    topic_key: statement.into(),
                    category: "workflow".into(),
                    statement: statement.into(),
                    quote: statement.into(),
                    applies_to: Vec::new(),
                    observed_at: 1,
                    contributor: None,
                    generality: ProfileGenerality::General,
                    confidence: 0.9,
                }],
                ProfileHarvestMark::default(),
            )
            .await
            .unwrap();
        sessions.push((sid, statement));
    }
    let counted = |rows: &[ai_memory_store::ProfileCandidateRow], statement: &str| {
        rows.iter()
            .find(|row| row.candidate.statement == statement)
            .map(|row| (row.project.clone(), row.restricted))
    };
    let before = store.reader.profile_candidates(100).await.unwrap();
    for (_, statement) in &sessions {
        assert_eq!(
            counted(&before, statement),
            Some(("open-source".to_owned(), false)),
            "every candidate counts from the open source project before the move"
        );
    }

    for ((sid, _), dest) in sessions.iter().zip([opted_out, restricted, open]) {
        store
            .writer
            .move_session(*sid, ws, dest, PagesMode::Move, None, true)
            .await
            .unwrap();
    }
    let after = store.reader.profile_candidates(100).await.unwrap();
    assert_eq!(
        counted(&after, "opted-out habit"),
        None,
        "a candidate moved into a non-contributing project must not count"
    );
    assert_eq!(
        counted(&after, "restricted habit"),
        Some(("private-repo".to_owned(), true)),
        "a candidate moved into a restricted project must carry its restriction"
    );
    assert_eq!(
        counted(&after, "open habit"),
        Some(("open-target".to_owned(), false)),
        "control: a candidate moved into an open project still counts, from there"
    );
}

/// #1843: the file path index follows its observation when the session
/// moves. A path query in the source project finds the session before the
/// move and nothing after; the target finds it after (control).
#[tokio::test]
async fn a_moved_session_takes_its_file_paths_along() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let source = project(&store, ws, "source").await;
    let target = project(&store, ws, "target").await;
    let sid = SessionId::new();
    let session = hook_session(sid, ws, source);
    store.writer.begin_session(session.clone()).await.unwrap();
    let mut edit = observation(&session, ObservationKind::PostToolUse, "tool_family: file");
    edit.file_path = Some("src/lib.rs".into());
    store
        .writer
        .insert_observation(sanitized(edit))
        .await
        .unwrap();
    let sessions = |proj| {
        let reader = store.reader.clone();
        async move {
            reader
                .observations_for_file(ws, proj, OwnerFilter::Any, "src/lib.rs".into(), 10)
                .await
                .unwrap()
                .len()
        }
    };
    assert_eq!(sessions(source).await, 1);

    store
        .writer
        .move_session(sid, ws, target, PagesMode::Move, None, true)
        .await
        .unwrap();

    assert_eq!(sessions(source).await, 0);
    assert_eq!(sessions(target).await, 1);
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str, entities: &[&str]) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: entities.iter().map(|name| (*name).to_owned()).collect(),
        evidence: Vec::new(),
    }
}

/// #2254: `page_feedback` moves with `sessions/<id>.md` under `PagesMode::Move`
/// so source lint findings stop reporting the moved page, target findings
/// and flags retain it, and purging the source project does not cascade it away.
#[tokio::test]
async fn a_moved_session_page_takes_feedback_and_survives_source_purge() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let source = project(&store, ws, "source").await;
    let target = project(&store, ws, "target").await;

    let sid = SessionId::new();
    store
        .writer
        .begin_session(hook_session(sid, ws, source))
        .await
        .unwrap();
    let page_path = format!("sessions/{sid}.md");
    let page_id = store
        .writer
        .upsert_page(page(ws, source, &page_path, "session summary", &[]))
        .await
        .unwrap();
    store
        .writer
        .record_page_feedback(
            ws,
            source,
            PagePath::new(page_path.as_str()).unwrap(),
            FeedbackKind::Stale,
            Some("needs refresh".to_owned()),
            None,
            DecayParams::default(),
        )
        .await
        .unwrap();

    let source_before = store
        .reader
        .open_feedback_findings(ws, source)
        .await
        .unwrap();
    assert_eq!(source_before.len(), 1);
    assert_eq!(source_before[0].path, page_path);
    assert!(
        store
            .reader
            .open_feedback_findings(ws, target)
            .await
            .unwrap()
            .is_empty()
    );

    // Dry run must leave `page_feedback` untouched.
    store
        .writer
        .move_session(sid, ws, target, PagesMode::Move, None, false)
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .open_feedback_findings(ws, source)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .reader
            .open_feedback_findings(ws, target)
            .await
            .unwrap()
            .is_empty()
    );

    store
        .writer
        .move_session(sid, ws, target, PagesMode::Move, None, true)
        .await
        .unwrap();

    assert!(
        store
            .reader
            .open_feedback_findings(ws, source)
            .await
            .unwrap()
            .is_empty(),
        "source lint must not report a session page moved to target"
    );
    assert!(
        store
            .reader
            .feedback_flagged_page_ids(ws, source, vec![page_id])
            .await
            .unwrap()
            .is_empty()
    );

    let target_after = store
        .reader
        .open_feedback_findings(ws, target)
        .await
        .unwrap();
    assert_eq!(target_after.len(), 1);
    assert_eq!(target_after[0].path, page_path);
    assert_eq!(target_after[0].kind, "stale");
    assert_eq!(target_after[0].reason.as_deref(), Some("needs refresh"));
    assert!(
        store
            .reader
            .feedback_flagged_page_ids(ws, target, vec![page_id])
            .await
            .unwrap()
            .contains(&page_id)
    );

    // Purging the old project must not cascade-delete moved feedback.
    store
        .writer
        .purge_project(
            ws,
            source,
            "source",
            None,
            true,
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .reader
            .open_feedback_findings(ws, target)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .reader
            .feedback_flagged_page_ids(ws, target, vec![page_id])
            .await
            .unwrap()
            .contains(&page_id)
    );
}

/// #2254: `query_page_signals` and `entity_page_links` follow moved session
/// page versions into the destination project, reuse matching destination
/// entities, preserve link validity windows, and prune orphaned source entities
/// while keeping shared source entities intact.
#[tokio::test]
async fn a_moved_session_page_takes_query_signals_and_entity_links() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let source = project(&store, ws, "source").await;
    let target = project(&store, ws, "target").await;

    let other_id = store
        .writer
        .upsert_page(page(
            ws,
            source,
            "notes/other.md",
            "other source page",
            &["shared-noun"],
        ))
        .await
        .unwrap();
    let existing_id = store
        .writer
        .upsert_page(page(
            ws,
            target,
            "notes/existing.md",
            "existing target page",
            &["target-noun"],
        ))
        .await
        .unwrap();

    let sid = SessionId::new();
    store
        .writer
        .begin_session(hook_session(sid, ws, source))
        .await
        .unwrap();
    let page_path = format!("sessions/{sid}.md");
    let v1_id = store
        .writer
        .upsert_page(page(
            ws,
            source,
            &page_path,
            "session summary v1",
            &["only-moved", "shared-noun"],
        ))
        .await
        .unwrap();
    let v2_id = store
        .writer
        .upsert_page(page(
            ws,
            source,
            &page_path,
            "session summary v2",
            &["only-moved", "shared-noun", "target-noun"],
        ))
        .await
        .unwrap();

    let v1_window: (Option<i64>, Option<i64>) = {
        let conn = Connection::open(store.db_path()).unwrap();
        conn.query_row(
            "SELECT l.valid_from, l.superseded_at \
             FROM entity_page_links l \
             JOIN entities e ON e.id = l.entity_id \
             WHERE l.page_id = ?1 AND e.name = 'only-moved'",
            params![v1_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert!(v1_window.0.is_some());
    assert!(v1_window.1.is_some());

    store
        .writer
        .record_query_page_signal(
            ws,
            source,
            v2_id,
            "postgres migration".to_owned(),
            Some(sid.to_string()),
        )
        .await
        .unwrap();

    // Dry run must leave signals and entities untouched.
    store
        .writer
        .move_session(sid, ws, target, PagesMode::Move, None, false)
        .await
        .unwrap();
    assert_eq!(
        store
            .reader
            .query_signals_for_pages(ws, source, vec![v2_id])
            .await
            .unwrap()
            .get(&v2_id)
            .map(Vec::len),
        Some(1)
    );
    assert!(
        store
            .reader
            .entity_hits_for_project_at(ws, target, "only-moved", 10, None, None, false)
            .await
            .unwrap()
            .is_empty()
    );

    store
        .writer
        .move_session(sid, ws, target, PagesMode::Move, None, true)
        .await
        .unwrap();

    assert!(
        store
            .reader
            .query_signals_for_pages(ws, source, vec![v2_id])
            .await
            .unwrap()
            .is_empty()
    );
    let target_signals = store
        .reader
        .query_signals_for_pages(ws, target, vec![v2_id])
        .await
        .unwrap();
    let v2_signals = target_signals.get(&v2_id).unwrap();
    assert_eq!(v2_signals.len(), 1);
    assert_eq!(v2_signals[0].query_text, "postgres migration");

    let target_only_moved = store
        .reader
        .entity_hits_for_project_at(ws, target, "only-moved", 10, None, None, false)
        .await
        .unwrap();
    assert_eq!(target_only_moved.len(), 1);
    assert_eq!(target_only_moved[0].hit.id, v2_id);

    let target_all_versions = store
        .reader
        .entity_hits_for_project_at(ws, target, "only-moved", 10, None, None, true)
        .await
        .unwrap();
    let all_ids: Vec<_> = target_all_versions.iter().map(|hit| hit.hit.id).collect();
    assert_eq!(all_ids.len(), 2);
    assert!(all_ids.contains(&v1_id));
    assert!(all_ids.contains(&v2_id));

    let target_shared = store
        .reader
        .entity_hits_for_project_at(ws, target, "shared-noun", 10, None, None, false)
        .await
        .unwrap();
    assert_eq!(target_shared.len(), 1);
    assert_eq!(target_shared[0].hit.id, v2_id);

    let target_reused = store
        .reader
        .entity_hits_for_project_at(ws, target, "target-noun", 10, None, None, false)
        .await
        .unwrap();
    let reused_ids: Vec<_> = target_reused.iter().map(|hit| hit.hit.id).collect();
    assert_eq!(reused_ids.len(), 2);
    assert!(reused_ids.contains(&existing_id));
    assert!(reused_ids.contains(&v2_id));

    let source_shared = store
        .reader
        .entity_hits_for_project_at(ws, source, "shared-noun", 10, None, None, false)
        .await
        .unwrap();
    assert_eq!(source_shared.len(), 1);
    assert_eq!(source_shared[0].hit.id, other_id);
    assert!(
        (source_shared[0].weight - 1.0).abs() < 1e-9,
        "source entity frequency must no longer count the moved session page: {:?}",
        source_shared
    );
    assert!(
        store
            .reader
            .entity_hits_for_project_at(ws, source, "only-moved", 10, None, None, true)
            .await
            .unwrap()
            .is_empty()
    );

    {
        let conn = Connection::open(store.db_path()).unwrap();
        let orphaned_only_moved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entities \
                 WHERE workspace_id = ?1 AND project_id = ?2 AND name = 'only-moved'",
                params![ws.as_bytes(), source.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            orphaned_only_moved, 0,
            "source entity with no remaining links must be pruned"
        );
        let scope_mismatches: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entity_page_links l \
                 JOIN entities e ON e.id = l.entity_id \
                 JOIN pages p ON p.id = l.page_id \
                 WHERE e.workspace_id != p.workspace_id OR e.project_id != p.project_id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(scope_mismatches, 0);
        let migrated_v1_window: (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT l.valid_from, l.superseded_at \
                 FROM entity_page_links l \
                 JOIN entities e ON e.id = l.entity_id \
                 WHERE l.page_id = ?1 AND e.name = 'only-moved'",
                params![v1_id.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(migrated_v1_window, v1_window);
    }

    // Purging the old project must keep target signals and entities intact.
    store
        .writer
        .purge_project(
            ws,
            source,
            "source",
            None,
            true,
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .reader
            .query_signals_for_pages(ws, target, vec![v2_id])
            .await
            .unwrap()
            .get(&v2_id)
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        store
            .reader
            .entity_hits_for_project_at(ws, target, "only-moved", 10, None, None, false)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Control (#2254): `PagesMode::Regenerate` retires source page versions in
/// place, so their historical feedback and query signals stay attached to the
/// source scope rather than migrating into the target project.
#[tokio::test]
async fn regenerate_mode_keeps_retired_feedback_and_signals_in_source() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let source = project(&store, ws, "source").await;
    let target = project(&store, ws, "target").await;

    let sid = SessionId::new();
    store
        .writer
        .begin_session(hook_session(sid, ws, source))
        .await
        .unwrap();
    let page_path = format!("sessions/{sid}.md");
    let page_id = store
        .writer
        .upsert_page(page(ws, source, &page_path, "session summary", &[]))
        .await
        .unwrap();
    store
        .writer
        .record_page_feedback(
            ws,
            source,
            PagePath::new(page_path.as_str()).unwrap(),
            FeedbackKind::Wrong,
            Some("wrong summary".to_owned()),
            None,
            DecayParams::default(),
        )
        .await
        .unwrap();
    store
        .writer
        .record_query_page_signal(
            ws,
            source,
            page_id,
            "wrong summary query".to_owned(),
            Some(sid.to_string()),
        )
        .await
        .unwrap();

    store
        .writer
        .move_session(sid, ws, target, PagesMode::Regenerate, None, true)
        .await
        .unwrap();

    assert!(
        store
            .reader
            .open_feedback_findings(ws, source)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .reader
            .open_feedback_findings(ws, target)
            .await
            .unwrap()
            .is_empty()
    );

    let conn = Connection::open(store.db_path()).unwrap();
    let source_feedback: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM page_feedback \
             WHERE workspace_id = ?1 AND project_id = ?2 AND page_id = ?3",
            params![ws.as_bytes(), source.as_bytes(), page_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    let source_signals: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM query_page_signals \
             WHERE workspace_id = ?1 AND project_id = ?2 AND page_id = ?3",
            params![ws.as_bytes(), source.as_bytes(), page_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source_feedback, 1);
    assert_eq!(source_signals, 1);
}
