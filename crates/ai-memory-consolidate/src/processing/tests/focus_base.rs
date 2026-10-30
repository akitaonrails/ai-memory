//! A full-page update of `_slots/current-focus.md` is derived from the version
//! the final call read. Before the base was captured with the context the plan
//! classified the slot from a later read and carried no base, so a version
//! written after the final call (during eval, or while a restart was pending)
//! was adopted as the base of the old proposal and then overwritten.

use ai_memory_core::{NewPage, Tier};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::super::{publish, run};
use super::*;

const FOCUS: &str = "_slots/current-focus.md";
const A: &str = "# Focus\n\nversion A";
const B: &str = "# Focus\n\nconcurrent version B";

fn sha(body: &str) -> String {
    super::super::chunk_plan::hex(&Sha256::digest(body.as_bytes()))
}

async fn put_focus(f: &Fixture, body: &str) {
    f.store
        .writer
        .upsert_page(NewPage {
            workspace_id: f.ws,
            project_id: f.proj,
            path: PagePath::new(FOCUS).unwrap(),
            title: "Focus".into(),
            body: body.into(),
            tier: Tier::Semantic,
            frontmatter_json: json!({}),
            pinned: true,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
}

async fn focus_body(f: &Fixture) -> String {
    f.store
        .reader
        .page_body_by_ids(f.ws, f.proj, FOCUS)
        .await
        .unwrap()
        .expect("the focus page exists")
        .body
}

/// A validated review of one full-page current-focus proposal; `base` is what
/// the server captured with the final context.
fn focus_report(f: &Fixture, base: Option<String>) -> crate::auto_improve::AutoImproveReport {
    serde_json::from_value(json!({
        "session_id": f.session.to_string(),
        "observations_considered": 6,
        "session_duration_secs": 60,
        "estimated_input_tokens": 100,
        "provider": "stage-llm",
        "model": "stage-llm",
        "min_confidence": 0.7,
        "proposal_actor": "auto_improve",
        "pending_path": "_pending/auto-improve",
        "summary": "refresh the focus",
        "proposals": [{
            "operation": "create_or_update", "path": FOCUS,
            "title": "Focus", "kind": "slot", "confidence": 0.9,
            "rationale": "seen in the session",
            "evidence": [{"page": "o0", "quote": "new focus"}],
            "body_markdown": "# Focus\n\nNew focus derived from the session.",
            "edit_mode": "full_page", "edits": [],
            "expected_base_body_sha256": base
        }],
        "rejected_candidates": [],
        "warnings": []
    }))
    .unwrap()
}

/// A claimed review job, its plan for `base`, and the plan persisted with its
/// publication identity: the state a restart finds.
async fn persisted_plan(
    f: &Fixture,
    svc: &ProcessingService,
    base: Option<String>,
) -> (
    run::Run,
    ai_memory_store::ProcessingPublication,
    serde_json::Value,
) {
    svc.enqueue(
        &start(f, ProcessingJobKind::AutoImprove),
        &super::super::StartOptions {
            instructions: None,
            review: Some(staged_cfg()),
            trigger: "durable",
        },
    )
    .await
    .unwrap();
    let run = claimed_run(f, svc).await;
    let plan = match run
        .plan_for_report(focus_report(f, base), &staged_cfg())
        .await
    {
        Ok(plan) => plan,
        Err(_) => panic!("plan"),
    };
    let json = serde_json::to_value(&plan).unwrap();
    let publication = run
        .writer
        .begin_processing_publication(&run.claim, "focus:test".into(), json.clone())
        .await
        .unwrap();
    (run, publication, json)
}

async fn resume(
    run: &mut run::Run,
    publication: ai_memory_store::ProcessingPublication,
) -> super::super::AutoImproveStaged {
    let status = match run.publish_resume(publication).await {
        Ok(status) => status,
        Err(_) => panic!("resume"),
    };
    assert_eq!(status.state, ProcessingJobState::Completed);
    publish::staged_from_result(status.result.as_ref()).unwrap()
}

/// Before: the plan staged the proposal over B and its approval overwrote B.
/// After: the persisted base names A, staging sees B and skips the proposal.
#[tokio::test]
async fn a_focus_update_whose_base_was_replaced_before_staging_is_skipped_and_b_survives() {
    let f = fixture(6, 500).await;
    put_focus(&f, A).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    let (mut run, publication, plan) = persisted_plan(&f, &svc, Some(sha(A))).await;
    assert_eq!(plan["t"], "stage");
    assert_eq!(plan["proposals"][0]["operation"], "update");
    assert_eq!(plan["proposals"][0]["expected_base_body_sha256"], sha(A));

    // The restart finds the persisted plan after somebody edited the slot.
    put_focus(&f, B).await;
    let staged = resume(&mut run, publication).await;

    assert!(staged.proposals.is_empty(), "nothing is staged over B");
    assert_eq!(staged.skipped.len(), 1);
    assert_eq!(staged.skipped[0].target_path, FOCUS);
    assert!(staged.skipped[0].reason.contains("changed"));
    assert_eq!(staged_rows(&f), (1, 0));
    assert_eq!(focus_body(&f).await, B, "B is never overwritten");
    let effects = recorded_effects(&f, &run).await;
    assert!(effects.iter().all(|e| !e.key.starts_with("approve:")));
}

/// Control: the same plan with the slot unchanged stages the update, bound
/// to the captured base.
#[tokio::test]
async fn a_focus_update_of_the_unchanged_version_stages_with_its_base() {
    let f = fixture(6, 500).await;
    put_focus(&f, A).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    let (mut run, publication, _) = persisted_plan(&f, &svc, Some(sha(A))).await;

    let staged = resume(&mut run, publication).await;

    assert_eq!(staged.proposals.len(), 1);
    assert!(staged.skipped.is_empty());
    assert_eq!(staged_rows(&f), (1, 1));
    let detail = f
        .store
        .reader
        .auto_improve_proposal_detail(f.ws, f.proj, staged.proposals[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(detail.summary.operation.as_str(), "update");
    assert_eq!(
        detail
            .expected_base_body_sha256
            .map(|h| super::super::chunk_plan::hex(&h)),
        Some(sha(A))
    );
}

/// The slot was absent when the final call ran. A later look must not turn
/// the proposal into an update of whatever appeared: it stays a create, and
/// the staging transaction refuses it.
#[tokio::test]
async fn a_focus_that_appeared_after_the_context_is_a_create_conflict_not_a_late_update() {
    let f = fixture(6, 500).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    let (mut run, publication, plan) = persisted_plan(&f, &svc, None).await;
    assert_eq!(plan["proposals"][0]["operation"], "create");
    assert!(plan["proposals"][0]["expected_base_body_sha256"].is_null());

    put_focus(&f, B).await;
    let staged = resume(&mut run, publication).await;

    assert!(staged.proposals.is_empty());
    assert_eq!(staged.skipped.len(), 1);
    assert_eq!(
        staged.skipped[0].reason,
        "create proposal target already exists"
    );
    assert_eq!(focus_body(&f).await, B);
}

/// Control: still absent, the create stages.
#[tokio::test]
async fn a_focus_that_is_still_absent_stages_as_a_create() {
    let f = fixture(6, 500).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    let (mut run, publication, _) = persisted_plan(&f, &svc, None).await;

    let staged = resume(&mut run, publication).await;

    assert_eq!(staged.proposals.len(), 1);
    assert_eq!(staged_rows(&f), (1, 1));
}

/// The slot vanished after the context was read: an update of nothing is
/// refused, not recreated.
#[tokio::test]
async fn a_focus_deleted_before_staging_is_skipped() {
    let f = fixture(6, 500).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    // never written: the base names a version that no longer exists
    let (mut run, publication, _) = persisted_plan(&f, &svc, Some(sha(A))).await;

    let staged = resume(&mut run, publication).await;

    assert!(staged.proposals.is_empty());
    assert_eq!(
        staged.skipped[0].reason,
        "update proposal target does not exist"
    );
    assert_eq!(staged_rows(&f), (1, 0));
}

/// A crash after staging committed and before its checkpoint: the resume
/// reuses the staged run and ids even though the slot changed meanwhile, and
/// its approval then conflicts instead of overwriting.
#[tokio::test]
async fn a_staged_focus_run_is_adopted_by_its_ids_and_a_later_edit_conflicts_at_approval() {
    let f = fixture(6, 500).await;
    put_focus(&f, A).await;
    let svc =
        service(&f, Arc::new(StageLlm::new()), true, 128).with_auto_improve(staged_cfg(), false);
    let (mut run, publication, _) = persisted_plan(&f, &svc, Some(sha(A))).await;
    let input = match serde_json::from_value::<publish::Plan>(publication.plan_json.clone()) {
        Ok(publish::Plan::Stage(stage)) => match stage.stage_input(&run) {
            Ok(input) => input,
            Err(_) => panic!("stage input"),
        },
        _ => panic!("a staging plan"),
    };
    let committed = run
        .writer
        .stage_auto_improve_run_for_owner(input, run.owner.clone())
        .await
        .unwrap();
    assert_eq!(committed.proposal_ids.len(), 1);

    put_focus(&f, B).await;
    let staged = resume(&mut run, publication).await;

    assert_eq!(staged.run_id, committed.run_id);
    assert_eq!(
        staged.proposals.iter().map(|p| p.id).collect::<Vec<_>>(),
        committed.proposal_ids,
        "the exact staged proposals are adopted"
    );
    assert_eq!(staged_rows(&f), (1, 1), "no second run or proposal");
    assert_eq!(staged.proposals[0].status, "conflict");
    assert_eq!(focus_body(&f).await, B, "B is never overwritten");
}

// ---- a proposal decided between staging and the resume --------------------

/// Decide the staged proposal the way an operator would while the job is
/// crashed between the staging commit and its checkpoint.
async fn decide(f: &Fixture, id: ai_memory_core::AutoImproveProposalId, approve: bool) {
    if approve {
        let result = f
            .wiki
            .approve_auto_improve_proposal(f.ws, f.proj, id, ActorContext::default(), None, None)
            .await
            .unwrap();
        assert!(matches!(
            result,
            ai_memory_store::ApproveAutoImproveProposalResult::Approved { .. }
        ));
    } else {
        f.store
            .writer
            .reject_auto_improve_proposal(ai_memory_store::RejectAutoImproveProposal {
                workspace_id: f.ws,
                project_id: f.proj,
                proposal_id: id,
                reason: "not useful".into(),
                actor: ActorContext::default(),
                author_id: None,
            })
            .await
            .unwrap();
    }
}

/// Before: with manual approval the resume reported every proposal "pending"
/// with no page without reading it, so an approved or rejected proposal came
/// back as pending. After: the stored decision and page id are reported under
/// both policies, and nothing is decided or published by the resume itself.
async fn decided_before_resume(require_approval: bool, approve: bool) {
    let f = fixture(6, 500).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128)
        .with_auto_improve(staged_cfg(), require_approval);
    let (mut run, publication, run_id, ids) = crashed_after_stage(&f, &svc).await;
    decide(&f, ids[0], approve).await;
    let page = PagePath::new(STAGED_PATH).unwrap();
    let version = f
        .store
        .reader
        .latest_page_id_by_ids(f.ws, f.proj, STAGED_PATH.to_string())
        .await
        .unwrap();
    assert_eq!(version.is_some(), approve);

    let staged = resume(&mut run, publication).await;

    assert_eq!(staged.run_id, run_id);
    assert_eq!(staged.proposals.len(), 1);
    assert_eq!(staged.proposals[0].id, ids[0], "the exact staged id");
    assert_eq!(staged_rows(&f), (1, 1), "no second run or proposal");
    if approve {
        assert_eq!(staged.proposals[0].status, "approved");
        assert_eq!(
            staged.proposals[0].page_id,
            version.map(|id| id.to_string()),
            "the page the operator's approval wrote"
        );
        assert!(f.wiki.read_page(f.ws, f.proj, &page).is_ok());
    } else {
        assert_eq!(staged.proposals[0].status, "rejected");
        assert!(staged.proposals[0].page_id.is_none());
        assert!(f.wiki.read_page(f.ws, f.proj, &page).is_err());
    }
    let after = f
        .store
        .reader
        .latest_page_id_by_ids(f.ws, f.proj, STAGED_PATH.to_string())
        .await
        .unwrap();
    assert_eq!(after, version, "the resume wrote no further page version");
}

#[tokio::test]
async fn a_manually_approved_proposal_is_reported_approved_with_its_page_under_manual_policy() {
    decided_before_resume(true, true).await;
}

#[tokio::test]
async fn a_manually_rejected_proposal_is_reported_rejected_under_manual_policy() {
    decided_before_resume(true, false).await;
}

#[tokio::test]
async fn a_manually_approved_proposal_is_reported_approved_with_its_page_under_auto_policy() {
    decided_before_resume(false, true).await;
}

#[tokio::test]
async fn a_manually_rejected_proposal_is_never_re_approved_under_auto_policy() {
    decided_before_resume(false, false).await;
}

/// Control: a still-pending proposal under manual approval stays pending and
/// is never approved by the resume.
#[tokio::test]
async fn a_pending_proposal_stays_pending_under_manual_policy() {
    let f = fixture(6, 500).await;
    let svc =
        service(&f, Arc::new(StageLlm::new()), true, 128).with_auto_improve(staged_cfg(), true);
    let (mut run, publication, _, ids) = crashed_after_stage(&f, &svc).await;

    let staged = resume(&mut run, publication).await;

    assert_eq!(staged.proposals[0].id, ids[0]);
    assert_eq!(staged.proposals[0].status, "pending");
    assert!(staged.proposals[0].page_id.is_none());
    assert!(
        f.wiki
            .read_page(f.ws, f.proj, &PagePath::new(STAGED_PATH).unwrap())
            .is_err()
    );
    assert!(
        recorded_effects(&f, &run)
            .await
            .iter()
            .all(|e| !e.key.starts_with("approve:"))
    );
}
