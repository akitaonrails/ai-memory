//! Integration tests for per-operator handoff ownership (V39).
//!
//! The behaviour these pin down is the one that breaks a shared server: the
//! open-handoff lookup used to be scoped by `(workspace, project, state)` only,
//! so the next session to start — whoever it belonged to — consumed the pending
//! baton, and its author never received it. Delivery is destructive, so the
//! handoff was simply lost.
//!
//! Owners are stored as qualified [`IdentityKey::storage_key`] TEXT
//! (`user:alice`, `oidc:…`), never raw names. Every owner and filter here is
//! built through the contract's own API — `owner_stamp`,
//! [`OwnerFilter::for_actor_context`], [`IdentityKey::storage_key`] — so these
//! tests exercise the same encoding production uses rather than a hand-written
//! string that could drift from it.

use ai_memory_core::{
    ActorContext, AgentKind, HandoffAcceptance, HandoffId, IdentityKey, NewHandoff, OwnerFilter,
    ProjectId, WorkspaceId, owner_stamp,
};
use ai_memory_store::Store;

/// The qualified storage key a username-identified operator owns rows under.
fn operator(name: &str) -> String {
    IdentityKey::User(name.into()).storage_key()
}

/// The read filter the same operator's requests resolve to, built through the
/// one identity rule instead of a hand-assembled variant.
fn filter_for(name: &str) -> OwnerFilter {
    OwnerFilter::for_actor_context(&ActorContext {
        user: Some(name.into()),
        ..ActorContext::default()
    })
}

fn acceptance(
    handoff_id: HandoffId,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    accepting_user: Option<String>,
    owner_filter: OwnerFilter,
    receiving_cwd: Option<String>,
) -> HandoffAcceptance {
    HandoffAcceptance {
        handoff_id,
        workspace_id,
        project_id,
        accepting_agent: AgentKind::ClaudeCode,
        accepting_session: None,
        accepting_user,
        owner_filter,
        receiving_cwd,
    }
}

/// Build an open handoff, optionally owned by the named operator.
fn handoff(
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    summary: &str,
    owner: Option<&str>,
) -> NewHandoff {
    NewHandoff {
        workspace_id,
        project_id,
        // `None` is what `memory_handoff_begin` writes: a manual handoff. It is
        // the shape that used to be project-wide AND top of the ranking, so it
        // is the one that leaked across operators.
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: None,
        summary: summary.into(),
        open_questions: Vec::new(),
        next_steps: Vec::new(),
        files_touched: Vec::new(),
        // The write-side gate, exactly as a distinguishing deployment applies
        // it: identity in, storage key out.
        owner_user: owner.and_then(|name| owner_stamp(Some(&IdentityKey::User(name.into())), true)),
    }
}

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("acme-hml".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "team-app".to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

/// The core regression: Alice leaves a baton, Bob starts a session in the same
/// project, and Bob must not receive — or consume — it.
#[tokio::test]
async fn one_operators_handoff_is_not_delivered_to_another() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .insert_handoff(handoff(
            ws,
            proj,
            "resume the OAuth refactor",
            Some("alice"),
        ))
        .await
        .unwrap();

    // Bob's session start finds nothing: the only open handoff is Alice's.
    assert!(
        store
            .reader
            .latest_open_handoff(ws, proj, None, filter_for("bob"))
            .await
            .unwrap()
            .is_none(),
        "Bob must not be offered Alice's handoff"
    );

    // And Alice still has hers — the point is that it was never consumed.
    let alice = store
        .reader
        .latest_open_handoff(ws, proj, None, filter_for("alice"))
        .await
        .unwrap()
        .expect("Alice keeps her own handoff");
    assert_eq!(alice.content.summary, "resume the OAuth refactor");
    assert_eq!(alice.origin.owner_user, Some(operator("alice")));
}

/// The SQL query must exclude foreign rows before it deserializes any of their
/// prompt-derived fields. Besides avoiding unnecessary exposure and work, this
/// means corruption in Bob's private row cannot deny Alice access to her own
/// valid baton.
#[tokio::test]
async fn latest_lookup_filters_foreign_rows_before_deserialization() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .insert_handoff(handoff(ws, proj, "alice's valid baton", Some("alice")))
        .await
        .unwrap();
    let bob = store
        .writer
        .insert_handoff(handoff(ws, proj, "bob's private baton", Some("bob")))
        .await
        .unwrap();

    // Simulate a corrupt row without routing invalid JSON through the typed
    // writer. With post-fetch filtering this row is deserialized first and the
    // whole lookup fails, even though Alice is not authorized to read it.
    let conn = rusqlite::Connection::open(tmp.path().join("db/memory.sqlite")).unwrap();
    conn.execute(
        "UPDATE handoffs SET open_questions = '{' WHERE id = ?1",
        rusqlite::params![bob.as_bytes()],
    )
    .unwrap();
    drop(conn);

    let alice = store
        .reader
        .latest_open_handoff(ws, proj, None, filter_for("alice"))
        .await
        .expect("a foreign corrupt row must not affect Alice's lookup")
        .expect("Alice keeps her valid baton");
    assert_eq!(alice.content.summary, "alice's valid baton");
}

#[tokio::test]
async fn ownership_writes_reject_malformed_identity_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let invalid_session = ai_memory_core::SessionId::new();
    assert!(
        store
            .writer
            .begin_session(ai_memory_core::NewSession {
                id: invalid_session,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: Some("alice".into()),
            })
            .await
            .is_err()
    );
    let mut invalid_handoff = handoff(ws, proj, "invalid", None);
    invalid_handoff.owner_user = Some("user:   ".into());
    assert!(store.writer.insert_handoff(invalid_handoff).await.is_err());

    let id = store
        .writer
        .insert_handoff(handoff(ws, proj, "valid", Some("alice")))
        .await
        .unwrap();
    assert!(
        store
            .writer
            .accept_handoff(acceptance(
                id,
                ws,
                proj,
                Some("alice".into()),
                filter_for("alice"),
                None,
            ))
            .await
            .is_err()
    );
    assert!(
        store
            .writer
            .accept_handoff(acceptance(
                id,
                ws,
                proj,
                Some(operator("bob")),
                filter_for("alice"),
                None,
            ))
            .await
            .is_err(),
        "the accepting identity and owner filter must not disagree"
    );
    assert_eq!(
        store
            .reader
            .handoff_by_id(id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        ai_memory_core::HandoffState::Open
    );

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE handoffs SET owner_user = 'user:   ' WHERE id = ?1",
        rusqlite::params![id.as_bytes()],
    )
    .unwrap();
    drop(conn);
    assert!(
        store.reader.handoff_by_id(id).await.is_err(),
        "raw database corruption must not materialize an invalid owner"
    );
}

/// The variant is part of the identity: a username equal to somebody else's
/// OIDC subject is a DIFFERENT operator, and neither may drain the other's
/// baton. Raw TEXT owners would have merged these two people.
#[tokio::test]
async fn a_username_does_not_alias_an_equal_subject() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let by_subject = IdentityKey::Subject {
        issuer: "https://idp.example".into(),
        subject: "alice".into(),
    };
    let mut owned = handoff(ws, proj, "the subject's baton", None);
    owned.owner_user = owner_stamp(Some(&by_subject), true);
    store.writer.insert_handoff(owned).await.unwrap();

    // The USERNAME alice — same raw string, different name space.
    assert!(
        store
            .reader
            .latest_open_handoff(ws, proj, None, filter_for("alice"))
            .await
            .unwrap()
            .is_none(),
        "user:alice must not receive the OIDC-owned baton"
    );
    // The subject-identified caller — the filter the auth layer builds for an
    // ingress that forwards the complete issuer/subject pair.
    let sub_filter = OwnerFilter::for_actor_context(&ActorContext {
        issuer: Some("https://idp.example".into()),
        sub: Some("alice".into()),
        ..ActorContext::default()
    });
    assert!(
        store
            .reader
            .latest_open_handoff(ws, proj, None, sub_filter)
            .await
            .unwrap()
            .is_some(),
        "the subject-identified operator keeps their own baton"
    );
}

/// Backwards compatibility, and the single-operator path: a handoff with no
/// owner (every pre-V39 row, anything written without an actor, and an explicit
/// `shared: true`) is still delivered to anybody.
#[tokio::test]
async fn shared_handoffs_are_delivered_to_everyone() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .insert_handoff(handoff(ws, proj, "team baton", None))
        .await
        .unwrap();

    for filter in [
        filter_for("alice"),
        filter_for("bob"),
        OwnerFilter::Unattributed,
        OwnerFilter::Any,
    ] {
        let got = store
            .reader
            .latest_open_handoff(ws, proj, None, filter.clone())
            .await
            .unwrap();
        assert!(
            got.is_some(),
            "an unowned handoff must stay visible to {filter:?}"
        );
    }
}

/// Claiming is guarded in the same UPDATE that flips the state, so a caller who
/// is not admitted changes zero rows and is told so — the body must not be
/// handed over on `false`.
#[tokio::test]
async fn another_operator_cannot_claim_the_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(handoff(ws, proj, "alice's baton", Some("alice")))
        .await
        .unwrap();

    let stolen = store
        .writer
        .accept_handoff(acceptance(
            id,
            ws,
            proj,
            Some(operator("bob")),
            filter_for("bob"),
            None,
        ))
        .await
        .unwrap();
    assert!(!stolen, "Bob must not be able to claim Alice's handoff");

    // Still open for its owner, and now claimable by her exactly once.
    let claimed = store
        .writer
        .accept_handoff(acceptance(
            id,
            ws,
            proj,
            Some(operator("alice")),
            filter_for("alice"),
            None,
        ))
        .await
        .unwrap();
    assert!(claimed, "the owner claims her own handoff");

    let twice = store
        .writer
        .accept_handoff(acceptance(
            id,
            ws,
            proj,
            Some(operator("alice")),
            filter_for("alice"),
            None,
        ))
        .await
        .unwrap();
    assert!(!twice, "a handoff is single-use even for its owner");

    // The acceptance is attributed to a person, not just an agent kind, so
    // "who took my baton" is answerable after the fact.
    let row = store.reader.handoff_by_id(id).await.unwrap().unwrap();
    assert_eq!(row.lifecycle.accepted_by_user, Some(operator("alice")));
}

/// An unattributed reader — the read-only `/api/v1` surface a browser reaches —
/// must not see a baton that belongs to somebody, because rendering it leaks the
/// raw prompt text it was built from.
#[tokio::test]
async fn unattributed_readers_do_not_see_owned_handoffs() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .insert_handoff(handoff(ws, proj, "private context", Some("alice")))
        .await
        .unwrap();

    assert!(
        store
            .reader
            .latest_open_handoff(ws, proj, None, OwnerFilter::Unattributed)
            .await
            .unwrap()
            .is_none()
    );
    // The workspace-wide overview query takes the same filter.
    assert!(
        store
            .reader
            .latest_open_handoff_for_workspace(ws, OwnerFilter::Unattributed)
            .await
            .unwrap()
            .is_none()
    );
    // …while an explicit recovery read still finds it.
    assert!(
        store
            .reader
            .latest_open_handoff_for_workspace(ws, OwnerFilter::Any)
            .await
            .unwrap()
            .is_some()
    );
}

/// Cancelling is scoped like accepting: you can discard your own baton, not a
/// teammate's.
#[tokio::test]
async fn another_operator_cannot_cancel_the_handoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let id = store
        .writer
        .insert_handoff(handoff(ws, proj, "alice's baton", Some("alice")))
        .await
        .unwrap();

    assert!(
        !store
            .writer
            .cancel_handoff(id, ws, proj, filter_for("bob"))
            .await
            .unwrap(),
        "Bob must not cancel Alice's handoff"
    );
    assert!(
        store
            .writer
            .cancel_handoff(id, ws, proj, filter_for("alice"))
            .await
            .unwrap(),
        "the owner can cancel her own"
    );
}

/// Destructive operations must recheck the resolved scope in the same UPDATE
/// that changes state. A preceding scoped read is not sufficient because a
/// concurrent lifecycle operation could move the project between that read and
/// the writer command.
#[tokio::test]
async fn destructive_handoff_updates_recheck_scope_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let other_proj = store
        .writer
        .get_or_create_project(ws, "other-app".to_string(), None)
        .await
        .unwrap();
    let id = store
        .writer
        .insert_handoff(handoff(ws, proj, "scoped baton", Some("alice")))
        .await
        .unwrap();

    assert!(
        !store
            .writer
            .accept_handoff(acceptance(
                id,
                ws,
                other_proj,
                Some(operator("alice")),
                OwnerFilter::Any,
                None,
            ))
            .await
            .unwrap(),
        "even a root recovery claim must not cross its resolved project"
    );
    assert!(
        !store
            .writer
            .cancel_handoff(id, ws, other_proj, OwnerFilter::Any)
            .await
            .unwrap(),
        "even a root recovery cancel must not cross its resolved project"
    );
    assert_eq!(
        store
            .reader
            .handoff_by_id(id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        ai_memory_core::HandoffState::Open
    );
    assert!(
        store
            .writer
            .accept_handoff(acceptance(
                id,
                ws,
                proj,
                Some(operator("alice")),
                OwnerFilter::Any,
                None,
            ))
            .await
            .unwrap(),
        "the exact project can still claim the handoff"
    );
}

/// An owned handoff must not be reachable just because the caller asks with a
/// different cwd: ownership is checked BEFORE the manual/cwd rules, which is
/// what stops `memory_handoff_begin` rows (always manual, always project-wide by
/// cwd) from crossing operators.
#[tokio::test]
async fn ownership_is_checked_before_the_manual_cwd_shortcut() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let mut owned = handoff(ws, proj, "alice's manual baton", Some("alice"));
    owned.cwd = Some("/home/alice/src/team-app".into());
    store.writer.insert_handoff(owned).await.unwrap();

    // Bob asks from his own checkout; the manual short-circuit would have
    // matched regardless of cwd before ownership existed.
    assert!(
        store
            .reader
            .latest_open_handoff(
                ws,
                proj,
                Some("/home/bob/src/team-app".into()),
                filter_for("bob"),
            )
            .await
            .unwrap()
            .is_none()
    );
}

/// Handoffs had no listing anywhere in the system — every reader fetched the
/// single pending one and consumed it, so a baton delivered to the wrong place
/// simply vanished with no way to look it up. The listing is what makes that
/// recoverable, and it is owner-scoped so it does not become a way to read
/// other operators' context.
#[tokio::test]
async fn handoff_listing_is_owner_scoped_and_covers_every_state() {
    use ai_memory_core::HandoffState;

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let alice = store
        .writer
        .insert_handoff(handoff(ws, proj, "alice's", Some("alice")))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(handoff(ws, proj, "bob's", Some("bob")))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(handoff(ws, proj, "team", None))
        .await
        .unwrap();

    // Alice sees hers + the shared one, never Bob's.
    let summaries = |filter: OwnerFilter, state: Option<HandoffState>| {
        let store = &store;
        async move {
            store
                .reader
                .list_handoffs(ws, proj, state, filter, 50)
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.content.summary)
                .collect::<Vec<_>>()
        }
    };
    let seen = summaries(filter_for("alice"), None).await;
    assert!(seen.contains(&"alice's".to_string()));
    assert!(seen.contains(&"team".to_string()));
    assert!(!seen.contains(&"bob's".to_string()));

    // Consume Alice's, then find it again by state — the recovery path.
    store
        .writer
        .accept_handoff(acceptance(
            alice,
            ws,
            proj,
            Some(operator("alice")),
            filter_for("alice"),
            None,
        ))
        .await
        .unwrap();
    let open = summaries(filter_for("alice"), Some(HandoffState::Open)).await;
    assert!(!open.contains(&"alice's".to_string()), "no longer pending");
    let accepted = summaries(filter_for("alice"), Some(HandoffState::Accepted)).await;
    assert_eq!(
        accepted,
        vec!["alice's".to_string()],
        "a consumed handoff is still findable, which is what makes it recoverable"
    );
}

/// The owner key is not a validated identifier: behind a trusted proxy what
/// follows the `oidc:` prefix includes the OIDC identity a trusted proxy
/// asserted. A value carrying a single quote therefore
/// has to filter exactly like any other one, on every surface that splices the
/// owner predicate into SQL — the listing (with and without a state filter) and
/// all three briefing counts.
#[tokio::test]
async fn an_owner_name_with_a_quote_filters_like_any_other() {
    use ai_memory_core::HandoffState;

    let quoted = "o'brien";
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    store
        .writer
        .insert_handoff(handoff(ws, proj, "quoted", Some(quoted)))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(handoff(ws, proj, "bob's", Some("bob")))
        .await
        .unwrap();
    store
        .writer
        .insert_handoff(handoff(ws, proj, "team", None))
        .await
        .unwrap();

    let filter = || filter_for(quoted);

    let seen = store
        .reader
        .list_handoffs(ws, proj, None, filter(), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.content.summary)
        .collect::<Vec<_>>();
    assert!(seen.contains(&"quoted".to_string()), "own row: {seen:?}");
    assert!(seen.contains(&"team".to_string()), "shared row: {seen:?}");
    assert!(!seen.contains(&"bob's".to_string()), "leak: {seen:?}");

    // The state filter shifts the owner parameter one slot along, so it is the
    // arm most likely to bind the key into the wrong placeholder.
    let open = store
        .reader
        .list_handoffs(ws, proj, Some(HandoffState::Open), filter(), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.content.summary)
        .collect::<Vec<_>>();
    assert_eq!(open.len(), 2, "own + shared, still no leak: {open:?}");

    for count in [
        store
            .reader
            .briefing(5, filter())
            .await
            .unwrap()
            .pending_handoff_count,
        store
            .reader
            .briefing_for_project(ws, proj, 5, filter())
            .await
            .unwrap()
            .pending_handoff_count,
        store
            .reader
            .briefing_for_workspace(ws, 5, filter())
            .await
            .unwrap()
            .pending_handoff_count,
    ] {
        assert_eq!(count, 2, "count must agree with the listing");
    }
}

/// Automatic handoffs are retired in bulk — once when a new SessionEnd handoff
/// lands in the same directory, and again after a claim supersedes older
/// eligible ones. Both sweeps are cwd-scoped, and on a shared server several
/// operators work in the SAME directory (frequently the same container path),
/// so cwd separates nothing between them. Without an owner predicate the sweeps
/// destroy another operator's pending baton outright, which is worse than the
/// misdelivery ownership exists to prevent: nothing is delivered at all.
#[tokio::test]
async fn automatic_supersession_does_not_reach_across_operators() {
    use ai_memory_core::{HandoffState, NewSession, SessionId};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    // Same project, same cwd, one handoff each — the shape upstream's sweeps
    // treat as a single stream of supersedable batons. Session and baton carry
    // the SAME stamped key, exactly as the SessionEnd path writes them.
    async fn auto_handoff(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        owner: &str,
        summary: &str,
    ) -> ai_memory_core::HandoffId {
        let session_id = SessionId::new();
        let stamp = owner_stamp(Some(&IdentityKey::User(owner.into())), true);
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: Some("/repo".into()),
                actor_user: stamp.clone(),
            })
            .await
            .unwrap();
        store
            .writer
            .insert_handoff(NewHandoff {
                workspace_id: ws,
                project_id: proj,
                from_session_id: Some(session_id),
                from_agent: AgentKind::ClaudeCode,
                to_agent: None,
                cwd: Some("/repo".into()),
                summary: summary.into(),
                open_questions: Vec::new(),
                next_steps: Vec::new(),
                files_touched: Vec::new(),
                owner_user: stamp,
            })
            .await
            .unwrap()
    }

    let bob = auto_handoff(&store, ws, proj, "bob", "bob's baton").await;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    // Alice ending a session in /repo must not expire Bob's open handoff from
    // the same directory (`insert_handoff_row`'s same-cwd sweep).
    let alice_older = auto_handoff(&store, ws, proj, "alice", "alice's older").await;
    assert_eq!(
        store
            .reader
            .handoff_by_id(bob)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        HandoffState::Open,
        "another operator's SessionEnd must not expire Bob's baton"
    );

    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let alice_newest = auto_handoff(&store, ws, proj, "alice", "alice's newest").await;
    assert_eq!(
        store
            .reader
            .handoff_by_id(alice_older)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        HandoffState::Expired,
        "within one operator, the same-cwd sweep still bounds accumulation"
    );

    // And the post-claim sweep is bounded the same way: Alice claiming her own
    // baton retires nothing of Bob's, even though his row matches the cwd rule
    // and ranks below hers.
    let claimed = store
        .writer
        .accept_handoff(acceptance(
            alice_newest,
            ws,
            proj,
            Some(operator("alice")),
            filter_for("alice"),
            Some("/repo".into()),
        ))
        .await
        .unwrap();
    assert!(claimed, "Alice claims her own handoff");
    assert_eq!(
        store
            .reader
            .handoff_by_id(bob)
            .await
            .unwrap()
            .unwrap()
            .lifecycle
            .state,
        HandoffState::Open,
        "Alice's session start must not expire Bob's pending baton"
    );

    // Bob still receives it, which is the whole point.
    let bobs = store
        .reader
        .latest_open_handoff(ws, proj, Some("/repo".into()), filter_for("bob"))
        .await
        .unwrap()
        .expect("Bob's baton survives another operator's accept");
    assert_eq!(bobs.content.summary, "bob's baton");
}

/// Sessions record their operator (V40), and the open-session lookup behind
/// `finalize-session` is scoped the same way. The callers act destructively on
/// what this returns — ending the session, synthesising a page from its
/// observations, minting a handoff from its raw prompts — so "the newest open
/// session in the scope" across everyone would do all of that to a colleague's
/// live session. A session with no recorded operator stays visible to every
/// filter, which is the single-operator behaviour.
#[tokio::test]
async fn open_session_lookup_is_owner_scoped() {
    use ai_memory_core::{NewSession, SessionId};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let alice_session = SessionId::new();
    let shared_session = SessionId::new();
    for (id, owner) in [
        (
            alice_session,
            owner_stamp(Some(&IdentityKey::User("alice".into())), true),
        ),
        (shared_session, None),
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

    let ids = |sessions: Vec<ai_memory_store::OpenSession>| {
        sessions
            .into_iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>()
    };

    // Bob sees only the shared session — never Alice's live one.
    let bobs = ids(store
        .reader
        .open_sessions_for_scope_agent(ws, proj, AgentKind::ClaudeCode, filter_for("bob"), None)
        .await
        .unwrap());
    assert_eq!(
        bobs,
        vec![shared_session],
        "Bob must not see Alice's session"
    );

    // Alice sees her own plus the shared one.
    let alices = ids(store
        .reader
        .open_sessions_for_scope_agent(ws, proj, AgentKind::ClaudeCode, filter_for("alice"), None)
        .await
        .unwrap());
    assert!(alices.contains(&alice_session) && alices.contains(&shared_session));

    // The recovery switch (`all_owners=true` on /admin/open-sessions) sees all.
    let all = ids(store
        .reader
        .open_sessions_for_scope_agent(ws, proj, AgentKind::ClaudeCode, OwnerFilter::Any, None)
        .await
        .unwrap());
    assert_eq!(all.len(), 2);

    // The owner travels to the SessionEnd attribution path via the same row.
    assert_eq!(
        store
            .reader
            .session_actor_user(alice_session)
            .await
            .unwrap(),
        Some(operator("alice")),
    );
    assert_eq!(
        store
            .reader
            .session_actor_user(shared_session)
            .await
            .unwrap(),
        None,
    );
}
