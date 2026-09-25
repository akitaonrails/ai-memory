//! Per-project access mode (#708, `docs/design-per-project-authz.md`).
//!
//! A project is `open` — any authenticated user, which is what every project
//! was before this existed — or `restricted` — root, its creator and grant
//! holders only.
//! These follow the design's verification plan: every caller shape against
//! both modes, the multi-session shape that single-session tests cannot see
//! (a teammate admitted while an outsider is refused, pages still shared
//! within the project), and the upgrade path.

use ai_memory_core::{NewPage, NewUser, PagePath, ProjectId, Tier, UserId, WorkspaceId};
use ai_memory_store::{
    AccessMode, GrantLevel, ProjectAccess, ProjectPrincipal, Store, lookup_existing_scope_guarded,
};

/// Whether the choke point admits `user` to `project` for `need`.
async fn admits(
    store: &Store,
    ws: WorkspaceId,
    project: ProjectId,
    user: UserId,
    need: ProjectAccess,
) -> bool {
    store
        .reader
        .authorize_project(ws, project, ProjectPrincipal::user(user), true, need)
        .await
        .unwrap()
        .is_ok()
}

fn page(
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    body: &str,
    author: Option<UserId>,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: author,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

async fn user(store: &Store, name: &str, byte: u8) -> UserId {
    store
        .writer
        .create_user(
            NewUser {
                username: name.to_owned(),
                name: None,
                email: None,
            },
            [byte; ai_memory_store::TOKEN_HASH_LEN],
        )
        .await
        .unwrap()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    store: Store,
    ws: WorkspaceId,
    project: ProjectId,
    reader: UserId,
    writer: UserId,
    outsider: UserId,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "client-work", None)
        .await
        .unwrap();
    let reader = user(&store, "ray", 1).await;
    let writer = user(&store, "wren", 2).await;
    let outsider = user(&store, "otto", 3).await;
    store
        .writer
        .grant_memory(reader, project, GrantLevel::Read, None)
        .await
        .unwrap();
    store
        .writer
        .grant_memory(writer, project, GrantLevel::Write, None)
        .await
        .unwrap();
    Fixture {
        _tmp: tmp,
        store,
        ws,
        project,
        reader,
        writer,
        outsider,
    }
}

/// Every caller shape against both modes, at both levels.
///
/// Root arrives with no viewer and is never checked. In an open project every
/// database user is admitted at every level — grants or not, which is how
/// every project behaved before this existed. In a restricted one the grant
/// decides, and holding less than the operation needs is its own refusal.
#[tokio::test]
async fn every_caller_against_both_modes() {
    let f = fixture().await;
    for mode in [AccessMode::Open, AccessMode::Restricted] {
        f.store
            .writer
            .set_access_mode(f.project, mode)
            .await
            .unwrap();
        let open = mode == AccessMode::Open;
        for (who, user, read, write) in [
            ("read grant", f.reader, true, open),
            ("write grant", f.writer, true, true),
            ("no grant", f.outsider, open, open),
        ] {
            let reads = admits(&f.store, f.ws, f.project, user, ProjectAccess::Read).await;
            let writes = admits(&f.store, f.ws, f.project, user, ProjectAccess::Write).await;
            assert_eq!(reads, read, "{who} reading a {mode:?} project");
            assert_eq!(writes, write, "{who} writing a {mode:?} project");
        }
        // Root: no viewer, no check, whatever the mode.
        lookup_existing_scope_guarded(
            &f.store.reader,
            "default",
            "client-work",
            None,
            ProjectAccess::Write,
        )
        .await
        .unwrap_or_else(|e| panic!("root refused on a {mode:?} project: {e}"));
    }
}

/// The multi-session shape from the design: in a restricted project a granted
/// teammate reads what the creator wrote while an outsider is refused by name.
/// Grants gate entry to the project, never rows within it — the teammate sees
/// the same shared page, not a filtered copy (invariant #16).
#[tokio::test]
async fn a_restricted_project_admits_the_team_and_refuses_the_outsider() {
    let f = fixture().await;
    f.store
        .writer
        .set_access_mode(f.project, AccessMode::Restricted)
        .await
        .unwrap();
    f.store
        .writer
        .upsert_page(page(
            f.ws,
            f.project,
            "decisions/shared.md",
            "zebracrossing decision",
            Some(f.writer),
        ))
        .await
        .unwrap();

    lookup_existing_scope_guarded(
        &f.store.reader,
        "default",
        "client-work",
        Some(f.reader),
        ProjectAccess::Read,
    )
    .await
    .expect("a granted teammate is admitted");
    let hits = f
        .store
        .reader
        .search_pages("zebracrossing".into(), 10, Some(f.reader))
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the teammate finds the page the creator wrote"
    );

    let refused = lookup_existing_scope_guarded(
        &f.store.reader,
        "default",
        "client-work",
        Some(f.outsider),
        ProjectAccess::Read,
    )
    .await
    .unwrap_err();
    assert!(refused.is_forbidden(), "{refused:?}");
    let hits = f
        .store
        .reader
        .search_pages("zebracrossing".into(), 10, Some(f.outsider))
        .await
        .unwrap();
    assert!(hits.is_empty(), "search must not leak a restricted project");

    // Open it again and the outsider reads it, grant or not.
    f.store
        .writer
        .set_access_mode(f.project, AccessMode::Open)
        .await
        .unwrap();
    let hits = f
        .store
        .reader
        .search_pages("zebracrossing".into(), 10, Some(f.outsider))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "an open project is visible to every user");
}

/// `[auth] new_projects_restricted`: once set, a project created from then on
/// starts restricted and admits the creator recorded on it. The reserved
/// projects stay open whatever it says.
#[tokio::test]
async fn new_projects_follow_the_server_default_and_admit_their_creator() {
    let f = fixture().await;
    let w = &f.store.writer;
    let (open_one, _) = w
        .get_or_create_project_as(f.ws, "before-the-flag", None, Some(f.outsider))
        .await
        .unwrap();
    assert!(admits(&f.store, f.ws, open_one, f.reader, ProjectAccess::Write).await);

    w.set_new_project_mode(AccessMode::Restricted)
        .await
        .unwrap();
    let (locked, created) = w
        .get_or_create_project_as(f.ws, "after-the-flag", None, Some(f.outsider))
        .await
        .unwrap();
    assert!(created);
    let conn = rusqlite::Connection::open(f.store.db_path()).unwrap();
    let creator: Option<Vec<u8>> = conn
        .query_row(
            "SELECT created_by FROM projects WHERE id = ?1",
            [locked.as_bytes().to_vec()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(creator, Some(f.outsider.as_bytes().to_vec()));
    assert!(
        admits(&f.store, f.ws, locked, f.outsider, ProjectAccess::Write).await,
        "the creator is admitted"
    );
    assert!(
        !admits(&f.store, f.ws, locked, f.reader, ProjectAccess::Read).await,
        "everyone else is refused"
    );
    // A create with no creator — root, server start-up, admin — follows the
    // default too; the writer applies it to every insert.
    let plain = w
        .get_or_create_project(f.ws, "created-by-root", None)
        .await
        .unwrap();
    assert!(
        !admits(&f.store, f.ws, plain, f.reader, ProjectAccess::Read).await,
        "every new project starts restricted, whoever creates it"
    );
    for reserved in [
        ai_memory_core::DEFAULT_PROJECT_NAME,
        ai_memory_core::GLOBAL_SCOPE_PROJECT,
    ] {
        let (id, _) = w
            .get_or_create_project_as(f.ws, reserved, None, Some(f.outsider))
            .await
            .unwrap();
        assert!(
            admits(&f.store, f.ws, id, f.reader, ProjectAccess::Write).await,
            "{reserved} must stay open"
        );
    }
    // An existing project is not touched by the flag.
    assert!(admits(&f.store, f.ws, open_one, f.reader, ProjectAccess::Write).await);
}

/// Restricting names the people who were working in the project and are about
/// to be refused — authors with no grant who did not create it — and never the
/// ones who keep access.
#[tokio::test]
async fn restricting_reports_the_authors_it_locks_out() {
    let f = fixture().await;
    for (path, author) in [
        ("notes/a.md", f.outsider),
        ("notes/b.md", f.writer),
        ("notes/c.md", f.outsider),
    ] {
        f.store
            .writer
            .upsert_page(page(f.ws, f.project, path, "body", Some(author)))
            .await
            .unwrap();
    }
    assert_eq!(
        f.store
            .reader
            .authors_without_grant(f.project)
            .await
            .unwrap(),
        vec!["otto".to_owned()]
    );
}

/// Upgrading changes nobody's access: every project that existed before the
/// migration is open.
#[tokio::test]
async fn existing_projects_are_open_after_upgrade() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "legacy", None)
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let mode: String = conn
        .query_row(
            "SELECT access_mode FROM projects WHERE id = ?1",
            [project.as_bytes().to_vec()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mode, "open");
    // And the database refuses a mode this version cannot read.
    let bad = conn.execute(
        "UPDATE projects SET access_mode = 'members_only' WHERE id = ?1",
        [project.as_bytes().to_vec()],
    );
    assert!(bad.is_err(), "an unknown mode must not be storable");
}
