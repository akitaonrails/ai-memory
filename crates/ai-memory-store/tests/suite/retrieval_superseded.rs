//! Opt-in `include_superseded` retrieval: superseded page versions are hidden
//! by default (every hot query constrains `is_latest = 1`), but a caller can
//! ask `hybrid_search` to return historical versions too, each marked so the
//! caller can tell them apart from the current version. Default-off behaviour
//! must be unchanged (invariant #16: the superseded loser stays reachable, but
//! only when explicitly requested).

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::Store;

fn page(
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    path: &str,
    title: &str,
    body: &str,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

async fn store_with_superseded_page() -> (
    tempfile::TempDir,
    Store,
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
    ai_memory_core::PageId, // v1 (superseded)
    ai_memory_core::PageId, // v2 (latest)
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

    // v1 mentions `postgres`; a shared `zebraquux` token matches both versions.
    let v1 = store
        .writer
        .upsert_page(page(ws, proj, "notes/db.md", "DB", "zebraquux postgres"))
        .await
        .unwrap();
    // v2 supersedes v1: the `postgres` term is gone, `sqlite` replaces it.
    let v2 = store
        .writer
        .upsert_page(page(ws, proj, "notes/db.md", "DB", "zebraquux sqlite"))
        .await
        .unwrap();
    assert_ne!(v1, v2, "the supersede must create a new page version");
    (tmp, store, ws, proj, v1, v2)
}

#[tokio::test]
async fn include_superseded_off_returns_only_latest() {
    let (_tmp, store, ws, proj, _v1, v2) = store_with_superseded_page().await;

    let hits = store
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
        .unwrap();

    assert_eq!(
        hits.len(),
        1,
        "default query returns only the latest version"
    );
    assert_eq!(hits[0].id, v2, "the latest version answers by default");
    assert!(
        !hits[0].superseded,
        "the latest version is not marked superseded"
    );

    // A term that only the superseded version carried must not surface by
    // default — the exact current hidden behaviour.
    let hidden = store
        .reader
        .hybrid_search(
            ws,
            proj,
            "postgres".to_string(),
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
        hidden.is_empty(),
        "a superseded-only term must not surface by default: {hidden:?}"
    );
}

#[tokio::test]
async fn include_superseded_on_returns_both_versions_marked() {
    let (_tmp, store, ws, proj, v1, v2) = store_with_superseded_page().await;

    let hits = store
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
            true,
        )
        .await
        .unwrap();

    let latest = hits
        .iter()
        .find(|h| h.id == v2)
        .expect("latest version must be present");
    let superseded = hits
        .iter()
        .find(|h| h.id == v1)
        .expect("superseded version must be present when include_superseded=true");
    assert!(
        !latest.superseded,
        "the current version is not marked superseded"
    );
    assert!(
        superseded.superseded,
        "the historical version must be marked superseded"
    );

    // A superseded-only term now surfaces, marked.
    let historical = store
        .reader
        .hybrid_search(
            ws,
            proj,
            "postgres".to_string(),
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
    let hit = historical
        .iter()
        .find(|h| h.id == v1)
        .expect("the superseded version carrying the term must surface when opted in");
    assert!(
        hit.superseded,
        "the historical hit must be labelled superseded"
    );
}
