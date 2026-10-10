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
    ActorContext, AgentKind, HandoffAcceptance, HandoffState, IdentityKey, NewHandoff, NewPage,
    NewSession, NewUser, OwnerFilter, PagePath, ProjectId, SessionId, Tier, UserRole, WorkspaceId,
    owner_stamp,
};
use ai_memory_store::{
    LinkOrAdoptManagedRunSession, ManagedRunSessionLink, PrepareWorkstreamRun, Store, StoreError,
    WorkstreamSelection,
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
        .create_user(
            NewUser {
                username: "alice-promotion".into(),
                name: None,
                email: None,
            },
            [41; ai_memory_store::TOKEN_HASH_LEN],
        )
        .await
        .unwrap();
    let bob = store
        .writer
        .create_user(
            NewUser {
                username: "bob-promotion".into(),
                name: None,
                email: None,
            },
            [42; ai_memory_store::TOKEN_HASH_LEN],
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
    store
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
    // Cancel and replacement both store the existing expired phase.
    for replaced in [false, true] {
        let old = store
            .writer
            .prepare_workstream_run(PrepareWorkstreamRun {
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
            store
                .writer
                .prepare_workstream_run(PrepareWorkstreamRun {
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
        assert_eq!(finish_snapshot(&store).await, before);
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
    // The legacy resolution remains fail-open; it is insufficient for finish.
    let legacy = ai_memory_store::resolve_project_authz(
        &conn,
        f.ws,
        f.project,
        &ai_memory_store::ProjectPrincipal::user(f.alice),
        true,
    )
    .unwrap();
    assert_eq!(legacy.access_mode, ai_memory_store::AccessMode::Open);
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
