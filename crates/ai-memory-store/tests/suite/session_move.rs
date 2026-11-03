//! What follows a session when `move_session` re-stamps it into another
//! project: the ingest keys that make a spool redelivery idempotent, and the
//! profile evidence harvested from its prompts, which must answer to the
//! target project's profile governance rather than the source's.

use ai_memory_core::profile::{ProfileCandidateSource, ProfileGenerality};
use ai_memory_core::{
    AgentKind, NewObservation, NewSession, ObservationKind, OwnerFilter, ProjectId, Sanitized,
    Sanitizer, SessionId, WorkspaceId,
};
use ai_memory_store::{
    AccessMode, HookSessionAdmission, IngestObservationOutcome, NewProfileCandidate, PagesMode,
    ProfileHarvestMark, ProjectProfileFlags, Store,
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
