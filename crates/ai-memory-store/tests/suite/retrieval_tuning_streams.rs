//! Opt-in ranking signals (`[retrieval]`): session-recall routing and the L0
//! abstract-embedding stream must be inert by default and change ordering
//! only when explicitly enabled.

use ai_memory_core::{FeedbackKind, NewPage, PagePath, Tier};
use ai_memory_store::{DecayParams, EmbeddingWrite, RetrievalTuning, Store, f32_vec_to_bytes};

fn page(
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    path: &str,
    title: &str,
    body: &str,
    tier: Tier,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.into(),
        body: body.into(),
        tier,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

fn tuning(session_recall: bool, bonus: f64, abstract_vectors: bool) -> RetrievalTuning {
    RetrievalTuning {
        session_recall_routing: session_recall,
        session_recall_bonus: bonus,
        abstract_vectors,
        belief_authority_weight: 0.0,
        observation_stream: false,
        feedback_demotion: 0.0,
        query_signal_weight: 0.0,
    }
}

async fn plain_search(
    store: &Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
) -> Vec<ai_memory_store::PageHit> {
    store
        .reader
        .hybrid_search(
            ws,
            proj,
            "zebraquux".to_string(),
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
}

async fn two_page_store() -> (
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

#[tokio::test]
async fn session_recall_routing_flips_session_page_ranking() {
    let (_tmp, mut store, ws, proj) = two_page_store().await;
    // Both pages carry the same discriminative token, so FTS ranks them
    // evenly and the authority factor alone decides the order.
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/fact.md",
            "fact",
            "zebraquux alpha",
            Tier::Semantic,
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "sessions/s1.md",
            "past session",
            "zebraquux bravo",
            Tier::Episodic,
        ))
        .await
        .unwrap();

    let query = "上次我们 zebraquux".to_string();

    // Default: the semantic note outranks the penalised session page.
    let hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            query.clone(),
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
    assert_eq!(hits[0].path.as_str(), "notes/fact.md");

    // Routing on: the session page gets its penalty back plus the bonus.
    store.reader.set_retrieval_tuning(tuning(true, 0.15, false));
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            query,
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
    assert_eq!(hits[0].0.path.as_str(), "sessions/s1.md");
    let details = hits[0].1.clone();
    assert_eq!(details.intent, Some("session_recall"));
    assert!(details.intent_boost.unwrap_or(1.0) > 1.0);

    // A plain fact query under the same tuning is untouched: the routing
    // does not fire, so ordering stays with the semantic page.
    let hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            "zebraquux alpha".to_string(),
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
    assert_eq!(hits[0].path.as_str(), "notes/fact.md");
}

#[tokio::test]
async fn abstract_vector_stream_is_opt_in() {
    let (_tmp, mut store, ws, proj) = two_page_store().await;
    store
        .writer
        .upsert_page(page(ws, proj, "notes/a.md", "a", "alpha", Tier::Semantic))
        .await
        .unwrap();
    let b = store
        .writer
        .upsert_page(page(ws, proj, "notes/b.md", "b", "bravo", Tier::Semantic))
        .await
        .unwrap();
    let a_id = store
        .reader
        .decay_candidates(ws, proj)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.path.as_str() == "notes/a.md")
        .unwrap()
        .id;

    // Query vector [1, 0]: a's body matches perfectly, b's only halfway.
    let query_vec = vec![1.0_f32, 0.0];
    let provider = "mock".to_string();
    let model = "mock-embed".to_string();
    let dim = 2_u32;
    store
        .writer
        .store_embedding(
            a_id,
            f32_vec_to_bytes(&[1.0, 0.0]),
            provider.clone(),
            model.clone(),
            dim,
        )
        .await
        .unwrap();
    store
        .writer
        .store_embedding(
            b,
            f32_vec_to_bytes(&[0.6, 0.8]),
            provider.clone(),
            model.clone(),
            dim,
        )
        .await
        .unwrap();
    // b also has an L0 abstract whose embedding matches the query exactly;
    // a has none.
    store
        .writer
        .store_abstract_embeddings(vec![EmbeddingWrite {
            page_id: b,
            vector_bytes: f32_vec_to_bytes(&[1.0, 0.0]),
            provider: provider.clone(),
            model: model.clone(),
            dim,
        }])
        .await
        .unwrap();

    let qv = Some(query_vec.clone());

    // Stream off: the body stream alone puts a first.
    store
        .reader
        .set_retrieval_tuning(tuning(false, 0.15, false));
    let hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            "anything".to_string(),
            qv.clone(),
            provider.clone(),
            model.clone(),
            dim,
            10,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(hits[0].path.as_str(), "notes/a.md");

    // Stream on: b's exact abstract match adds a rank-1 RRF vote and flips
    // the order. a has no abstract row, so the stream degrades to nothing
    // for it.
    store.reader.set_retrieval_tuning(tuning(false, 0.15, true));
    let hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            "anything".to_string(),
            qv,
            provider,
            model,
            dim,
            10,
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(hits[0].path.as_str(), "notes/b.md");
}

#[tokio::test]
async fn feedback_demotion_is_opt_in_and_clears_on_rewrite() {
    let (_tmp, mut store, ws, proj) = two_page_store().await;
    let make_page =
        |path: &str, body: &str| page(ws, proj, path, "same title", body, Tier::Semantic);
    let old_a = store
        .writer
        .upsert_page(make_page("notes/a.md", "zebraquux"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(make_page("notes/b.md", "zebraquux"))
        .await
        .unwrap();

    let baseline = plain_search(&store, ws, proj).await;
    assert_eq!(baseline[0].path.as_str(), "notes/a.md");

    store
        .writer
        .record_page_feedback(
            ws,
            proj,
            PagePath::new("notes/a.md").unwrap(),
            FeedbackKind::Stale,
            None,
            None,
            DecayParams::default(),
        )
        .await
        .unwrap();
    store.reader.set_retrieval_tuning(RetrievalTuning {
        feedback_demotion: 1.0,
        ..tuning(false, 0.0, false)
    });
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "zebraquux".to_string(),
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
    assert_eq!(hits[0].0.path.as_str(), "notes/b.md");
    let flagged = hits
        .iter()
        .find(|(hit, _)| hit.path.as_str() == "notes/a.md")
        .unwrap();
    assert_eq!(flagged.1.feedback_demotion, Some(1.0));

    let mut refreshed = make_page("notes/a.md", "zebraquux");
    refreshed.frontmatter_json = serde_json::json!({"revision": 2});
    store.writer.upsert_page(refreshed).await.unwrap();
    let rewritten = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "zebraquux".to_string(),
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
    assert!(
        store
            .reader
            .feedback_flagged_page_ids(ws, proj, vec![old_a])
            .await
            .unwrap()
            .is_empty(),
        "a rewrite supersedes the flagged version and clears the penalty"
    );
    assert!(
        rewritten
            .iter()
            .all(|(_, details)| details.feedback_demotion.is_none()),
        "explain must report no penalty after the rewrite: {rewritten:?}"
    );
}

#[tokio::test]
async fn query_signals_boost_only_their_similar_query_and_current_version() {
    let (_tmp, mut store, ws, proj) = two_page_store().await;
    let other = store
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();
    let make_page =
        |project, path: &str| page(ws, project, path, "same title", "zebraquux", Tier::Semantic);
    store
        .writer
        .upsert_page(make_page(proj, "notes/a.md"))
        .await
        .unwrap();
    let b = store
        .writer
        .upsert_page(make_page(proj, "notes/b.md"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(make_page(other, "notes/other.md"))
        .await
        .unwrap();

    // One session repeats the same pairing; a second session moves the count.
    assert_eq!(
        store
            .writer
            .record_query_page_signal(
                ws,
                proj,
                b,
                "zebraquux caddy".to_string(),
                Some("s1".to_string()),
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .writer
            .record_query_page_signal(
                ws,
                proj,
                b,
                "zebraquux caddy".to_string(),
                Some("s1".to_string()),
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .writer
            .record_query_page_signal(
                ws,
                proj,
                b,
                "zebraquux caddy".to_string(),
                Some("s2".to_string()),
            )
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .writer
            .record_query_page_signal(
                ws,
                other,
                b,
                "zebraquux caddy".to_string(),
                Some("s3".to_string()),
            )
            .await
            .unwrap(),
        0,
        "a page id from another project must not create a signal there"
    );

    store.reader.set_retrieval_tuning(RetrievalTuning {
        query_signal_weight: 1.0,
        ..tuning(false, 0.0, false)
    });
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "zebraquux caddy".to_string(),
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
    assert_eq!(hits[0].0.path.as_str(), "notes/b.md");
    assert_eq!(hits[0].1.query_signal_boost, Some(0.5));
    let unconfirmed = hits
        .iter()
        .find(|(hit, _)| hit.path.as_str() == "notes/a.md")
        .unwrap();
    assert_eq!(unconfirmed.1.query_signal_boost, None);

    let mut rewritten_page = make_page(proj, "notes/b.md");
    rewritten_page.frontmatter_json = serde_json::json!({"revision": 2});
    let new_b = store.writer.upsert_page(rewritten_page).await.unwrap();
    assert_ne!(new_b, b);
    let rewritten = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "zebraquux caddy".to_string(),
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
    assert_eq!(rewritten[0].0.path.as_str(), "notes/a.md");
    assert!(
        rewritten
            .iter()
            .all(|(_, details)| details.query_signal_boost.is_none()),
        "an association attached to a superseded version must not boost its successor"
    );
}

#[tokio::test]
async fn query_signal_boost_does_not_lift_a_negative_tag_cap() {
    let (_tmp, mut store, ws, proj) = two_page_store().await;
    let mut fixture = page(
        ws,
        proj,
        "notes/fixture.md",
        "fixture",
        "zebraquux",
        Tier::Semantic,
    );
    fixture.frontmatter_json = serde_json::json!({"tags": ["test-fixture"]});
    let fixture_id = store.writer.upsert_page(fixture).await.unwrap();
    for session in ["s1", "s2"] {
        store
            .writer
            .record_query_page_signal(
                ws,
                proj,
                fixture_id,
                "zebraquux caddy".to_string(),
                Some(session.to_string()),
            )
            .await
            .unwrap();
    }

    store.reader.set_retrieval_tuning(RetrievalTuning {
        query_signal_weight: 1.0,
        ..tuning(false, 0.0, false)
    });
    let hits = store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            "zebraquux caddy".to_string(),
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
    let details = &hits
        .iter()
        .find(|(hit, _)| hit.path.as_str() == "notes/fixture.md")
        .unwrap()
        .1;
    assert!(details.query_signal_boost.is_some());
    assert!(
        details.authority.unwrap() <= 0.55,
        "a test-fixture page rose to authority {:?}",
        details.authority
    );
}

/// A purged session takes its query text with it (#1941): an association the
/// session wrote last is deleted in the purge transaction, while one another
/// session wrote last stays. The client's native session id is stored as the
/// same `SessionId` the hook ingress derives, which is what lets the purge
/// find it.
#[tokio::test]
async fn purging_a_session_deletes_the_query_signals_it_wrote_last() {
    use ai_memory_core::{AgentKind, NewSession, SessionId};
    use ai_memory_store::{Compaction, PurgeMode};
    let (_tmp, store, ws, proj) = two_page_store().await;
    let page_id = store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/purge.md",
            "purge",
            "zebraquux",
            Tier::Semantic,
        ))
        .await
        .unwrap();
    let other_page = store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/keep.md",
            "keep",
            "zebraquux",
            Tier::Semantic,
        ))
        .await
        .unwrap();
    let native = "claude-native-session";
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: SessionId::from_native(native),
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    for (page, session) in [(page_id, native), (other_page, "another-session")] {
        store
            .writer
            .record_query_page_signal(
                ws,
                proj,
                page,
                "zebraquux private question".to_string(),
                Some(session.to_string()),
            )
            .await
            .unwrap();
    }

    store
        .writer
        .purge_session(
            ws,
            proj,
            SessionId::from_native(native),
            None,
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();

    let rows = store
        .reader
        .query_signals_for_pages(ws, proj, vec![page_id, other_page])
        .await
        .unwrap();
    assert!(
        rows.get(&page_id).is_none_or(Vec::is_empty),
        "the purged session's query text must be gone: {rows:?}"
    );
    assert_eq!(
        rows.get(&other_page).map(Vec::len),
        Some(1),
        "another session's association stays"
    );
}

/// Adversarial (#1941): a signal row is filed under the QUERIED project, so a
/// session in project A that searched project B left its query text in B.
/// Purging the session, its project, or its workspace must reach those B rows
/// too, while a different session's row in B survives every purge.
#[tokio::test]
async fn purges_delete_query_signals_a_session_left_in_another_project() {
    use ai_memory_core::{AgentKind, NewSession, SessionId};
    use ai_memory_store::{Compaction, PurgeMode};
    let (_tmp, store, ws, proj_a) = two_page_store().await;
    let proj_b = store
        .writer
        .get_or_create_project(ws, "other".to_string(), None)
        .await
        .unwrap();
    let other_ws = store
        .writer
        .get_or_create_workspace("elsewhere".to_string())
        .await
        .unwrap();
    let proj_c = store
        .writer
        .get_or_create_project(other_ws, "far".to_string(), None)
        .await
        .unwrap();
    let begin = |native: &'static str, ws, project| {
        let writer = store.writer.clone();
        async move {
            writer
                .begin_session(NewSession {
                    occurred_at: None,
                    id: SessionId::from_native(native),
                    workspace_id: ws,
                    project_id: project,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
        }
    };
    begin("a-purged-session", ws, proj_a).await;
    begin("a-sibling-session", ws, proj_a).await;
    begin("b-own-session", ws, proj_b).await;
    begin("c-far-session", other_ws, proj_c).await;
    let mut pages = Vec::new();
    for (path, session) in [
        ("notes/by-purged.md", "a-purged-session"),
        ("notes/by-sibling.md", "a-sibling-session"),
        ("notes/by-far.md", "c-far-session"),
        ("notes/by-b.md", "b-own-session"),
    ] {
        let page_id = store
            .writer
            .upsert_page(page(ws, proj_b, path, path, "zebraquux", Tier::Semantic))
            .await
            .unwrap();
        store
            .writer
            .record_query_page_signal(
                ws,
                proj_b,
                page_id,
                "zebraquux private question".to_string(),
                Some(session.to_string()),
            )
            .await
            .unwrap();
        pages.push(page_id);
    }
    let [by_purged, by_sibling, by_far, by_b] = pages[..] else {
        unreachable!()
    };
    let surviving = || async {
        let rows = store
            .reader
            .query_signals_for_pages(ws, proj_b, vec![by_purged, by_sibling, by_far, by_b])
            .await
            .unwrap();
        [by_purged, by_sibling, by_far, by_b]
            .map(|page| rows.get(&page).is_some_and(|rows| !rows.is_empty()))
    };
    assert_eq!(surviving().await, [true, true, true, true]);

    store
        .writer
        .purge_session(
            ws,
            proj_a,
            SessionId::from_native("a-purged-session"),
            None,
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();
    assert_eq!(
        surviving().await,
        [false, true, true, true],
        "purge-session must delete its query text from the project it queried"
    );

    store
        .writer
        .purge_project(
            ws,
            proj_a,
            "default/app",
            None,
            false,
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();
    assert_eq!(
        surviving().await,
        [false, false, true, true],
        "purge-project must delete its sessions' query text from sibling projects"
    );

    store
        .writer
        .delete_workspace(other_ws, true, Compaction::Skip, PurgeMode::Commit)
        .await
        .unwrap();
    assert_eq!(
        surviving().await,
        [false, false, false, true],
        "deleting a workspace must delete its sessions' query text elsewhere; \
         a session of the queried project keeps its own row"
    );
}

/// `forget_query_page_signal` (the `not_helpful` negative signal) removes
/// exactly the matching (query, page) association.
#[tokio::test]
async fn forgetting_a_query_signal_removes_only_that_pair() {
    let (_tmp, store, ws, proj) = two_page_store().await;
    let page_id = store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/f.md",
            "f",
            "zebraquux",
            Tier::Semantic,
        ))
        .await
        .unwrap();
    for query in ["zebraquux caddy", "zebraquux postgres"] {
        store
            .writer
            .record_query_page_signal(ws, proj, page_id, query.to_string(), Some("s".into()))
            .await
            .unwrap();
    }
    assert!(
        store
            .writer
            .forget_query_page_signal(ws, proj, page_id, "  ZEBRAQUUX   caddy ".to_string())
            .await
            .unwrap(),
        "the normalized query must match the stored row"
    );
    let rows = store
        .reader
        .query_signals_for_pages(ws, proj, vec![page_id])
        .await
        .unwrap()
        .remove(&page_id)
        .unwrap_or_default();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].query_text, "zebraquux postgres");
    assert!(
        !store
            .writer
            .forget_query_page_signal(ws, proj, page_id, "zebraquux caddy".to_string())
            .await
            .unwrap()
    );
}
