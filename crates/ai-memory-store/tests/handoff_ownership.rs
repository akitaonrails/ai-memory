//! Integration tests for per-operator handoff ownership (V39).
//!
//! The behaviour these pin down is the one that breaks a shared server: the
//! open-handoff lookup used to be scoped by `(workspace, project, state)` only,
//! so the next session to start — whoever it belonged to — consumed the pending
//! baton, and its author never received it. Delivery is destructive, so the
//! handoff was simply lost.

use ai_memory_core::{AgentKind, NewHandoff, OwnerFilter, ProjectId, WorkspaceId};
use ai_memory_store::Store;

/// Build an open handoff, optionally owned by `owner`.
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
        owner_user: owner.map(str::to_string),
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
            .latest_open_handoff(ws, proj, None, OwnerFilter::User("bob".into()))
            .await
            .unwrap()
            .is_none(),
        "Bob must not be offered Alice's handoff"
    );

    // And Alice still has hers — the point is that it was never consumed.
    let alice = store
        .reader
        .latest_open_handoff(ws, proj, None, OwnerFilter::User("alice".into()))
        .await
        .unwrap()
        .expect("Alice keeps her own handoff");
    assert_eq!(alice.summary, "resume the OAuth refactor");
    assert_eq!(alice.owner_user.as_deref(), Some("alice"));
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
        OwnerFilter::User("alice".into()),
        OwnerFilter::User("bob".into()),
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
        .accept_handoff(
            id,
            AgentKind::ClaudeCode,
            None,
            Some("bob".into()),
            OwnerFilter::User("bob".into()),
            None,
        )
        .await
        .unwrap();
    assert!(!stolen, "Bob must not be able to claim Alice's handoff");

    // Still open for its owner, and now claimable by her exactly once.
    let claimed = store
        .writer
        .accept_handoff(
            id,
            AgentKind::ClaudeCode,
            None,
            Some("alice".into()),
            OwnerFilter::User("alice".into()),
            None,
        )
        .await
        .unwrap();
    assert!(claimed, "the owner claims her own handoff");

    let twice = store
        .writer
        .accept_handoff(
            id,
            AgentKind::ClaudeCode,
            None,
            Some("alice".into()),
            OwnerFilter::User("alice".into()),
            None,
        )
        .await
        .unwrap();
    assert!(!twice, "a handoff is single-use even for its owner");

    // The acceptance is attributed to a person, not just an agent kind, so
    // "who took my baton" is answerable after the fact.
    let row = store.reader.handoff_by_id(id).await.unwrap().unwrap();
    assert_eq!(row.accepted_by_user.as_deref(), Some("alice"));
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
            .cancel_handoff(id, OwnerFilter::User("bob".into()))
            .await
            .unwrap(),
        "Bob must not cancel Alice's handoff"
    );
    assert!(
        store
            .writer
            .cancel_handoff(id, OwnerFilter::User("alice".into()))
            .await
            .unwrap(),
        "the owner can cancel her own"
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
                OwnerFilter::User("bob".into()),
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
                .map(|h| h.summary)
                .collect::<Vec<_>>()
        }
    };
    let seen = summaries(OwnerFilter::User("alice".into()), None).await;
    assert!(seen.contains(&"alice's".to_string()));
    assert!(seen.contains(&"team".to_string()));
    assert!(!seen.contains(&"bob's".to_string()));

    // Consume Alice's, then find it again by state — the recovery path.
    store
        .writer
        .accept_handoff(
            alice,
            AgentKind::ClaudeCode,
            None,
            Some("alice".into()),
            OwnerFilter::User("alice".into()),
            None,
        )
        .await
        .unwrap();
    let open = summaries(OwnerFilter::User("alice".into()), Some(HandoffState::Open)).await;
    assert!(!open.contains(&"alice's".to_string()), "no longer pending");
    let accepted = summaries(
        OwnerFilter::User("alice".into()),
        Some(HandoffState::Accepted),
    )
    .await;
    assert_eq!(
        accepted,
        vec!["alice's".to_string()],
        "a consumed handoff is still findable, which is what makes it recoverable"
    );
}

/// The owner name is not a validated identifier: behind a trusted proxy it is
/// whatever `X-Memory-Actor-Sub` carried, an OIDC subject the engine never
/// parses. A name carrying a single quote therefore has to filter exactly like
/// any other one, on every surface that splices the owner predicate into SQL —
/// the listing (with and without a state filter) and all three briefing counts.
#[tokio::test]
async fn an_owner_name_with_a_quote_filters_like_any_other() {
    use ai_memory_core::{HandoffState, SlotVisibility};

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

    let filter = || OwnerFilter::User(quoted.into());

    let seen = store
        .reader
        .list_handoffs(ws, proj, None, filter(), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.summary)
        .collect::<Vec<_>>();
    assert!(seen.contains(&"quoted".to_string()), "own row: {seen:?}");
    assert!(seen.contains(&"team".to_string()), "shared row: {seen:?}");
    assert!(!seen.contains(&"bob's".to_string()), "leak: {seen:?}");

    // The state filter shifts the owner parameter one slot along, so it is the
    // arm most likely to bind the name into the wrong placeholder.
    let open = store
        .reader
        .list_handoffs(ws, proj, Some(HandoffState::Open), filter(), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.summary)
        .collect::<Vec<_>>();
    assert_eq!(open.len(), 2, "own + shared, still no leak: {open:?}");

    for count in [
        store
            .reader
            .briefing(5, filter(), &SlotVisibility::default())
            .await
            .unwrap()
            .pending_handoff_count,
        store
            .reader
            .briefing_for_project(ws, proj, 5, filter(), &SlotVisibility::default())
            .await
            .unwrap()
            .pending_handoff_count,
        store
            .reader
            .briefing_for_workspace(ws, 5, filter(), &SlotVisibility::default())
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
    // treat as a single stream of supersedable batons.
    async fn auto_handoff(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        owner: &str,
        summary: &str,
    ) -> ai_memory_core::HandoffId {
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: Some("/repo".into()),
                actor_user: Some(owner.to_string()),
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
                owner_user: Some(owner.to_string()),
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
            .state,
        HandoffState::Expired,
        "within one operator, the same-cwd sweep still bounds accumulation"
    );

    // And the post-claim sweep is bounded the same way: Alice claiming her own
    // baton retires nothing of Bob's, even though his row matches the cwd rule
    // and ranks below hers.
    let claimed = store
        .writer
        .accept_handoff(
            alice_newest,
            AgentKind::ClaudeCode,
            None,
            Some("alice".into()),
            OwnerFilter::User("alice".into()),
            Some("/repo".into()),
        )
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
            .state,
        HandoffState::Open,
        "Alice's session start must not expire Bob's pending baton"
    );

    // Bob still receives it, which is the whole point.
    let bobs = store
        .reader
        .latest_open_handoff(
            ws,
            proj,
            Some("/repo".into()),
            OwnerFilter::User("bob".into()),
        )
        .await
        .unwrap()
        .expect("Bob's baton survives another operator's accept");
    assert_eq!(bobs.summary, "bob's baton");
}
