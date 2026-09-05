//! Lexical-search usefulness suite.
//!
//! Same philosophy as `local_embeddings.rs` but CI-runnable (no model):
//! these tests assert that search RESULTS serve the user, not merely
//! that the machinery runs. The scenario is the one that exposed the
//! problem live: with no stopword handling, a natural-language question
//! OR-joined into FTS5 let a page containing five "the"s outrank the
//! page whose content matched — and every page containing any stopword
//! entered the result list as trash.

use ai_memory_core::{PagePath, Tier};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

async fn seeded() -> (
    TempDir,
    Store,
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    // Stopword-heavy bodies on purpose — the pre-fix failure needed
    // nothing more than "the" repetition to produce trash rankings.
    for (path, body) in [
        (
            "notes/changelog-style.md",
            "Every new version of the app gets a changelog entry describing \
             what the new version changed for app users.",
        ),
        (
            "procedures/release.md",
            "The release procedure: tag main, wait for the pipeline, then \
             push the image to the registry and restart the compose stack.",
        ),
        (
            "notes/pasta.md",
            "Boil the water with plenty of the salt before adding the \
             spaghetti to the pot.",
        ),
        (
            "notes/howto-index.md",
            "How do we keep this wiki organised: one page per concept.",
        ),
    ] {
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": path}),
            body: body.into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();
    }
    (tmp, store, ws, proj)
}

async fn fts(
    store: &Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    q: &str,
) -> Vec<String> {
    store
        .reader
        .hybrid_search(
            ws,
            proj,
            q.to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
        )
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.path.as_str().to_string())
        .collect()
}

/// The anti-trash property: for a natural-language question, pages
/// matching only the question's stopwords do not appear at all, and the
/// page matching the CONTENT words ranks first.
#[tokio::test]
async fn natural_questions_rank_content_not_stopword_frequency() {
    let (_tmp, store, ws, proj) = seeded().await;
    let hits = fts(
        &store,
        ws,
        proj,
        "how do we deploy a new version of the app?",
    )
    .await;
    assert_eq!(
        hits.first().map(String::as_str),
        Some("notes/changelog-style.md"),
        "content-word matches must rank first: {hits:?}"
    );
    for trash in ["notes/pasta.md", "procedures/release.md"] {
        assert!(
            !hits.iter().any(|h| h == trash),
            "{trash} matches only stopwords and must not appear: {hits:?}"
        );
    }
}

/// A query that is ONLY stopwords still searches the user's literal
/// terms — an empty result page for "how do we" would be worse.
#[tokio::test]
async fn all_stopword_queries_fall_back_to_literal_terms() {
    let (_tmp, store, ws, proj) = seeded().await;
    let hits = fts(&store, ws, proj, "how do we").await;
    assert!(
        hits.iter().any(|h| h == "notes/howto-index.md"),
        "literal stopword terms must still match when nothing else is \
         in the query: {hits:?}"
    );
}

/// Quoted phrases keep their stopwords — "the compose stack" is an
/// exact-phrase request, not a bag of words.
#[tokio::test]
async fn quoted_phrases_keep_stopwords() {
    let (_tmp, store, ws, proj) = seeded().await;
    let hits = fts(&store, ws, proj, "\"restart the compose stack\"").await;
    assert_eq!(
        hits,
        vec!["procedures/release.md".to_string()],
        "exact phrase must match exactly one page"
    );
}

/// Keyword queries (the common agent-issued shape) are unaffected:
/// every content word still ORs.
#[tokio::test]
async fn keyword_queries_are_unchanged() {
    let (_tmp, store, ws, proj) = seeded().await;
    let hits = fts(&store, ws, proj, "changelog registry").await;
    assert!(
        hits.iter().any(|h| h == "notes/changelog-style.md"),
        "{hits:?}"
    );
    assert!(
        hits.iter().any(|h| h == "procedures/release.md"),
        "{hits:?}"
    );
}
