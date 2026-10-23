//! The cross-project profile's store side
//! (`docs/design-cross-project-profile.md`): where each share mode lives, the
//! reserved-name rules, the per-project `[profile]` flags, and the bounded
//! digest reads. The isolation half is adversarial: a private profile must
//! refuse every other operator, and a shared one must refuse writes from a
//! database user without a grant.

use ai_memory_core::profile::{EffectiveProfileShare, user_profile_project};
use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, NewUser, ObservationKind, PagePath, ProjectId,
    Sanitized, Sanitizer, SessionId, Tier, UserId, UserRole, WorkspaceId,
};
use ai_memory_store::{
    AccessMode, GrantLevel, ProjectAccess, ProjectPrincipal, ProjectProfileFlags,
    ScopeResolutionError, Store, create_profile_scope, lookup_existing_scope_guarded,
    lookup_profile_scope,
};

async fn user(store: &Store, name: &str) -> UserId {
    store
        .writer
        .create_human_user(
            NewUser {
                username: name.into(),
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

async fn default_workspace(store: &Store) -> WorkspaceId {
    store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap()
}

fn profile_page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
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
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

async fn access_mode_of(store: &Store, name: &str) -> (String, Option<Vec<u8>>) {
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.query_row(
        "SELECT access_mode, created_by FROM projects WHERE name = ?1",
        [name],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

/// A private profile is created restricted with its operator as creator,
/// whatever `new_projects_restricted` says; a workspace profile is created
/// open even on a server that restricts new projects.
#[tokio::test]
async fn reserved_profile_projects_get_their_fixed_access_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let alice = user(&store, "alice").await;
    let ws = default_workspace(&store).await;

    let private = create_profile_scope(
        &store.reader,
        &store.writer,
        EffectiveProfileShare::User,
        ws,
        Some(alice),
    )
    .await
    .unwrap();
    let (mode, creator) = access_mode_of(&store, &user_profile_project(alice)).await;
    assert_eq!(mode, "restricted");
    assert_eq!(creator.as_deref(), Some(alice.as_bytes().as_slice()));
    assert_eq!(private.workspace_id, ws);

    store
        .writer
        .set_new_project_mode(AccessMode::Restricted)
        .await
        .unwrap();
    create_profile_scope(
        &store.reader,
        &store.writer,
        EffectiveProfileShare::Workspace,
        ws,
        None,
    )
    .await
    .unwrap();
    assert_eq!(access_mode_of(&store, "_profile").await.0, "open");
}

/// Adversarial: another operator can neither find nor open my private
/// profile. Their own lookup resolves to their own (absent) profile, and
/// naming mine explicitly is refused; my lookup reads it back (control).
#[tokio::test]
async fn a_private_profile_refuses_every_other_operator() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let alice = user(&store, "alice").await;
    let bob = user(&store, "bob").await;
    let ws = default_workspace(&store).await;
    let mine = create_profile_scope(
        &store.reader,
        &store.writer,
        EffectiveProfileShare::User,
        ws,
        Some(alice),
    )
    .await
    .unwrap();

    let found = lookup_profile_scope(&store.reader, EffectiveProfileShare::User, ws, Some(alice))
        .await
        .unwrap();
    assert_eq!(found, Some(mine), "the owner reads her profile back");
    let theirs = lookup_profile_scope(&store.reader, EffectiveProfileShare::User, ws, Some(bob))
        .await
        .unwrap();
    assert_eq!(theirs, None, "bob's own profile does not exist yet");

    for need in [ProjectAccess::Read, ProjectAccess::Write] {
        let err = lookup_existing_scope_guarded(
            &store.reader,
            "default",
            &user_profile_project(alice),
            Some(bob),
            need,
        )
        .await
        .expect_err("bob must not open alice's profile by name");
        assert!(
            matches!(err, ScopeResolutionError::Forbidden(_)),
            "{need:?}: {err}"
        );
    }

    // A private profile with no database user behind the request: nothing to
    // read, and a write is refused rather than landing in a shared place.
    assert_eq!(
        lookup_profile_scope(&store.reader, EffectiveProfileShare::User, ws, None)
            .await
            .unwrap(),
        None
    );
    let err = create_profile_scope(
        &store.reader,
        &store.writer,
        EffectiveProfileShare::User,
        ws,
        None,
    )
    .await
    .expect_err("no database user, no private profile");
    assert!(matches!(err, ScopeResolutionError::Forbidden(_)), "{err}");
}

/// Adversarial: the shared scopes are unioned into everybody's reads, so on a
/// server with database users a write needs root or a write grant on them,
/// for a workspace profile exactly as for `_global`. Reads stay open.
#[tokio::test]
async fn shared_profiles_are_write_gated_for_database_users() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let bob = user(&store, "bob").await;
    let ws = default_workspace(&store).await;

    for share in [
        EffectiveProfileShare::Workspace,
        EffectiveProfileShare::Global,
    ] {
        let err = create_profile_scope(&store.reader, &store.writer, share, ws, Some(bob))
            .await
            .expect_err("bob holds no grant on the shared profile");
        assert!(
            matches!(err, ScopeResolutionError::Forbidden(_)),
            "{share:?}: {err}"
        );
    }

    // Root (no viewer) creates and writes the workspace profile.
    let shared = create_profile_scope(
        &store.reader,
        &store.writer,
        EffectiveProfileShare::Workspace,
        ws,
        None,
    )
    .await
    .unwrap();
    // Reads are open to every user.
    let read = store
        .reader
        .authorize_project(
            shared.workspace_id,
            shared.project_id,
            ProjectPrincipal::user(bob),
            true,
            ProjectAccess::Read,
        )
        .await
        .unwrap();
    assert!(read.is_ok(), "the workspace profile is read-open");

    store
        .writer
        .grant_memory(bob, shared.project_id, GrantLevel::Write, None)
        .await
        .unwrap();
    create_profile_scope(
        &store.reader,
        &store.writer,
        EffectiveProfileShare::Workspace,
        ws,
        Some(bob),
    )
    .await
    .expect("a write grant on _profile admits the write");
}

/// The hollow-project sweep never deletes an empty profile project.
#[tokio::test]
async fn the_hollow_project_sweep_keeps_empty_profile_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    for name in ["_profile", "_profile.0123456789abcdef", "hollow"] {
        store
            .writer
            .get_or_create_project(ws, name.to_owned(), None)
            .await
            .unwrap();
    }
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute("UPDATE projects SET created_at = 0", [])
        .unwrap();
    let swept = store.writer.sweep_hollow_projects(0).await.unwrap();
    assert!(swept.contains(&"hollow".to_owned()), "{swept:?}");
    assert!(
        !swept.iter().any(|name| name.starts_with("_profile")),
        "{swept:?}"
    );
}

/// The `[profile]` flags default on, persist, and are listed when a project
/// opts out of contributing.
#[tokio::test]
async fn project_profile_flags_persist_and_list_opt_outs() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let proj = store
        .writer
        .get_or_create_project(ws, "client-work".to_owned(), None)
        .await
        .unwrap();
    assert_eq!(
        store.reader.project_profile_flags(ws, proj).await.unwrap(),
        ProjectProfileFlags::default()
    );
    let off = ProjectProfileFlags {
        contribute: false,
        consume: false,
    };
    store
        .writer
        .set_project_profile_flags(proj, off)
        .await
        .unwrap();
    assert_eq!(
        store.reader.project_profile_flags(ws, proj).await.unwrap(),
        off
    );
    let opt_outs = store.reader.profile_contribute_opt_outs().await.unwrap();
    assert_eq!(opt_outs.len(), 1);
    assert_eq!(opt_outs[0].project, "client-work");

    store
        .writer
        .set_project_profile_flags(proj, ProjectProfileFlags::default())
        .await
        .unwrap();
    assert!(
        store
            .reader
            .profile_contribute_opt_outs()
            .await
            .unwrap()
            .is_empty()
    );
}

/// Digest inputs: only current, unexpired `profile/` pages, the stack the
/// project's activity shows, and whether the project is new.
#[tokio::test]
async fn digest_inputs_read_entries_stack_and_newness() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let global = ai_memory_store::create_global_scope(&store.writer)
        .await
        .unwrap();
    let app = store
        .writer
        .get_or_create_project(ws, "app".to_owned(), None)
        .await
        .unwrap();
    for (path, body) in [
        ("profile/tools/pnpm.md", "# Pnpm\n\nUse pnpm, not npm."),
        ("profile/stack/rust.md", "# Rust\n\nAsync code uses tokio."),
        ("notes/unrelated.md", "# Note\n\nnot a profile entry"),
    ] {
        store
            .writer
            .upsert_page(profile_page(
                global.workspace_id,
                global.project_id,
                path,
                body,
            ))
            .await
            .unwrap();
    }
    let mut expired = profile_page(
        global.workspace_id,
        global.project_id,
        "profile/habits/old.md",
        "# Old\n\nexpired habit",
    );
    expired.expires_at = Some(jiff::Timestamp::from_second(1).unwrap());
    store.writer.upsert_page(expired).await.unwrap();

    let inputs = store
        .reader
        .profile_digest_inputs(global.as_tuple(), (ws, app))
        .await
        .unwrap();
    let paths: Vec<&str> = inputs.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["profile/stack/rust.md", "profile/tools/pnpm.md"]
    );
    assert!(inputs.project_tags.is_empty());
    assert!(!inputs.project_has_pages, "a project without pages is new");

    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: app,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some("/repo/app".into()),
            actor_user: None,
        })
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id: session,
                workspace_id: ws,
                project_id: app,
                kind: ObservationKind::PostToolUse,
                extension: None,
                source_event: None,
                title: "Edit crates/core/Cargo.toml".into(),
                body: String::new(),
                importance: 5,
                occurred_at: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(profile_page(ws, app, "notes/first.md", "# First\n\nhello"))
        .await
        .unwrap();
    let inputs = store
        .reader
        .profile_digest_inputs(global.as_tuple(), (ws, app))
        .await
        .unwrap();
    assert!(
        inputs.project_tags.contains("rust"),
        "{:?}",
        inputs.project_tags
    );
    assert!(inputs.project_has_pages);
}

/// Newness honours the TTL, both directions. A project whose every page has
/// passed its `expires_at` is as new as one that never had a page: its pages
/// are already hidden from every other digest read, so counting them would
/// halve the digest budget and suppress the `profile apply` offer for a
/// project with nothing left to show. A single live page still counts.
#[tokio::test]
async fn expired_pages_do_not_make_a_project_look_established() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = default_workspace(&store).await;
    let global = ai_memory_store::create_global_scope(&store.writer)
        .await
        .unwrap();
    let app = store
        .writer
        .get_or_create_project(ws, "app".to_owned(), None)
        .await
        .unwrap();

    let mut expired = profile_page(ws, app, "notes/stale.md", "# Stale\n\nlong gone");
    expired.expires_at = Some(jiff::Timestamp::from_second(1).unwrap());
    store.writer.upsert_page(expired).await.unwrap();

    let inputs = store
        .reader
        .profile_digest_inputs(global.as_tuple(), (ws, app))
        .await
        .unwrap();
    assert!(
        !inputs.project_has_pages,
        "a project whose every page has expired is still new"
    );

    store
        .writer
        .upsert_page(profile_page(
            ws,
            app,
            "notes/live.md",
            "# Live\n\nstill here",
        ))
        .await
        .unwrap();
    let inputs = store
        .reader
        .profile_digest_inputs(global.as_tuple(), (ws, app))
        .await
        .unwrap();
    assert!(
        inputs.project_has_pages,
        "one unexpired page means the project has memory"
    );
}
