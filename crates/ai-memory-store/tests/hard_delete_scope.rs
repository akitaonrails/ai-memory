//! The retention hard-delete must stay inside the scope it was asked to sweep.
//!
//! `hard_delete_decayed_pages` used to take no workspace/project: its
//! `DELETE FROM pages` matched the whole database. `run_sweep` is scoped, but
//! called it unconditionally on every non-dry run — even with nothing to evict
//! in the swept project — so sweeping one project irreversibly deleted evicted
//! page versions belonging to every other project on the server.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

fn page(ws: WorkspaceId, proj: ProjectId, path: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: "t".into(),
        body: "body".into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
    }
}

/// Age a soft-deleted row past the hard-delete cutoff so it becomes eligible.
fn backdate_supersession(db: &std::path::Path, days: i64) {
    let conn = Connection::open(db).unwrap();
    let cutoff_us = jiff::Timestamp::now().as_microsecond() - days * 86_400_000_000;
    conn.execute(
        "UPDATE pages SET superseded_at = ?1 WHERE is_latest = 0",
        params![cutoff_us],
    )
    .unwrap();
}

#[tokio::test]
async fn hard_delete_only_touches_the_swept_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let ws = store
        .writer
        .get_or_create_workspace("shared".to_string())
        .await
        .unwrap();
    let mine = store
        .writer
        .get_or_create_project(ws, "mine".to_string(), None)
        .await
        .unwrap();
    let theirs = store
        .writer
        .get_or_create_project(ws, "theirs".to_string(), None)
        .await
        .unwrap();

    // One page per project, both evicted by decay and both old enough to be
    // hard-delete eligible.
    let mine_page = store
        .writer
        .upsert_page(page(ws, mine, "notes/mine.md"))
        .await
        .unwrap();
    let theirs_page = store
        .writer
        .upsert_page(page(ws, theirs, "notes/theirs.md"))
        .await
        .unwrap();
    store
        .writer
        .soft_delete_for_decay(vec![mine_page, theirs_page])
        .await
        .unwrap();
    backdate_supersession(store.db_path(), 400);

    // Sweep MY project only.
    let removed = store
        .writer
        .hard_delete_decayed(ws, mine, 90)
        .await
        .unwrap();
    assert_eq!(removed, 1, "exactly my own evicted page is hard deleted");

    let conn = Connection::open(store.db_path()).unwrap();
    let count = |project: ProjectId| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM pages WHERE project_id = ?1",
            params![project.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(count(mine), 0, "the swept project's evicted row is gone");
    assert_eq!(
        count(theirs),
        1,
        "another project's evicted row must survive a sweep it never asked for"
    );
}

/// Sweeping a project with nothing of its own to remove must be a no-op, not a
/// database-wide purge. This is the shape that actually bit: the old code
/// issued the unscoped DELETE even when the scoped eviction list was empty.
#[tokio::test]
async fn sweeping_an_empty_project_deletes_nothing_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let ws = store
        .writer
        .get_or_create_workspace("shared".to_string())
        .await
        .unwrap();
    let quiet = store
        .writer
        .get_or_create_project(ws, "quiet".to_string(), None)
        .await
        .unwrap();
    let busy = store
        .writer
        .get_or_create_project(ws, "busy".to_string(), None)
        .await
        .unwrap();

    let busy_page = store
        .writer
        .upsert_page(page(ws, busy, "notes/busy.md"))
        .await
        .unwrap();
    store
        .writer
        .soft_delete_for_decay(vec![busy_page])
        .await
        .unwrap();
    backdate_supersession(store.db_path(), 400);

    let removed = store
        .writer
        .hard_delete_decayed(ws, quiet, 90)
        .await
        .unwrap();
    assert_eq!(removed, 0);

    let conn = Connection::open(store.db_path()).unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE project_id = ?1",
            params![busy.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1, "an unrelated project is untouched");
}

/// Sessions carry an owner (V40) so anything that picks "the open session for
/// this scope" cannot pick a colleague's. `finalize-session` drives off this
/// lookup and acts destructively on the result.
#[tokio::test]
async fn open_session_lookup_is_scoped_to_its_owner() {
    use ai_memory_core::{AgentKind, NewSession, OwnerFilter, SessionId};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("shared".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "team-app".to_string(), None)
        .await
        .unwrap();

    let alice_session = SessionId::new();
    let legacy_session = SessionId::new();
    for (id, owner) in [
        (alice_session, Some("alice".to_string())),
        // A pre-V40 row, or an unauthenticated server: no owner recorded.
        (legacy_session, None),
    ] {
        store
            .writer
            .begin_session(NewSession {
                id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: owner,
            })
            .await
            .unwrap();
    }

    let ids = |filter: OwnerFilter| {
        let store = &store;
        async move {
            store
                .reader
                .open_sessions_for_scope_agent(ws, proj, AgentKind::ClaudeCode, filter, None)
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.session_id)
                .collect::<Vec<_>>()
        }
    };

    // Bob sees only the ownerless session — never Alice's live one.
    let bob = ids(OwnerFilter::User("bob".into())).await;
    assert!(
        !bob.contains(&alice_session),
        "Bob must not see Alice's session"
    );
    assert!(bob.contains(&legacy_session));

    // Alice sees her own plus the ownerless one.
    let alice = ids(OwnerFilter::User("alice".into())).await;
    assert!(alice.contains(&alice_session));
    assert!(alice.contains(&legacy_session));

    // Explicit recovery still sees everything.
    assert_eq!(ids(OwnerFilter::Any).await.len(), 2);
}
