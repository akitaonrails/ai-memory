//! A colliding proposal must not destroy the run it arrived with.
//!
//! `stage_run` inserted each proposal blind inside one transaction, so the
//! partial UNIQUE index on pending targets aborted the whole thing via `?`:
//! the run row, every non-colliding proposal beside it, and the paid LLM call
//! that produced them were all lost. It fires with a single operator, through
//! the path the prompt itself recommends.

use ai_memory_core::{ActorContext, PagePath, ProjectId, WorkspaceId};
use ai_memory_store::{
    AutoImproveProposalOperation, NewAutoImproveProposal, StageAutoImproveRun, Store,
};

fn proposal(path: &str, title: &str) -> NewAutoImproveProposal {
    NewAutoImproveProposal {
        operation: AutoImproveProposalOperation::Create,
        target_path: PagePath::new(path).unwrap(),
        kind: "rule".into(),
        title: title.into(),
        confidence: 0.9,
        rationale: "because".into(),
        evidence_json: serde_json::json!([]),
        body_markdown: format!("# {title}\n\nbody"),
        artifact_sha256: None,
        edit_mode: None,
        patch_json: None,
        expected_base_body_sha256: None,
    }
}

fn run(
    ws: WorkspaceId,
    proj: ProjectId,
    proposals: Vec<NewAutoImproveProposal>,
) -> StageAutoImproveRun {
    StageAutoImproveRun {
        workspace_id: ws,
        project_id: proj,
        session_id: None,
        proposals,
        proposal_actor: ActorContext::anonymous(),
        staged_by_actor_user: None,
        warnings_json: serde_json::json!([]),
        rejected_candidates_json: serde_json::json!([]),
        config_json: serde_json::json!({}),
        summary: Some("s".into()),
        provider: None,
        model: None,
    }
}

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "p".to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

/// A second run targeting an already-pending path keeps its own run row and its
/// unrelated proposal; only the duplicate is skipped, and it is reported.
#[tokio::test]
async fn a_colliding_proposal_does_not_destroy_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let first = store
        .writer
        .stage_auto_improve_run(run(
            ws,
            proj,
            vec![proposal("_slots/current-focus.md", "Focus")],
        ))
        .await
        .unwrap();
    assert_eq!(first.proposal_ids.len(), 1);
    assert!(first.skipped.is_empty());

    // Second run: one duplicate target + one perfectly good new proposal.
    let second = store
        .writer
        .stage_auto_improve_run(run(
            ws,
            proj,
            vec![
                proposal("_slots/current-focus.md", "Focus again"),
                proposal("_rules/unrelated.md", "Unrelated"),
            ],
        ))
        .await
        .unwrap();

    assert_eq!(
        second.proposal_ids.len(),
        1,
        "the non-colliding proposal must still be staged"
    );
    assert_eq!(
        second.skipped.len(),
        1,
        "the duplicate is reported, not silent"
    );
    assert_eq!(second.skipped[0].target_path, "_slots/current-focus.md");
}

/// A batch that contradicts itself is refused wholesale, and must keep being
/// refused: two proposals for one page mean the model emitted conflicting
/// suggestions, and picking one arbitrarily is worse than failing. This is a
/// different situation from colliding with an EARLIER run's pending proposal,
/// which is skipped per-proposal so the rest of the run survives.
#[tokio::test]
async fn duplicate_targets_within_one_run_still_fail_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let result = store
        .writer
        .stage_auto_improve_run(run(
            ws,
            proj,
            vec![
                proposal("_slots/current-focus.md", "First"),
                proposal("_slots/current-focus.md", "Second"),
            ],
        ))
        .await;
    assert!(result.is_err(), "a self-contradicting batch is refused");

    // …and nothing from it is left behind.
    assert!(
        store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// The one-pending-per-target rule must be right in BOTH modes, from one index.
///
/// V42 folds the operator into the unique key through `COALESCE(actor, '')`:
/// unattributed rows (single operator, or any caller the server cannot name)
/// all land in one bucket and keep the original invariant, while distinct
/// operators get their own. A plain `UNIQUE (…, staged_by_actor_user)` would
/// NOT do this — SQLite treats NULLs as distinct, so every existing
/// single-operator database would silently start accepting unlimited pending
/// proposals per page.
///
/// The key is the actor USERNAME, not a `users(id)`: operators asserted by a
/// trusted proxy have no row, so keying on the id would leave every one of them
/// NULL and collapse them all back into a single bucket — the exact collision
/// this index exists to break.
#[tokio::test]
async fn pending_uniqueness_is_per_operator_without_weakening_single_user() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    let staged_by = |actor: Option<&str>| {
        let mut r = run(ws, proj, vec![proposal("_rules/style.md", "Style")]);
        r.staged_by_actor_user = actor.map(str::to_string);
        r
    };

    // --- single-operator behaviour is untouched: unattributed rows collapse
    // into one bucket, so the second pending proposal is still refused.
    let first = store
        .writer
        .stage_auto_improve_run(staged_by(None))
        .await
        .unwrap();
    assert_eq!(first.proposal_ids.len(), 1);
    let second = store
        .writer
        .stage_auto_improve_run(staged_by(None))
        .await
        .unwrap();
    assert!(
        second.proposal_ids.is_empty() && second.skipped.len() == 1,
        "an unattributed duplicate must still be refused, as before V42"
    );

    // --- multi-operator: two proxy-asserted humans, neither with a `users`
    // row, must not block each other on the same page.
    let alices = store
        .writer
        .stage_auto_improve_run(staged_by(Some("alice")))
        .await
        .unwrap();
    assert_eq!(
        alices.proposal_ids.len(),
        1,
        "one operator's pending proposal must not block another's for the same page"
    );
    assert!(alices.skipped.is_empty());

    let bobs = store
        .writer
        .stage_auto_improve_run(staged_by(Some("bob")))
        .await
        .unwrap();
    assert_eq!(bobs.proposal_ids.len(), 1);
    assert!(bobs.skipped.is_empty());

    // …and Alice still cannot stack two of her own.
    let alice_again = store
        .writer
        .stage_auto_improve_run(staged_by(Some("alice")))
        .await
        .unwrap();
    assert_eq!(alice_again.skipped.len(), 1);
}

/// Only the UNIQUE collision may be swallowed.
///
/// `ErrorCode::ConstraintViolation` is the PRIMARY code that NOT NULL, CHECK, FK
/// and `RAISE(ABORT)` failures all share, and `auto_improve_proposals` carries
/// CHECKs on `status`/`operation`, FKs to four tables, and a workspace/project
/// pairing trigger. Accepting the primary code relabels any of those "a proposal
/// is already pending review for this path" and drops the proposal, so a broken
/// schema contract looks like ordinary contention and nothing ever reports it.
#[tokio::test]
async fn a_non_unique_constraint_failure_is_not_reported_as_a_pending_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;

    // Stand-in for the constraints the table already has, reached with a target
    // path the test controls: same primary code as the pending-collision index,
    // different extended one.
    {
        let conn =
            rusqlite::Connection::open(tmp.path().join("db").join(ai_memory_store::DB_FILENAME))
                .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER test_schema_contract \
             BEFORE INSERT ON auto_improve_proposals \
             FOR EACH ROW WHEN NEW.target_path = 'notes/contract.md' \
             BEGIN SELECT RAISE(ABORT, 'schema contract violated'); END;",
        )
        .unwrap();
    }

    let err = store
        .writer
        .stage_auto_improve_run(run(
            ws,
            proj,
            vec![proposal("notes/contract.md", "Contract")],
        ))
        .await
        .expect_err("a schema-contract failure must surface, not be skipped");
    assert!(
        err.to_string().contains("schema contract violated"),
        "the real cause must reach the caller: {err}"
    );
    assert!(
        store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing may be staged by an aborted run"
    );

    // The genuine collision is still tolerated per-proposal.
    let first = store
        .writer
        .stage_auto_improve_run(run(ws, proj, vec![proposal("notes/ok.md", "Ok")]))
        .await
        .unwrap();
    assert_eq!(first.proposal_ids.len(), 1);
    let again = store
        .writer
        .stage_auto_improve_run(run(ws, proj, vec![proposal("notes/ok.md", "Ok again")]))
        .await
        .unwrap();
    assert_eq!(again.skipped.len(), 1);
    assert_eq!(
        again.skipped[0].reason,
        "a proposal is already pending review for this path"
    );
}
