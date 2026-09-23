//! `ReaderPool::list_pinned_pages`: the "list pinned latest pages" primitive
//! behind the opt-in `memory_query(pin_first=true)` prepend and the briefing's
//! `pinned` standing-context list (2.4 line).
//!
//! Invariants under test:
//! - **Only `pinned = 1 AND is_latest = 1`.** An unpinned latest page never
//!   appears, and a *superseded* (older) pinned version never appears — only
//!   the current pinned versions.
//! - **Recency-ordered.** Most-recently-updated pinned page first.
//! - **Bounded.** The requested limit caps the result.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::Store;

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str, pinned: bool) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

async fn seeded() -> (tempfile::TempDir, Store, WorkspaceId, ProjectId) {
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

/// Only current pinned pages surface, newest first, and the limit bounds it.
#[tokio::test]
async fn list_pinned_pages_returns_only_pinned_latest_recency_ordered() {
    let (_tmp, store, ws, proj) = seeded().await;

    // An unpinned page that must never appear.
    store
        .writer
        .upsert_page(page(ws, proj, "notes/plain.md", "plain body", false))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    // A pinned page written first (older).
    store
        .writer
        .upsert_page(page(ws, proj, "_slots/older.md", "older pinned", true))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    // A pinned page written last (newest) -> must rank first.
    store
        .writer
        .upsert_page(page(ws, proj, "_slots/newer.md", "newer pinned", true))
        .await
        .unwrap();

    let pins = store.reader.list_pinned_pages(ws, proj, 10).await.unwrap();
    let paths: Vec<&str> = pins.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(
        paths,
        vec!["_slots/newer.md", "_slots/older.md"],
        "only pinned latest pages, newest first: {paths:?}"
    );

    // The limit bounds the result.
    let bounded = store.reader.list_pinned_pages(ws, proj, 1).await.unwrap();
    assert_eq!(bounded.len(), 1, "limit must bound the result");
    assert_eq!(bounded[0].path.as_str(), "_slots/newer.md");
}

/// A superseded pinned version (is_latest = 0) is excluded: only the current
/// pinned version of a path is listed.
#[tokio::test]
async fn list_pinned_pages_excludes_superseded_versions() {
    let (_tmp, store, ws, proj) = seeded().await;

    // v1 pinned, then v2 pinned supersedes it on the same path.
    store
        .writer
        .upsert_page(page(ws, proj, "_slots/focus.md", "focus v1", true))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(page(ws, proj, "_slots/focus.md", "focus v2", true))
        .await
        .unwrap();

    let pins = store.reader.list_pinned_pages(ws, proj, 10).await.unwrap();
    assert_eq!(
        pins.len(),
        1,
        "only the latest pinned version of a path is listed: {:?}",
        pins.iter().map(|p| p.path.as_str()).collect::<Vec<_>>()
    );
    assert_eq!(pins[0].path.as_str(), "_slots/focus.md");
    assert!(
        pins[0].title.contains("focus"),
        "the current version answers"
    );
}
