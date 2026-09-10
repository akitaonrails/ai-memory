//! Integration tests for `most_recently_active_scope`.
//!
//! The active-project pointer lives in process memory, so a daemon restart
//! mid-session drops it and unscoped reads silently answer from the baked
//! default scope (#678). This reader query is what `serve` seeds the shared
//! fallback slot from at startup: the newest observation names the project the
//! operator was actually in, and the caller's cutoff keeps a long-idle server
//! from resurrecting a scope the live pointer would have expired.

use ai_memory_core::{ProjectId, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

const NOW: i64 = 1_700_000_000_000_000;

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = n;
    b
}

/// Two projects in one workspace, each with observations: `proj-a` holds the
/// older activity, `proj-b` the newer.
fn seed(db_path: &std::path::Path) {
    let conn = Connection::open(db_path).unwrap();
    let (ws, a, b) = (id(1), id(2), id(3));
    let session = id(10);

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
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
        params![&session[..], &ws[..], &a[..], "/w/a", NOW],
    )
    .unwrap();
    // proj-a first, proj-b last: the pointer must follow the newest row.
    for (n, proj, ts) in [
        (20u8, &a, NOW),
        (21, &a, NOW + 1_000),
        (22, &b, NOW + 2_000),
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
async fn resolves_the_scope_of_the_newest_observation() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());

    let ws = WorkspaceId::from_slice(&id(1)).unwrap();
    let proj_b = ProjectId::from_slice(&id(3)).unwrap();

    assert_eq!(
        store
            .reader
            .most_recently_active_scope(NOW - 1)
            .await
            .unwrap(),
        Some((ws, proj_b)),
        "the newest observation names the last active project"
    );
}

#[tokio::test]
async fn activity_older_than_the_cutoff_is_not_resurrected() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    seed(store.db_path());

    // One microsecond past the newest row: a server that was down long enough
    // for the pointer TTL to elapse must start with no fallback at all rather
    // than answer for a project nobody is in any more.
    assert_eq!(
        store
            .reader
            .most_recently_active_scope(NOW + 2_001)
            .await
            .unwrap(),
        None,
    );
    // ...and a cutoff that admits only the newest row still resolves.
    assert_eq!(
        store
            .reader
            .most_recently_active_scope(NOW + 2_000)
            .await
            .unwrap(),
        Some((
            WorkspaceId::from_slice(&id(1)).unwrap(),
            ProjectId::from_slice(&id(3)).unwrap(),
        )),
    );
}

#[tokio::test]
async fn a_store_with_no_activity_has_no_scope_to_seed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(
        store.reader.most_recently_active_scope(0).await.unwrap(),
        None,
    );
}
