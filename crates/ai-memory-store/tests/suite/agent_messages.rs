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
    AgentKind, MessageBox, MessageClaim, MessageNature, MessageState, NewAgentMessage, ProjectId,
    WorkspaceId,
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

#[tokio::test]
async fn measured_messages_require_evidence_at_the_store_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;

    let mut measured = message(a, b, "the benchmark improved");
    measured.nature = MessageNature::Measured;
    measured.evidence = Some("   ".into());
    let error = store
        .writer
        .insert_message(measured)
        .await
        .expect_err("blank evidence must not support a measured claim");
    assert!(error.to_string().contains("evidence_required"), "{error}");

    let mut inferred = message(a, b, "the lock probably expired");
    inferred.nature = MessageNature::Inferred;
    let id = store.writer.insert_message(inferred).await.unwrap();
    let listed = store
        .reader
        .list_messages(b.0, b.1, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].nature, MessageNature::Inferred);
    assert_eq!(listed[0].evidence, None);
}

#[tokio::test]
async fn message_evidence_is_bounded_at_the_store_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;
    let mut measured = message(a, b, "the benchmark improved");
    measured.nature = MessageNature::Measured;
    measured.evidence = Some("x".repeat(2_000));
    store.writer.insert_message(measured).await.unwrap();

    let listed = store
        .reader
        .list_messages(b.0, b.1, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert_eq!(listed[0].evidence.as_deref().unwrap().len(), 1_000);
}

#[tokio::test]
async fn pre_tag_rows_read_as_unknown_without_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "default", "project-a").await;
    let b = project(&store, "default", "project-b").await;
    let id = ai_memory_core::MessageId::new();
    rusqlite::Connection::open(store.db_path())
        .unwrap()
        .execute(
            "INSERT INTO agent_messages (id, from_workspace_id, from_project_id, from_agent, \
             to_workspace_id, to_project_id, body, state, created_at) \
             VALUES (?1, ?2, ?3, 'other', ?4, ?5, 'legacy', 'pending', ?6)",
            rusqlite::params![
                id.as_bytes(),
                a.0.as_bytes(),
                a.1.as_bytes(),
                b.0.as_bytes(),
                b.1.as_bytes(),
                jiff::Timestamp::now().as_microsecond(),
            ],
        )
        .unwrap();

    let listed = store
        .reader
        .list_messages(b.0, b.1, MessageBox::Inbox, 50)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].nature, MessageNature::Unknown);
    assert_eq!(listed[0].evidence, None);
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
        to_agent: None,
        subject: Some("need a thing".into()),
        body: body.into(),
        nature: ai_memory_core::MessageNature::Unknown,
        evidence: None,
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

#[tokio::test]
async fn addressed_messages_are_claimed_once_only_by_the_selected_harness() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "team", "sender").await;
    let b = project(&store, "team", "receiver").await;
    let mut directed = message(a, b, "for codex");
    directed.to_agent = Some(AgentKind::Codex);
    let id = store.writer.insert_message(directed).await.unwrap();
    let mut wrong = claim(b);
    wrong.claiming_agent = AgentKind::ClaudeCode;
    assert!(
        store
            .writer
            .pop_message(wrong.clone(), None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .writer
            .pop_message(wrong, Some(id))
            .await
            .unwrap()
            .is_none()
    );
    let shared = store
        .writer
        .insert_message(message(a, b, "shared"))
        .await
        .unwrap();
    assert_eq!(
        store
            .writer
            .pop_message(claim(b), None)
            .await
            .unwrap()
            .unwrap()
            .id,
        shared
    );
    let mut correct = claim(b);
    correct.claiming_agent = AgentKind::Codex;
    let (left, right) = tokio::join!(
        store.writer.pop_message(correct.clone(), Some(id)),
        store.writer.pop_message(correct, Some(id))
    );
    let claimed: Vec<_> = [left.unwrap(), right.unwrap()]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].to_agent, Some(AgentKind::Codex));
    assert_eq!(
        store.reader.pending_message_count(b.0, b.1).await.unwrap(),
        0
    );
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

/// A message being edited leaves the queue: it cannot be popped, counted or held
/// twice, and only its own recipient project can hold, save or release it.
#[tokio::test]
async fn a_message_being_edited_leaves_the_queue_until_it_is_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "team", "sender").await;
    let b = project(&store, "team", "receiver").await;
    let other = project(&store, "team", "other").await;
    let other_workspace = project(&store, "other-team", "receiver").await;
    let id = store
        .writer
        .insert_message(message(a, b, "first draft"))
        .await
        .unwrap();

    for foreign in [other, other_workspace] {
        assert!(
            store
                .writer
                .begin_message_edit(foreign.0, foreign.1, id)
                .await
                .unwrap()
                .is_none(),
            "another project or workspace cannot hold the message"
        );
    }
    for replacement in [None, Some((Some("unheld".into()), "unheld".into()))] {
        assert!(
            !store
                .writer
                .finish_message_edit(b.0, b.1, id, replacement)
                .await
                .unwrap(),
            "save and release require a held message"
        );
    }
    assert_eq!(
        store.reader.pending_message_count(b.0, b.1).await.unwrap(),
        1
    );

    let held = store
        .writer
        .begin_message_edit(b.0, b.1, id)
        .await
        .unwrap()
        .expect("a pending message can be held");
    assert_eq!(held.body, "first draft");
    assert_eq!(held.state, MessageState::Editing);
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
            .is_none()
    );
    assert!(
        store
            .writer
            .pop_message(claim(b), Some(id))
            .await
            .unwrap()
            .is_none(),
        "a held message cannot be popped even by id"
    );
    assert!(
        store
            .writer
            .begin_message_edit(b.0, b.1, id)
            .await
            .unwrap()
            .is_none(),
        "a held message cannot be held again"
    );
    for foreign in [other, other_workspace] {
        for replacement in [None, Some((Some("foreign".into()), "foreign".into()))] {
            assert!(
                !store
                    .writer
                    .finish_message_edit(foreign.0, foreign.1, id, replacement)
                    .await
                    .unwrap(),
                "another project or workspace cannot save or release the held message"
            );
        }
    }

    assert!(
        store
            .writer
            .finish_message_edit(
                b.0,
                b.1,
                id,
                Some((Some("new subject".into()), "second draft".into())),
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store.reader.pending_message_count(b.0, b.1).await.unwrap(),
        1
    );
    let popped = store
        .writer
        .pop_message(claim(b), None)
        .await
        .unwrap()
        .expect("the saved message is back in the queue");
    assert_eq!(popped.body, "second draft");
    assert_eq!(popped.subject.as_deref(), Some("new subject"));
    assert!(
        !store
            .writer
            .finish_message_edit(b.0, b.1, id, None)
            .await
            .unwrap(),
        "a message that is no longer held cannot be saved again"
    );
}

/// Releasing an edit without changes puts the message back unchanged and in its
/// original place in the queue.
#[tokio::test]
async fn releasing_an_edit_keeps_the_text_and_the_queue_order() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "team", "sender").await;
    let b = project(&store, "team", "receiver").await;
    let first = store
        .writer
        .insert_message(message(a, b, "one"))
        .await
        .unwrap();
    store
        .writer
        .insert_message(message(a, b, "two"))
        .await
        .unwrap();
    let before = store
        .reader
        .message_in_mailbox(b.0, b.1, first)
        .await
        .unwrap()
        .unwrap();

    store
        .writer
        .begin_message_edit(b.0, b.1, first)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store
            .writer
            .finish_message_edit(b.0, b.1, first, None)
            .await
            .unwrap()
    );
    let after = store
        .reader
        .message_in_mailbox(b.0, b.1, first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state, MessageState::Pending);
    assert_eq!(after.body, before.body);
    assert_eq!(after.created_at, before.created_at);

    let popped = store
        .writer
        .pop_message(claim(b), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(popped.body, "one", "the released message is still first");
}

/// Only a pending message can be held: collected and cancelled mail is final.
#[tokio::test]
async fn collected_or_cancelled_mail_cannot_be_taken_for_editing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "team", "sender").await;
    let b = project(&store, "team", "receiver").await;
    let collected = store
        .writer
        .insert_message(message(a, b, "collected"))
        .await
        .unwrap();
    store
        .writer
        .pop_message(claim(b), Some(collected))
        .await
        .unwrap()
        .unwrap();
    let cancelled = store
        .writer
        .insert_message(message(a, b, "cancelled"))
        .await
        .unwrap();
    store
        .writer
        .cancel_messages(a.0, a.1, Some(cancelled))
        .await
        .unwrap();

    for id in [collected, cancelled] {
        assert!(
            store
                .writer
                .begin_message_edit(b.0, b.1, id)
                .await
                .unwrap()
                .is_none()
        );
    }
}

/// A message whose body is within the published 8 000-character budget must
/// survive the store boundary whole, on both the insert and the edit-save
/// paths. The store field cap used to reuse the 16 KiB handoff budget, which
/// silently truncated multibyte bodies well inside that character budget
/// (#2241); the dedicated 32 KiB message cap holds 8 000 characters even at
/// UTF-8's four-byte worst case.
#[tokio::test]
async fn a_message_within_the_character_budget_is_not_truncated() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let a = project(&store, "team", "sender").await;
    let b = project(&store, "team", "receiver").await;

    // 6 000 three-byte characters plus a tail sentinel: 18 015 bytes, over the
    // old 16 KiB cap that would have cut the sentinel off.
    let three_byte = format!("{}{}", "界".repeat(6000), " FINAL-SENTINEL");
    assert_eq!(three_byte.chars().count(), 6015);
    assert!(three_byte.len() > 16 * 1024 && three_byte.len() < 32 * 1024);

    // insert_message path.
    let id = store
        .writer
        .insert_message(message(a, b, &three_byte))
        .await
        .unwrap();
    let popped = store
        .writer
        .pop_message(claim(b), Some(id))
        .await
        .unwrap()
        .expect("the message is in the inbox");
    assert_eq!(
        popped.body, three_byte,
        "insert_message must not truncate a body inside the character budget"
    );

    // finish_message_edit path, worst case: 8 000 four-byte characters =
    // 32 000 bytes, the largest a body within the 8 000-character budget can
    // be, still under the 32 KiB cap.
    let four_byte = "𝐀".repeat(8000);
    assert_eq!(four_byte.chars().count(), 8000);
    assert_eq!(four_byte.len(), 32_000);

    let edit_id = store
        .writer
        .insert_message(message(a, b, "placeholder"))
        .await
        .unwrap();
    store
        .writer
        .begin_message_edit(b.0, b.1, edit_id)
        .await
        .unwrap()
        .expect("a pending message can be held");
    assert!(
        store
            .writer
            .finish_message_edit(b.0, b.1, edit_id, Some((None, four_byte.clone())))
            .await
            .unwrap()
    );
    let edited = store
        .writer
        .pop_message(claim(b), Some(edit_id))
        .await
        .unwrap()
        .expect("the saved message is back in the queue");
    assert_eq!(
        edited.body, four_byte,
        "finish_message_edit must not truncate a body inside the character budget"
    );
}
