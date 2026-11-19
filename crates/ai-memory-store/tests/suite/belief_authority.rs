//! Belief-strength confidence in ranking (P2,
//! docs/design-hindsight-borrowings.md §3).
//!
//! The evidence-derived `confidence` is exposed in `SearchExplain` inertly
//! (always), and folded into `PageAuthority` only behind the default-OFF
//! `belief_authority_weight`. These tests pin: (1) with the weight off, ranking
//! is unaffected while explain still exposes the fields; (2) with it on, a
//! high-distinct-session page outranks an equally-relevant low-evidence one;
//! (3) a supersession always wins regardless of the prior version's evidence —
//! a superseded version's stale evidence never boosts it (invariant #16 /
//! anti-entrenchment).

use ai_memory_core::{
    LinkTarget, NewPage, PageEvidence, PageEvidenceKind, PagePath, Relation, Tier,
};
use ai_memory_store::{RetrievalTuning, Store};
use jiff::Timestamp;

#[allow(clippy::too_many_arguments)]
fn page(
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    path: &str,
    body: &str,
    evidence: Vec<PageEvidence>,
    links: Vec<LinkTarget>,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links,
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence,
    }
}

/// `n` distinct supporting sessions.
fn sessions(n: usize) -> Vec<PageEvidence> {
    (0..n)
        .map(|i| PageEvidence {
            kind: PageEvidenceKind::Session,
            source_id: format!("session-{i}"),
            observations: Default::default(),
        })
        .collect()
}

fn belief_tuning(weight: f64) -> RetrievalTuning {
    RetrievalTuning {
        belief_authority_weight: weight,
        ..RetrievalTuning::default()
    }
}

fn expired_at() -> Timestamp {
    "2020-01-01T00:00:00Z".parse().unwrap()
}

fn future_at() -> Timestamp {
    "2099-01-01T00:00:00Z".parse().unwrap()
}

fn with_expiry(mut page: NewPage, expires_at: Timestamp) -> NewPage {
    page.expires_at = Some(expires_at);
    page
}

async fn store_with_scope() -> (
    tempfile::TempDir,
    Store,
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    (tmp, store, ws, proj)
}

/// With the belief weight OFF (the default), ranking must be byte-identical to
/// a store that never accrued any evidence — evidence never touches authority —
/// yet an explained query still exposes the inert `confidence` and
/// `evidence_count`. This is the no-surprise-on-upgrade proof.
#[tokio::test]
async fn belief_off_is_byte_identical_but_explain_exposes_confidence() {
    // Byte-identity: the same two pages, one store where a.md carries heavy
    // evidence and one where it carries none. With the weight off, both must
    // produce the exact same ranks — a grid over the two pages.
    async fn ranks(a_evidence: usize) -> Vec<(String, f64)> {
        let (_tmp, store, ws, proj) = store_with_scope().await;
        store
            .writer
            .upsert_page(page(
                ws,
                proj,
                "notes/a.md",
                "widgetquux shared",
                sessions(a_evidence),
                vec![],
            ))
            .await
            .unwrap();
        store
            .writer
            .upsert_page(page(
                ws,
                proj,
                "notes/b.md",
                "widgetquux shared",
                vec![],
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(store.reader.retrieval_tuning().belief_authority_weight, 0.0);
        store
            .reader
            .hybrid_search(
                ws,
                proj,
                "widgetquux".to_string(),
                None,
                String::new(),
                String::new(),
                0,
                10,
                None,
                false,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|h| (h.path.as_str().to_string(), h.rank))
            .collect()
    }
    let with_evidence = ranks(6).await;
    let without = ranks(0).await;
    assert_eq!(
        with_evidence, without,
        "evidence must not change any rank while the belief weight is off"
    );

    // Exposure is on regardless: an explained query surfaces the count and a
    // positive confidence for the supported page, and 0 for the bare one —
    // without folding anything into the rank (belief_factor stays None).
    let (_tmp, store, ws, proj) = store_with_scope().await;
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/a.md",
            "widgetquux shared",
            sessions(6),
            vec![],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/b.md",
            "widgetquux shared",
            vec![],
            vec![],
        ))
        .await
        .unwrap();
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "widgetquux".to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
            false,
        )
        .await
        .unwrap();
    let a = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/a.md")
        .unwrap();
    let b = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/b.md")
        .unwrap();
    assert_eq!(
        a.1.authority, b.1.authority,
        "evidence must not change authority when off"
    );
    assert_eq!(a.1.belief_factor, None);
    assert_eq!(b.1.belief_factor, None);
    assert_eq!(a.1.evidence_count, Some(6));
    assert!(a.1.confidence.unwrap() > 0.0);
    assert_eq!(b.1.evidence_count, Some(0));
    assert_eq!(b.1.confidence, Some(0.0));
}

/// With the weight ON, the well-supported page (more distinct sessions) must
/// outrank an equally-relevant page with no evidence, and explain must account
/// for it via `belief_factor`.
#[tokio::test]
async fn belief_on_boosts_the_higher_evidence_page() {
    let (_tmp, mut store, ws, proj) = store_with_scope().await;
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/a.md",
            "widgetquux shared",
            sessions(8),
            vec![],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/b.md",
            "widgetquux shared",
            vec![],
            vec![],
        ))
        .await
        .unwrap();

    store.reader.set_retrieval_tuning(belief_tuning(0.5));
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "widgetquux".to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        hits[0].0.path.as_str(),
        "notes/a.md",
        "the supported page must lead"
    );
    let a = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/a.md")
        .unwrap();
    let b = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/b.md")
        .unwrap();
    assert!(a.0.rank < b.0.rank);
    assert!(a.1.belief_factor.unwrap() > 0.0);
    assert!(a.1.authority.unwrap() > b.1.authority.unwrap());
    // The bare page's confidence is 0, so its applied belief factor is 0.
    assert_eq!(b.1.belief_factor, Some(0.0));
}

/// A live contradiction must lower a page's confidence relative to an
/// otherwise-identical page — the "weaken" half of strengthen/weaken/extend.
#[tokio::test]
async fn a_live_contradiction_lowers_confidence() {
    let (_tmp, mut store, ws, proj) = store_with_scope().await;
    // Target the contradiction points at must exist as a latest page for the
    // edge to count as live.
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/target.md",
            "target body",
            vec![],
            vec![],
        ))
        .await
        .unwrap();
    // Two equally-supported pages; one declares it contradicts the target.
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/clean.md",
            "widgetquux shared",
            sessions(5),
            vec![],
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/disputed.md",
            "widgetquux shared",
            sessions(5),
            vec![LinkTarget {
                workspace: None,
                project: None,
                path: PagePath::new("notes/target.md").unwrap(),
                relation: Some(Relation::Contradicts),
            }],
        ))
        .await
        .unwrap();

    store.reader.set_retrieval_tuning(belief_tuning(0.5));
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "widgetquux".to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
            false,
        )
        .await
        .unwrap();
    let clean = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/clean.md")
        .unwrap();
    let disputed = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/disputed.md")
        .unwrap();
    assert!(
        disputed.1.confidence.unwrap() < clean.1.confidence.unwrap(),
        "a live contradiction must weaken the belief"
    );
    // Same evidence count; only the contradiction differs.
    assert_eq!(disputed.1.evidence_count, clean.1.evidence_count);
}

/// A contradiction edge targeting an expired or missing page is stale and must
/// NOT count as an unresolved contradiction in `page_belief_inputs` or weaken
/// confidence. Only a latest, unexpired target page lowers confidence.
#[tokio::test]
async fn contradiction_to_expired_target_does_not_lower_confidence() {
    let (_tmp, mut store, ws, proj) = store_with_scope().await;

    // Target that has already expired past its TTL.
    store
        .writer
        .upsert_page(with_expiry(
            page(
                ws,
                proj,
                "notes/expired_target.md",
                "expired target body",
                vec![],
                vec![],
            ),
            expired_at(),
        ))
        .await
        .unwrap();

    // Target that is live and unexpired (future TTL).
    store
        .writer
        .upsert_page(with_expiry(
            page(
                ws,
                proj,
                "notes/live_target.md",
                "live target body",
                vec![],
                vec![],
            ),
            future_at(),
        ))
        .await
        .unwrap();

    // Baseline page with 5 sessions of evidence and no contradictions.
    let clean_id = store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/clean.md",
            "widgetquux shared",
            sessions(5),
            vec![],
        ))
        .await
        .unwrap();

    // Page contradicting the expired target: contradiction is stale, so it must
    // not weaken belief.
    let disputed_expired_id = store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/disputed_expired.md",
            "widgetquux shared",
            sessions(5),
            vec![LinkTarget {
                workspace: None,
                project: None,
                path: PagePath::new("notes/expired_target.md").unwrap(),
                relation: Some(Relation::Contradicts),
            }],
        ))
        .await
        .unwrap();

    // Page contradicting the live target: contradiction is active, so it weakens belief.
    let disputed_live_id = store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/disputed_live.md",
            "widgetquux shared",
            sessions(5),
            vec![LinkTarget {
                workspace: None,
                project: None,
                path: PagePath::new("notes/live_target.md").unwrap(),
                relation: Some(Relation::Contradicts),
            }],
        ))
        .await
        .unwrap();

    // Verify page_belief_inputs directly:
    let inputs = store
        .reader
        .page_belief_inputs(&[clean_id, disputed_expired_id, disputed_live_id])
        .await
        .unwrap();

    assert_eq!(
        inputs[&clean_id].unresolved_contradictions, 0,
        "clean page has no contradictions"
    );
    assert_eq!(
        inputs[&disputed_expired_id].unresolved_contradictions, 0,
        "contradiction to expired target is stale and must not count"
    );
    assert_eq!(
        inputs[&disputed_live_id].unresolved_contradictions, 1,
        "contradiction to live target must count as unresolved"
    );

    // Verify through search ranking and explain output:
    store.reader.set_retrieval_tuning(belief_tuning(0.5));
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "widgetquux".to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
            false,
        )
        .await
        .unwrap();

    let clean = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/clean.md")
        .unwrap();
    let disputed_expired = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/disputed_expired.md")
        .unwrap();
    let disputed_live = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "notes/disputed_live.md")
        .unwrap();

    assert!(
        (disputed_expired.1.confidence.unwrap() - clean.1.confidence.unwrap()).abs() < 1e-4,
        "expired target contradiction must not lower confidence"
    );
    assert!(
        disputed_live.1.confidence.unwrap() < clean.1.confidence.unwrap(),
        "live target contradiction must lower confidence"
    );
}

/// The anti-entrenchment guarantee: a superseding correction always takes and
/// is the returned answer, no matter how much evidence the prior version had —
/// and a superseded version's stale evidence never boosts it in ranking, even
/// with the belief weight cranked up (invariant #16).
#[tokio::test]
async fn supersession_always_wins_regardless_of_prior_evidence() {
    let (_tmp, mut store, ws, proj) = store_with_scope().await;
    // v1: a heavily-supported prior belief.
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "decisions/queue.md",
            "queue zzentry against",
            sessions(8),
            vec![],
        ))
        .await
        .unwrap();
    // A human/agent correction supersedes it. Different body → a new version;
    // it starts its own (small) evidence trail.
    let latest_id = store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "decisions/queue.md",
            "queue zzentry adopt",
            sessions(1),
            vec![],
        ))
        .await
        .unwrap();

    // Even with the belief weight high, the correction is what a normal query
    // returns — evidence never gates whether the write took.
    store.reader.set_retrieval_tuning(belief_tuning(0.8));
    let hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            "queue zzentry".to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        hits.len(),
        1,
        "default search returns the latest version only"
    );
    assert_eq!(
        hits[0].id, latest_id,
        "the correction, not the high-evidence prior, is returned"
    );
    assert!(!hits[0].superseded);

    // With superseded versions surfaced, the stale high-evidence prior must NOT
    // be boosted past the correction: its belief factor is withheld.
    let explained = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "queue zzentry".to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
            true,
        )
        .await
        .unwrap();
    let latest = explained.iter().find(|(h, _)| h.id == latest_id).unwrap();
    let prior = explained
        .iter()
        .find(|(h, _)| h.path.as_str() == "decisions/queue.md" && h.id != latest_id)
        .expect("the superseded prior version should surface with include_superseded");

    assert!(prior.0.superseded);
    assert_eq!(
        prior.1.evidence_count,
        Some(8),
        "the prior kept its evidence trail"
    );
    assert_eq!(
        prior.1.belief_factor, None,
        "a superseded version's stale evidence must never boost it"
    );
    assert!(
        latest.1.belief_factor.is_some(),
        "the live correction is the one belief may shade"
    );
    assert!(
        latest.0.rank <= prior.0.rank,
        "the correction must not rank below its own superseded, higher-evidence prior"
    );
}
