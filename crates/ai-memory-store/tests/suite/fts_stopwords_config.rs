//! Issue #953: `[search.fts] stopwords` must actually reach the FTS query
//! path used by real search calls, not just parse in config.
//!
//! Mirrors the failure mode from the issue report: a non-English function
//! word with high document frequency (Portuguese "de") is not on the
//! built-in (English-only) stopword list, so it passes straight through the
//! OR-join and lets an unrelated, noisy page match a query alongside the
//! page that actually matches the query's topic. Configuring `stopwords`
//! with that word removes the noise entirely (the noisy page stops matching
//! the query at all, since it never contains the real topic word); the
//! default configuration must still show it present (control case).
//!
//! Membership, not rank order, is the assertion: BM25 margins are thin and
//! `hybrid_search` layers RRF fusion and the bounded authority adjustment on
//! top, so "which page ranks first" is not a stable property to pin a test
//! to. "Does the noise page appear in the result set at all" is exactly the
//! property #953 is about, and it flips deterministically once "de" is
//! filtered (the noise page then matches nothing).
//!
//! Covers the three production search paths that prepare a bare FTS query
//! from operator/agent text and read `ReaderPool`'s configured stopwords:
//! `search_pages` (plain FTS), `hybrid_search` (RRF fusion over
//! `search_page_candidates_for_project`, the path `memory_query` reports use
//! for #953), and `search_observations_for_project` (raw observation
//! search). Swapping any one of those call sites back to a hardcoded
//! `FtsStopwords::default()` would fail its assertion here (see the
//! bite-check performed alongside this change).
//!
//! Not covered: the `serve.rs` line that wires `config.search.fts.stopwords()`
//! into `ReaderPool::set_fts_stopwords` at startup has no test of its own —
//! it is a one-line, argument-order-only call with nothing to assert beyond
//! "was it called", which this store-level suite cannot see.

use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, ObservationKind, PagePath, Sanitized,
    Sanitizer, SessionId, Tier,
};
use ai_memory_store::{FtsStopwords, Store};

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

/// "de" repeated many times: the "large unrelated page whose body contains
/// the target language's common function words many times" from the issue
/// repro. Topic-free noise with no occurrence of "deploy" at all.
fn noisy_de_body() -> String {
    std::iter::repeat_n("de", 40).collect::<Vec<_>>().join(" ")
}

#[tokio::test]
async fn configured_stopword_list_removes_noise_from_search_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
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

    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/deploy.md",
            "deploy procedure",
            "run the deploy steps carefully before every release",
            Tier::Semantic,
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/noise.md",
            "unrelated notes",
            &noisy_de_body(),
            Tier::Semantic,
        ))
        .await
        .unwrap();

    let query = "de deploy".to_string();
    let has_noise = |hits: &[ai_memory_store::PageHit]| {
        hits.iter().any(|h| h.path.as_str() == "notes/noise.md")
    };
    let has_topic = |hits: &[ai_memory_store::PageHit]| {
        hits.iter().any(|h| h.path.as_str() == "notes/deploy.md")
    };

    // Control: default config (built-in English list only). "de" is not
    // English, so it leaks into the OR-join and the noise page matches.
    let hits = store
        .reader
        .search_pages(query.clone(), 10, None)
        .await
        .unwrap();
    assert!(
        has_noise(&hits),
        "control must still match the noise page: {hits:?}"
    );
    assert!(
        has_topic(&hits),
        "control must also match the topic page: {hits:?}"
    );

    // Fix: configure the Portuguese stopword list. "de" is dropped before
    // the OR-join, so the query becomes just "deploy" — the noise page,
    // which never contains that word, no longer matches at all.
    store.reader.set_fts_stopwords(FtsStopwords::new(["de"]));
    let hits = store.reader.search_pages(query, 10, None).await.unwrap();
    assert!(
        !has_noise(&hits),
        "configured pt stopword list must remove the noise page entirely: {hits:?}"
    );
    assert!(
        has_topic(&hits),
        "the topic page must still match: {hits:?}"
    );
}

/// Same contamination, exercised through `hybrid_search` — the RRF-fusion
/// path (`search_page_candidates_for_project` internally) that
/// `memory_query` actually uses, and the path #953's repro is about (the FTS
/// stream contaminating RRF fusion even when a vector stream ranks
/// correctly). No embedder is configured here, so the FTS stream is the only
/// one that can match at all; a query-vector-free call still exercises it.
#[tokio::test]
async fn configured_stopword_list_removes_noise_from_hybrid_search() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
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

    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/deploy.md",
            "deploy procedure",
            "run the deploy steps carefully before every release",
            Tier::Semantic,
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/noise.md",
            "unrelated notes",
            &noisy_de_body(),
            Tier::Semantic,
        ))
        .await
        .unwrap();

    let query = "de deploy".to_string();
    let has_noise = |hits: &[ai_memory_store::PageHit]| {
        hits.iter().any(|h| h.path.as_str() == "notes/noise.md")
    };
    let has_topic = |hits: &[ai_memory_store::PageHit]| {
        hits.iter().any(|h| h.path.as_str() == "notes/deploy.md")
    };

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
    assert!(
        has_noise(&hits),
        "control: hybrid_search must still surface the noise page: {hits:?}"
    );
    assert!(
        has_topic(&hits),
        "control must also surface the topic page: {hits:?}"
    );

    store.reader.set_fts_stopwords(FtsStopwords::new(["de"]));
    let hits = store
        .reader
        .hybrid_search(
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
    assert!(
        !has_noise(&hits),
        "configured pt stopword list must remove the noise page from hybrid_search too: {hits:?}"
    );
    assert!(
        has_topic(&hits),
        "the topic page must still surface: {hits:?}"
    );
}

/// Same contamination on the raw-observation search path
/// (`search_observations_for_project`), independent of the page FTS table.
#[tokio::test]
async fn configured_stopword_list_removes_noise_from_observation_search() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
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
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,

            occurred_at: None,
            origin: None,
        })
        .await
        .unwrap();

    let sanitizer = Sanitizer::builtin();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "deploy-observation".into(),
                body: "run the deploy steps carefully before every release".into(),
                importance: 5,

                occurred_at: None,
                file_path: None,
            },
            &sanitizer,
        ))
        .await
        .unwrap();
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "noise-observation".into(),
                body: noisy_de_body(),
                importance: 5,

                occurred_at: None,
                file_path: None,
            },
            &sanitizer,
        ))
        .await
        .unwrap();

    let query = "de deploy".to_string();
    let has_noise = |hits: &[ai_memory_store::ObservationHit]| {
        hits.iter().any(|h| h.title == "noise-observation")
    };
    let has_topic = |hits: &[ai_memory_store::ObservationHit]| {
        hits.iter().any(|h| h.title == "deploy-observation")
    };

    let hits = store
        .reader
        .search_observations_for_project(ws, proj, query.clone(), 10)
        .await
        .unwrap();
    assert!(
        has_noise(&hits),
        "control: observation search must still match the noise observation: {hits:?}"
    );
    assert!(
        has_topic(&hits),
        "control must also match the topic observation: {hits:?}"
    );

    store.reader.set_fts_stopwords(FtsStopwords::new(["de"]));
    let hits = store
        .reader
        .search_observations_for_project(ws, proj, query, 10)
        .await
        .unwrap();
    assert!(
        !has_noise(&hits),
        "configured pt stopword list must remove the noise observation too: {hits:?}"
    );
    assert!(
        has_topic(&hits),
        "the topic observation must still match: {hits:?}"
    );
}
