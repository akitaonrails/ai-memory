//! Scoped per-session observation reads and the session listing behind them
//! (`session_observations_scoped`, `sessions_for_scope`,
//! `session_summary_scoped`).
//!
//! A session that crossed repositories mid-flight has observations in two
//! scopes while its `sessions` row stays frozen on the first one. A caller
//! resolved to one scope must see exactly that scope's rows, learn that more
//! exist elsewhere, and still find the session in the listing of the scope
//! that holds its work.

use ai_memory_core::{
    ActorContext, IdentityKey, ObservationKind, OwnerFilter, ProjectId, SessionId, WorkspaceId,
};
use ai_memory_store::{ObservationOrder, ObservationPage, Store};
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

fn proj_b() -> ProjectId {
    ProjectId::from_slice(&id(3)).unwrap()
}

fn session(n: u8) -> SessionId {
    SessionId::from_slice(&id(n)).unwrap()
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

fn page(limit: usize) -> ObservationPage {
    ObservationPage {
        limit,
        offset: 0,
        order: ObservationOrder::Asc,
        kinds: None,
        query: None,
    }
}

/// Session ids: 10 crossed from `proj-a` into `proj-b`; 11 is open in
/// `proj-a`; 12 is alice's, anchored in `proj-b` but with one observation in
/// `proj-a`; 13 is bob's, anchored in `proj-a`, with no observations at all.
/// Observation ids: 20..=22 in `proj-a` and 23 in `proj-b` for session 10; 24
/// for session 11; 25 (`proj-a`) and 26 (`proj-b`) for session 12.
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
        (10, &a, "claude-code", "/w/a", NOW, Some(NOW + 100), None),
        (11, &a, "codex", "/w/a", NOW + 10, None, None),
        (
            12,
            &b,
            "cursor",
            "/w/b",
            NOW + 20,
            Some(NOW + 30),
            Some(operator("alice")),
        ),
        (
            13,
            &a,
            "claude-code",
            "/w/a",
            NOW + 30,
            Some(NOW + 40),
            Some(operator("bob")),
        ),
    ];
    for (n, proj, agent, cwd, started, ended, owner) in sessions {
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, actor_user) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &id(n)[..],
                &ws[..],
                &proj[..],
                agent,
                cwd,
                started,
                ended,
                owner
            ],
        )
        .unwrap();
    }
    let observations = [
        (
            20,
            10,
            &a,
            "user-prompt",
            "alpha prompt",
            "deploy the widget",
            NOW + 1,
        ),
        (
            21,
            10,
            &a,
            "post-tool-use",
            "tool result",
            "cargo build ok",
            NOW + 2,
        ),
        (
            22,
            10,
            &a,
            "user-prompt",
            "beta prompt",
            "widget colors",
            NOW + 3,
        ),
        (
            23,
            10,
            &b,
            "user-prompt",
            "crossed",
            "widget elsewhere",
            NOW + 4,
        ),
        (
            24,
            11,
            &a,
            "user-prompt",
            "open work",
            "still running",
            NOW + 11,
        ),
        (
            25,
            12,
            &a,
            "post-tool-use",
            "alice tool",
            "touched proj-a",
            NOW + 21,
        ),
        (
            26,
            12,
            &b,
            "user-prompt",
            "alice prompt",
            "home scope",
            NOW + 22,
        ),
    ];
    for (n, sid, proj, kind, title, body, ts) in observations {
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, importance, \
              created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 7, ?8)",
            params![
                &id(n)[..],
                &id(sid)[..],
                &ws[..],
                &proj[..],
                kind,
                title,
                body,
                ts
            ],
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

fn last_byte(sid: SessionId) -> u8 {
    sid.as_bytes()[15]
}

#[tokio::test]
async fn page_reads_only_the_callers_scope_and_reports_the_rest() {
    let (_tmp, store) = open_seeded();

    let from_a = store
        .reader
        .session_observations_scoped(ws(), proj_a(), session(10), page(50))
        .await
        .unwrap();
    assert_eq!(from_a.total, 3);
    assert_eq!(from_a.elided_other_scope, 1, "one row landed in proj-b");
    let ids: Vec<u8> = from_a.records.iter().map(|r| r.id.as_bytes()[15]).collect();
    assert_eq!(ids, vec![20, 21, 22], "capture order by default");
    let first = &from_a.records[0];
    assert_eq!(first.session_id, session(10));
    assert_eq!(first.kind, "user-prompt");
    assert_eq!(first.title, "alpha prompt");
    assert_eq!(first.body, "deploy the widget", "the full body is returned");
    assert_eq!(first.importance, 7);
    assert_eq!(first.extension, None);
    assert_eq!(first.source_event, None);
    assert_eq!(
        first.created_at,
        jiff::Timestamp::from_microsecond(NOW + 1)
            .unwrap()
            .to_string()
    );

    // The same session read from the other scope shows the mirror image.
    let from_b = store
        .reader
        .session_observations_scoped(ws(), proj_b(), session(10), page(50))
        .await
        .unwrap();
    assert_eq!(from_b.total, 1);
    assert_eq!(from_b.elided_other_scope, 3);
    assert_eq!(from_b.records[0].id.as_bytes()[15], 23);
}

#[tokio::test]
async fn limit_offset_and_order_shape_the_page_without_changing_the_counts() {
    let (_tmp, store) = open_seeded();

    let desc = store
        .reader
        .session_observations_scoped(
            ws(),
            proj_a(),
            session(10),
            ObservationPage {
                limit: 2,
                offset: 1,
                order: ObservationOrder::Desc,
                kinds: None,
                query: None,
            },
        )
        .await
        .unwrap();
    let ids: Vec<u8> = desc.records.iter().map(|r| r.id.as_bytes()[15]).collect();
    assert_eq!(ids, vec![21, 20], "newest first, skipping the newest one");
    assert_eq!(desc.total, 3, "total ignores the page window");
    assert_eq!(desc.elided_other_scope, 1);

    let none = store
        .reader
        .session_observations_scoped(ws(), proj_a(), session(10), page(0))
        .await
        .unwrap();
    assert!(none.records.is_empty(), "limit 0 returns no rows");
    assert_eq!(none.total, 3, "but still counts");
    assert_eq!(none.elided_other_scope, 1);
}

#[tokio::test]
async fn kinds_filter_narrows_rows_and_total() {
    let (_tmp, store) = open_seeded();

    let prompts = store
        .reader
        .session_observations_scoped(
            ws(),
            proj_a(),
            session(10),
            ObservationPage {
                kinds: Some(vec![ObservationKind::UserPrompt]),
                ..page(50)
            },
        )
        .await
        .unwrap();
    let ids: Vec<u8> = prompts
        .records
        .iter()
        .map(|r| r.id.as_bytes()[15])
        .collect();
    assert_eq!(ids, vec![20, 22]);
    assert_eq!(prompts.total, 2);
    assert_eq!(
        prompts.elided_other_scope, 1,
        "the cross-scope count is not filtered by kind"
    );

    let empty_list = store
        .reader
        .session_observations_scoped(
            ws(),
            proj_a(),
            session(10),
            ObservationPage {
                kinds: Some(Vec::new()),
                ..page(50)
            },
        )
        .await
        .unwrap();
    assert_eq!(empty_list.total, 3, "an empty kind list is no filter");
}

#[tokio::test]
async fn query_matches_within_the_session_and_scope_only() {
    let (_tmp, store) = open_seeded();

    let widget = store
        .reader
        .session_observations_scoped(
            ws(),
            proj_a(),
            session(10),
            ObservationPage {
                query: Some("widget".into()),
                ..page(50)
            },
        )
        .await
        .unwrap();
    let ids: Vec<u8> = widget.records.iter().map(|r| r.id.as_bytes()[15]).collect();
    assert_eq!(
        ids,
        vec![20, 22],
        "row 23 also says widget but lives in proj-b"
    );
    assert_eq!(widget.total, 2);
    assert_eq!(widget.elided_other_scope, 1);

    let cargo = store
        .reader
        .session_observations_scoped(
            ws(),
            proj_a(),
            session(10),
            ObservationPage {
                query: Some("cargo".into()),
                kinds: Some(vec![ObservationKind::PostToolUse]),
                ..page(50)
            },
        )
        .await
        .unwrap();
    assert_eq!(cargo.records.len(), 1);
    assert_eq!(cargo.records[0].id.as_bytes()[15], 21);

    let blank = store
        .reader
        .session_observations_scoped(
            ws(),
            proj_a(),
            session(10),
            ObservationPage {
                query: Some("   ".into()),
                ..page(50)
            },
        )
        .await
        .unwrap();
    assert_eq!(blank.total, 3, "a query with nothing searchable is ignored");
}

#[tokio::test]
async fn unknown_session_yields_an_empty_page() {
    let (_tmp, store) = open_seeded();
    let result = store
        .reader
        .session_observations_scoped(ws(), proj_a(), SessionId::new(), page(50))
        .await
        .unwrap();
    assert!(result.records.is_empty());
    assert_eq!(result.total, 0);
    assert_eq!(result.elided_other_scope, 0);
}

#[tokio::test]
async fn sessions_for_scope_lists_touched_sessions_newest_first() {
    let (_tmp, store) = open_seeded();

    let all = store
        .reader
        .sessions_for_scope(ws(), proj_a(), OwnerFilter::Any, true, 10, 0)
        .await
        .unwrap();
    let seen: Vec<(u8, u64)> = all
        .iter()
        .map(|s| (last_byte(s.session_id), s.observation_count))
        .collect();
    assert_eq!(
        seen,
        vec![(13, 0), (12, 1), (11, 1), (10, 3)],
        "started_at desc; 12 is anchored in proj-b but touched proj-a; \
         counts are per scope",
    );
    let crossed = &all[3];
    assert_eq!(crossed.agent_kind, "claude-code");
    assert_eq!(crossed.cwd.as_deref(), Some("/w/a"));
    assert!(crossed.ended_at.is_some());
    assert_eq!(crossed.actor_user, None);
    assert_eq!(
        crossed.started_at,
        jiff::Timestamp::from_microsecond(NOW).unwrap().to_string()
    );
    assert!(all[2].ended_at.is_none(), "session 11 is still open");

    let ended_only = store
        .reader
        .sessions_for_scope(ws(), proj_a(), OwnerFilter::Any, false, 10, 0)
        .await
        .unwrap();
    let seen: Vec<u8> = ended_only.iter().map(|s| last_byte(s.session_id)).collect();
    assert_eq!(seen, vec![13, 12, 10]);

    let window = store
        .reader
        .sessions_for_scope(ws(), proj_a(), OwnerFilter::Any, true, 2, 1)
        .await
        .unwrap();
    let seen: Vec<u8> = window.iter().map(|s| last_byte(s.session_id)).collect();
    assert_eq!(seen, vec![12, 11]);

    let from_b = store
        .reader
        .sessions_for_scope(ws(), proj_b(), OwnerFilter::Any, true, 10, 0)
        .await
        .unwrap();
    let seen: Vec<(u8, u64)> = from_b
        .iter()
        .map(|s| (last_byte(s.session_id), s.observation_count))
        .collect();
    assert_eq!(
        seen,
        vec![(12, 1), (10, 1)],
        "10 is anchored in proj-a but one row crossed into proj-b",
    );
}

#[tokio::test]
async fn sessions_for_scope_respects_the_owner_filter() {
    let (_tmp, store) = open_seeded();

    let alice = store
        .reader
        .sessions_for_scope(ws(), proj_a(), filter_for("alice"), true, 10, 0)
        .await
        .unwrap();
    let seen: Vec<u8> = alice.iter().map(|s| last_byte(s.session_id)).collect();
    assert_eq!(
        seen,
        vec![12, 11, 10],
        "own rows plus shared rows, never bob's"
    );
    assert_eq!(
        alice[0].actor_user.as_deref(),
        Some(operator("alice").as_str())
    );

    let anonymous = store
        .reader
        .sessions_for_scope(ws(), proj_a(), OwnerFilter::Unattributed, true, 10, 0)
        .await
        .unwrap();
    let seen: Vec<u8> = anonymous.iter().map(|s| last_byte(s.session_id)).collect();
    assert_eq!(seen, vec![11, 10], "shared rows only");
}

#[tokio::test]
async fn session_summary_scoped_narrows_the_listing_predicates() {
    let (_tmp, store) = open_seeded();

    let anchored = store
        .reader
        .session_summary_scoped(ws(), proj_a(), session(10), OwnerFilter::Any)
        .await
        .unwrap()
        .expect("session 10 is anchored in proj-a");
    assert_eq!(anchored.observation_count, 3);
    assert_eq!(anchored.agent_kind, "claude-code");

    let touched = store
        .reader
        .session_summary_scoped(ws(), proj_a(), session(12), OwnerFilter::Any)
        .await
        .unwrap()
        .expect("session 12 touched proj-a through an observation");
    assert_eq!(touched.observation_count, 1);

    let open = store
        .reader
        .session_summary_scoped(ws(), proj_a(), session(11), OwnerFilter::Any)
        .await
        .unwrap()
        .expect("open sessions are visible by id");
    assert!(open.ended_at.is_none());

    assert!(
        store
            .reader
            .session_summary_scoped(ws(), proj_b(), session(13), OwnerFilter::Any)
            .await
            .unwrap()
            .is_none(),
        "no row and no observation in proj-b",
    );
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), proj_a(), session(12), OwnerFilter::Unattributed)
            .await
            .unwrap()
            .is_none(),
        "alice's session is not an unidentified caller's to read",
    );
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), proj_a(), session(12), filter_for("bob"))
            .await
            .unwrap()
            .is_none(),
        "nor bob's",
    );
    assert!(
        store
            .reader
            .session_summary_scoped(ws(), proj_a(), SessionId::new(), OwnerFilter::Any)
            .await
            .unwrap()
            .is_none(),
        "unknown id",
    );
}
