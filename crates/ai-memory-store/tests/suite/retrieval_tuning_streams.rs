//! Opt-in ranking signals (`[retrieval]`): session-recall routing and the L0
//! abstract-embedding stream must be inert by default and change ordering
//! only when explicitly enabled.

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::{EmbeddingWrite, RetrievalTuning, Store, f32_vec_to_bytes};

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
    }
}

fn tuning(session_recall: bool, bonus: f64, abstract_vectors: bool) -> RetrievalTuning {
    RetrievalTuning {
        session_recall_routing: session_recall,
        session_recall_bonus: bonus,
        abstract_vectors,
    }
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
        )
        .await
        .unwrap();
    assert_eq!(hits[0].path.as_str(), "notes/b.md");
}
