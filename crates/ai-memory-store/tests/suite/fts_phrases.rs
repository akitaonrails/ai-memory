//! Quoted FTS phrases constrain both word order and adjacency, including
//! when auxiliary CJK retrieval participates in a search.

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::Store;

use super::retrieval_observation_stream::{observe, scope, session};

#[tokio::test]
async fn quoted_phrases_keep_adjacency_in_pages_and_observations() {
    for (query, exact, separated, reversed) in [
        (
            "\"alpha beta\"",
            "alpha beta",
            "alpha gap beta",
            "beta alpha",
        ),
        ("\"東京 大阪\"", "東京 大阪", "東京 京都 大阪", "大阪 東京"),
        (
            "\"alpha bet\"*",
            "alpha beta",
            "alpha gap beta",
            "beta alpha",
        ),
        (
            "\"say \"\"hello\"\" now\"",
            "say \"hello\" now",
            "say before \"hello\" now",
            "now \"hello\" say",
        ),
        (
            "(\"alpha beta\") AND final",
            "alpha beta final",
            "alpha gap beta final",
            "beta alpha final",
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let (ws, project) = scope(&store, "default", "app").await;
        let session_id = session(&store, ws, project).await;
        let mut exact_observation = None;
        for (path, body) in [
            ("exact.md", exact),
            ("separated.md", separated),
            ("reversed.md", reversed),
        ] {
            store
                .writer
                .upsert_page(NewPage {
                    workspace_id: ws,
                    project_id: project,
                    path: PagePath::new(path).unwrap(),
                    title: "Notes".into(),
                    body: body.into(),
                    tier: Tier::Semantic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                    expires_at: None,
                    entities: Vec::new(),
                    evidence: Vec::new(),
                })
                .await
                .unwrap();
            let observation = observe(&store, ws, project, session_id, body).await;
            if path == "exact.md" {
                exact_observation = Some(observation);
            }
        }

        let project_hits = store
            .reader
            .search_pages_for_project(ws, project, query.into(), 10, None)
            .await
            .unwrap();
        assert_eq!(
            project_hits
                .iter()
                .map(|hit| hit.path.as_str())
                .collect::<Vec<_>>(),
            ["exact.md"],
            "project search widened {query}"
        );
        let global_hits = store
            .reader
            .search_pages(query.into(), 10, None)
            .await
            .unwrap();
        assert_eq!(
            global_hits
                .iter()
                .map(|hit| hit.path.as_str())
                .collect::<Vec<_>>(),
            ["exact.md"],
            "global search widened {query}"
        );
        let hybrid_hits = store
            .reader
            .hybrid_search(
                ws,
                project,
                query.into(),
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
            hybrid_hits
                .iter()
                .map(|hit| hit.path.as_str())
                .collect::<Vec<_>>(),
            ["exact.md"],
            "hybrid search widened {query}"
        );
        let observations = store
            .reader
            .search_observations_for_project(ws, project, query.into(), 10)
            .await
            .unwrap();
        assert_eq!(
            observations.len(),
            1,
            "observation search widened {query}: {observations:?}"
        );
        assert_eq!(Some(observations[0].id), exact_observation);

        let bare = store
            .reader
            .search_pages_for_project(ws, project, exact.into(), 10, None)
            .await
            .unwrap();
        assert_eq!(bare.len(), 3, "bare queries must keep OR-join recall");
    }
}
