//! `recent_session_activity`: which sessions hold a scope's newest work.
//!
//! The list feeds the session-start pointer to a harness that was cut off
//! (a quota limit, a crash) without ending. Such a session never writes a
//! fresh start or a clean end, so the ordering must come from its captured
//! work, a session resumed after an earlier end must read as not ended, and
//! the owner filter must keep one operator's session ids from another. The
//! same rules pick the session `latest_interrupted_session_candidate` offers
//! as a startup recovery snapshot.

use ai_memory_core::{ActorContext, IdentityKey, OwnerFilter, ProjectId, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

const NOW: i64 = 1_700_000_000_000_000;

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = n;
    b
}

fn ws() -> WorkspaceId {
    WorkspaceId::from_slice(&id(1)).unwrap()
}

fn proj_a() -> ProjectId {
    ProjectId::from_slice(&id(2)).unwrap()
}

fn operator(name: &str) -> String {
    IdentityKey::User(name.into()).storage_key()
}

fn filter_for(name: &str) -> OwnerFilter {
    OwnerFilter::for_actor_context(&ActorContext {
        user: Some(name.into()),
        ..ActorContext::default()
    })
}

fn ts(us: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_microsecond(us).unwrap()
}

/// Sessions in `proj-a` unless noted:
/// - 30 `open-code`: started first and ended at `NOW + 100`, then resumed and
///   worked until `NOW + 500` with no new end (the quota cut-off shape);
/// - 31 `claude-code`: started later, worked at `NOW + 300`, ended cleanly at
///   `NOW + 310`;
/// - 32 `codex`: only a session start and a session end, no work;
/// - 33 `codex`, bob's: worked at `NOW + 900`;
/// - 34 `cursor` in `proj-b`: worked at `NOW + 1000`.
fn seed(db_path: &std::path::Path) {
    let conn = Connection::open(db_path).unwrap();
    let (ws, a, b) = (id(1), id(2), id(3));
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'w', ?2)",
        params![&ws[..], NOW],
    )
    .unwrap();
    for (pid, name, rp) in [(&a, "proj-a", "/w/a"), (&b, "proj-b", "/w/b")] {
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&pid[..], &ws[..], name, rp, NOW],
        )
        .unwrap();
    }
    let sessions = [
        (30, &a, "open-code", NOW, Some(NOW + 100), None),
        (31, &a, "claude-code", NOW + 200, Some(NOW + 310), None),
        (32, &a, "codex", NOW + 600, Some(NOW + 601), None),
        (33, &a, "codex", NOW + 800, None, Some(operator("bob"))),
        (34, &b, "cursor", NOW + 1000, None, None),
    ];
    for (n, proj, agent, started, ended, owner) in sessions {
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, actor_user) \
             VALUES (?1, ?2, ?3, ?4, '/w/a', ?5, ?6, ?7)",
            params![&id(n)[..], &ws[..], &proj[..], agent, started, ended, owner],
        )
        .unwrap();
    }
    let observations = [
        (40, 30, &a, "user-prompt", NOW + 1),
        (41, 30, &a, "session-end", NOW + 100),
        (42, 30, &a, "user-prompt", NOW + 450),
        (43, 30, &a, "post-tool-use", NOW + 500),
        (44, 31, &a, "user-prompt", NOW + 300),
        (45, 31, &a, "session-end", NOW + 310),
        (46, 32, &a, "session-start", NOW + 600),
        (47, 32, &a, "session-end", NOW + 601),
        (48, 33, &a, "user-prompt", NOW + 900),
        (49, 34, &b, "user-prompt", NOW + 1000),
    ];
    for (n, sid, proj, kind, at) in observations {
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, importance, \
              created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 't', 'b', 5, ?6)",
            params![&id(n)[..], &id(sid)[..], &ws[..], &proj[..], kind, at],
        )
        .unwrap();
    }
}

fn open_seeded() -> (tempfile::TempDir, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());
    (tmp, store)
}

fn ids(list: &[ai_memory_store::SessionActivity]) -> Vec<u8> {
    list.iter().map(|a| a.session_id.as_bytes()[15]).collect()
}

#[tokio::test]
async fn ordered_by_newest_work_with_a_resumed_session_read_as_not_ended() {
    let (_tmp, store) = open_seeded();
    let list = store
        .reader
        .recent_session_activity(ws(), proj_a(), OwnerFilter::Unattributed, ts(NOW), 10)
        .await
        .unwrap();
    // 31 started after 30, so a start-ordered listing puts it first; 30 holds
    // the newer work. 32 never worked and is absent; 33 is bob's; 34 is in
    // another project.
    assert_eq!(ids(&list), vec![30, 31]);
    let resumed = &list[0];
    assert_eq!(resumed.agent_kind, "open-code");
    assert_eq!(resumed.last_activity_us, NOW + 500);
    assert_eq!(resumed.last_activity_at, ts(NOW + 500).to_string());
    assert!(
        !resumed.ended,
        "an end before the resumed work does not cover it"
    );
    assert!(list[1].ended, "a clean end after the last work is ended");
}

#[tokio::test]
async fn the_owner_filter_hides_another_operators_sessions() {
    let (_tmp, store) = open_seeded();
    let read = |filter| {
        let store = &store;
        async move {
            ids(&store
                .reader
                .recent_session_activity(ws(), proj_a(), filter, ts(NOW), 10)
                .await
                .unwrap())
        }
    };
    assert_eq!(read(filter_for("alice")).await, vec![30, 31]);
    assert_eq!(read(filter_for("bob")).await, vec![33, 30, 31]);
    assert_eq!(read(OwnerFilter::Any).await, vec![33, 30, 31]);
}

#[tokio::test]
async fn the_window_and_limit_bound_the_list() {
    let (_tmp, store) = open_seeded();
    let since_cut = store
        .reader
        .recent_session_activity(ws(), proj_a(), OwnerFilter::Unattributed, ts(NOW + 400), 10)
        .await
        .unwrap();
    assert_eq!(ids(&since_cut), vec![30], "31's work predates the window");
    let limited = store
        .reader
        .recent_session_activity(ws(), proj_a(), OwnerFilter::Unattributed, ts(NOW), 1)
        .await
        .unwrap();
    assert_eq!(ids(&limited), vec![30]);
}

/// A handoff from the session written at or after its last work covers it
/// (a per-turn checkpoint baton, claimed or not); one written before does not.
#[tokio::test]
async fn a_handoff_from_the_session_after_its_last_work_covers_it() {
    let (_tmp, store) = open_seeded();
    let conn = Connection::open(store.db_path()).unwrap();
    for (n, sid, state, at) in [(60, 30, "accepted", NOW + 450), (61, 31, "open", NOW + 310)] {
        conn.execute(
            "INSERT INTO handoffs \
             (id, workspace_id, project_id, from_session_id, from_agent, summary, \
              open_questions, next_steps, files_touched, state, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'other', 's', '[]', '[]', '[]', ?5, ?6)",
            params![&id(n)[..], &id(1)[..], &id(2)[..], &id(sid)[..], state, at],
        )
        .unwrap();
    }
    let list = store
        .reader
        .recent_session_activity(ws(), proj_a(), OwnerFilter::Unattributed, ts(NOW), 10)
        .await
        .unwrap();
    let covered: Vec<(u8, bool)> = list
        .iter()
        .map(|a| (a.session_id.as_bytes()[15], a.handed_off))
        .collect();
    // 30's baton predates its last work at NOW + 500; 31's follows its work.
    assert_eq!(covered, vec![(30, false), (31, true)]);
}

/// The legacy end remains the writer's watermark; it must not make the
/// scoped read disagree with the activity feed after substantive work resumes.
#[tokio::test]
async fn resumed_summary_is_open_and_preserves_the_persisted_end() {
    let (_tmp, store) = open_seeded();
    let resumed_id = ai_memory_core::SessionId::from_slice(&id(30)).unwrap();
    let resumed = store
        .reader
        .session_summary_scoped(ws(), proj_a(), resumed_id, OwnerFilter::Unattributed)
        .await
        .unwrap()
        .unwrap();
    assert!(resumed.ended_at.is_none(), "new work reopens the read view");
    for n in [31, 32] {
        let closed = store
            .reader
            .session_summary_scoped(
                ws(),
                proj_a(),
                ai_memory_core::SessionId::from_slice(&id(n)).unwrap(),
                OwnerFilter::Unattributed,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(closed.ended_at.is_some(), "closed sessions stay closed");
    }
    let conn = Connection::open(store.db_path()).unwrap();
    let persisted: i64 = conn
        .query_row(
            "SELECT ended_at FROM sessions WHERE id = ?1",
            params![&id(30)[..]],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(persisted, NOW + 100, "the writer watermark is untouched");
}

#[tokio::test]
async fn completed_listing_excludes_resumed_work_and_keeps_closed_controls() {
    let (_tmp, store) = open_seeded();
    let completed = store
        .reader
        .sessions_for_scope(ws(), proj_a(), OwnerFilter::Unattributed, false, 10, 0)
        .await
        .unwrap();
    let listed: Vec<_> = completed
        .iter()
        .map(|s| s.session_id.as_bytes()[15])
        .collect();
    assert_eq!(listed, vec![32, 31]);
    let all = store
        .reader
        .sessions_for_scope(ws(), proj_a(), OwnerFilter::Unattributed, true, 10, 0)
        .await
        .unwrap();
    let listed: Vec<_> = all.iter().map(|s| s.session_id.as_bytes()[15]).collect();
    assert_eq!(listed, vec![32, 31, 30]);
}

#[tokio::test]
async fn resumed_summary_keeps_owner_and_scope_guards_with_legitimate_controls() {
    let (_tmp, store) = open_seeded();
    let bob = ai_memory_core::SessionId::from_slice(&id(33)).unwrap();
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), proj_a(), bob, filter_for("alice"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), proj_a(), bob, filter_for("bob"))
            .await
            .unwrap()
            .is_some()
    );
    let other = ProjectId::from_slice(&id(3)).unwrap();
    let resumed = ai_memory_core::SessionId::from_slice(&id(30)).unwrap();
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), other, resumed, OwnerFilter::Unattributed)
            .await
            .unwrap()
            .is_none()
    );
    let foreign = ai_memory_core::SessionId::from_slice(&id(34)).unwrap();
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), proj_a(), foreign, OwnerFilter::Unattributed)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), other, foreign, OwnerFilter::Unattributed)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn only_scoped_substantive_work_reopens_the_summary() {
    let (_tmp, store) = open_seeded();
    let conn = Connection::open(store.db_path()).unwrap();
    for (n, sid, project, kind) in [(70, 31, 3, "user-prompt"), (71, 32, 2, "session-start")] {
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 't', 'synthetic', 5, ?6)",
            params![
                &id(n)[..],
                &id(sid)[..],
                &id(1)[..],
                &id(project)[..],
                kind,
                NOW + 900
            ],
        )
        .unwrap();
    }
    for n in [31, 32] {
        let summary = store
            .reader
            .session_summary_scoped(
                ws(),
                proj_a(),
                ai_memory_core::SessionId::from_slice(&id(n)).unwrap(),
                OwnerFilter::Unattributed,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(summary.ended_at.is_some());
    }
    let other = store
        .reader
        .session_summary_scoped(
            ws(),
            ProjectId::from_slice(&id(3)).unwrap(),
            ai_memory_core::SessionId::from_slice(&id(31)).unwrap(),
            OwnerFilter::Unattributed,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        other.ended_at.is_none(),
        "new work opens only its own scope"
    );
}

#[tokio::test]
async fn a_later_persisted_end_closes_the_resumed_read_view() {
    let (_tmp, store) = open_seeded();
    let conn = Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
        params![NOW + 600, &id(30)[..]],
    )
    .unwrap();
    let completed = store
        .reader
        .sessions_for_scope(ws(), proj_a(), OwnerFilter::Unattributed, false, 10, 0)
        .await
        .unwrap();
    let listed: Vec<_> = completed
        .iter()
        .map(|s| s.session_id.as_bytes()[15])
        .collect();
    assert_eq!(listed, vec![32, 31, 30]);
    assert!(completed.iter().all(|s| s.ended_at.is_some()));
}

fn observe(conn: &Connection, n: u8, session: u8, project: u8, kind: &str, at: i64) {
    conn.execute(
        "INSERT INTO observations \
         (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 't', 'b', 5, ?6)",
        params![
            &id(n)[..],
            &id(session)[..],
            &id(1)[..],
            &id(project)[..],
            kind,
            at
        ],
    )
    .unwrap();
}

/// The recovery candidate in `proj-a`: its session byte and last activity.
async fn interrupted(store: &Store, filter: OwnerFilter) -> Option<(u8, String)> {
    store
        .reader
        .latest_interrupted_session_candidate(ws(), proj_a(), filter, None)
        .await
        .unwrap()
        .map(|candidate| {
            (
                candidate.session_id.as_bytes()[15],
                candidate.last_activity_at,
            )
        })
}

/// Recovery offers the newest session whose substantive work nothing covers.
/// Activity counts every event, eligibility only prompts and tool calls: the
/// open 35 holds the newest events but no work, 31 ended after its work, 32
/// never worked, 33 is bob's and 34 works in another project.
#[tokio::test]
async fn interrupted_candidate_is_the_newest_uncovered_substantive_work() {
    let (_tmp, store) = open_seeded();
    let conn = Connection::open(store.db_path()).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', '/w/a', ?4)",
        params![&id(35)[..], &id(1)[..], &id(2)[..], NOW + 700],
    )
    .unwrap();
    observe(&conn, 80, 35, 2, "session-start", NOW + 700);
    observe(&conn, 81, 35, 2, "stop", NOW + 710);
    observe(&conn, 82, 30, 2, "stop", NOW + 520);
    assert_eq!(
        interrupted(&store, OwnerFilter::Unattributed).await,
        Some((30, ts(NOW + 520).to_string()))
    );
    for filter in [filter_for("bob"), OwnerFilter::Any] {
        assert_eq!(interrupted(&store, filter).await.map(|c| c.0), Some(33));
    }

    // An end after 30's newest work covers it; the stop that follows is not work.
    conn.execute(
        "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
        params![NOW + 510, &id(30)[..]],
    )
    .unwrap();
    assert_eq!(interrupted(&store, OwnerFilter::Unattributed).await, None);
}

/// A handoff from the session written at or after its newest work covers it,
/// even when lifecycle events follow; one written before does not.
#[tokio::test]
async fn a_handoff_at_the_newest_work_covers_the_interrupted_candidate() {
    let (_tmp, store) = open_seeded();
    let conn = Connection::open(store.db_path()).unwrap();
    observe(&conn, 82, 30, 2, "stop", NOW + 520);
    let baton = |n: u8, at: i64| {
        conn.execute(
            "INSERT INTO handoffs \
             (id, workspace_id, project_id, from_session_id, from_agent, summary, \
              open_questions, next_steps, files_touched, state, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'other', 's', '[]', '[]', '[]', 'accepted', ?5)",
            params![&id(n)[..], &id(1)[..], &id(2)[..], &id(30)[..], at],
        )
        .unwrap();
    };
    baton(60, NOW + 499);
    assert_eq!(
        interrupted(&store, OwnerFilter::Unattributed)
            .await
            .map(|c| c.0),
        Some(30),
        "a baton before the newest work leaves it uncovered"
    );
    baton(61, NOW + 500);
    assert_eq!(interrupted(&store, OwnerFilter::Unattributed).await, None);
}
