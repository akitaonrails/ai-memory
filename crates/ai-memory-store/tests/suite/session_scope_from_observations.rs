//! Integration tests for `session_scope_from_observations`.
//!
//! A "hybrid" session — one that installed its scope marker mid-flight — keeps
//! a `sessions` row frozen on the pre-marker scope (`begin_session` uses
//! `ON CONFLICT DO NOTHING`). Its observations, however, carry the correct
//! per-cwd scope. These tests prove the reader resolves the scope from where
//! the observations actually landed, independent of the stale session row.

use ai_memory_core::{ProjectId, SessionId, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = n;
    b
}

/// Seed a workspace with two projects and a session anchored to `proj-b`
/// (the stale pre-marker scope) whose observations mostly landed in `proj-a`.
fn seed(db_path: &std::path::Path) {
    let conn = Connection::open(db_path).unwrap();
    let now = 1_700_000_000_000_i64;
    let (ws, a, b) = (id(1), id(2), id(3));
    let session = id(10);

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
    // The session row froze on proj-b at session start (pre-marker scope).
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
        params![&session[..], &ws[..], &b[..], "/w/b", now],
    )
    .unwrap();
    // But the durable work happened under proj-a: 3 observations there vs 1 in
    // proj-b, so the majority scope is proj-a.
    for (n, proj, ts) in [
        (20u8, &a, now + 1),
        (21, &a, now + 2),
        (22, &a, now + 3),
        (23, &b, now + 4),
    ] {
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'note', 't', 'x', ?5)",
            params![&id(n)[..], &session[..], &ws[..], &proj[..], ts],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn resolves_majority_observation_scope_over_stale_session_row() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());

    let session = SessionId::from_slice(&id(10)).unwrap();
    let ws = WorkspaceId::from_slice(&id(1)).unwrap();
    let proj_a = ProjectId::from_slice(&id(2)).unwrap();
    let proj_b = ProjectId::from_slice(&id(3)).unwrap();

    // The session ROW is anchored to proj-b (the stale pre-marker scope)...
    assert_eq!(
        store.reader.session_project_ids(session).await.unwrap(),
        Some((ws, proj_b)),
    );
    // ...but the observations resolve to proj-a, where the work actually landed.
    assert_eq!(
        store
            .reader
            .session_scope_from_observations(session)
            .await
            .unwrap(),
        Some((ws, proj_a)),
    );
}

#[tokio::test]
async fn returns_none_when_session_has_no_observations() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    // No seed: an unknown session has no observations to resolve from.
    let session = SessionId::from_slice(&id(99)).unwrap();
    assert_eq!(
        store
            .reader
            .session_scope_from_observations(session)
            .await
            .unwrap(),
        None,
    );
}
