//! Integration tests for the cross-project agent message inbox/queue (V64).
//!
//! The behaviours pinned here are the ones that make cross-project messaging
//! safe: it crosses the per-project isolation boundary on purpose, so a project
//! must only ever see mail addressed TO it or sent FROM it; a message is popped
//! exactly once (the handoff claim-once discipline); the sender can retract but
//! the recipient cannot cancel someone else's outbox; an unknown recipient is a
//! caller error handled above this layer (the store trusts resolved ids); and a
//! recipient inbox is bounded so a flood cannot exhaust its context/storage.

use ai_memory_core::{
    AgentKind, MessageBox, MessageClaim, NewAgentMessage, ProjectId, WorkspaceId,
};
use ai_memory_store::{MAX_PENDING_INBOX_MESSAGES, Store};

async fn project(store: &Store, workspace: &str, project: &str) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace(workspace.to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project.to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

fn message(
    from: (WorkspaceId, ProjectId),
    to: (WorkspaceId, ProjectId),
    body: &str,
) -> NewAgentMessage {
    NewAgentMessage {
        from_workspace_id: from.0,
        from_project_id: from.1,
        from_agent: AgentKind::Other,
        from_session_id: None,
        from_owner_user: None,
        to_workspace_id: to.0,
        to_project_id: to.1,
        subject: Some("need a thing".into()),
        body: body.into(),
    }
}

fn claim(to: (WorkspaceId, ProjectId)) -> MessageClaim {
    MessageClaim {
        workspace_id: to.0,
        project_id: to.1,
        claiming_agent: AgentKind::Other,
        claiming_session: None,
        claiming_user: None,
    }
}

/// The core queue contract: A sends to B, B pops it exactly once, a second pop
/// finds nothing.
#[tokio::test]
async fn send_then_pop_delivers_exactly_once() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    store
        .writer
        .insert_message(message(a, b, "please add the export endpoint"))
        .await
        .unwrap();

    assert_eq!(
        store.reader.pending_message_count(b.0, b.1).await.unwrap(),
        1
    );

    let first = store.writer.pop_message(claim(b), None).await.unwrap();
    let popped = first.expect("B pops the pending message");
    assert_eq!(popped.body, "please add the export endpoint");
    assert_eq!(popped.origin.from_project_id, a.1);

    // Consumed: the inbox is empty and a second pop returns nothing.
    assert_eq!(
        store.reader.pending_message_count(b.0, b.1).await.unwrap(),
        0
    );
    assert!(
        store
            .writer
            .pop_message(claim(b), None)
            .await
            .unwrap()
            .is_none(),
        "a claimed message must not be poppable again"
    );
}

/// Isolation: a message addressed to B is invisible to a third project C, both
/// in C's inbox listing and when C tries to pop.
#[tokio::test]
async fn a_message_is_only_visible_to_its_recipient() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;
    let c = project(&store, "default", "project-c").await;

    store
        .writer
        .insert_message(message(a, b, "for B only"))
        .await
        .unwrap();

    assert_eq!(
        store.reader.pending_message_count(c.0, c.1).await.unwrap(),
        0
    );
    assert!(
        store
            .reader
            .list_messages(c.0, c.1, MessageBox::Inbox, 50)
            .await
            .unwrap()
            .is_empty(),
        "C must not see mail addressed to B"
    );
    assert!(
        store
            .writer
            .pop_message(claim(c), None)
            .await
            .unwrap()
            .is_none(),
        "C must not be able to pop B's mail"
    );

    // B does see it.
    let inbox = store
        .reader
        .list_messages(b.0, b.1, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].body, "for B only");
}

/// The sender sees its message in its OUTBOX and can cancel it; once cancelled
/// it is no longer poppable by the recipient.
#[tokio::test]
async fn sender_can_cancel_a_pending_message_before_it_is_popped() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    store
        .writer
        .insert_message(message(a, b, "never mind soon"))
        .await
        .unwrap();

    let outbox = store
        .reader
        .list_messages(a.0, a.1, MessageBox::Outbox, 50)
        .await
        .unwrap();
    assert_eq!(outbox.len(), 1, "A sees its own pending outbound message");

    let cancelled = store.writer.cancel_messages(a.0, a.1, None).await.unwrap();
    assert_eq!(cancelled, 1);

    assert_eq!(
        store.reader.pending_message_count(b.0, b.1).await.unwrap(),
        0
    );
    assert!(
        store
            .writer
            .pop_message(claim(b), None)
            .await
            .unwrap()
            .is_none(),
        "a cancelled message must not be delivered"
    );
}

/// Cancel is scoped to the SENDER coordinate: the recipient cannot clear the
/// mail it received by calling cancel from its own project.
#[tokio::test]
async fn recipient_cannot_cancel_the_senders_outbox() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    store
        .writer
        .insert_message(message(a, b, "do the work"))
        .await
        .unwrap();

    // B calls cancel scoped to its own project — it has no outbox mail, so
    // nothing is cancelled and A's message survives.
    let cancelled = store.writer.cancel_messages(b.0, b.1, None).await.unwrap();
    assert_eq!(
        cancelled, 0,
        "B cancelling its own outbox must not touch A's mail"
    );
    assert_eq!(
        store.reader.pending_message_count(b.0, b.1).await.unwrap(),
        1
    );

    let popped = store.writer.pop_message(claim(b), None).await.unwrap();
    assert_eq!(
        popped.expect("A's message is still deliverable").body,
        "do the work"
    );
}

/// The sibling of `recipient_cannot_cancel_the_senders_outbox` for the
/// specific-id path: the existing test only exercises whole-outbox cancel
/// (`specific_id: None`), which is scoped by `from_workspace_id`/
/// `from_project_id` alone and would still refuse B even if the id-scoped
/// branch dropped that guard. This targets B's exact knowledge of A's
/// message id directly, so it can only pass if the id-scoped `UPDATE` also
/// carries the sender-coordinate `AND from_workspace_id = ... AND
/// from_project_id = ...` guard.
#[tokio::test]
async fn recipient_cannot_cancel_a_specific_message_by_id() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    let a_msg_id = store
        .writer
        .insert_message(message(a, b, "A's own message"))
        .await
        .unwrap();

    // B knows the exact id (e.g. from its own inbox listing) but must not be
    // able to cancel A's outbound message by targeting it directly.
    let cancelled = store
        .writer
        .cancel_messages(b.0, b.1, Some(a_msg_id))
        .await
        .unwrap();
    assert_eq!(
        cancelled, 0,
        "B cancelling A's message by exact id must not touch it"
    );

    let popped = store
        .writer
        .pop_message(claim(b), Some(a_msg_id))
        .await
        .unwrap();
    assert_eq!(
        popped
            .expect("A's message must still be pending and poppable by B")
            .body,
        "A's own message"
    );

    // Control: A cancelling its OWN message by the same exact id succeeds.
    let a_msg_id_2 = store
        .writer
        .insert_message(message(a, b, "A's second message"))
        .await
        .unwrap();
    let cancelled_by_owner = store
        .writer
        .cancel_messages(a.0, a.1, Some(a_msg_id_2))
        .await
        .unwrap();
    assert_eq!(
        cancelled_by_owner, 1,
        "A cancelling its own message by exact id must succeed"
    );
    assert!(
        store
            .writer
            .pop_message(claim(b), Some(a_msg_id_2))
            .await
            .unwrap()
            .is_none(),
        "a cancelled message must not be delivered"
    );
}

/// Popping by an explicit id claims that specific message; FIFO otherwise.
#[tokio::test]
async fn pop_can_target_a_specific_message_id() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    let _first = store
        .writer
        .insert_message(message(a, b, "first"))
        .await
        .unwrap();
    let second = store
        .writer
        .insert_message(message(a, b, "second"))
        .await
        .unwrap();

    let popped = store
        .writer
        .pop_message(claim(b), Some(second))
        .await
        .unwrap()
        .expect("the requested message is popped");
    assert_eq!(popped.body, "second");
    assert_eq!(popped.id, second);

    // The other one is still pending.
    let remaining = store
        .reader
        .list_messages(b.0, b.1, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].body, "first");
}

/// Two racing pops of the SAME pending message must not both claim it: the
/// baton claim-once discipline this module's own doc comment promises. Both
/// calls are dispatched together via `tokio::join!` so their `WriteCmd`s land
/// on the single-writer actor back-to-back, exercising the real
/// `state = 'pending'` guards in `pop_message_in_transaction` rather than
/// relying on test-side sequencing to keep them apart.
#[tokio::test]
async fn concurrent_pops_of_one_message_deliver_it_exactly_once() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    let id = store
        .writer
        .insert_message(message(a, b, "only one winner"))
        .await
        .unwrap();

    // Targeted by exact id (rather than `None`/FIFO) so the race exercises
    // Guard 1 + Guard 2 in `pop_message_in_transaction` directly, instead of
    // being pre-filtered by the FIFO candidate selection's own `state =
    // 'pending'` clause before either guard runs.
    let writer1 = store.writer.clone();
    let writer2 = store.writer.clone();
    let (r1, r2) = tokio::join!(
        writer1.pop_message(claim(b), Some(id)),
        writer2.pop_message(claim(b), Some(id)),
    );
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();

    let winners = [&r1, &r2].into_iter().filter(|r| r.is_some()).count();
    assert_eq!(
        winners, 1,
        "exactly one of two concurrent pops must claim the message: {r1:?} / {r2:?}",
    );
}

/// A full recipient inbox rejects new sends so a flood cannot exhaust the
/// recipient's context or storage.
#[tokio::test]
async fn a_full_inbox_rejects_further_sends() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    for i in 0..MAX_PENDING_INBOX_MESSAGES {
        store
            .writer
            .insert_message(message(a, b, &format!("msg {i}")))
            .await
            .unwrap();
    }
    let err = store
        .writer
        .insert_message(message(a, b, "one too many"))
        .await
        .expect_err("a full inbox must reject the next send");
    assert!(
        err.to_string().contains("full"),
        "the rejection should explain the inbox is full: {err}"
    );

    // Popping one frees a slot so a send succeeds again.
    store
        .writer
        .pop_message(claim(b), None)
        .await
        .unwrap()
        .unwrap();
    store
        .writer
        .insert_message(message(a, b, "now there is room"))
        .await
        .expect("a send succeeds once the inbox drops below the cap");
}
