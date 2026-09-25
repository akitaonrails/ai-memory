//! Adversarial integration tests for the per-project authorization choke
//! point (#708, slice 2).
//!
//! These prove the gate BITES: with the guard removed or inverted every
//! restricted-project row here would flip. They deliberately live at
//! integration level (invariant #16) — a single-tenant happy path cannot see
//! an authorization defect. The gate is an allow/deny check for ENTRY to a
//! project; it is never an `OwnerFilter` on page rows, so nothing here filters
//! page reads by user.
//!
//! Slice 2 ships inert: every project defaults to `open`, so the gate is a
//! pass-through until an operator opts a project into `restricted`. The tests
//! set `access_mode='restricted'` and insert `project_grants` rows directly via
//! SQL to exercise the enforcing branch the management surface (slice 3) will
//! drive.

use ai_memory_core::{ActorKey, AuthzError, NewUser, ProjectId, UserId, UserRole, WorkspaceId};
use ai_memory_store::{
    AccessMode, GrantLevel, ProjectAccess, ProjectAuthz, ProjectPrincipal, ScopeResolver, Store,
    authorize_project,
};
use rusqlite::{Connection, params};

const GRANTED_AT: i64 = 1_700_000_000_000_000;

async fn make_user(store: &Store, username: &str) -> UserId {
    store
        .writer
        .create_human_user(
            NewUser {
                username: username.to_string(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap()
}

/// Flip a project's access mode via raw SQL — stands in for the slice-3
/// management surface.
fn set_access_mode(store: &Store, project_id: ProjectId, mode: &str) {
    let conn = Connection::open(store.db_path()).unwrap();
    let updated = conn
        .execute(
            "UPDATE projects SET access_mode = ?1 WHERE id = ?2",
            params![mode, project_id.as_bytes()],
        )
        .unwrap();
    assert_eq!(updated, 1, "expected to update exactly one project row");
}

/// Insert a grant row via raw SQL.
fn insert_grant(
    store: &Store,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    user_id: UserId,
    level: &str,
) {
    let conn = Connection::open(store.db_path()).unwrap();
    conn.execute(
        "INSERT INTO project_grants \
         (workspace_id, project_id, user_id, level, granted_by, granted_at) \
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            user_id.as_bytes(),
            level,
            GRANTED_AT
        ],
    )
    .unwrap();
}

async fn scope(store: &Store, ws_name: &str, proj_name: &str) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace(ws_name.to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, proj_name.to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

/// (a) AUTHZ MATRIX — root / granted-read / granted-write / no-grant /
/// anonymous × {open, restricted}, resolved from real SQL through both the
/// reader-pool and writer-actor entry points. Every restricted row flips if the
/// gate is removed; the legitimate controls (root, creator, matching grant) are
/// admitted.
#[tokio::test]
async fn authz_matrix_reader_and_writer_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let (ws, restricted) = scope(&store, "acme", "restricted").await;
    let (_, open) = scope(&store, "acme", "open").await;

    let reader = make_user(&store, "reader-user").await;
    let writer = make_user(&store, "writer-user").await;
    let nogrant = make_user(&store, "no-grant-user").await;

    set_access_mode(&store, restricted, "restricted");
    insert_grant(&store, ws, restricted, reader, "read");
    insert_grant(&store, ws, restricted, writer, "write");

    let root = ProjectPrincipal::root();
    let creator = ProjectPrincipal {
        is_root: false,
        is_creator: true,
        user_id: None,
    };
    let read_grantee = ProjectPrincipal {
        is_root: false,
        is_creator: false,
        user_id: Some(reader),
    };
    let write_grantee = ProjectPrincipal {
        is_root: false,
        is_creator: false,
        user_id: Some(writer),
    };
    let no_grant = ProjectPrincipal {
        is_root: false,
        is_creator: false,
        user_id: Some(nogrant),
    };
    let anon = ProjectPrincipal::anonymous();

    // (principal, need, target_project, expect_ok)
    use ProjectAccess::{Read, Write};
    let cases: Vec<(&str, ProjectPrincipal, ProjectAccess, ProjectId, bool)> = vec![
        // Restricted project.
        (
            "root reads restricted",
            root.clone(),
            Read,
            restricted,
            true,
        ),
        (
            "root writes restricted",
            root.clone(),
            Write,
            restricted,
            true,
        ),
        (
            "creator reads restricted",
            creator.clone(),
            Read,
            restricted,
            true,
        ),
        (
            "creator writes restricted",
            creator.clone(),
            Write,
            restricted,
            true,
        ),
        (
            "read-grant reads restricted",
            read_grantee.clone(),
            Read,
            restricted,
            true,
        ),
        (
            "read-grant CANNOT write restricted",
            read_grantee.clone(),
            Write,
            restricted,
            false,
        ),
        (
            "write-grant reads restricted",
            write_grantee.clone(),
            Read,
            restricted,
            true,
        ),
        (
            "write-grant writes restricted",
            write_grantee.clone(),
            Write,
            restricted,
            true,
        ),
        (
            "no-grant CANNOT read restricted",
            no_grant.clone(),
            Read,
            restricted,
            false,
        ),
        (
            "no-grant CANNOT write restricted",
            no_grant.clone(),
            Write,
            restricted,
            false,
        ),
        (
            "anonymous CANNOT read restricted",
            anon.clone(),
            Read,
            restricted,
            false,
        ),
        (
            "anonymous CANNOT write restricted",
            anon.clone(),
            Write,
            restricted,
            false,
        ),
        // Open project — everyone, including anonymous, is admitted.
        ("anonymous reads open", anon.clone(), Read, open, true),
        ("anonymous writes open", anon.clone(), Write, open, true),
        ("no-grant reads open", no_grant.clone(), Read, open, true),
        ("no-grant writes open", no_grant.clone(), Write, open, true),
    ];

    for (name, principal, need, project, expect_ok) in cases {
        // Reader-pool entry point.
        let via_reader = store
            .reader
            .authorize_project(ws, project, principal.clone(), true, need)
            .await
            .unwrap();
        assert_eq!(via_reader.is_ok(), expect_ok, "reader: {name}");

        // Writer-actor entry point (defense in depth) — same decision.
        let via_writer = store
            .writer
            .authorize_project(ws, project, principal.clone(), true, need)
            .await
            .unwrap();
        assert_eq!(via_writer.is_ok(), expect_ok, "writer: {name}");

        if !expect_ok {
            assert!(
                matches!(via_reader, Err(AuthzError::Forbidden(_))),
                "{name} must be Forbidden, not some other outcome",
            );
        }
    }
}

/// (b) SHIP-INERT — an empty grants table with the default `open` mode admits
/// everyone; a `restricted` project with ZERO grants still admits root and the
/// creator (never locked out of your own data).
#[tokio::test]
async fn ship_inert_open_admits_all_and_restricted_admits_root_and_creator() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let (ws, open) = scope(&store, "team", "open").await;
    let (_, restricted) = scope(&store, "team", "locked").await;
    set_access_mode(&store, restricted, "restricted");

    // Empty grants + default open: anonymous is admitted for read and write.
    for need in [ProjectAccess::Read, ProjectAccess::Write] {
        let decision = store
            .reader
            .authorize_project(ws, open, ProjectPrincipal::anonymous(), true, need)
            .await
            .unwrap();
        assert!(decision.is_ok(), "open project admits everyone: {need:?}");
    }

    // Restricted + zero grants: root and creator still admitted.
    let creator = ProjectPrincipal {
        is_root: false,
        is_creator: true,
        user_id: None,
    };
    for principal in [ProjectPrincipal::root(), creator] {
        for need in [ProjectAccess::Read, ProjectAccess::Write] {
            let decision = store
                .reader
                .authorize_project(ws, restricted, principal.clone(), true, need)
                .await
                .unwrap();
            assert!(
                decision.is_ok(),
                "restricted-with-zero-grants must admit root/creator: {principal:?} {need:?}",
            );
        }
    }

    // The control that proves the restriction is real: a bare authenticated
    // user with no grant is refused the same restricted project.
    let stranger = make_user(&store, "stranger").await;
    let decision = store
        .reader
        .authorize_project(
            ws,
            restricted,
            ProjectPrincipal {
                is_root: false,
                is_creator: false,
                user_id: Some(stranger),
            },
            true,
            ProjectAccess::Read,
        )
        .await
        .unwrap();
    assert!(
        decision.is_err(),
        "a restricted project with zero grants must still refuse an ungranted stranger",
    );
}

/// (d) distinguishes_operators() == false skips the gate entirely: even a
/// restricted project with no grant admits an anonymous caller, because a
/// single-user / loopback deployment has no notion of "another operator".
#[tokio::test]
async fn gate_is_skipped_when_operators_are_not_distinguished() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let (ws, restricted) = scope(&store, "solo", "locked").await;
    set_access_mode(&store, restricted, "restricted");

    for need in [ProjectAccess::Read, ProjectAccess::Write] {
        // distinguishes_operators = false -> allow.
        let skipped = store
            .reader
            .authorize_project(ws, restricted, ProjectPrincipal::anonymous(), false, need)
            .await
            .unwrap();
        assert!(skipped.is_ok(), "single-user mode skips the gate: {need:?}");

        // The control: with distinguishing ON, the same call is refused, so the
        // pass above is the skip, not an always-allow bug.
        let enforced = store
            .reader
            .authorize_project(ws, restricted, ProjectPrincipal::anonymous(), true, need)
            .await
            .unwrap();
        assert!(
            enforced.is_err(),
            "multi-user mode enforces the gate: {need:?}"
        );
    }
}

/// (c) MIGRATION IDEMPOTENCY — V68 applies cleanly (the table exists, the column
/// defaults to 'open'), the CHECK constraints bite, and re-opening the store
/// (which re-runs the migration set) is a no-op that preserves the data.
#[tokio::test]
async fn migration_applies_defaults_open_and_reopen_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    let project_id;
    {
        let store = Store::open(tmp.path()).unwrap();
        let (_, proj) = scope(&store, "acme", "app").await;
        project_id = proj;

        let conn = Connection::open(store.db_path()).unwrap();

        // Additive: an existing project defaults to 'open'.
        let mode: String = conn
            .query_row(
                "SELECT access_mode FROM projects WHERE id = ?1",
                params![proj.as_bytes()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mode, "open");

        // The grants table exists and starts empty.
        let grant_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_grants", [], |row| row.get(0))
            .unwrap();
        assert_eq!(grant_count, 0);

        // CHECK constraints bite.
        assert!(
            conn.execute(
                "UPDATE projects SET access_mode = 'bogus' WHERE id = ?1",
                params![proj.as_bytes()],
            )
            .is_err(),
            "access_mode CHECK must reject an unknown mode",
        );
        assert!(
            conn.execute(
                "INSERT INTO project_grants \
                 (workspace_id, project_id, user_id, level, granted_by, granted_at) \
                 VALUES (randomblob(16), ?1, randomblob(16), 'admin', NULL, ?2)",
                params![proj.as_bytes(), GRANTED_AT],
            )
            .is_err(),
            "level CHECK must reject a level other than read/write",
        );
    }

    // Re-open the same data dir: migrations re-run and must be a no-op.
    let store = Store::open(tmp.path()).unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    let mode: String = conn
        .query_row(
            "SELECT access_mode FROM projects WHERE id = ?1",
            params![project_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        mode, "open",
        "re-open must preserve the applied schema + data"
    );
}

/// The choke point is genuinely wired into `ScopeResolver` read + write
/// resolution: a restricted project is refused there, an ungated resolver still
/// admits it (proving the gate — not something else — is what bites), and root
/// is admitted through the same wiring.
#[tokio::test]
async fn scope_resolver_gates_restricted_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let (ws, restricted) = scope(&store, "acme", "restricted").await;
    let (default_ws, default_proj) = scope(&store, "acme", "fallback").await;
    set_access_mode(&store, restricted, "restricted");

    let actor = ActorKey::default();

    // With the authz principal attached, an anonymous caller is Forbidden the
    // restricted project on both the read and write paths.
    let gated_read = ScopeResolver::new(&store.reader, default_ws, default_proj)
        .with_project_authz(ProjectPrincipal::anonymous(), true)
        .resolve_read_args(Some("acme"), Some("restricted"), &actor)
        .await
        .unwrap_err();
    assert!(gated_read.is_forbidden(), "read: {gated_read:?}");

    let gated_write = ScopeResolver::new(&store.reader, default_ws, default_proj)
        .with_writer(&store.writer)
        .with_project_authz(ProjectPrincipal::anonymous(), true)
        .resolve_write_args(Some("acme"), Some("restricted"), &actor)
        .await
        .unwrap_err();
    assert!(gated_write.is_forbidden(), "write: {gated_write:?}");

    // Proof it bites: the SAME resolution WITHOUT the authz principal admits the
    // restricted project (today's behaviour — the gate is opt-in and inert).
    let ungated = ScopeResolver::new(&store.reader, default_ws, default_proj)
        .resolve_read_args(Some("acme"), Some("restricted"), &actor)
        .await
        .unwrap();
    assert_eq!(ungated.as_tuple(), (ws, restricted));

    // The legitimate control: root is admitted through the same wiring.
    let root_ok = ScopeResolver::new(&store.reader, default_ws, default_proj)
        .with_project_authz(ProjectPrincipal::root(), true)
        .resolve_read_args(Some("acme"), Some("restricted"), &actor)
        .await
        .unwrap();
    assert_eq!(root_ok.as_tuple(), (ws, restricted));

    // And an open project is unaffected: the gate admits everyone.
    let open_ok = ScopeResolver::new(&store.reader, default_ws, default_proj)
        .with_project_authz(ProjectPrincipal::anonymous(), true)
        .resolve_read_args(Some("acme"), Some("fallback"), &actor)
        .await
        .unwrap();
    assert_eq!(open_ok.as_tuple(), (default_ws, default_proj));
}

/// The pure decision function reused as documentation of intent: a resolved
/// restricted context with no grant denies, a write grant admits both.
#[tokio::test]
async fn pure_authorize_project_reference() {
    let deny = ProjectAuthz {
        distinguishes_operators: true,
        access_mode: AccessMode::Restricted,
        is_root: false,
        is_creator: false,
        grant: None,
    };
    assert!(authorize_project(&deny, ProjectAccess::Read).is_err());

    let write_grant = ProjectAuthz {
        grant: Some(GrantLevel::Write),
        ..deny
    };
    assert!(authorize_project(&write_grant, ProjectAccess::Read).is_ok());
    assert!(authorize_project(&write_grant, ProjectAccess::Write).is_ok());

    let read_grant = ProjectAuthz {
        grant: Some(GrantLevel::Read),
        ..deny
    };
    assert!(authorize_project(&read_grant, ProjectAccess::Read).is_ok());
    assert!(authorize_project(&read_grant, ProjectAccess::Write).is_err());
}
