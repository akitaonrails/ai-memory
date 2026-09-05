//! Integration tests for `ReaderPool::session_ids_touching_scope` (#402).
//!
//! A phantom project filled by pre-sticky mid-session routing holds
//! observations of sessions rooted elsewhere and no `sessions` row of its
//! own. The batch `move-session` must see those sessions, so "touching a
//! scope" is: a `sessions` row in it OR at least one observation in it.

use ai_memory_core::{ProjectId, SessionId, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = n;
    b
}

/// Workspace `w` with projects `a` and `b`; session 10 rooted in `b` with 3
/// observations in `a` and 1 in `b`; session 11 rooted in `a` (started later)
/// with 1 observation in `a`; session 12 rooted in `b` with no observations.
fn seed(db_path: &std::path::Path) {
    let conn = Connection::open(db_path).unwrap();
    let now = 1_700_000_000_000_i64;
    let (ws, a, b) = (id(1), id(2), id(3));

    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'w', ?2)",
        params![&ws[..], now],
    )
    .unwrap();
    for (pid, name, rp) in [(&a, "proj-a", "/w/a"), (&b, "proj-b", "/w/b")] {
        conn.execute(
            "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&pid[..], &ws[..], name, rp, now],
        )
        .unwrap();
    }
    for (sid, proj, started) in [(10u8, &b, now), (11, &a, now + 10), (12, &b, now + 20)] {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, 'claude-code', '/w', ?4)",
            params![&id(sid)[..], &ws[..], &proj[..], started],
        )
        .unwrap();
    }
    for (n, sid, proj, ts) in [
        (20u8, 10u8, &a, now + 1),
        (21, 10, &a, now + 2),
        (22, 10, &a, now + 3),
        (23, 10, &b, now + 4),
        (24, 11, &a, now + 11),
    ] {
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'note', 't', 'x', ?5)",
            params![&id(n)[..], &id(sid)[..], &ws[..], &proj[..], ts],
        )
        .unwrap();
    }
}

fn sid(n: u8) -> SessionId {
    SessionId::from_slice(&id(n)).unwrap()
}

#[tokio::test]
async fn lists_sessions_with_a_row_or_an_observation_in_the_scope_oldest_first() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());
    let ws = WorkspaceId::from_slice(&id(1)).unwrap();
    let a = ProjectId::from_slice(&id(2)).unwrap();
    let b = ProjectId::from_slice(&id(3)).unwrap();

    // `a`: session 10 through its observations (first at now+1), session 11
    // through its row (now+10). Session 12 never touched `a`.
    assert_eq!(
        store
            .reader
            .session_ids_touching_scope(ws, a)
            .await
            .unwrap(),
        vec![sid(10), sid(11)]
    );
    // `b`: session 10 through its row (now) and its late observation,
    // session 12 through its row alone; each id once.
    assert_eq!(
        store
            .reader
            .session_ids_touching_scope(ws, b)
            .await
            .unwrap(),
        vec![sid(10), sid(12)]
    );
}

#[tokio::test]
async fn empty_scope_lists_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());
    let ws = WorkspaceId::from_slice(&id(1)).unwrap();
    assert!(
        store
            .reader
            .session_ids_touching_scope(ws, ProjectId::from_slice(&id(9)).unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}
