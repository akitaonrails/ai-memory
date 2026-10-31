//! Integration tests for the multi-session and multi-user scenarios.
//!
//! These are the guarantees a team and a parallel-harness workflow depend on,
//! and they are deliberately at integration level: the unit tests around them
//! exercise one session at a time, which is exactly the shape that cannot see
//! a collaboration or concurrency defect.
//!
//! Two halves, and they pull in opposite directions:
//!
//! * **Shared** — knowledge written by one operator must be readable by
//!   another in the same project. Pages carry an `author_id` for attribution,
//!   and it must never become a filter.
//! * **Owned** — a handoff is a baton. Exactly one session may take it, and a
//!   second attempt must not be able to steal it.
//!
//! Anything that makes pages owner-filtered, or handoffs stealable, breaks a
//! core capability rather than a detail.

use ai_memory_core::{
    ActorContext, AgentKind, FeedbackKind, HandoffAcceptance, HandoffState, IdentityKey,
    NewHandoff, NewPage, NewSession, NewUser, OwnerFilter, PagePath, ProjectId, SessionId, Tier,
    UserRole, WorkspaceId, owner_stamp,
};
use ai_memory_store::{
    ConditionalUpsert, DecayParams, LinkOrAdoptManagedRunSession, ManagedRunSessionLink,
    PagePrecondition, PrepareWorkstreamRun, Store, StoreError, WorkstreamSelection,
};

fn operator(name: &str) -> String {
    IdentityKey::User(name.into()).storage_key()
}

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "shared-app".to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

/// Open a real session row. `accept_handoff` requires the receiver to exist —
/// a guard worth keeping, so the tests satisfy it rather than route around it.
async fn open_session(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    agent_kind: AgentKind,
) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind,
            cwd: Some("/repo".into()),
            actor_user: None,
        })
        .await
        .unwrap();
    id
}

/// A SessionEnd digest takes a raw session id internally, but the reader must
/// still require the admitted scope. Otherwise a future caller could render a
/// foreign project's prompts merely by learning its id.
#[tokio::test]
async fn session_end_readers_refuse_foreign_session_ids() {
    use ai_memory_core::{NewObservation, ObservationKind, Sanitized, Sanitizer};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let home = store
        .writer
        .get_or_create_project(ws, "home", None)
        .await
        .unwrap();
    let sibling = store
        .writer
        .get_or_create_project(ws, "sibling", None)
        .await
        .unwrap();
    let other_ws = store.writer.get_or_create_workspace("other").await.unwrap();
    let session = open_session(&store, ws, home, AgentKind::Codex).await;
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                occurred_at: None,
                session_id: session,
                workspace_id: ws,
                project_id: home,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "private prompt".into(),
                body: "foreign scopes must not read this".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();

    assert!(
        store
            .reader
            .session_has_substantive_observation(ws, home, session)
            .await
            .unwrap(),
        "the owning scope is the legitimate control"
    );
    let total = store
        .reader
        .read_session_observations(ws, home, session, |snapshot| {
            Ok(snapshot.kind_summary()?.total)
        })
        .await
        .unwrap();
    assert_eq!(total, 1);

    for (attempt_ws, attempt_project) in [(ws, sibling), (other_ws, home)] {
        assert!(
            !store
                .reader
                .session_has_substantive_observation(attempt_ws, attempt_project, session)
                .await
                .unwrap(),
            "a foreign scope must not learn the session's substantive state"
        );
        let error = store
            .reader
            .read_session_observations(attempt_ws, attempt_project, session, |snapshot| {
                Ok(snapshot.kind_summary()?.total)
            })
            .await
            .unwrap_err();
        assert!(matches!(error, StoreError::SessionCollision));
    }

    let missing = SessionId::new();
    let error = store
        .reader
        .read_session_observations(ws, home, missing, |snapshot| {
            Ok(snapshot.kind_summary()?.total)
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::SessionCollision),
        "missing and foreign ids deliberately expose the same refusal"
    );
}

/// The point-fetch rowid API is scoped to the snapshot's session: a
/// `rowid` belonging to another session is refused through a legitimate
/// snapshot, while the session's own rowid is returned (the legitimate
/// control). A constructor-only test cannot see the removal of the
/// `session_id = ?1` predicate alone — this one hands a foreign rowid to a
/// legitimate session and asserts the refusal.
#[tokio::test]
async fn rowid_fetch_refuses_foreign_sessions_with_legitimate_control() {
    use ai_memory_core::{NewObservation, ObservationKind, Sanitized, Sanitizer};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let session_a = open_session(&store, ws, proj, AgentKind::Codex).await;
    let session_b = open_session(&store, ws, proj, AgentKind::Codex).await;
    let sanitizer = Sanitizer::builtin();
    for (session, tag) in [(session_a, "a"), (session_b, "b")] {
        for i in 0..12u32 {
            store
                .writer
                .insert_observation(Sanitized::new(
                    NewObservation {
                        occurred_at: None,
                        session_id: session,
                        workspace_id: ws,
                        project_id: proj,
                        kind: ObservationKind::PostToolUse,
                        extension: None,
                        source_event: None,
                        title: "routine".into(),
                        body: format!("{tag} row {i}"),
                        importance: 1,
                    },
                    &sanitizer,
                ))
                .await
                .unwrap();
        }
    }

    // Each session's first rowid: session B's rows landed after session
    // A's, so the rowids are distinct.
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let first_rowid = |session: SessionId| {
        conn.query_row(
            "SELECT rowid FROM observations WHERE session_id = ?1 ORDER BY rowid LIMIT 1",
            rusqlite::params![session.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    let rowid_a = first_rowid(session_a);
    let rowid_b = first_rowid(session_b);
    assert_ne!(rowid_a, rowid_b, "the attack needs distinct rowids");

    // Legitimate control: a session reads its own rowid through its own
    // legitimate snapshot.
    let own = store
        .reader
        .read_session_observations_in_scope(ws, proj, session_a, move |snapshot| {
            snapshot.row_by_rowid(rowid_a)
        })
        .await
        .unwrap();
    assert_eq!(
        own.as_ref().map(|observation| observation.body.as_str()),
        Some("a row 0"),
        "the session's own rowid must be readable"
    );

    // The attack: session A's legitimate snapshot handed session B's rowid.
    let foreign = store
        .reader
        .read_session_observations_in_scope(ws, proj, session_a, move |snapshot| {
            snapshot.row_by_rowid(rowid_b)
        })
        .await
        .unwrap();
    assert!(
        foreign.is_none(),
        "a foreign session's rowid must be refused by the session-scoped fetch"
    );

    // And the mirror: session B's snapshot handed session A's rowid.
    let foreign_mirror = store
        .reader
        .read_session_observations_in_scope(ws, proj, session_b, move |snapshot| {
            snapshot.row_by_rowid(rowid_a)
        })
        .await
        .unwrap();
    assert!(
        foreign_mirror.is_none(),
        "the refusal must hold in both directions"
    );
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, title: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.into(),
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

/// #986 slice 2 under invariant #16: two harnesses edit one page from the
/// same version with a precondition. Exactly one wins; the loser writes
/// nothing, and the version both started from stays reachable as superseded.
/// Without the in-transaction check both would supersede it and the first
/// winner's text would be buried under the second.
#[tokio::test]
async fn conditional_writers_on_one_base_exactly_one_wins_and_nothing_is_destroyed() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "shared", None)
        .await
        .unwrap();
    let base = store
        .writer
        .upsert_page(page(ws, proj, "notes/plan.md", "Plan", "# Plan\n\nbase"))
        .await
        .unwrap();

    let (a, b) = tokio::join!(
        store.writer.upsert_page_if(
            page(
                ws,
                proj,
                "notes/plan.md",
                "Plan",
                "# Plan\n\nfrom harness A"
            ),
            PagePrecondition::Latest(base),
        ),
        store.writer.upsert_page_if(
            page(
                ws,
                proj,
                "notes/plan.md",
                "Plan",
                "# Plan\n\nfrom harness B"
            ),
            PagePrecondition::Latest(base),
        ),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    let winners: Vec<_> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            ConditionalUpsert::Upserted(id) => Some(*id),
            ConditionalUpsert::Mismatch { .. } => None,
        })
        .collect();
    assert_eq!(winners.len(), 1, "{outcomes:?}");
    assert!(
        outcomes.contains(&ConditionalUpsert::Mismatch {
            current: Some(winners[0])
        }),
        "the loser is told the winner's version: {outcomes:?}"
    );

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let rows: Vec<(String, i64)> = conn
        .prepare(
            "SELECT body, is_latest FROM pages WHERE path = 'notes/plan.md' ORDER BY created_at",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "base plus one winner, no loser row: {rows:?}"
    );
    assert_eq!((rows[0].0.as_str(), rows[0].1), ("# Plan\n\nbase", 0));
    assert_eq!(rows[1].1, 1);

    // A precondition naming no page, against an existing page, writes nothing.
    assert_eq!(
        store
            .writer
            .upsert_page_if(
                page(ws, proj, "notes/plan.md", "Plan", "# Plan\n\nfresh"),
                PagePrecondition::Absent,
            )
            .await
            .unwrap(),
        ConditionalUpsert::Mismatch {
            current: Some(winners[0])
        }
    );
}

/// #1033 under invariant #16: two operators on two clones of one repository,
/// in folders named differently, use the default `path` style and capture at
/// the same time. They must land in one project — the same name and the same row —
/// so each reads what the other writes; a race must not split the repository.
#[tokio::test]
async fn two_operators_on_two_clones_of_one_repository_share_its_path_named_project() {
    use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let repo = RepositoryIdentity {
        identity: "github.com/acme/api".into(),
        source: IdentitySource::GitRemote,
    };
    let clone = |folder: &'static str, path: &'static str| {
        let writer = store.writer.clone();
        let repo = repo.clone();
        async move {
            writer
                .resolve_project_by_identity_for_capture(
                    ws,
                    repo,
                    IdentityStyle::Path,
                    folder,
                    Some(path.to_owned()),
                    None,
                    None,
                )
                .await
                .unwrap()
                .0
        }
    };
    let (alice, bob) = tokio::join!(
        clone("api", "/home/alice/src/api"),
        clone("api-main", "/srv/bob/worktrees/api-main"),
    );
    assert_eq!(alice, bob, "two clones of one repository share one project");
    assert_eq!(
        store
            .reader
            .find_project(ws, "acme-api".into())
            .await
            .unwrap(),
        Some(alice)
    );
    for folder in ["api", "api-main"] {
        assert!(
            store
                .reader
                .find_project(ws, folder.into())
                .await
                .unwrap()
                .is_none(),
            "no per-folder fragment for {folder}"
        );
    }

    store
        .writer
        .upsert_page(page(ws, alice, "notes/shared.md", "Shared", "alice's note"))
        .await
        .unwrap();
    let seen = store
        .reader
        .page_body_by_ids(ws, bob, "notes/shared.md")
        .await
        .unwrap()
        .expect("bob reads alice's page in the shared project");
    assert!(seen.body.contains("alice's note"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_operators_promote_two_clone_keys_to_one_uuid_without_moving_active_pointers() {
    use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
    use ai_memory_core::{ActiveProject, ActiveProjectMode, ActorKey};
    use ai_memory_store::{ProjectPrincipal, ScopeResolver};
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    let repo = RepositoryIdentity {
        identity: "github.com/acme/api".into(),
        source: IdentitySource::GitRemote,
    };
    store
        .writer
        .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let project = store
        .writer
        .resolve_project_by_identity(
            ws,
            repo,
            IdentityStyle::HostPath,
            "api",
            Some("/home/alice/api".into()),
            None,
            None,
        )
        .await
        .unwrap()
        .0;
    let alice = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice-promotion".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let bob = store
        .writer
        .create_human_user(
            NewUser {
                username: "bob-promotion".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let active = ActiveProject::with_config(
        ActiveProjectMode::PerActor,
        Duration::from_secs(60),
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    );
    let alice_actor = ActorKey {
        user: Some("user:alice-promotion".into()),
        session_id: Some("alice-clone".into()),
    };
    let bob_actor = ActorKey {
        user: Some("user:bob-promotion".into()),
        session_id: Some("bob-clone".into()),
    };
    active.set_for(&alice_actor, ws, project, false);
    active.set_for(&bob_actor, ws, project, false);
    store
        .writer
        .grant_memory(alice, project, ai_memory_store::GrantLevel::Write, None)
        .await
        .unwrap();
    store
        .writer
        .grant_memory(bob, project, ai_memory_store::GrantLevel::Read, None)
        .await
        .unwrap();
    let resolve = |viewer, actor: ActorKey| {
        let reader = store.reader.clone();
        let writer = store.writer.clone();
        let active = active.clone();
        async move {
            ScopeResolver::new(&reader, ws, project)
                .with_writer(&writer)
                .with_active_project(&active)
                .with_project_authz(ProjectPrincipal::user(viewer), true)
                .resolve_write_args(Some("acme"), Some("acme-api"), &actor)
                .await
        }
    };
    let (alice_scope, bob_write) = tokio::join!(
        resolve(alice, alice_actor.clone()),
        resolve(bob, bob_actor.clone())
    );
    let alice_scope = alice_scope.unwrap();
    assert!(bob_write.unwrap_err().is_forbidden());
    let bob_scope = ScopeResolver::new(&store.reader, ws, project)
        .with_active_project(&active)
        .with_project_authz(ProjectPrincipal::user(bob), true)
        .resolve_read_args(Some("acme"), Some("api"), &bob_actor)
        .await
        .unwrap();
    assert_eq!(alice_scope.scope.project_id, project);
    assert_eq!(bob_scope.project_id, project);
    assert_eq!(active.get_for(&alice_actor), Some((ws, project)));
    assert_eq!(active.get_for(&bob_actor), Some((ws, project)));

    let mut authored = page(
        ws,
        alice_scope.scope.project_id,
        "notes/promoted.md",
        "Promoted",
        "alice wrote after promotion",
    );
    authored.author_id = Some(alice);
    store.writer.upsert_page(authored).await.unwrap();
    let shared = store
        .reader
        .page_body_by_ids(ws, bob_scope.project_id, "notes/promoted.md")
        .await
        .unwrap()
        .unwrap();
    assert!(shared.body.contains("alice wrote after promotion"));
}

/// The collaboration guarantee, and the reason a team can use one server:
/// what Alice writes, Carol reads.
///
/// `pages.author_id` exists for attribution and must never become a read
/// filter. If it ever does, this fails — and a team silently stops sharing
/// knowledge while every single-user test still passes.
#[tokio::test]
async fn one_operators_page_is_readable_by_another_in_the_same_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "decisions/0001.md",
            "Chose SQLite",
            "We picked SQLite for the derived index.",
        ))
        .await
        .unwrap();

    // Carol's read of the same project: no owner coordinate involved.
    let hits = store
        .reader
        .search_pages("SQLite".to_string(), 10, None)
        .await
        .unwrap();

    assert!(
        hits.iter().any(|h| h.path.as_str() == "decisions/0001.md"),
        "a page written in this project must be visible to any operator \
         reading it; got {:?}",
        hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>()
    );

    // …and readable in full, not just rankable.
    let body = store
        .reader
        .page_body_by_ids(ws, proj, "decisions/0001.md")
        .await
        .unwrap()
        .expect("the page resolves by path for any reader");
    assert!(body.body.contains("We picked SQLite"));
}

/// The stronger form of the collaboration guarantee: the existing sibling
/// test writes with `author_id: None`, so it cannot tell an "authored pages
/// are private" regression from a genuine bug — a filter keyed on the
/// caller's identity would happily let a NULL-authored page through. This
/// stamps a real, non-null `author_id` (operator A) and asserts operator B —
/// a *different* identity, reading with no owner coordinate at all, exactly
/// as `search_pages_for_project` and `page_body_by_ids` are shaped — still
/// sees the page in full, through both the search path and the direct-body
/// path. `pages.author_id` is attribution, never a read filter.
#[tokio::test]
async fn an_authored_page_is_readable_by_a_different_operator_via_search_and_body() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let operator_a = store
        .writer
        .create_human_user(
            NewUser {
                username: "operator-a".into(),
                name: Some("Operator A".into()),
                email: Some("operator-a@example.com".into()),
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();

    store
        .writer
        .upsert_page(NewPage {
            author_id: Some(operator_a),
            ..page(
                ws,
                proj,
                "decisions/0002.md",
                "Chose SQLite Again",
                "We picked SQLite for the derived index, authored by operator A.",
            )
        })
        .await
        .unwrap();

    // Operator B's read: no owner coordinate passed anywhere, because none of
    // these signatures accept one — that absence IS the invariant.
    let hits = store
        .reader
        .search_pages_for_project(ws, proj, "SQLite Again".to_string(), 10, None)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.path.as_str() == "decisions/0002.md"),
        "an authored page must be visible to a different operator's search; \
         got {:?}",
        hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>()
    );

    let body = store
        .reader
        .page_body_by_ids(ws, proj, "decisions/0002.md")
        .await
        .unwrap()
        .expect("a different operator can still resolve the page by path");
    assert!(body.body.contains("authored by operator A"));
}

/// Two harnesses editing the same page keep both versions.
///
/// The latest write wins the `is_latest` flag — there is no merge, and none is
/// claimed — but the superseded version stays reachable through the chain.
/// "Last write wins" must never mean "the other version is gone".
#[tokio::test]
async fn concurrent_writes_to_one_path_supersede_rather_than_destroy() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let first = store
        .writer
        .upsert_page(page(ws, proj, "notes/shared.md", "Shared", "alice's text"))
        .await
        .unwrap();
    let second = store
        .writer
        .upsert_page(page(ws, proj, "notes/shared.md", "Shared", "carol's text"))
        .await
        .unwrap();

    assert_ne!(first, second, "a divergent write creates a new version");

    let latest = store
        .reader
        .page_body_by_ids(ws, proj, "notes/shared.md")
        .await
        .unwrap()
        .expect("the path still resolves");
    assert!(
        latest.body.contains("carol's text"),
        "the later write is the latest version"
    );

    let latest_id = store
        .reader
        .latest_page_id_by_ids(ws, proj, "notes/shared.md".to_string())
        .await
        .unwrap()
        .expect("a latest version exists");
    assert_eq!(latest_id, second, "the later write holds is_latest");

    // The overwritten version is still a row, reachable by its own id.
    let earlier_survives = store
        .reader
        .with_conn(move |conn| {
            let body: String = conn.query_row(
                "SELECT body FROM pages WHERE id = ?1",
                rusqlite::params![&first.as_bytes()[..]],
                |r| r.get(0),
            )?;
            Ok(body)
        })
        .await
        .unwrap();
    assert!(
        earlier_survives.contains("alice's text"),
        "the overwritten version must survive in the supersession chain, \
         not be destroyed"
    );
}

/// The history readers behind the web page-history view: every version at a
/// path, the overwritten one included, and nothing from another project or
/// workspace at the same path. A foreign version id reads as `None`, the
/// same as an unknown one.
#[tokio::test]
async fn page_history_keeps_the_overwritten_version_and_stays_in_scope() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let sibling = store
        .writer
        .get_or_create_project(ws, "sibling".to_string(), None)
        .await
        .unwrap();
    let far_ws = store
        .writer
        .get_or_create_workspace("far".to_string())
        .await
        .unwrap();
    let far = store
        .writer
        .get_or_create_project(far_ws, "shared-app".to_string(), None)
        .await
        .unwrap();
    let path = "notes/shared.md";
    let first = store
        .writer
        .upsert_page(page(ws, proj, path, "Shared", "alice's text"))
        .await
        .unwrap();
    let second = store
        .writer
        .upsert_page(page(ws, proj, path, "Shared", "carol's text"))
        .await
        .unwrap();
    let foreign = store
        .writer
        .upsert_page(page(ws, sibling, path, "Shared", "sibling text"))
        .await
        .unwrap();
    let far_id = store
        .writer
        .upsert_page(page(far_ws, far, path, "Shared", "far text"))
        .await
        .unwrap();

    let history = store
        .reader
        .page_versions(ws, proj, path.to_string(), 50, 0)
        .await
        .unwrap();
    assert_eq!(history.total, 2);
    let ids: Vec<_> = history.versions.iter().map(|v| v.id).collect();
    assert_eq!(
        ids,
        [second, first],
        "newest first, the overwritten one kept"
    );
    assert_eq!(history.versions[0].supersedes, Some(first));
    assert!(history.versions[0].is_latest && !history.versions[1].is_latest);

    let body = |version| {
        let reader = store.reader.clone();
        async move {
            reader
                .page_version_body(ws, proj, path.to_string(), version)
                .await
                .unwrap()
        }
    };
    assert!(body(first).await.unwrap().body.contains("alice's text"));
    assert_eq!(body(second).await.unwrap().supersedes, Some(first));
    assert!(body(foreign).await.is_none(), "a sibling project's version");
    assert!(body(far_id).await.is_none(), "another workspace's version");
    assert!(
        store
            .reader
            .page_version_body(ws, proj, "notes/other.md".to_string(), first)
            .await
            .unwrap()
            .is_none(),
        "a version under a path it does not belong to"
    );
}

/// The batched path-existence probe the consolidation link guard validates
/// proposed cross-scope wikilinks against (#1218): it reports exactly the
/// paths with a latest row, in any input order, and never invents one.
#[tokio::test]
async fn existing_latest_paths_reports_exactly_the_live_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .upsert_page(page(ws, proj, "notes/real.md", "Real", "a real page"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(ws, proj, "decisions/adr.md", "ADR", "a decision"))
        .await
        .unwrap();

    let mut found = store
        .reader
        .existing_latest_paths(
            ws,
            proj,
            vec![
                "notes/missing.md".to_string(),
                "decisions/adr.md".to_string(),
                "notes/real.md".to_string(),
                // A duplicate must not double-report.
                "notes/real.md".to_string(),
            ],
        )
        .await
        .unwrap();
    found.sort();
    assert_eq!(found, vec!["decisions/adr.md", "notes/real.md"]);

    // A superseded path still counts: the latest version is the one a link
    // resolves to.
    store
        .writer
        .upsert_page(page(ws, proj, "notes/real.md", "Real", "rewritten"))
        .await
        .unwrap();
    let found = store
        .reader
        .existing_latest_paths(ws, proj, vec!["notes/real.md".to_string()])
        .await
        .unwrap();
    assert_eq!(found, vec!["notes/real.md"]);

    // A foreign scope never sees this project's pages.
    let other_proj = store
        .writer
        .get_or_create_project(ws, "other-app".to_string(), None)
        .await
        .unwrap();
    let found = store
        .reader
        .existing_latest_paths(ws, other_proj, vec!["notes/real.md".to_string()])
        .await
        .unwrap();
    assert!(found.is_empty());

    // Empty input is a no-op, not an error.
    let found = store
        .reader
        .existing_latest_paths(ws, proj, Vec::new())
        .await
        .unwrap();
    assert!(found.is_empty());

    // An expired page is never reported as live.
    let mut expired = page(ws, proj, "notes/expired.md", "Expired", "expired page");
    expired.expires_at = Some(jiff::Timestamp::from_second(1).unwrap());
    store.writer.upsert_page(expired).await.unwrap();

    let mut future_exp = page(ws, proj, "notes/future.md", "Future", "future page");
    future_exp.expires_at = Some(jiff::Timestamp::from_second(4_102_444_800).unwrap());
    store.writer.upsert_page(future_exp).await.unwrap();

    let mut found = store
        .reader
        .existing_latest_paths(
            ws,
            proj,
            vec![
                "notes/expired.md".to_string(),
                "notes/future.md".to_string(),
            ],
        )
        .await
        .unwrap();
    found.sort();
    assert_eq!(found, vec!["notes/future.md"]);
}

/// The link guard's path probe is no existence oracle for a restricted
/// project (#1218): a restricted target reports nothing whether or not the
/// path exists, so a link that survives consolidation tells a reader of the
/// source page nothing about a project they may not read. Reopening the
/// project is the control: the same probe then sees the page.
#[tokio::test]
async fn existing_latest_paths_reports_nothing_for_a_restricted_project() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, _proj) = scope(&store).await;
    let secret = store
        .writer
        .get_or_create_project(ws, "secret-app".to_string(), None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(ws, secret, "notes/plan.md", "Plan", "private plan"))
        .await
        .unwrap();
    let probe = || {
        store.reader.existing_latest_paths(
            ws,
            secret,
            vec!["notes/plan.md".to_string(), "notes/absent.md".to_string()],
        )
    };

    store
        .writer
        .set_access_mode(secret, ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    assert!(
        probe().await.unwrap().is_empty(),
        "a restricted project must answer the same for a real and an absent path"
    );

    store
        .writer
        .set_access_mode(secret, ai_memory_store::AccessMode::Open)
        .await
        .unwrap();
    assert_eq!(probe().await.unwrap(), vec!["notes/plan.md"]);
}

/// Re-writing identical content must not churn a new version.
///
/// Two harnesses syncing the same file would otherwise manufacture a version
/// per pass and bloat the chain with nothing to show for it.
#[tokio::test]
async fn an_identical_rewrite_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let first = store
        .writer
        .upsert_page(page(ws, proj, "notes/same.md", "Same", "identical body"))
        .await
        .unwrap();
    let again = store
        .writer
        .upsert_page(page(ws, proj, "notes/same.md", "Same", "identical body"))
        .await
        .unwrap();

    assert_eq!(
        first, again,
        "identical content must return the same version, not create one"
    );
}

/// A handoff is a baton: exactly one session takes it.
///
/// The pre-existing unit test asserted only that a second accept does not
/// *error*, which would still pass if the second accept overwrote
/// `accepted_by`. This asserts the property that actually matters — the first
/// accepter keeps it.
#[tokio::test]
async fn a_second_accept_cannot_steal_an_accepted_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            from_session_id: None,
            summary: "pick this up".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: None,
        })
        .await
        .unwrap();

    let accept = |agent: AgentKind, session: SessionId| HandoffAcceptance {
        handoff_id: id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: agent,
        accepting_session: Some(session),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    };

    let winner = open_session(&store, ws, proj, AgentKind::Codex).await;
    let loser = open_session(&store, ws, proj, AgentKind::ClaudeCode).await;

    let first = store
        .writer
        .accept_handoff(accept(AgentKind::Codex, winner))
        .await
        .unwrap();
    assert!(first, "the first accept claims the baton");

    let second = store
        .writer
        .accept_handoff(accept(AgentKind::ClaudeCode, loser))
        .await
        .unwrap();
    assert!(
        !second,
        "a second accept must report that it claimed nothing"
    );

    let handoff_bytes = id.as_bytes().to_vec();
    let (accepted_by, accepted_session): (Option<String>, Option<Vec<u8>>) = store
        .reader
        .with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT accepted_by, accepted_by_session FROM handoffs WHERE id = ?1",
                rusqlite::params![handoff_bytes],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();

    assert_eq!(
        accepted_by.as_deref(),
        Some("codex"),
        "the original accepter must still own the handoff"
    );
    assert_eq!(
        accepted_session.as_deref(),
        Some(&winner.as_bytes()[..]),
        "and the winning session must not have been overwritten by the loser"
    );
}

/// Two parallel sessions in separate Tokio worker threads concurrently race
/// to claim the exact same open handoff baton via `tokio::join!`.
///
/// Invariant: Under genuine multi-threaded concurrency, the single-writer
/// transaction guard (`WHERE state = 'open'`) must ensure that exactly one
/// session claims the baton (`true`) and the loser receives `false`.
/// The winning session's attribution is committed atomically and cannot be
/// overwritten or corrupted by the losing task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_accept_race_on_single_handoff_yields_exactly_one_winner() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            from_session_id: None,
            summary: "concurrent race for the baton".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: None,
        })
        .await
        .unwrap();

    let accept = |agent: AgentKind, session: SessionId| HandoffAcceptance {
        handoff_id: id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: agent,
        accepting_session: Some(session),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    };

    let session_a = open_session(&store, ws, proj, AgentKind::Codex).await;
    let session_b = open_session(&store, ws, proj, AgentKind::OpenCode).await;

    let (res_a, res_b) = tokio::join!(
        store
            .writer
            .accept_handoff(accept(AgentKind::Codex, session_a)),
        store
            .writer
            .accept_handoff(accept(AgentKind::OpenCode, session_b)),
    );
    let outcome_a = res_a.unwrap();
    let outcome_b = res_b.unwrap();

    // Invariant: Exactly one must succeed, and the other must be refused.
    assert!(
        (outcome_a && !outcome_b) || (!outcome_a && outcome_b),
        "exactly one session must win the claim; got outcome_a={outcome_a}, outcome_b={outcome_b}"
    );

    let (winner_agent, winner_session, loser_agent, loser_session) = if outcome_a {
        ("codex", session_a, AgentKind::OpenCode, session_b)
    } else {
        ("opencode", session_b, AgentKind::Codex, session_a)
    };

    let handoff_bytes = id.as_bytes().to_vec();
    let (state, accepted_by, accepted_session): (String, Option<String>, Option<Vec<u8>>) = store
        .reader
        .with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT state, accepted_by, accepted_by_session FROM handoffs WHERE id = ?1",
                rusqlite::params![handoff_bytes],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .unwrap();

    assert_eq!(
        state, "accepted",
        "the baton state must be transitioned to accepted"
    );
    assert_eq!(
        accepted_by.as_deref(),
        Some(winner_agent),
        "the winner agent must be recorded as accepted_by"
    );
    assert_eq!(
        accepted_session.as_deref(),
        Some(&winner_session.as_bytes()[..]),
        "the winning session id must be recorded in accepted_by_session"
    );

    // Subsequent retry by the loser must be cleanly rejected without modifying anything.
    let retry = store
        .writer
        .accept_handoff(accept(loser_agent, loser_session))
        .await
        .unwrap();
    assert!(
        !retry,
        "subsequent retry by the losing session must return false"
    );
}

/// The receiver named in a claim must be a real session of the claim's own
/// workspace, project and agent kind. A session from a sibling project, from
/// another workspace (even one using the same project name), of a different
/// agent kind, or one that does not exist must be refused with `InvalidState`
/// and must leave the baton open and untouched; a matching receiver still
/// claims it, and nobody can claim it a second time.
#[tokio::test]
async fn handoff_accept_rejects_foreign_or_missing_receiver_session() {
    /// The columns a claim writes, read straight from the row.
    #[derive(Debug, PartialEq)]
    struct ClaimColumns {
        state: HandoffState,
        accepted_by: Option<String>,
        accepted_at: Option<i64>,
        accepted_by_session: Option<Vec<u8>>,
        accepted_by_user: Option<String>,
    }

    async fn claim_columns(store: &Store, id: ai_memory_core::HandoffId) -> ClaimColumns {
        let handoff_bytes = id.as_bytes().to_vec();
        store
            .reader
            .with_conn(move |conn| {
                Ok(conn.query_row(
                    "SELECT state, accepted_by, accepted_at, accepted_by_session, \
                            accepted_by_user \
                     FROM handoffs WHERE id = ?1",
                    rusqlite::params![handoff_bytes],
                    |r| {
                        Ok(ClaimColumns {
                            state: r.get::<_, String>(0)?.parse().unwrap(),
                            accepted_by: r.get(1)?,
                            accepted_at: r.get(2)?,
                            accepted_by_session: r.get(3)?,
                            accepted_by_user: r.get(4)?,
                        })
                    },
                )?)
            })
            .await
            .unwrap()
    }

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let sibling_proj = store
        .writer
        .get_or_create_project(ws, "sibling-app".to_string(), None)
        .await
        .unwrap();
    let rival_ws = store
        .writer
        .get_or_create_workspace("rival".to_string())
        .await
        .unwrap();
    // Same project name as the real one, in another workspace: a lookup by
    // name alone would conflate the two.
    let rival_proj = store
        .writer
        .get_or_create_project(rival_ws, "shared-app".to_string(), None)
        .await
        .unwrap();

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            from_session_id: None,
            summary: "pick this up".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: None,
        })
        .await
        .unwrap();

    // Every claim names the handoff's own scope and `ClaudeCode`; only the
    // receiver session varies.
    let claim = |session: SessionId| HandoffAcceptance {
        handoff_id: id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: AgentKind::ClaudeCode,
        accepting_session: Some(session),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    };

    let valid = open_session(&store, ws, proj, AgentKind::ClaudeCode).await;
    let other_project = open_session(&store, ws, sibling_proj, AgentKind::ClaudeCode).await;
    let other_workspace = open_session(&store, rival_ws, rival_proj, AgentKind::ClaudeCode).await;
    let wrong_agent = open_session(&store, ws, proj, AgentKind::Codex).await;
    let missing = SessionId::new();

    let open = ClaimColumns {
        state: HandoffState::Open,
        accepted_by: None,
        accepted_at: None,
        accepted_by_session: None,
        accepted_by_user: None,
    };
    assert_eq!(claim_columns(&store, id).await, open, "fixture starts open");

    for (label, receiver) in [
        ("a session of a sibling project", other_project),
        ("a session of another workspace", other_workspace),
        ("a session of a different agent kind", wrong_agent),
        ("a session that does not exist", missing),
    ] {
        let err = store
            .writer
            .accept_handoff(claim(receiver))
            .await
            .expect_err(label);
        assert!(
            matches!(&err, StoreError::InvalidState(_)),
            "{label} must be refused as InvalidState, got {err:?}"
        );
        assert_eq!(
            claim_columns(&store, id).await,
            open,
            "{label} must leave the handoff open and unclaimed"
        );
    }

    // Control: the matching receiver takes the baton.
    assert!(
        store.writer.accept_handoff(claim(valid)).await.unwrap(),
        "a receiver in the handoff's own workspace, project and agent kind claims it"
    );
    let accepted = claim_columns(&store, id).await;
    assert_eq!(accepted.state, HandoffState::Accepted);
    assert_eq!(
        accepted.accepted_by.as_deref(),
        Some(AgentKind::ClaudeCode.as_str())
    );
    assert_eq!(
        accepted.accepted_by_session.as_deref(),
        Some(&valid.as_bytes()[..])
    );
    assert!(accepted.accepted_at.is_some(), "the claim is timestamped");
    assert_eq!(
        accepted.accepted_by_user, None,
        "the claim named no operator"
    );

    // Nobody claims it twice: neither the same receiver retrying nor another
    // matching session (the latter reaches the `state = 'open'` guards rather
    // than the per-receiver early return).
    let second_match = open_session(&store, ws, proj, AgentKind::ClaudeCode).await;
    for (label, receiver) in [
        ("the same receiver retrying", valid),
        ("a second matching session", second_match),
    ] {
        assert!(
            !store.writer.accept_handoff(claim(receiver)).await.unwrap(),
            "{label} must not claim an accepted handoff"
        );
        assert_eq!(
            claim_columns(&store, id).await,
            accepted,
            "{label} must leave the first claim intact"
        );
    }
}

/// Ownership still applies to batons even though pages are shared: the two
/// halves of the model must not collapse into each other.
#[tokio::test]
async fn an_owned_handoff_stays_with_its_owner_while_pages_stay_shared() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            from_session_id: None,
            summary: "alice's baton".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: owner_stamp(Some(&IdentityKey::User("alice".into())), true),
        })
        .await
        .unwrap();

    let carol = OwnerFilter::for_actor_context(&ActorContext {
        user: Some("carol".into()),
        ..ActorContext::default()
    });

    let stolen = store
        .writer
        .accept_handoff(HandoffAcceptance {
            handoff_id: id,
            workspace_id: ws,
            project_id: proj,
            accepting_agent: AgentKind::Codex,
            accepting_session: Some(open_session(&store, ws, proj, AgentKind::Codex).await),
            accepting_user: Some(operator("carol")),
            owner_filter: carol,
            receiving_cwd: None,
        })
        .await
        .unwrap();

    assert!(
        !stolen,
        "carol must not be able to accept a baton owned by {}",
        operator("alice")
    );
}

/// Grok reuses one session id across a SessionEnd→restart, so
/// `accept_handoff` must reopen an already-ended receiver session instead of
/// rejecting it (#840) — but only *after* the exactly-once claim guard, so the
/// resurrection can never become a way to steal an already-taken baton.
#[tokio::test]
async fn accept_reopens_an_ended_receiver_session_but_keeps_claim_once() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_agent: AgentKind::Grok,
            to_agent: None,
            from_session_id: None,
            summary: "resume after restart".into(),
            next_steps: Vec::new(),
            open_questions: Vec::new(),
            files_touched: Vec::new(),
            cwd: None,
            owner_user: None,
        })
        .await
        .unwrap();

    let accept = |session: SessionId| HandoffAcceptance {
        handoff_id: id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: AgentKind::Grok,
        accepting_session: Some(session),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    };

    // The same Grok session id: opened, then ended (SessionEnd), then reused
    // when the conversation restarts and calls its first tool.
    let grok = open_session(&store, ws, proj, AgentKind::Grok).await;
    store.writer.end_session(grok, None).await.unwrap();

    let claimed = store.writer.accept_handoff(accept(grok)).await.unwrap();
    assert!(
        claimed,
        "an ended session that reuses its id must be able to accept the handoff"
    );

    // The receiver row was reopened (ended_at cleared), not left a corpse.
    let grok_bytes = grok.as_bytes().to_vec();
    let ended_at: Option<i64> = store
        .reader
        .with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                rusqlite::params![grok_bytes],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        ended_at.is_none(),
        "accepting a handoff must reopen the ended receiver session"
    );

    // Claim-once still holds: a *different* session cannot steal the baton the
    // reopened session already took, even though the loser is wide open.
    let loser = open_session(&store, ws, proj, AgentKind::Codex).await;
    let stolen = store
        .writer
        .accept_handoff(HandoffAcceptance {
            handoff_id: id,
            workspace_id: ws,
            project_id: proj,
            accepting_agent: AgentKind::Codex,
            accepting_session: Some(loser),
            accepting_user: None,
            owner_filter: OwnerFilter::Any,
            receiving_cwd: None,
        })
        .await
        .unwrap();
    assert!(
        !stolen,
        "the resurrection path must not let a second session steal an accepted baton"
    );
}

/// Kimi Code reuses its native session id when a session is resumed after
/// SessionEnd (observed in production 2026-10: one kimi session row carried a
/// SessionEnd and then fresh session-start/user-prompt observations under the
/// same id), so it needs the same reopen exemption as Grok — without weakening
/// any guard around it: other agents stay final-after-end, a receiver in a
/// different project is still refused, and claim-once still binds the resumed
/// session.
#[tokio::test]
async fn accept_reopens_an_ended_kimi_receiver_but_keeps_every_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let new_handoff = |summary: &str| NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        from_session_id: None,
        summary: summary.into(),
        next_steps: Vec::new(),
        open_questions: Vec::new(),
        files_touched: Vec::new(),
        cwd: None,
        owner_user: None,
    };
    let accept = |id, agent: AgentKind, session: SessionId, ws, proj| HandoffAcceptance {
        handoff_id: id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: agent,
        accepting_session: Some(session),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: None,
    };

    // Adversarial control: an ended Claude Code session is still final — the
    // reuse exemption must not leak to agents that never reuse ids.
    let claude = open_session(&store, ws, proj, AgentKind::ClaudeCode).await;
    store.writer.end_session(claude, None).await.unwrap();
    let claude_baton = store
        .writer
        .insert_handoff(new_handoff("claude control"))
        .await
        .unwrap();
    let err = store
        .writer
        .accept_handoff(accept(
            claude_baton,
            AgentKind::ClaudeCode,
            claude,
            ws,
            proj,
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidState(_)),
        "an ended claude-code session must still be refused: {err}"
    );

    // The resumed kimi-code session: ended at SessionEnd, then the same
    // native id comes back on resume and must be able to take the baton.
    let kimi = open_session(&store, ws, proj, AgentKind::KimiCode).await;
    store.writer.end_session(kimi, None).await.unwrap();

    let id = store
        .writer
        .insert_handoff(new_handoff("resume after restart"))
        .await
        .unwrap();
    let claimed = store
        .writer
        .accept_handoff(accept(id, AgentKind::KimiCode, kimi, ws, proj))
        .await
        .unwrap();
    assert!(
        claimed,
        "a resumed kimi-code session must be able to accept the handoff"
    );

    // The receiver row was reopened (ended_at cleared), not left a corpse.
    let kimi_bytes = kimi.as_bytes().to_vec();
    let ended_at: Option<i64> = store
        .reader
        .with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT ended_at FROM sessions WHERE id = ?1",
                rusqlite::params![kimi_bytes],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        ended_at.is_none(),
        "accepting a handoff must reopen the ended kimi-code receiver session"
    );

    // Claim-once still binds kimi: the same resumed session must not take a
    // second baton.
    let second = store
        .writer
        .insert_handoff(new_handoff("a second baton"))
        .await
        .unwrap();
    let claimed_again = store
        .writer
        .accept_handoff(accept(second, AgentKind::KimiCode, kimi, ws, proj))
        .await
        .unwrap();
    assert!(
        !claimed_again,
        "a kimi-code session that already claimed a handoff must not claim another"
    );

    // Adversarial control: a kimi-code session living in a DIFFERENT project
    // must not claim this project's baton (scope isolation).
    let other_proj = store
        .writer
        .get_or_create_project(ws, "other-app".to_string(), None)
        .await
        .unwrap();
    let foreign = open_session(&store, ws, other_proj, AgentKind::KimiCode).await;
    let third = store
        .writer
        .insert_handoff(new_handoff("foreign attempt"))
        .await
        .unwrap();
    let err = store
        .writer
        .accept_handoff(accept(third, AgentKind::KimiCode, foreign, ws, proj))
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidState(_)),
        "a kimi-code session from another project must be refused: {err}"
    );
}

/// Parallel live sessions in one directory each own a turn-checkpoint baton.
/// One session's checkpoint, and a receiver claiming it, must leave the other
/// live session's baton open: before this held, every completed turn retired
/// the other sessions' batons and the survivor went to whoever started next.
#[tokio::test]
async fn parallel_live_sessions_keep_their_own_checkpoint_batons() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let baton = |session: SessionId, summary: &str| NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: Some(session),
        from_agent: AgentKind::OpenCode,
        to_agent: None,
        cwd: Some("/repo".into()),
        summary: summary.into(),
        open_questions: Vec::new(),
        next_steps: Vec::new(),
        files_touched: Vec::new(),
        owner_user: None,
    };
    let alpha = open_session(&store, ws, proj, AgentKind::OpenCode).await;
    let beta = open_session(&store, ws, proj, AgentKind::OpenCode).await;
    let alpha_baton = store
        .writer
        .checkpoint_session_handoff(baton(alpha, "alpha"))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let beta_baton = store
        .writer
        .checkpoint_session_handoff(baton(beta, "beta"))
        .await
        .unwrap()
        .unwrap();
    let state = |id| {
        let reader = store.reader.clone();
        async move {
            reader
                .handoff_by_id(id)
                .await
                .unwrap()
                .unwrap()
                .lifecycle
                .state
        }
    };
    assert_eq!(state(alpha_baton).await, HandoffState::Open);

    let receiver = open_session(&store, ws, proj, AgentKind::OpenCode).await;
    let claimed = store
        .writer
        .accept_handoff(HandoffAcceptance {
            handoff_id: beta_baton,
            workspace_id: ws,
            project_id: proj,
            accepting_agent: AgentKind::OpenCode,
            accepting_session: Some(receiver),
            accepting_user: None,
            owner_filter: OwnerFilter::Any,
            receiving_cwd: Some("/repo".into()),
        })
        .await
        .unwrap();
    assert!(claimed);
    assert_eq!(
        state(alpha_baton).await,
        HandoffState::Open,
        "claiming one live session's baton must not sweep another's"
    );

    // Once alpha ends, its baton is an ordinary SessionEnd baton again and the
    // same-cwd supersession applies to it.
    store.writer.end_session(alpha, None).await.unwrap();
    let gamma = open_session(&store, ws, proj, AgentKind::OpenCode).await;
    store
        .writer
        .checkpoint_session_handoff(baton(gamma, "gamma"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state(alpha_baton).await, HandoffState::Expired);
}

/// Automatic delivery re-checks the source inside the claim, and the claim's
/// sweep retires batons of quiet (abandoned) open sessions while sparing one
/// still in use. The selection runs on a reader before the writer claims, so
/// a source can resume in between; and OpenCode sessions that never end would
/// otherwise leave one open baton each, surfacing older conversations one by
/// one to later sessions.
#[tokio::test]
async fn startup_claim_rechecks_the_source_and_sweeps_only_quiet_open_batons() {
    use ai_memory_core::{NewObservation, ObservationKind, Sanitized, Sanitizer};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let mut batons = Vec::new();
    // Checkpoint order, oldest first: busy, abandoned, quiet.
    for summary in ["busy", "abandoned", "quiet"] {
        let session = open_session(&store, ws, proj, AgentKind::OpenCode).await;
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id: session,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: summary.into(),
                    importance: 5,

                    occurred_at: None,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
        let id = store
            .writer
            .checkpoint_session_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: Some(session),
                from_agent: AgentKind::OpenCode,
                to_agent: None,
                cwd: Some("/repo".into()),
                summary: summary.into(),
                open_questions: Vec::new(),
                next_steps: Vec::new(),
                files_touched: Vec::new(),
                owner_user: None,
            })
            .await
            .unwrap()
            .unwrap();
        batons.push((session, id));
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let [
        (_, busy),
        (abandoned_session, abandoned),
        (quiet_session, quiet),
    ] = batons[..]
    else {
        unreachable!()
    };
    // Only "busy" captured anything in the last hour.
    let hour_ago = (jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1)).as_microsecond();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    for session in [abandoned_session, quiet_session] {
        conn.execute(
            "UPDATE observations SET created_at = ?1 WHERE session_id = ?2",
            rusqlite::params![hour_ago, session.as_bytes()],
        )
        .unwrap();
    }
    drop(conn);
    let cutoff = jiff::Timestamp::now() - jiff::SignedDuration::from_mins(10);
    let claim = |handoff_id, receiver| HandoffAcceptance {
        handoff_id,
        workspace_id: ws,
        project_id: proj,
        accepting_agent: AgentKind::OpenCode,
        accepting_session: Some(receiver),
        accepting_user: None,
        owner_filter: OwnerFilter::Any,
        receiving_cwd: Some("/repo".into()),
    };
    let state = |id| {
        let reader = store.reader.clone();
        async move {
            reader
                .handoff_by_id(id)
                .await
                .unwrap()
                .unwrap()
                .lifecycle
                .state
        }
    };

    // Selected earlier, but its source is in use by the time of the claim.
    let receiver = open_session(&store, ws, proj, AgentKind::OpenCode).await;
    let raced = store
        .writer
        .accept_startup_context(Some(claim(busy, receiver)), None, None, cutoff)
        .await
        .unwrap();
    assert!(!raced.handoff_accepted, "a source in use keeps its baton");
    assert_eq!(state(busy).await, HandoffState::Open);

    let receiver = open_session(&store, ws, proj, AgentKind::OpenCode).await;
    let delivered = store
        .writer
        .accept_startup_context(Some(claim(quiet, receiver)), None, None, cutoff)
        .await
        .unwrap();
    assert!(delivered.handoff_accepted);
    assert_eq!(
        state(abandoned).await,
        HandoffState::Expired,
        "an older baton of a quiet open session is superseded"
    );
    assert_eq!(
        state(busy).await,
        HandoffState::Open,
        "a session in use keeps its baton even when it is older"
    );
}

/// Two workstreams launched at once in one checkout each get a managed run.
/// A session one run's child links marks that run only: the other run's
/// status must neither report it nor count as linked, or its launcher would
/// import the other launch's transcript.
#[tokio::test]
async fn a_session_linked_by_one_managed_run_is_not_another_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let prepare = |name: &str| PrepareWorkstreamRun {
        scope_by_id: false,
        workspace_id: ws,
        project_id: proj,
        repo_fingerprint: "repo".into(),
        worktree_fingerprint: "worktree".into(),
        cwd: "/repo".into(),
        agent: AgentKind::Codex,
        automatic_harness: false,
        available_agents: Vec::new(),
        selection: WorkstreamSelection::New(name.into()),
        lease_owner: format!("launcher-{name}"),
    };
    let alpha = store
        .writer
        .prepare_workstream_run(prepare("alpha"))
        .await
        .unwrap();
    let beta = store
        .writer
        .prepare_workstream_run(prepare("beta"))
        .await
        .unwrap();
    assert_ne!(alpha.workstream_id, beta.workstream_id);
    let status = async |run| {
        let status = store.reader.managed_run_status(run).await.unwrap().unwrap();
        (status.native_session_id, status.native_session_linked)
    };

    assert!(
        store
            .writer
            .link_managed_run_session(beta.run_id, AgentKind::Codex, "native-beta")
            .await
            .unwrap()
    );
    assert_eq!(status(alpha.run_id).await, (None, false));
    assert_eq!(
        status(beta.run_id).await,
        (Some("native-beta".into()), true)
    );

    // Control: alpha's own link marks alpha, and leaves beta as it was.
    assert!(
        store
            .writer
            .link_managed_run_session(alpha.run_id, AgentKind::Codex, "native-alpha")
            .await
            .unwrap()
    );
    assert_eq!(
        status(alpha.run_id).await,
        (Some("native-alpha".into()), true)
    );
    assert_eq!(
        status(beta.run_id).await,
        (Some("native-beta".into()), true)
    );
}

/// Stale-run recovery is a scoped, owned, single-candidate operation. The
/// controls prove that the same candidate is usable once every boundary
/// matches, while foreign owners/projects and a second linker cannot take it.
#[tokio::test]
async fn stale_codex_run_recovery_cannot_cross_project_owner_or_session_boundaries() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other_proj = store
        .writer
        .get_or_create_project(ws, "other-app", None)
        .await
        .unwrap();
    let alice = operator("alice");
    let bob = operator("bob");
    let prepare = |project_id, name: &str| PrepareWorkstreamRun {
        scope_by_id: false,
        workspace_id: ws,
        project_id,
        repo_fingerprint: format!("repo-{project_id}"),
        worktree_fingerprint: format!("worktree-{project_id}"),
        cwd: format!("/repo/{project_id}"),
        agent: AgentKind::Codex,
        automatic_harness: false,
        available_agents: Vec::new(),
        selection: WorkstreamSelection::New(name.into()),
        lease_owner: format!("launcher-{name}"),
    };

    let stale = store
        .writer
        .prepare_workstream_run_owned(prepare(proj, "stale"), Some(alice.clone()))
        .await
        .unwrap();
    assert!(store.writer.cancel_managed_run(stale.run_id).await.unwrap());
    let current = store
        .writer
        .prepare_workstream_run_owned(prepare(proj, "current"), Some(alice.clone()))
        .await
        .unwrap();
    let foreign = store
        .writer
        .prepare_workstream_run_owned(prepare(other_proj, "foreign"), Some(alice.clone()))
        .await
        .unwrap();

    let recover = |project_id, owner: Option<String>, native: &'static str| {
        store
            .writer
            .link_or_adopt_managed_run_session(LinkOrAdoptManagedRunSession {
                supplied_run_id: stale.run_id,
                workspace_id: ws,
                project_id,
                cwd: format!("/repo/{project_id}"),
                agent: AgentKind::Codex,
                native_session_id: native.into(),
                owner_user: owner,
            })
    };
    assert_eq!(
        recover(proj, Some(bob), "native-bob").await.unwrap(),
        ManagedRunSessionLink::NoMatch,
        "a second operator must not see Alice's candidate"
    );
    assert_eq!(
        store
            .writer
            .link_or_adopt_managed_run_session(LinkOrAdoptManagedRunSession {
                supplied_run_id: stale.run_id,
                workspace_id: ws,
                project_id: proj,
                cwd: "/repo/a-different-worktree".into(),
                agent: AgentKind::Codex,
                native_session_id: "native-other-worktree".into(),
                owner_user: Some(alice.clone()),
            })
            .await
            .unwrap(),
        ManagedRunSessionLink::NoMatch,
        "a run in another checkout cwd must not be adopted"
    );
    assert_eq!(
        recover(other_proj, Some(alice.clone()), "native-foreign")
            .await
            .unwrap(),
        ManagedRunSessionLink::Adopted(foreign.run_id),
        "the same owner may recover only the candidate in the named project"
    );
    assert_eq!(
        recover(proj, Some(alice.clone()), "native-alice")
            .await
            .unwrap(),
        ManagedRunSessionLink::Adopted(current.run_id)
    );
    assert_eq!(
        recover(proj, Some(alice), "native-thief").await.unwrap(),
        ManagedRunSessionLink::NoMatch,
        "a linked run cannot be rebound by a racing SessionStart"
    );
    let current_status = store
        .reader
        .managed_run_status(current.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current_status.native_session_id.as_deref(),
        Some("native-alice")
    );
}

/// Forced lease recovery is deliberately narrower than project write access:
/// the same operator can recover their own abandoned launcher, while another
/// operator in the same project cannot evict it. The old run becomes terminal
/// in the same transaction that creates the replacement.
#[tokio::test]
async fn force_unlock_replaces_only_the_same_operators_active_run() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let alice = operator("alice");
    let bob = operator("bob");
    let prepare = |lease_owner: &str| PrepareWorkstreamRun {
        scope_by_id: false,
        workspace_id: ws,
        project_id: proj,
        repo_fingerprint: "repo".into(),
        worktree_fingerprint: "worktree".into(),
        cwd: "/repo".into(),
        agent: AgentKind::Codex,
        automatic_harness: false,
        available_agents: Vec::new(),
        selection: WorkstreamSelection::Current,
        lease_owner: lease_owner.into(),
    };

    let abandoned = store
        .writer
        .prepare_workstream_run_owned(prepare("alice:1"), Some(alice.clone()))
        .await
        .unwrap();
    let refused = store
        .writer
        .prepare_workstream_run_owned_with_unlock(prepare("bob:2"), Some(bob), true)
        .await
        .unwrap_err();
    assert!(matches!(refused, StoreError::WorkstreamBusy(_)));
    assert!(
        store
            .writer
            .heartbeat_managed_run(abandoned.run_id)
            .await
            .unwrap(),
        "a refused cross-owner takeover must leave the original lease active"
    );

    let replacement = store
        .writer
        .prepare_workstream_run_owned_with_unlock(prepare("alice:3"), Some(alice.clone()), true)
        .await
        .unwrap();
    assert_ne!(replacement.run_id, abandoned.run_id);
    assert!(
        !store
            .writer
            .heartbeat_managed_run(abandoned.run_id)
            .await
            .unwrap(),
        "the replaced run must be terminal"
    );
    assert!(
        store
            .writer
            .heartbeat_managed_run(replacement.run_id)
            .await
            .unwrap(),
        "the replacement is the sole live control"
    );

    let solo = store
        .writer
        .prepare_workstream_run_owned(
            PrepareWorkstreamRun {
                scope_by_id: false,
                selection: WorkstreamSelection::New("solo".into()),
                ..prepare("solo:1")
            },
            None,
        )
        .await
        .unwrap();
    let solo_replacement = store
        .writer
        .prepare_workstream_run_owned_with_unlock(
            PrepareWorkstreamRun {
                scope_by_id: false,
                selection: WorkstreamSelection::Named("solo".into()),
                ..prepare("solo:2")
            },
            None,
            true,
        )
        .await
        .unwrap();
    assert_ne!(solo_replacement.run_id, solo.run_id);
}

/// Two launches in one repository are a real possibility when the operator
/// uses separate named workstreams. Recovery must refuse to guess between
/// them, and an active foreign id must not be treated as a stale trigger.
#[tokio::test]
async fn stale_codex_run_recovery_fails_closed_on_ambiguity_and_active_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other_proj = store
        .writer
        .get_or_create_project(ws, "other-app", None)
        .await
        .unwrap();
    let owner = operator("alice");
    let prepare = |project_id, name: &str| PrepareWorkstreamRun {
        scope_by_id: false,
        workspace_id: ws,
        project_id,
        repo_fingerprint: format!("repo-{project_id}"),
        worktree_fingerprint: format!("worktree-{project_id}"),
        cwd: format!("/repo/{project_id}"),
        agent: AgentKind::Codex,
        automatic_harness: false,
        available_agents: Vec::new(),
        selection: WorkstreamSelection::New(name.into()),
        lease_owner: format!("launcher-{name}"),
    };
    let alpha = store
        .writer
        .prepare_workstream_run_owned(prepare(proj, "alpha"), Some(owner.clone()))
        .await
        .unwrap();
    let beta = store
        .writer
        .prepare_workstream_run_owned(prepare(proj, "beta"), Some(owner.clone()))
        .await
        .unwrap();
    let foreign = store
        .writer
        .prepare_workstream_run_owned(prepare(other_proj, "foreign"), Some(owner.clone()))
        .await
        .unwrap();

    assert_eq!(
        store
            .writer
            .link_or_adopt_managed_run_session(LinkOrAdoptManagedRunSession {
                supplied_run_id: ai_memory_core::ManagedRunId::new(),
                workspace_id: ws,
                project_id: proj,
                cwd: format!("/repo/{proj}"),
                agent: AgentKind::Codex,
                native_session_id: "native-ambiguous".into(),
                owner_user: Some(owner.clone()),
            })
            .await
            .unwrap(),
        ManagedRunSessionLink::Ambiguous
    );
    assert_eq!(
        store
            .writer
            .link_or_adopt_managed_run_session(LinkOrAdoptManagedRunSession {
                supplied_run_id: alpha.run_id,
                workspace_id: ws,
                project_id: other_proj,
                cwd: format!("/repo/{other_proj}"),
                agent: AgentKind::Codex,
                native_session_id: "native-cross-project".into(),
                owner_user: Some(owner.clone()),
            })
            .await
            .unwrap(),
        ManagedRunSessionLink::Refused,
        "an active run with a mismatched boundary must not trigger adoption"
    );
    assert_eq!(
        store
            .writer
            .link_or_adopt_managed_run_session(LinkOrAdoptManagedRunSession {
                supplied_run_id: foreign.run_id,
                workspace_id: ws,
                project_id: other_proj,
                cwd: format!("/repo/{other_proj}"),
                agent: AgentKind::Codex,
                native_session_id: "native-control".into(),
                owner_user: Some(owner),
            })
            .await
            .unwrap(),
        ManagedRunSessionLink::Exact(foreign.run_id)
    );
    for run_id in [alpha.run_id, beta.run_id] {
        assert!(
            store
                .reader
                .managed_run_status(run_id)
                .await
                .unwrap()
                .unwrap()
                .native_session_id
                .is_none()
        );
    }
}

fn finish_authority(
    level: ai_memory_core::AuthLevel,
    user: Option<ai_memory_core::UserId>,
    name: Option<&str>,
) -> ai_memory_store::ManagedRunAuthority {
    ai_memory_store::ManagedRunAuthority::from_auth(
        level,
        user.map(ai_memory_core::AuthorizedViewer),
        user,
        &ActorContext {
            user: name.map(str::to_owned),
            ..ActorContext::anonymous()
        },
        false,
    )
}

fn finish_input(run_id: ai_memory_core::ManagedRunId) -> ai_memory_store::FinishWorkstreamRun {
    ai_memory_store::FinishWorkstreamRun {
        sanitizer: ai_memory_core::Sanitizer::default(),
        run_id,
        native_session_id: Some("native-finish".into()),
        source_cursor: Some("cursor-finish".into()),
        events: vec![ai_memory_core::NewWorkstreamEvent {
            event_id: "finish-event".into(),
            agent: AgentKind::Codex,
            native_session_id: "native-finish".into(),
            source_record_id: Some("record-finish".into()),
            kind: ai_memory_core::WorkstreamEventKind::Message,
            role: Some("assistant".into()),
            content: "finish snapshot evidence".into(),
            occurred_at: None,
            metadata: serde_json::json!({"owner_user": "user:alice", "is_root": true}),
        }],
        complete: true,
        segment_path: Some("raw/fixture-finish.jsonl".into()),
        exit_code: Some(0),
    }
}

async fn finish_snapshot(store: &Store) -> Vec<Vec<String>> {
    store
        .reader
        .with_conn(|conn| {
            let mut result = Vec::new();
            for table in [
                "workspaces",
                "projects",
                "managed_runs",
                "workstreams",
                "workstream_events",
                "workstream_native_sessions",
                "workstream_events_fts_data",
                "workstream_events_fts_idx",
                "workstream_events_fts_docsize",
                "workstream_events_fts_config",
                "sessions",
            ] {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))?;
                let columns = stmt.column_count();
                let rows = stmt.query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get_ref(i).map(|v| format!("{v:?}")))
                        .collect()
                })?;
                result.push(vec![table.to_owned()]);
                result.extend(rows.collect::<Result<Vec<Vec<String>>, _>>()?);
            }
            Ok(result)
        })
        .await
        .unwrap()
}

struct FinishFixture {
    _temp: tempfile::TempDir,
    store: Store,
    ws: WorkspaceId,
    project: ProjectId,
    alice: ai_memory_core::UserId,
    bob: ai_memory_core::UserId,
}

impl FinishFixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let (ws, project) = scope(&store).await;
        let mut users = Vec::new();
        for name in ["alice", "bob"] {
            let id = store
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
                .unwrap();
            store
                .writer
                .grant_memory(id, project, ai_memory_store::GrantLevel::Write, None)
                .await
                .unwrap();
            users.push(id);
        }
        store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        Self {
            _temp: temp,
            store,
            ws,
            project,
            alice: users[0],
            bob: users[1],
        }
    }

    fn alice(&self) -> ai_memory_store::ManagedRunAuthority {
        finish_authority(
            ai_memory_core::AuthLevel::User,
            Some(self.alice),
            Some("alice"),
        )
    }

    async fn run(
        &self,
        name: &str,
        owner: Option<String>,
    ) -> ai_memory_store::PreparedWorkstreamRun {
        self.store
            .writer
            .prepare_workstream_run_owned(
                PrepareWorkstreamRun {
                    scope_by_id: false,
                    workspace_id: self.ws,
                    project_id: self.project,
                    repo_fingerprint: "repo-finish".into(),
                    worktree_fingerprint: "tree-finish".into(),
                    cwd: "/repo".into(),
                    agent: AgentKind::Codex,
                    automatic_harness: false,
                    available_agents: Vec::new(),
                    selection: WorkstreamSelection::New(name.into()),
                    lease_owner: "display-only".into(),
                },
                owner,
            )
            .await
            .unwrap()
    }

    async fn refused_unchanged(
        &self,
        run_id: ai_memory_core::ManagedRunId,
        authority: ai_memory_store::ManagedRunAuthority,
    ) {
        let before = finish_snapshot(&self.store).await;
        assert!(
            self.store
                .reader
                .authorize_managed_run(run_id, authority.clone())
                .await
                .is_err(),
            "preflight must refuse"
        );
        assert!(
            self.store
                .writer
                .finish_workstream_run(authority, finish_input(run_id))
                .await
                .is_err(),
            "writer must refuse"
        );
        assert_eq!(
            finish_snapshot(&self.store).await,
            before,
            "refusal must preserve every run/event/index/link/cursor row"
        );
    }
}

#[tokio::test]
async fn owner_finish_store_refuses_another_writer_including_finished_retry() {
    let f = FinishFixture::new().await;
    let run = f.run("owned", Some(operator("alice"))).await;
    let bob = finish_authority(ai_memory_core::AuthLevel::User, Some(f.bob), Some("bob"));
    for complete in [false, true] {
        let before = finish_snapshot(&f.store).await;
        assert!(matches!(
            f.store
                .reader
                .authorize_managed_run(run.run_id, bob.clone())
                .await,
            Err(StoreError::Forbidden(_))
        ));
        let mut input = finish_input(run.run_id);
        input.complete = complete;
        assert!(
            matches!(
                f.store
                    .writer
                    .finish_workstream_run(bob.clone(), input)
                    .await,
                Err(StoreError::Forbidden(_))
            ),
            "another writer must not finish or import a batch"
        );
        assert_eq!(finish_snapshot(&f.store).await, before);
    }
    // Root has project authority but no owner identity; it never means Any.
    f.refused_unchanged(
        run.run_id,
        finish_authority(ai_memory_core::AuthLevel::Root, None, None),
    )
    .await;
    f.store
        .reader
        .authorize_managed_run(run.run_id, f.alice())
        .await
        .unwrap();
    let result = f
        .store
        .writer
        .finish_workstream_run(f.alice(), finish_input(run.run_id))
        .await
        .unwrap();
    assert_eq!(result.imported_events, 1);
    let before = finish_snapshot(&f.store).await;
    assert!(
        matches!(
            f.store
                .writer
                .finish_workstream_run(bob, finish_input(run.run_id))
                .await,
            Err(StoreError::Forbidden(_))
        ),
        "finished retries must check the owner before the dedup return"
    );
    let retry = f
        .store
        .writer
        .finish_workstream_run(f.alice(), finish_input(run.run_id))
        .await
        .unwrap();
    assert_eq!(retry.imported_events, 0);
    assert_eq!(retry.latest_sequence, result.latest_sequence);
    assert_eq!(finish_snapshot(&f.store).await, before);
    for mutate in ["event-id", "content", "cursor", "exit-code"] {
        let mut changed = finish_input(run.run_id);
        match mutate {
            "event-id" => changed.events[0].event_id = "losing-event".into(),
            "content" => changed.events[0].content = "losing payload".into(),
            "cursor" => changed.source_cursor = Some("uncommitted-cursor".into()),
            "exit-code" => changed.exit_code = Some(9),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                f.store
                    .writer
                    .recover_expired_workstream_run(f.alice(), changed)
                    .await,
                Err(StoreError::InvalidState(_))
            ),
            "finished recovery must reject changed {mutate}"
        );
        assert_eq!(finish_snapshot(&f.store).await, before);
    }
    assert_eq!(
        f.store
            .writer
            .recover_expired_workstream_run(f.alice(), finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        0,
        "an exact owner retry remains idempotent"
    );
    for (name, auth) in [
        (
            "root-shared",
            finish_authority(ai_memory_core::AuthLevel::Root, None, None),
        ),
        ("user-shared", f.alice()),
    ] {
        let shared = f.run(name, None).await;
        f.store
            .reader
            .authorize_managed_run(shared.run_id, auth.clone())
            .await
            .unwrap();
        assert_eq!(
            f.store
                .writer
                .finish_workstream_run(auth, finish_input(shared.run_id))
                .await
                .unwrap()
                .imported_events,
            1
        );
    }
}

#[tokio::test]
async fn owner_finish_store_rechecks_write_after_preflight() {
    let f = FinishFixture::new().await;
    let run = f.run("revoke", Some(operator("alice"))).await;
    let auth = f.alice();
    f.store
        .reader
        .authorize_managed_run(run.run_id, auth.clone())
        .await
        .unwrap();
    f.store
        .writer
        .revoke_memory(f.alice, f.project, None)
        .await
        .unwrap();
    let before = finish_snapshot(&f.store).await;
    assert!(
        matches!(
            f.store
                .writer
                .finish_workstream_run(auth.clone(), finish_input(run.run_id))
                .await,
            Err(StoreError::Forbidden(_))
        ),
        "the transaction must read the current Write grant"
    );
    assert_eq!(finish_snapshot(&f.store).await, before);
    f.store
        .writer
        .grant_memory(f.alice, f.project, ai_memory_store::GrantLevel::Read, None)
        .await
        .unwrap();
    f.refused_unchanged(run.run_id, auth.clone()).await;
    f.store
        .writer
        .grant_memory(f.alice, f.project, ai_memory_store::GrantLevel::Write, None)
        .await
        .unwrap();
    f.store
        .reader
        .authorize_managed_run(run.run_id, auth.clone())
        .await
        .unwrap();
    f.store
        .writer
        .set_user_disabled(f.alice, true)
        .await
        .unwrap();
    f.store
        .reader
        .authorize_managed_run(run.run_id, auth.clone())
        .await
        .unwrap();
    // Attribution UserId is real middleware data even if a viewer marker is absent.
    let fallback = ai_memory_store::ManagedRunAuthority::from_auth(
        ai_memory_core::AuthLevel::User,
        None,
        Some(f.alice),
        &ActorContext {
            user: Some("alice".into()),
            ..ActorContext::anonymous()
        },
        false,
    );
    assert!(
        f.store
            .reader
            .authorize_managed_run(run.run_id, fallback.clone())
            .await
            .is_ok(),
        "the real DB-user attribution id must retain its Write grant without a viewer marker"
    );
    assert_eq!(
        f.store
            .writer
            .finish_workstream_run(fallback, finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        1
    );
}

#[tokio::test]
async fn owner_finish_store_refuses_a_deleted_user_after_preflight() {
    let f = FinishFixture::new().await;
    let run = f.run("deleted-user", Some(operator("alice"))).await;
    let auth = f.alice();
    f.store
        .writer
        .set_access_mode(f.project, ai_memory_store::AccessMode::Open)
        .await
        .unwrap();
    f.store
        .reader
        .authorize_managed_run(run.run_id, auth.clone())
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(f.store.db_path()).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    assert_eq!(
        conn.execute(
            "DELETE FROM users WHERE id = ?1",
            rusqlite::params![f.alice.as_bytes()]
        )
        .unwrap(),
        1
    );
    let before = finish_snapshot(&f.store).await;
    assert!(
        matches!(
            f.store
                .reader
                .authorize_managed_run(run.run_id, auth.clone())
                .await,
            Err(StoreError::Forbidden(_))
        ),
        "a deleted DB user must be refused even in an open project"
    );
    assert!(
        matches!(
            f.store
                .writer
                .finish_workstream_run(auth, finish_input(run.run_id))
                .await,
            Err(StoreError::Forbidden(_))
        ),
        "the writer must refuse a user deleted after preflight"
    );
    assert_eq!(finish_snapshot(&f.store).await, before);
    let shared = f.run("existing-control", None).await;
    let bob = finish_authority(ai_memory_core::AuthLevel::User, Some(f.bob), Some("bob"));
    assert_eq!(
        f.store
            .writer
            .finish_workstream_run(bob, finish_input(shared.run_id))
            .await
            .unwrap()
            .imported_events,
        1
    );
}

#[tokio::test]
async fn owner_finish_store_proxy_policy_requires_server_and_authenticated_identity() {
    let f = FinishFixture::new().await;
    let run = f.run("proxy-policy", None).await;
    let actor = ActorContext {
        issuer: Some("https://idp.example".into()),
        sub: Some("alice".into()),
        ..ActorContext::anonymous()
    };
    for (level, identity, configured, message) in [
        (
            ai_memory_core::AuthLevel::Anonymous,
            actor.clone(),
            true,
            "proxy compatibility requires authenticated User capability",
        ),
        (
            ai_memory_core::AuthLevel::User,
            ActorContext::anonymous(),
            true,
            "proxy compatibility requires a canonical authenticated identity",
        ),
        (
            ai_memory_core::AuthLevel::User,
            actor.clone(),
            false,
            "proxy compatibility requires trusted server configuration",
        ),
    ] {
        let auth = ai_memory_store::ManagedRunAuthority::from_auth(
            level, None, None, &identity, configured,
        );
        let before = finish_snapshot(&f.store).await;
        assert!(
            matches!(
                f.store
                    .reader
                    .authorize_managed_run(run.run_id, auth.clone())
                    .await,
                Err(StoreError::Forbidden(_))
            ),
            "{message}"
        );
        assert!(
            matches!(
                f.store
                    .writer
                    .finish_workstream_run(auth, finish_input(run.run_id))
                    .await,
                Err(StoreError::Forbidden(_))
            ),
            "{message}"
        );
        assert_eq!(finish_snapshot(&f.store).await, before);
    }
    let proxy = ai_memory_store::ManagedRunAuthority::from_auth(
        ai_memory_core::AuthLevel::User,
        None,
        None,
        &actor,
        true,
    );
    // The legacy project exception still requires the real paired SQL scope.
    let other_ws = f
        .store
        .writer
        .get_or_create_workspace("proxy-other")
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(f.store.db_path()).unwrap();
    conn.execute(
        "UPDATE workstreams SET workspace_id = ?1 WHERE id = ?2",
        rusqlite::params![other_ws.as_bytes(), run.workstream_id.as_bytes()],
    )
    .unwrap();
    f.refused_unchanged(run.run_id, proxy.clone()).await;
    conn.execute(
        "UPDATE workstreams SET workspace_id = ?1 WHERE id = ?2",
        rusqlite::params![f.ws.as_bytes(), run.workstream_id.as_bytes()],
    )
    .unwrap();
    assert_eq!(
        f.store
            .writer
            .finish_workstream_run(proxy, finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        1
    );
}

#[tokio::test]
async fn owner_finish_store_raw_ids_use_the_actual_project_and_workspace() {
    let f = FinishFixture::new().await;
    for (ws_name, project_name) in [
        ("acme", "foreign-project"),
        ("foreign-workspace", "shared-app"),
    ] {
        let ws = f
            .store
            .writer
            .get_or_create_workspace(ws_name)
            .await
            .unwrap();
        let project = f
            .store
            .writer
            .get_or_create_project(ws, project_name, None)
            .await
            .unwrap();
        f.store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        let run = f
            .store
            .writer
            .prepare_workstream_run_owned(
                PrepareWorkstreamRun {
                    scope_by_id: false,
                    workspace_id: ws,
                    project_id: project,
                    repo_fingerprint: "foreign".into(),
                    worktree_fingerprint: "foreign".into(),
                    cwd: "/repo".into(),
                    agent: AgentKind::Codex,
                    automatic_harness: false,
                    available_agents: Vec::new(),
                    selection: WorkstreamSelection::Current,
                    lease_owner: "foreign".into(),
                },
                Some(operator("alice")),
            )
            .await
            .unwrap();
        f.refused_unchanged(run.run_id, f.alice()).await;
        f.store
            .writer
            .grant_memory(f.alice, project, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
        assert_eq!(
            f.store
                .writer
                .finish_workstream_run(f.alice(), finish_input(run.run_id))
                .await
                .unwrap()
                .imported_events,
            1
        );
    }
    // A well-shaped but nonexistent user id cannot inherit a creator or grant.
    let run = f.run("missing-user", None).await;
    f.refused_unchanged(
        run.run_id,
        finish_authority(
            ai_memory_core::AuthLevel::User,
            Some(ai_memory_core::UserId::new()),
            Some("alice"),
        ),
    )
    .await;
}

#[tokio::test]
async fn owner_finish_store_topology_reload_and_phase_controls() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path()).unwrap();
    let (ws, project) = scope(&store).await;
    store
        .writer
        .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let run = store
        .writer
        .prepare_workstream_run(PrepareWorkstreamRun {
            scope_by_id: false,
            workspace_id: ws,
            project_id: project,
            repo_fingerprint: "reload".into(),
            worktree_fingerprint: "reload".into(),
            cwd: "/repo".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            selection: WorkstreamSelection::Current,
            lease_owner: "reload".into(),
        })
        .await
        .unwrap();
    let anonymous = finish_authority(ai_memory_core::AuthLevel::Anonymous, None, None);
    store
        .reader
        .authorize_managed_run(run.run_id, anonymous.clone())
        .await
        .unwrap();
    let owner_user = store
        .writer
        .create_human_user(
            NewUser {
                username: "new-user".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let before = finish_snapshot(&store).await;
    assert!(
        matches!(
            store
                .reader
                .authorize_managed_run(run.run_id, anonymous.clone())
                .await,
            Err(StoreError::Forbidden(_))
        ),
        "topology must be reloaded without restarting the reader"
    );
    assert!(
        matches!(
            store
                .writer
                .finish_workstream_run(anonymous.clone(), finish_input(run.run_id))
                .await,
            Err(StoreError::Forbidden(_))
        ),
        "topology must be reloaded in the transaction"
    );
    assert_eq!(finish_snapshot(&store).await, before);
    let root = finish_authority(ai_memory_core::AuthLevel::Root, None, None);
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute_batch("ALTER TABLE users RENAME TO unavailable_users;")
        .unwrap();
    let before = finish_snapshot(&store).await;
    assert!(
        store
            .reader
            .authorize_managed_run(run.run_id, anonymous.clone())
            .await
            .is_err(),
        "unreadable topology must fail closed in preflight"
    );
    assert!(
        store
            .writer
            .finish_workstream_run(anonymous.clone(), finish_input(run.run_id))
            .await
            .is_err(),
        "unreadable topology must fail closed in the transaction"
    );
    assert_eq!(finish_snapshot(&store).await, before);
    conn.execute_batch("ALTER TABLE unavailable_users RENAME TO users;")
        .unwrap();
    conn.execute(
        "UPDATE managed_runs SET lease_expires_at = 0 WHERE id = ?1",
        [run.run_id.as_bytes()],
    )
    .unwrap();
    store
        .reader
        .authorize_managed_run(run.run_id, root.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .writer
            .finish_workstream_run(root.clone(), finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        1,
        "late active finish remains legitimate"
    );
    let before = finish_snapshot(&store).await;
    assert_eq!(
        store
            .writer
            .finish_workstream_run(root.clone(), finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        0
    );
    assert_eq!(finish_snapshot(&store).await, before);
    // Explicit recovery admits the original owner only when no newer run exists.
    store
        .writer
        .grant_memory(
            owner_user,
            project,
            ai_memory_store::GrantLevel::Write,
            None,
        )
        .await
        .unwrap();
    let recoverable = store
        .writer
        .prepare_workstream_run_owned(
            PrepareWorkstreamRun {
                scope_by_id: false,
                workspace_id: ws,
                project_id: project,
                repo_fingerprint: "recoverable".into(),
                worktree_fingerprint: "recoverable".into(),
                cwd: "/repo".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                selection: WorkstreamSelection::New("recoverable".into()),
                lease_owner: "reload".into(),
            },
            Some("user:new-user".into()),
        )
        .await
        .unwrap();
    store
        .writer
        .cancel_managed_run(recoverable.run_id)
        .await
        .unwrap();
    let foreign_user = store
        .writer
        .create_human_user(
            NewUser {
                username: "foreign-recovery".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    store
        .writer
        .grant_memory(
            foreign_user,
            project,
            ai_memory_store::GrantLevel::Write,
            None,
        )
        .await
        .unwrap();
    let foreign = finish_authority(
        ai_memory_core::AuthLevel::User,
        Some(foreign_user),
        Some("foreign-recovery"),
    );
    assert!(matches!(
        store
            .writer
            .recover_expired_workstream_run(foreign, finish_input(recoverable.run_id))
            .await,
        Err(StoreError::Forbidden(_))
    ));
    let owner = finish_authority(
        ai_memory_core::AuthLevel::User,
        Some(owner_user),
        Some("new-user"),
    );
    assert_eq!(
        store
            .writer
            .recover_expired_workstream_run(owner, finish_input(recoverable.run_id))
            .await
            .unwrap()
            .imported_events,
        1
    );

    // Cancel and replacement both store the existing expired phase.
    for replaced in [false, true] {
        let old = store
            .writer
            .prepare_workstream_run(PrepareWorkstreamRun {
                scope_by_id: false,
                workspace_id: ws,
                project_id: project,
                repo_fingerprint: "reload".into(),
                worktree_fingerprint: "reload".into(),
                cwd: "/repo".into(),
                agent: AgentKind::Codex,
                automatic_harness: false,
                available_agents: Vec::new(),
                selection: WorkstreamSelection::Current,
                lease_owner: "reload".into(),
            })
            .await
            .unwrap();
        if replaced {
            conn.execute(
                "UPDATE managed_runs SET lease_expires_at = 0 WHERE id = ?1",
                [old.run_id.as_bytes()],
            )
            .unwrap();
            let replacement = store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
                    scope_by_id: false,
                    workspace_id: ws,
                    project_id: project,
                    repo_fingerprint: "reload".into(),
                    worktree_fingerprint: "reload".into(),
                    cwd: "/repo".into(),
                    agent: AgentKind::Codex,
                    automatic_harness: false,
                    available_agents: Vec::new(),
                    selection: WorkstreamSelection::Current,
                    lease_owner: "replacement".into(),
                })
                .await
                .unwrap();
            conn.execute(
                "UPDATE managed_runs SET started_at = 7 WHERE id IN (?1, ?2)",
                rusqlite::params![old.run_id.as_bytes(), replacement.run_id.as_bytes()],
            )
            .unwrap();
        } else {
            store.writer.cancel_managed_run(old.run_id).await.unwrap();
        }
        let before = finish_snapshot(&store).await;
        assert!(matches!(
            store
                .writer
                .finish_workstream_run(root.clone(), finish_input(old.run_id))
                .await,
            Err(StoreError::InvalidState(_))
        ));
        if replaced {
            assert!(matches!(
                store
                    .writer
                    .recover_expired_workstream_run(root.clone(), finish_input(old.run_id))
                    .await,
                Err(StoreError::InvalidState(_))
            ));
        } else {
            assert_eq!(
                store
                    .writer
                    .recover_expired_workstream_run(root.clone(), finish_input(old.run_id))
                    .await
                    .unwrap()
                    .imported_events,
                0,
                "the unsuperseded expired run is recoverable and stable event ids remain idempotent"
            );
        }
        if replaced {
            assert_eq!(finish_snapshot(&store).await, before);
        }
    }
    let before = finish_snapshot(&store).await;
    assert!(matches!(
        store
            .reader
            .authorize_managed_run(ai_memory_core::ManagedRunId::new(), root.clone())
            .await,
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store
            .writer
            .finish_workstream_run(root, finish_input(ai_memory_core::ManagedRunId::new()))
            .await,
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(finish_snapshot(&store).await, before);
}

#[tokio::test]
async fn owner_finish_store_strict_resolution_refuses_pairing_missing_and_sql_failure() {
    let f = FinishFixture::new().await;
    let run = f.run("broken-scope", Some(operator("alice"))).await;
    let conn = rusqlite::Connection::open(f.store.db_path()).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA ignore_check_constraints=ON;")
        .unwrap();
    let other_ws = f
        .store
        .writer
        .get_or_create_workspace("other")
        .await
        .unwrap();
    conn.execute(
        "UPDATE workstreams SET workspace_id = ?1 WHERE id = ?2",
        rusqlite::params![other_ws.as_bytes(), run.workstream_id.as_bytes()],
    )
    .unwrap();
    let root_owner = finish_authority(ai_memory_core::AuthLevel::Root, None, Some("alice"));
    f.refused_unchanged(run.run_id, root_owner).await;
    conn.execute(
        "UPDATE workstreams SET workspace_id = ?1 WHERE id = ?2",
        rusqlite::params![f.ws.as_bytes(), run.workstream_id.as_bytes()],
    )
    .unwrap();
    for bytes in [vec![1, 2, 3], WorkspaceId::new().as_bytes().to_vec()] {
        conn.execute(
            "UPDATE workstreams SET workspace_id = ?1 WHERE id = ?2",
            rusqlite::params![bytes, run.workstream_id.as_bytes()],
        )
        .unwrap();
        f.refused_unchanged(run.run_id, f.alice()).await;
    }
    conn.execute(
        "UPDATE workstreams SET workspace_id = ?1 WHERE id = ?2",
        rusqlite::params![f.ws.as_bytes(), run.workstream_id.as_bytes()],
    )
    .unwrap();
    conn.execute(
        "UPDATE workstreams SET project_id = ?1 WHERE id = ?2",
        rusqlite::params![ProjectId::new().as_bytes(), run.workstream_id.as_bytes()],
    )
    .unwrap();
    f.refused_unchanged(run.run_id, f.alice()).await;
    conn.execute(
        "UPDATE workstreams SET project_id = ?1 WHERE id = ?2",
        rusqlite::params![f.project.as_bytes(), run.workstream_id.as_bytes()],
    )
    .unwrap();
    conn.execute(
        "UPDATE projects SET access_mode = 'unknown' WHERE id = ?1",
        [f.project.as_bytes()],
    )
    .unwrap();
    f.refused_unchanged(run.run_id, f.alice()).await;
    conn.execute(
        "UPDATE projects SET access_mode = 'restricted' WHERE id = ?1",
        [f.project.as_bytes()],
    )
    .unwrap();
    conn.execute_batch("ALTER TABLE project_grants RENAME TO unavailable_grants;")
        .unwrap();
    f.refused_unchanged(run.run_id, f.alice()).await;
    // The common resolver also refuses when its authorization data is
    // unreadable; a store fault must not open a restricted project.
    let common = ai_memory_store::resolve_project_authz(
        &conn,
        f.ws,
        f.project,
        &ai_memory_store::ProjectPrincipal::user(f.alice),
        true,
    );
    assert!(common.is_err());
    conn.execute_batch("ALTER TABLE unavailable_grants RENAME TO project_grants;")
        .unwrap();
    conn.execute(
        "UPDATE projects SET created_by = ?1 WHERE id = ?2",
        rusqlite::params![vec![1_u8, 2, 3], f.project.as_bytes()],
    )
    .unwrap();
    f.refused_unchanged(run.run_id, f.alice()).await;
    conn.execute(
        "UPDATE projects SET created_by = NULL WHERE id = ?1",
        [f.project.as_bytes()],
    )
    .unwrap();
    conn.execute(
        "UPDATE managed_runs SET workstream_id = ?1 WHERE id = ?2",
        rusqlite::params![
            ai_memory_core::WorkstreamId::new().as_bytes(),
            run.run_id.as_bytes()
        ],
    )
    .unwrap();
    f.refused_unchanged(run.run_id, f.alice()).await;
    conn.execute(
        "UPDATE managed_runs SET workstream_id = ?1 WHERE id = ?2",
        rusqlite::params![run.workstream_id.as_bytes(), run.run_id.as_bytes()],
    )
    .unwrap();
    f.store
        .reader
        .authorize_managed_run(run.run_id, f.alice())
        .await
        .unwrap();
    assert_eq!(
        f.store
            .writer
            .finish_workstream_run(f.alice(), finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        1
    );
}

#[tokio::test]
async fn owner_finish_store_keeps_qualified_identity_namespaces() {
    let f = FinishFixture::new().await;
    let identity = IdentityKey::Subject {
        issuer: "https://idp.example".into(),
        subject: "alice".into(),
    };
    let run = f.run("subject-owned", Some(identity.storage_key())).await;
    // A display username matching another identity's subject is a different owner.
    f.refused_unchanged(run.run_id, f.alice()).await;
    let subject = ai_memory_store::ManagedRunAuthority::from_auth(
        ai_memory_core::AuthLevel::User,
        Some(ai_memory_core::AuthorizedViewer(f.alice)),
        Some(f.alice),
        &identity.to_actor_context(),
        false,
    );
    let other_issuer = IdentityKey::Subject {
        issuer: "https://other-idp.example".into(),
        subject: "alice".into(),
    };
    let foreign = ai_memory_store::ManagedRunAuthority::from_auth(
        ai_memory_core::AuthLevel::User,
        Some(ai_memory_core::AuthorizedViewer(f.alice)),
        Some(f.alice),
        &other_issuer.to_actor_context(),
        false,
    );
    f.refused_unchanged(run.run_id, foreign).await;
    f.store
        .reader
        .authorize_managed_run(run.run_id, subject.clone())
        .await
        .unwrap();
    assert_eq!(
        f.store
            .writer
            .finish_workstream_run(subject, finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        1
    );
}

#[tokio::test]
async fn native_identity_store_link_refuses_original_dirty_bytes() {
    let f = FinishFixture::new().await;
    let run = f.run("identity-link", Some(operator("alice"))).await;
    for id in [
        "native\0tail".to_owned(),
        "native\u{202e}tail".into(),
        "sk-abcdefghijklmnopqrstuvwx".into(),
        "x".repeat(513),
    ] {
        let before = finish_snapshot(&f.store).await;
        let result = f
            .store
            .writer
            .link_managed_run_session(run.run_id, AgentKind::Codex, id)
            .await;
        assert!(
            matches!(result, Err(_) | Ok(false)),
            "dirty native identity must not link"
        );
        assert_eq!(finish_snapshot(&f.store).await, before);
    }
    let exact = "界".repeat(170) + "ab";
    assert_eq!(exact.len(), 512);
    assert!(
        f.store
            .writer
            .link_managed_run_session(run.run_id, AgentKind::Codex, exact.clone())
            .await
            .unwrap()
    );
    assert_eq!(
        f.store
            .reader
            .managed_run_status(run.run_id)
            .await
            .unwrap()
            .unwrap()
            .native_session_id,
        Some(exact)
    );
}

#[tokio::test]
async fn native_identity_store_adoption_refuses_before_link() {
    let f = FinishFixture::new().await;
    let run = f.run("identity-adopt", Some(operator("alice"))).await;
    let input = |id: &str| LinkOrAdoptManagedRunSession {
        supplied_run_id: run.run_id,
        workspace_id: f.ws,
        project_id: f.project,
        cwd: "/repo".into(),
        agent: AgentKind::Codex,
        native_session_id: id.into(),
        owner_user: Some(operator("alice")),
    };
    let before = finish_snapshot(&f.store).await;
    let result = f
        .store
        .writer
        .link_or_adopt_managed_run_session(input("sk-abcdefghijklmnopqrstuvwx"))
        .await;
    assert!(
        matches!(result, Ok(ManagedRunSessionLink::Refused) | Err(_)),
        "dirty native identity must not adopt"
    );
    assert_eq!(finish_snapshot(&f.store).await, before);
    assert_eq!(
        f.store
            .writer
            .link_or_adopt_managed_run_session(input("vendor-session_01"))
            .await
            .unwrap(),
        ManagedRunSessionLink::Exact(run.run_id)
    );
}

#[tokio::test]
async fn native_identity_store_finish_refuses_without_sql_or_index_mutation() {
    let f = FinishFixture::new().await;
    let run = f.run("identity-finish", Some(operator("alice"))).await;
    for (run_id, event_id) in [
        (
            Some("sk-abcdefghijklmnopqrstuvwx"),
            "sk-abcdefghijklmnopqrstuvwx",
        ),
        (None, "native\u{200b}tail"),
    ] {
        let mut input = finish_input(run.run_id);
        input.native_session_id = run_id.map(str::to_owned);
        input.events[0].native_session_id = event_id.into();
        if run_id.is_some() {
            input.events.clear();
        }
        let before = finish_snapshot(&f.store).await;
        assert!(
            matches!(
                f.store
                    .writer
                    .finish_workstream_run(f.alice(), input.clone())
                    .await,
                Err(StoreError::InvalidState(_))
            ),
            "dirty finish identity must be refused"
        );
        assert_eq!(finish_snapshot(&f.store).await, before);
        let bob = finish_authority(ai_memory_core::AuthLevel::User, Some(f.bob), Some("bob"));
        assert!(
            matches!(
                f.store.writer.finish_workstream_run(bob, input).await,
                Err(StoreError::Forbidden(_))
            ),
            "ownership must precede identity validation"
        );
        assert_eq!(finish_snapshot(&f.store).await, before);
    }
    assert_eq!(
        f.store
            .writer
            .finish_workstream_run(f.alice(), finish_input(run.run_id))
            .await
            .unwrap()
            .imported_events,
        1
    );
}

#[tokio::test]
async fn native_identity_store_history_projects_unknown_without_rewriting() {
    let f = FinishFixture::new().await;
    let run = f.run("identity-history", Some(operator("alice"))).await;
    f.store
        .writer
        .finish_workstream_run(f.alice(), finish_input(run.run_id))
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(f.store.db_path()).unwrap();
    let dirty = "sk-abcdefghijklmnopqrstuvwx";
    conn.execute(
        "UPDATE workstream_events SET native_session_id = ?1",
        [dirty],
    )
    .unwrap();
    let before = finish_snapshot(&f.store).await;
    for query in ["", "snapshot"] {
        let events = f
            .store
            .reader
            .search_workstream_events(
                run.workstream_id,
                query.to_owned(),
                10,
                ai_memory_core::Sanitizer::default(),
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].native_session_id, "",
            "legacy dirty identity must project UNKNOWN"
        );
        assert_eq!(events[0].content, "finish snapshot evidence");
        assert_eq!(events[0].sequence, 1);
    }
    assert_eq!(finish_snapshot(&f.store).await, before);
    let stored: String = conn
        .query_row("SELECT native_session_id FROM workstream_events", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(stored, dirty);
    conn.execute(
        "UPDATE workstream_events SET native_session_id = ?1",
        ["vendor-界-01"],
    )
    .unwrap();
    assert_eq!(
        f.store
            .reader
            .search_workstream_events(
                run.workstream_id,
                "".into(),
                10,
                ai_memory_core::Sanitizer::default(),
            )
            .await
            .unwrap()[0]
            .native_session_id,
        "vendor-界-01"
    );
}

/// The cross-project profile under invariant #16: two harnesses of one
/// operator (or that operator's two machines) resolving the private profile
/// at the same moment converge on one project, never two, and an entry either
/// writes is the other's to read. A second operator resolves elsewhere and
/// cannot open the first one's profile, while knowledge in an ordinary shared
/// project stays shared between them.
#[tokio::test]
async fn two_harnesses_of_one_operator_share_one_private_profile() {
    use ai_memory_core::profile::{EffectiveProfileShare, user_profile_project};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let new_user = |name: &str| NewUser {
        username: name.into(),
        name: None,
        email: None,
    };
    let alice = store
        .writer
        .create_human_user(new_user("alice"), UserRole::User, None, false)
        .await
        .unwrap();
    let bob = store
        .writer
        .create_human_user(new_user("bob"), UserRole::User, None, false)
        .await
        .unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();

    let resolve = || {
        ai_memory_store::create_profile_scope(
            &store.reader,
            &store.writer,
            EffectiveProfileShare::User,
            ws,
            Some(alice),
        )
    };
    let (claude, codex) = tokio::join!(resolve(), resolve());
    let (claude, codex) = (claude.unwrap(), codex.unwrap());
    assert_eq!(claude, codex, "two harnesses must not split one profile");

    store
        .writer
        .upsert_page(page(
            claude.workspace_id,
            claude.project_id,
            "profile/tools/pnpm.md",
            "Pnpm",
            "Use pnpm, not npm.",
        ))
        .await
        .unwrap();
    let seen_by_codex = ai_memory_store::lookup_profile_scope(
        &store.reader,
        EffectiveProfileShare::User,
        ws,
        Some(alice),
    )
    .await
    .unwrap()
    .expect("the other harness finds the same profile");
    let entries = store
        .reader
        .profile_entries(seen_by_codex.workspace_id, seen_by_codex.project_id, 10)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].statement, "Use pnpm, not npm.");

    assert_eq!(
        ai_memory_store::lookup_profile_scope(
            &store.reader,
            EffectiveProfileShare::User,
            ws,
            Some(bob),
        )
        .await
        .unwrap(),
        None,
        "bob resolves to his own profile, not alice's"
    );
    let refused = ai_memory_store::lookup_existing_scope_guarded(
        &store.reader,
        "default",
        &user_profile_project(alice),
        Some(bob),
        ai_memory_store::ProjectAccess::Read,
    )
    .await;
    assert!(refused.is_err(), "bob cannot open alice's private profile");

    // Control: the profile being private changes nothing about shared pages.
    let (shared_ws, shared_proj) = scope(&store).await;
    store
        .writer
        .upsert_page(page(
            shared_ws,
            shared_proj,
            "notes/db.md",
            "Db",
            "Postgres 17.",
        ))
        .await
        .unwrap();
    for viewer in [alice, bob] {
        let read = store
            .reader
            .authorize_project(
                shared_ws,
                shared_proj,
                ai_memory_store::ProjectPrincipal::user(viewer),
                true,
                ai_memory_store::ProjectAccess::Read,
            )
            .await
            .unwrap();
        assert!(read.is_ok(), "an open project stays shared");
    }
}

/// R1 observation stream, per-project isolation (row 1). The stream maps an
/// observation to `sessions/<id>.md` by session id, so the two lookups it
/// joins are each a place a foreign scope could leak in: an observation from
/// another project voting for this project's page, or this project's
/// observation voting for a same-path page in another project. Both must be
/// filtered to the queried `(workspace_id, project_id)`.
#[tokio::test]
async fn observation_stream_never_crosses_a_project_or_workspace() {
    use super::retrieval_observation_stream::{observe, search, session, session_page, stream_on};

    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, home) = scope(&store).await;
    let foreign = store
        .writer
        .get_or_create_project(ws, "foreign-app".to_string(), None)
        .await
        .unwrap();
    let other_ws = store
        .writer
        .get_or_create_workspace("other-ws".to_string())
        .await
        .unwrap();
    let twin = store
        .writer
        .get_or_create_project(other_ws, "shared-app".to_string(), None)
        .await
        .unwrap();

    // A home session whose evidence lives only in its observation. Pages at
    // its session path exist in the foreign project and the cross-workspace
    // twin, but not (yet) at home.
    let sid = session(&store, ws, home).await;
    // The foreign and twin scopes write before and after it, so home's
    // observation sits inside their slice of the shared log and only the
    // scope predicates can keep it out.
    let foreign_sid = session(&store, ws, foreign).await;
    let twin_sid = session(&store, other_ws, twin).await;
    observe(&store, ws, foreign, foreign_sid, "foreign chatter before").await;
    observe(&store, other_ws, twin, twin_sid, "twin chatter before").await;
    let evidence = observe(&store, ws, home, sid, "the lynx password rotation plan").await;
    observe(&store, ws, foreign, foreign_sid, "foreign chatter after").await;
    observe(&store, other_ws, twin, twin_sid, "twin chatter after").await;
    session_page(&store, ws, foreign, sid, "foreign copy").await;
    session_page(&store, other_ws, twin, sid, "twin copy").await;

    // Home's observation must not vote for a page outside home...
    assert!(
        search(&store, ws, home, "lynx").await.is_empty(),
        "a home observation lifted a foreign page into home results"
    );
    // ...and the foreign scopes must not be lifted by home's observation.
    assert!(
        search(&store, ws, foreign, "lynx").await.is_empty(),
        "another project's observation lifted this project's page"
    );
    assert!(
        search(&store, other_ws, twin, "lynx").await.is_empty(),
        "another workspace's observation lifted this workspace's page"
    );

    // A foreign session's own observation votes for its own page there, and
    // never surfaces at home.
    session_page(&store, ws, foreign, foreign_sid, "foreign session").await;
    observe(&store, ws, foreign, foreign_sid, "ibex budget numbers").await;
    assert!(search(&store, ws, home, "ibex").await.is_empty());
    assert_eq!(search(&store, ws, foreign, "ibex").await.len(), 1);

    // Control: once home has its own session page, home's observation lifts
    // exactly that page.
    session_page(&store, ws, home, sid, "home copy").await;
    let hits = search(&store, ws, home, "lynx").await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.path.as_str(), format!("sessions/{sid}.md"));
    assert_eq!(hits[0].0.title, format!("sessions/{sid}.md"));
    assert_eq!(hits[0].1.observation_ids, vec![evidence]);
    let home_ids = store
        .reader
        .latest_page_ids_by_paths(ws, home, vec![format!("sessions/{sid}.md")])
        .await
        .unwrap();
    assert_eq!(vec![hits[0].0.id], home_ids, "the vote went to home's page");
}

/// R1 observation stream, pages shared (row 5a, invariant #16). Operator B's
/// session, page and observations in a shared project are evidence for
/// operator A's query: neither the session owner nor `author_id` becomes a
/// read filter on the stream.
#[tokio::test]
async fn another_operators_observations_lift_their_session_page_in_a_shared_project() {
    use super::retrieval_observation_stream::{observe, search, stream_on};

    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store).await;
    let operator_b = store
        .writer
        .create_human_user(
            NewUser {
                username: "operator-b".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: Some("/repo".into()),
            actor_user: Some(operator("operator-b")),
        })
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            author_id: Some(operator_b),
            ..page(
                ws,
                proj,
                &format!("sessions/{sid}.md"),
                "B's session",
                "Operator B worked on the build.",
            )
        })
        .await
        .unwrap();
    let evidence = observe(
        &store,
        ws,
        proj,
        sid,
        "the walrus migration needs a backfill",
    )
    .await;

    // Operator A's query carries no owner coordinate: hybrid search has none.
    let hits = search(&store, ws, proj, "walrus backfill").await;
    assert_eq!(hits.len(), 1, "B's session evidence must be visible to A");
    assert_eq!(hits[0].0.path.as_str(), format!("sessions/{sid}.md"));
    assert_eq!(hits[0].1.observation_ids, vec![evidence]);
}

/// Begin a session owned by `owner` (`None`: unattributed, as a single-user
/// server or a legacy row stores it).
async fn owned_session(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    owner: Option<String>,
) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: Some("/repo".into()),
            actor_user: owner,
        })
        .await
        .unwrap();
    id
}

async fn explained_search_as(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    query: &str,
    owner: &OwnerFilter,
) -> Vec<(ai_memory_store::PageHit, ai_memory_store::SearchExplain)> {
    store
        .reader
        .hybrid_search_scopes(
            &[(ws, proj)],
            ai_memory_store::HybridSearch {
                query,
                vector: None,
                limit: 10,
                expiry_cutoff_us: None,
                include_superseded: false,
                explain: true,
                owner,
                filter: ai_memory_store::PageFilter::default(),
            },
        )
        .await
        .unwrap()
        .0
        .into_iter()
        .map(|(hit, details)| (hit, details.unwrap()))
        .collect()
}

/// Session pages are shared (invariant #16), so another operator's
/// observations may still vote for one. The observation text is not: the
/// excerpt a vote attaches, the observation ids in `explain`, and the
/// raw-observation fallback follow the session timeline's owner rule, so
/// operator B searching A's words learns that A's session page matches but
/// never reads A's observation. Controls: A sees their own excerpt, and an
/// unattributed session's excerpt reaches everyone, as does the unfiltered
/// single-user path.
#[tokio::test]
async fn observation_excerpts_follow_the_session_owner_while_the_vote_stays_shared() {
    use super::retrieval_observation_stream::{observe, session_page};

    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let alice = OwnerFilter::User(operator("alice"));
    let bob = OwnerFilter::User(operator("bob"));

    let a_session = owned_session(&store, ws, proj, Some(operator("alice"))).await;
    session_page(&store, ws, proj, a_session, "Alice worked on the build.").await;
    let a_obs = observe(
        &store,
        ws,
        proj,
        a_session,
        "alice private quasar credentials discussion",
    )
    .await;
    let shared_session = owned_session(&store, ws, proj, None).await;
    session_page(&store, ws, proj, shared_session, "A legacy session.").await;
    let shared_obs = observe(&store, ws, proj, shared_session, "the zebra migration plan").await;

    let a_path = format!("sessions/{a_session}.md");

    // B: the vote still lifts A's shared session page, but nothing of A's
    // observation comes with it.
    let hits = explained_search_as(&store, ws, proj, "quasar", &bob).await;
    let (hit, details) = hits
        .iter()
        .find(|(hit, _)| hit.path.as_str() == a_path)
        .expect("the shared session page still ranks for B");
    assert_eq!(details.observation_rank, Some(1));
    assert!(details.observation_ids.is_empty(), "{details:?}");
    assert!(!hit.snippet.contains("quasar"), "{}", hit.snippet);
    assert!(!hit.snippet.contains("credentials"), "{}", hit.snippet);
    assert!(
        hit.snippet.contains("Alice worked"),
        "page's own snippet: {}",
        hit.snippet
    );
    assert!(!hit.title.is_empty());
    for filter in [&bob, &OwnerFilter::Unattributed] {
        let raw = store
            .reader
            .search_observations_for_project_as(ws, proj, "quasar".into(), 10, filter.clone())
            .await
            .unwrap();
        assert!(
            raw.is_empty(),
            "{filter:?} must get no raw hit from A: {raw:?}"
        );
    }

    // Control: A reads their own excerpt and raw hit.
    let hits = explained_search_as(&store, ws, proj, "quasar", &alice).await;
    let (hit, details) = hits
        .iter()
        .find(|(hit, _)| hit.path.as_str() == a_path)
        .unwrap();
    assert_eq!(details.observation_ids, vec![a_obs]);
    assert!(hit.snippet.contains("quasar"), "{}", hit.snippet);
    let raw = store
        .reader
        .search_observations_for_project_as(ws, proj, "quasar".into(), 10, alice.clone())
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].session_id, a_session);

    // Control: an unattributed session's excerpt reaches every viewer.
    let shared_path = format!("sessions/{shared_session}.md");
    for filter in [&alice, &bob, &OwnerFilter::Unattributed, &OwnerFilter::Any] {
        let hits = explained_search_as(&store, ws, proj, "zebra", filter).await;
        let (hit, details) = hits
            .iter()
            .find(|(hit, _)| hit.path.as_str() == shared_path)
            .unwrap();
        assert_eq!(details.observation_ids, vec![shared_obs], "{filter:?}");
        assert!(hit.snippet.contains("zebra"), "{filter:?}: {}", hit.snippet);
        let raw = store
            .reader
            .search_observations_for_project_as(ws, proj, "zebra".into(), 10, filter.clone())
            .await
            .unwrap();
        assert_eq!(raw.len(), 1, "{filter:?}");
    }

    // Control: the unfiltered single-user path is unchanged.
    let hits = explained_search_as(&store, ws, proj, "quasar", &OwnerFilter::Any).await;
    let (hit, _) = hits
        .iter()
        .find(|(hit, _)| hit.path.as_str() == a_path)
        .unwrap();
    assert!(hit.snippet.contains("quasar"), "{}", hit.snippet);
}

/// The self-service purge checks exact ownership in the writer transaction,
/// including previews; shared legacy rows are readable but never user-deletable.
#[tokio::test]
async fn owned_session_purge_refuses_foreign_shared_and_wrong_scope_sessions() {
    use ai_memory_core::IdentityKey;
    use ai_memory_store::{Compaction, PurgeMode};
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let user = store
        .writer
        .create_human_user(
            NewUser {
                username: "purge-alice".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let mut sessions = Vec::new();
    for owner in [Some("user:purge-alice"), Some("user:purge-bob"), None] {
        let sid = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: sid,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: None,
                actor_user: owner.map(str::to_string),
            })
            .await
            .unwrap();
        sessions.push(sid);
    }
    for mode in [PurgeMode::Preview, PurgeMode::Commit] {
        for (project, sid) in [
            (proj, sessions[1]),
            (proj, sessions[2]),
            (other, sessions[0]),
        ] {
            let result = store
                .writer
                .purge_owned_session(
                    ws,
                    project,
                    sid,
                    IdentityKey::User("purge-alice".into()),
                    Some(user),
                    mode,
                )
                .await;
            assert!(matches!(result, Err(StoreError::NotFound(_))));
            assert!(
                store
                    .reader
                    .session_summary_scoped(ws, proj, sid, OwnerFilter::Any)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
    store
        .writer
        .purge_owned_session(
            ws,
            proj,
            sessions[0],
            IdentityKey::User("purge-alice".into()),
            Some(user),
            PurgeMode::Preview,
        )
        .await
        .unwrap();
    assert!(
        store
            .reader
            .session_summary_scoped(ws, proj, sessions[0], OwnerFilter::Any)
            .await
            .unwrap()
            .is_some()
    );
    // Simulate a grant revoked after the HTTP preflight: the writer must refuse.
    store
        .writer
        .set_access_mode(proj, ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    assert!(matches!(
        store
            .writer
            .purge_owned_session(
                ws,
                proj,
                sessions[0],
                IdentityKey::User("purge-alice".into()),
                Some(user),
                PurgeMode::Commit
            )
            .await,
        Err(StoreError::Forbidden(_))
    ));
    store
        .writer
        .set_access_mode(proj, ai_memory_store::AccessMode::Open)
        .await
        .unwrap();
    store
        .writer
        .purge_owned_session(
            ws,
            proj,
            sessions[0],
            IdentityKey::User("purge-alice".into()),
            Some(user),
            PurgeMode::Commit,
        )
        .await
        .unwrap();
    assert!(
        store
            .reader
            .session_summary_scoped(ws, proj, sessions[0], OwnerFilter::Any)
            .await
            .unwrap()
            .is_none()
    );
    // Admin control remains able to purge an ownerless legacy session.
    store
        .writer
        .purge_session(
            ws,
            proj,
            sessions[2],
            Some(user),
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn owned_session_purge_refuses_cross_project_cascades_with_admin_control() {
    use ai_memory_core::{IdentityKey, NewObservation, ObservationKind, Sanitized, Sanitizer};
    use ai_memory_store::{Compaction, PurgeMode};
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let user = store
        .writer
        .create_human_user(
            NewUser {
                username: "cascade-alice".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let foreign = store
        .writer
        .get_or_create_project(ws, "foreign", None)
        .await
        .unwrap();
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: Some("user:cascade-alice".into()),
        })
        .await
        .unwrap();
    let observation = NewObservation {
        session_id: sid,
        workspace_id: ws,
        project_id: foreign,
        kind: ObservationKind::UserPrompt,
        extension: None,
        source_event: None,
        title: "foreign record".into(),
        body: "must survive a self-service purge".into(),
        importance: 5,
        occurred_at: None,
    };
    store
        .writer
        .insert_observation(Sanitized::new(observation, &Sanitizer::default()))
        .await
        .unwrap();
    for mode in [PurgeMode::Preview, PurgeMode::Commit] {
        assert!(matches!(
            store
                .writer
                .purge_owned_session(
                    ws,
                    proj,
                    sid,
                    IdentityKey::User("cascade-alice".into()),
                    Some(user),
                    mode
                )
                .await,
            Err(StoreError::InvalidState(_))
        ));
        assert_eq!(
            store
                .reader
                .observations_for_session(sid)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    let preview = store
        .writer
        .purge_session(
            ws,
            proj,
            sid,
            Some(user),
            Compaction::Skip,
            PurgeMode::Preview,
        )
        .await
        .unwrap();
    assert_eq!(preview.collateral_observations_deleted, 1);
    store
        .writer
        .purge_session(
            ws,
            proj,
            sid,
            Some(user),
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();
    assert!(
        store
            .reader
            .observations_for_session(sid)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Open feedback findings ignore feedback attached to expired pages.
#[tokio::test]
async fn open_feedback_findings_ignores_expired_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let live = page(ws, proj, "notes/live.md", "Live", "active content");
    store.writer.upsert_page(live).await.unwrap();

    let mut expired = page(ws, proj, "notes/expired.md", "Expired", "expired content");
    expired.expires_at = Some(jiff::Timestamp::from_second(1).unwrap());
    store.writer.upsert_page(expired).await.unwrap();

    store
        .writer
        .record_page_feedback(
            ws,
            proj,
            PagePath::new("notes/live.md").unwrap(),
            FeedbackKind::Stale,
            Some("live page is stale".to_string()),
            None,
            DecayParams::default(),
        )
        .await
        .unwrap();

    store
        .writer
        .record_page_feedback(
            ws,
            proj,
            PagePath::new("notes/expired.md").unwrap(),
            FeedbackKind::Wrong,
            Some("expired page was wrong".to_string()),
            None,
            DecayParams::default(),
        )
        .await
        .unwrap();

    let findings = store.reader.open_feedback_findings(ws, proj).await.unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].path, "notes/live.md");
    assert_eq!(findings[0].kind, "stale");
    assert_eq!(findings[0].reason.as_deref(), Some("live page is stale"));
}

/// A link authored by a superseded page version is not a reference.
/// `replace_links_in_tx` deletes rows only for the new version's id, so v1's
/// row survives an edit, and `refresh_incoming_links_for_path` repoints it at
/// the new latest id without filtering on `is_latest` — so without a join to a
/// live source page, a link the author removed still answers for its target.
#[tokio::test]
async fn memory_health_orphans_ignore_links_from_a_superseded_version() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .upsert_page(page(ws, proj, "notes/b.md", "B", "target"))
        .await
        .unwrap();

    // a.md v1 links b.md.
    let mut a_v1 = page(ws, proj, "notes/a.md", "A", "see b");
    a_v1.links = vec![ai_memory_core::LinkTarget {
        workspace: None,
        project: None,
        path: PagePath::new("notes/b.md").unwrap(),
        relation: None,
    }];
    store.writer.upsert_page(a_v1).await.unwrap();

    // Control: while the link is live, b.md is not an orphan.
    let (_, _, linked_orphans) = store
        .reader
        .memory_health_for_project(ws, proj)
        .await
        .unwrap();
    assert_eq!(
        linked_orphans, 0,
        "with a live inbound link neither page is an orphan"
    );

    // a.md v2 drops the link; v1's row survives, repointed at b.md's latest id.
    store
        .writer
        .upsert_page(page(ws, proj, "notes/a.md", "A", "no link now"))
        .await
        .unwrap();

    let (_, _, orphans) = store
        .reader
        .memory_health_for_project(ws, proj)
        .await
        .unwrap();
    assert_eq!(
        orphans, 2,
        "both pages are orphans once the only link is gone from the latest version"
    );

    let detail = store
        .reader
        .health_detail_for_project(ws, proj, 10)
        .await
        .unwrap();
    let orphan_paths: Vec<&str> = detail.orphans.iter().map(|p| p.path.as_str()).collect();
    assert!(
        orphan_paths.contains(&"notes/b.md"),
        "b.md lost its only inbound link and must be listed: {orphan_paths:?}"
    );
}

/// A link authored by a TTL-expired page is not a reference either, and the
/// workspace-wide health queries (which bind no project) agree with the
/// project-scoped ones. A live link to a second target is the control.
#[tokio::test]
async fn memory_health_orphans_ignore_links_from_an_expired_page() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let link_to = |path: &str| ai_memory_core::LinkTarget {
        workspace: None,
        project: None,
        path: PagePath::new(path).unwrap(),
        relation: None,
    };

    for path in ["notes/expired_target.md", "notes/live_target.md"] {
        store
            .writer
            .upsert_page(page(ws, proj, path, path, "target"))
            .await
            .unwrap();
    }
    let mut expired_source = page(ws, proj, "notes/expired_source.md", "E", "see target");
    expired_source.links = vec![link_to("notes/expired_target.md")];
    expired_source.expires_at = Some(jiff::Timestamp::from_second(1).unwrap());
    store.writer.upsert_page(expired_source).await.unwrap();
    let mut live_source = page(ws, proj, "notes/live_source.md", "L", "see target");
    live_source.links = vec![link_to("notes/live_target.md")];
    store.writer.upsert_page(live_source).await.unwrap();

    let (_, _, orphans) = store
        .reader
        .memory_health_for_workspace(ws, None)
        .await
        .unwrap();
    assert_eq!(
        orphans, 1,
        "only the target of the expired page's link is an orphan"
    );

    let detail = store
        .reader
        .health_detail_for_workspace(ws, 10, None)
        .await
        .unwrap();
    let orphan_paths: Vec<&str> = detail.orphans.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(orphan_paths, ["notes/expired_target.md"]);
}

/// Memory health counts and detail lists ignore TTL-expired pages.
#[tokio::test]
async fn memory_health_ignores_expired_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let past_time = jiff::Timestamp::from_second(1).unwrap();

    // 1. Live episodic page and expired episodic page.
    let mut live_stale = page(ws, proj, "notes/live_stale.md", "LiveStale", "content");
    live_stale.tier = Tier::Episodic;
    store.writer.upsert_page(live_stale).await.unwrap();

    let mut expired_stale = page(
        ws,
        proj,
        "notes/expired_stale.md",
        "ExpiredStale",
        "content",
    );
    expired_stale.tier = Tier::Episodic;
    expired_stale.expires_at = Some(past_time);
    store.writer.upsert_page(expired_stale).await.unwrap();

    // 2. Duplicate titles: one live page and one expired page sharing title "SameTitle".
    let live_dup = page(ws, proj, "notes/live_dup.md", "SameTitle", "content");
    store.writer.upsert_page(live_dup).await.unwrap();

    let mut expired_dup = page(ws, proj, "notes/expired_dup.md", "SameTitle", "content");
    expired_dup.expires_at = Some(past_time);
    store.writer.upsert_page(expired_dup).await.unwrap();

    // 3. Orphans: live_stale and live_dup have no links.
    // An expired orphan page should not be counted or listed as an orphan.
    let mut expired_orphan = page(
        ws,
        proj,
        "notes/expired_orphan.md",
        "ExpiredOrphan",
        "content",
    );
    expired_orphan.expires_at = Some(past_time);
    store.writer.upsert_page(expired_orphan).await.unwrap();

    // Set updated_at on the episodic pages to 60 days ago so they qualify as stale (< 30 days).
    let sixty_days_ago_us = jiff::Timestamp::now().as_microsecond() - 60 * 86_400 * 1_000_000;
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE pages SET updated_at = ?1 WHERE path IN ('notes/live_stale.md', 'notes/expired_stale.md')",
        rusqlite::params![sixty_days_ago_us],
    )
    .unwrap();

    // Workspace-level health counts and detail:
    let (stale, dups, orphans) = store
        .reader
        .memory_health_for_workspace(ws, None)
        .await
        .unwrap();
    assert_eq!(
        stale, 1,
        "only live_stale is counted, expired_stale is ignored"
    );
    assert_eq!(
        dups, 0,
        "SameTitle has only one live page, expired_dup is ignored"
    );
    assert_eq!(
        orphans, 2,
        "only live_stale and live_dup are orphans; expired_orphan is ignored"
    );

    let detail = store
        .reader
        .health_detail_for_workspace(ws, 10, None)
        .await
        .unwrap();
    assert_eq!(detail.stale.len(), 1);
    assert_eq!(detail.stale[0].path, "notes/live_stale.md");
    assert!(detail.duplicates.is_empty());
    let orphan_paths: Vec<&str> = detail.orphans.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(
        orphan_paths,
        vec!["notes/live_dup.md", "notes/live_stale.md"]
    );

    // Project-level health counts and detail:
    let (p_stale, p_dups, p_orphans) = store
        .reader
        .memory_health_for_project(ws, proj)
        .await
        .unwrap();
    assert_eq!(p_stale, 1);
    assert_eq!(p_dups, 0);
    assert_eq!(p_orphans, 2);

    let p_detail = store
        .reader
        .health_detail_for_project(ws, proj, 10)
        .await
        .unwrap();
    assert_eq!(p_detail.stale.len(), 1);
    assert_eq!(p_detail.stale[0].path, "notes/live_stale.md");
    assert!(p_detail.duplicates.is_empty());
    assert_eq!(p_detail.orphans.len(), 2);
}

/// The briefing's `pages_latest` must agree with the page lists in the same
/// snapshot: an expired page is hidden from every one of them, so counting it
/// in the headline makes the brief advertise a page the agent cannot reach.
#[tokio::test]
async fn briefing_pages_latest_ignores_expired_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .upsert_page(page(ws, proj, "notes/live.md", "Live", "content"))
        .await
        .unwrap();
    let mut expired = page(ws, proj, "notes/expired.md", "Expired", "content");
    expired.expires_at = Some(jiff::Timestamp::from_second(1).unwrap());
    store.writer.upsert_page(expired).await.unwrap();

    let live_pages = store
        .reader
        .list_unexpired_pages("acme", "shared-app")
        .await
        .unwrap();
    assert_eq!(live_pages.len(), 1, "one of the two pages is expired");

    let project = store
        .reader
        .briefing_for_project(ws, proj, 5, ai_memory_core::OwnerFilter::Any, false)
        .await
        .unwrap();
    assert_eq!(
        project.counts.pages_latest,
        live_pages.len() as u64,
        "project pages_latest counts only the unexpired latest page"
    );
    assert_eq!(
        project.recent_pages.len(),
        1,
        "control: the list the count is supposed to agree with holds one page"
    );
    // Control: the all-versions total is deliberately TTL-blind, so a fix that
    // merely broke both counts the same way cannot satisfy this test.
    assert_eq!(
        project.counts.pages_all, 2,
        "control: pages_all stays the raw all-versions total"
    );

    let workspace = store
        .reader
        .briefing_for_workspace(ws, 5, ai_memory_core::OwnerFilter::Any, None)
        .await
        .unwrap();
    assert_eq!(workspace.counts.pages_latest, 1);
    assert_eq!(workspace.counts.pages_all, 2);

    let global = store
        .reader
        .briefing(5, ai_memory_core::OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(global.counts.pages_latest, 1);
    assert_eq!(global.counts.pages_all, 2);
}

/// The briefing's activity windows are what an agent calibrates verbosity
/// against, and `recent_pages` in the same snapshot already hides TTL-expired
/// pages. A page written today with an expiry an hour ago must not make the
/// project look active.
#[tokio::test]
async fn briefing_activity_windows_ignore_expired_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let mut expired = page(ws, proj, "notes/expired.md", "Expired", "content");
    expired.expires_at = Some(jiff::Timestamp::from_second(1).unwrap());
    store.writer.upsert_page(expired).await.unwrap();

    let project = store
        .reader
        .briefing_for_project(ws, proj, 5, ai_memory_core::OwnerFilter::Any, false)
        .await
        .unwrap();
    assert!(
        project.recent_pages.is_empty(),
        "control: the only page in the window is already hidden from the list"
    );
    assert_eq!(
        project.activity_7d.pages_updated, 0,
        "the expired page does not count as 7-day project activity"
    );
    assert_eq!(project.activity_30d.pages_updated, 0);

    let workspace = store
        .reader
        .briefing_for_workspace(ws, 5, ai_memory_core::OwnerFilter::Any, None)
        .await
        .unwrap();
    assert_eq!(workspace.activity_7d.pages_updated, 0);
    assert_eq!(workspace.activity_30d.pages_updated, 0);

    let global = store
        .reader
        .briefing(5, ai_memory_core::OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(global.activity_7d.pages_updated, 0);
    assert_eq!(global.activity_30d.pages_updated, 0);

    // Control: a live page written in the same window still counts, so the
    // guard cannot pass by zeroing the counter outright.
    store
        .writer
        .upsert_page(page(ws, proj, "notes/live.md", "Live", "content"))
        .await
        .unwrap();
    let project = store
        .reader
        .briefing_for_project(ws, proj, 5, ai_memory_core::OwnerFilter::Any, false)
        .await
        .unwrap();
    assert_eq!(project.activity_7d.pages_updated, 1);
    assert_eq!(project.activity_30d.pages_updated, 1);
    let workspace = store
        .reader
        .briefing_for_workspace(ws, 5, ai_memory_core::OwnerFilter::Any, None)
        .await
        .unwrap();
    assert_eq!(workspace.activity_7d.pages_updated, 1);
    let global = store
        .reader
        .briefing(5, ai_memory_core::OwnerFilter::Any)
        .await
        .unwrap();
    assert_eq!(global.activity_7d.pages_updated, 1);
}

/// Multiple concurrent sessions of the same human operator under
/// `ActiveProjectMode::PerActor` must isolate active project pointers
/// per `(user, session_id)` tuple.
///
/// Mappings must not leak across session IDs:
/// 1. Unscoped read resolution resolves each session to its own project with
///    `ScopeSource::Session`.
/// 2. An unbound session of the same operator fails closed on write with
///    `AmbiguousUnscopedWrite` and falls back on read with `ScopeSource::DefaultAfterMismatch`
///    (invariant: never silently guess or leak another session's project).
/// 3. Updating the active project in Session 1 leaves Session 2's pointer intact.
/// 4. Concurrent unscoped writes and reads land in the respective projects without
///    cross-scope contamination.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_operator_multiple_sessions_isolate_active_project_pointers_and_resolution() {
    use ai_memory_core::{ActiveProject, ActiveProjectMode, ActorKey};
    use ai_memory_store::{ProjectPrincipal, ScopeResolutionError, ScopeResolver, ScopeSource};
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("acme".to_string())
        .await
        .unwrap();
    store
        .writer
        .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();

    let proj_alpha = store
        .writer
        .get_or_create_project(ws, "project-alpha".to_string(), None)
        .await
        .unwrap();
    let proj_beta = store
        .writer
        .get_or_create_project(ws, "project-beta".to_string(), None)
        .await
        .unwrap();
    let proj_gamma = store
        .writer
        .get_or_create_project(ws, "project-gamma".to_string(), None)
        .await
        .unwrap();

    let alice = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice-multisession".into(),
                name: Some("Alice".into()),
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();

    for proj in [proj_alpha, proj_beta, proj_gamma] {
        store
            .writer
            .grant_memory(alice, proj, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
    }

    let active = ActiveProject::with_config(
        ActiveProjectMode::PerActor,
        Duration::from_secs(60),
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    );

    let alice_session_1 = ActorKey {
        user: Some(operator("alice-multisession")),
        session_id: Some("session-alpha".into()),
    };
    let alice_session_2 = ActorKey {
        user: Some(operator("alice-multisession")),
        session_id: Some("session-beta".into()),
    };
    let alice_session_unbound = ActorKey {
        user: Some(operator("alice-multisession")),
        session_id: Some("session-fresh".into()),
    };

    active.set_for(&alice_session_1, ws, proj_alpha, false);
    active.set_for(&alice_session_2, ws, proj_beta, false);

    assert_eq!(active.get_for(&alice_session_1), Some((ws, proj_alpha)));
    assert_eq!(active.get_for(&alice_session_2), Some((ws, proj_beta)));

    let resolve_read = |actor: ActorKey| {
        let reader = store.reader.clone();
        let active = active.clone();
        async move {
            ScopeResolver::new(&reader, ws, proj_alpha)
                .with_active_project(&active)
                .with_project_authz(ProjectPrincipal::user(alice), true)
                .resolve_read_args_traced(None, None, &actor)
                .await
        }
    };

    let resolve_write = |actor: ActorKey| {
        let reader = store.reader.clone();
        let writer = store.writer.clone();
        let active = active.clone();
        async move {
            ScopeResolver::new(&reader, ws, proj_alpha)
                .with_writer(&writer)
                .with_active_project(&active)
                .with_project_authz(ProjectPrincipal::user(alice), true)
                .resolve_write_args(None, None, &actor)
                .await
        }
    };

    let (res_1, res_2) = tokio::join!(
        resolve_read(alice_session_1.clone()),
        resolve_read(alice_session_2.clone()),
    );
    let (scope_1, source_1) = res_1.unwrap();
    let (scope_2, source_2) = res_2.unwrap();

    assert_eq!(scope_1.project_id, proj_alpha);
    assert_eq!(source_1, ScopeSource::Session);
    assert_eq!(scope_2.project_id, proj_beta);
    assert_eq!(source_2, ScopeSource::Session);

    let write_unbound = resolve_write(alice_session_unbound.clone()).await;
    assert!(
        matches!(
            write_unbound,
            Err(ScopeResolutionError::AmbiguousUnscopedWrite)
        ),
        "an unbound session of an operator with active sessions must fail closed with AmbiguousUnscopedWrite: {write_unbound:?}"
    );
    let (read_unbound_scope, read_unbound_source) =
        resolve_read(alice_session_unbound.clone()).await.unwrap();
    assert_eq!(read_unbound_scope.project_id, proj_alpha);
    assert_eq!(
        read_unbound_source,
        ScopeSource::DefaultAfterMismatch,
        "an unbound session on read must fall back to the default scope with DefaultAfterMismatch rather than leaking another session's project"
    );

    active.set_for(&alice_session_1, ws, proj_gamma, false);
    assert_eq!(active.get_for(&alice_session_1), Some((ws, proj_gamma)));
    assert_eq!(
        active.get_for(&alice_session_2),
        Some((ws, proj_beta)),
        "session 1 pointer mutation must not move session 2's pointer"
    );

    let (scope_1_shifted, _) = resolve_read(alice_session_1.clone()).await.unwrap();
    let (scope_2_steady, _) = resolve_read(alice_session_2.clone()).await.unwrap();
    assert_eq!(scope_1_shifted.project_id, proj_gamma);
    assert_eq!(scope_2_steady.project_id, proj_beta);

    let (write_1, write_2) = tokio::join!(
        resolve_write(alice_session_1.clone()),
        resolve_write(alice_session_2.clone()),
    );
    let write_1 = write_1.unwrap();
    let write_2 = write_2.unwrap();

    assert_eq!(write_1.scope.project_id, proj_gamma);
    assert_eq!(write_2.scope.project_id, proj_beta);

    store
        .writer
        .upsert_page(page(
            ws,
            write_1.scope.project_id,
            "notes/session.md",
            "Gamma Session",
            "written from session 1",
        ))
        .await
        .unwrap();

    store
        .writer
        .upsert_page(page(
            ws,
            write_2.scope.project_id,
            "notes/session.md",
            "Beta Session",
            "written from session 2",
        ))
        .await
        .unwrap();

    let body_gamma = store
        .reader
        .page_body_by_ids(ws, proj_gamma, "notes/session.md")
        .await
        .unwrap()
        .unwrap();
    assert!(body_gamma.body.contains("written from session 1"));

    let body_beta = store
        .reader
        .page_body_by_ids(ws, proj_beta, "notes/session.md")
        .await
        .unwrap()
        .unwrap();
    assert!(body_beta.body.contains("written from session 2"));
}

#[tokio::test]
async fn interrupted_recovery_refuses_foreign_scope_owner_and_receiver_with_controls() {
    use ai_memory_core::{NewObservation, ObservationKind, Sanitized, Sanitizer};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (workspace, project) = scope(&store).await;
    let sibling = store
        .writer
        .get_or_create_project(workspace, "sibling", None)
        .await
        .unwrap();
    let other_workspace = store.writer.get_or_create_workspace("other").await.unwrap();
    let other_project = store
        .writer
        .get_or_create_project(other_workspace, "shared-app", None)
        .await
        .unwrap();
    let mut sessions = Vec::new();
    for (index, (ws, proj, owner)) in [
        (workspace, project, "alice"),
        (workspace, sibling, "alice"),
        (other_workspace, other_project, "alice"),
        (workspace, project, "bob"),
    ]
    .into_iter()
    .enumerate()
    {
        let session = SessionId::new();
        let occurred = 1_790_812_800_000_i64 + i64::try_from(index).unwrap() * 1_000;
        store
            .writer
            .begin_session(NewSession {
                occurred_at: Some(occurred),
                id: session,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: Some("/repo".into()),
                actor_user: Some(operator(owner)),
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    occurred_at: Some(occurred),
                    session_id: session,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: format!("{owner} unfinished task"),
                    body: format!("private evidence {index}"),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
        sessions.push(session);
    }

    let alice = OwnerFilter::User(operator("alice"));
    let own = store
        .reader
        .latest_interrupted_session_candidate(workspace, project, alice.clone(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        own.session_id, sessions[0],
        "newer foreign scope and owner rows must not replace the legitimate candidate"
    );
    assert!(
        store
            .reader
            .latest_interrupted_session_candidate(workspace, project, alice, Some(sessions[0]))
            .await
            .unwrap()
            .is_none(),
        "the receiver cannot recover its own observations"
    );
    assert!(
        store
            .reader
            .latest_interrupted_session_candidate(other_workspace, project, OwnerFilter::Any, None)
            .await
            .unwrap()
            .is_none(),
        "a project id under a foreign workspace must fail closed"
    );
    assert!(
        store
            .reader
            .latest_interrupted_session_candidate(
                workspace,
                project,
                OwnerFilter::Unattributed,
                None
            )
            .await
            .unwrap()
            .is_none(),
        "an unattributed caller cannot read owned recovery evidence"
    );
    let root = store
        .reader
        .latest_interrupted_session_candidate(workspace, project, OwnerFilter::Any, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        root.session_id, sessions[3],
        "an authorized unfiltered caller can read the newer home session"
    );
    let foreign_control = store
        .reader
        .latest_interrupted_session_candidate(
            other_workspace,
            other_project,
            OwnerFilter::User(operator("alice")),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(foreign_control.session_id, sessions[2]);
}

/// One session id can carry work and handoffs in several scopes. Neither may
/// decide recovery in another scope: foreign work must not reopen an ended
/// session here, and a foreign baton must not hide unfinished work here.
#[tokio::test]
async fn interrupted_recovery_ignores_foreign_scope_work_and_batons_with_controls() {
    use ai_memory_core::{NewObservation, ObservationKind, Sanitized, Sanitizer};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (workspace, project) = scope(&store).await;
    let sibling = store
        .writer
        .get_or_create_project(workspace, "sibling", None)
        .await
        .unwrap();
    // Prompts land before or after the wall-clock end and baton timestamps.
    let (before, after) = (1_790_812_800_000_i64, 4_102_444_800_000_000_i64);
    let prompt = |session: SessionId, proj: ProjectId, occurred: i64| {
        Sanitized::new(
            NewObservation {
                occurred_at: Some(occurred),
                session_id: session,
                workspace_id: workspace,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "unfinished task".into(),
                body: "private evidence".into(),
                importance: 5,
            },
            &Sanitizer::builtin(),
        )
    };
    let baton = |session: SessionId, proj: ProjectId| NewHandoff {
        workspace_id: workspace,
        project_id: proj,
        from_agent: AgentKind::Codex,
        to_agent: None,
        from_session_id: Some(session),
        summary: "baton".into(),
        next_steps: Vec::new(),
        open_questions: Vec::new(),
        files_touched: Vec::new(),
        cwd: None,
        owner_user: None,
    };
    let candidate = || async {
        store
            .reader
            .latest_interrupted_session_candidate(workspace, project, OwnerFilter::Any, None)
            .await
            .unwrap()
            .map(|candidate| candidate.session_id)
    };

    let unfinished = open_session(&store, workspace, project, AgentKind::Codex).await;
    store
        .writer
        .insert_observation(prompt(unfinished, project, before))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(baton(unfinished, sibling))
        .await
        .unwrap();
    assert_eq!(
        candidate().await,
        Some(unfinished),
        "a baton from another scope must not hide unfinished work here"
    );
    store
        .writer
        .insert_handoff(baton(unfinished, project))
        .await
        .unwrap();
    assert_eq!(
        candidate().await,
        None,
        "control: a baton from this scope covers the work"
    );

    let ended = open_session(&store, workspace, project, AgentKind::Codex).await;
    store
        .writer
        .insert_observation(prompt(ended, project, before))
        .await
        .unwrap();
    store.writer.end_session(ended, None).await.unwrap();
    store
        .writer
        .insert_observation(prompt(ended, sibling, after))
        .await
        .unwrap();
    assert_eq!(
        candidate().await,
        None,
        "work in another scope must not reopen a session ended here"
    );
    store
        .writer
        .insert_observation(prompt(ended, project, after))
        .await
        .unwrap();
    assert_eq!(
        candidate().await,
        Some(ended),
        "control: work resumed in this scope reopens it"
    );
}
