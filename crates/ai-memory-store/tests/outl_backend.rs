//! Integration tests: OutlContentBackend against a real tempdir Outl
//! workspace + a real tempdir SQLite store, including the external-edit
//! reconcile path and a concurrent second actor.

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::adapters::outl::{OutlContentBackend, reconcile, slug};
use ai_memory_store::{ContentBackend, Store};
use tempfile::TempDir;

fn init_outl_workspace(dir: &std::path::Path) {
    let paths = outl_ws::layout::Paths::at(dir.to_path_buf());
    outl_ws::layout::init(&paths).expect("init outl workspace");
}

struct Rig {
    _data_dir: TempDir,
    _ws_dir: TempDir,
    store: Store,
    backend: std::sync::Arc<OutlContentBackend>,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
}

async fn rig() -> Rig {
    let data_dir = TempDir::new().unwrap();
    let ws_dir = TempDir::new().unwrap();
    init_outl_workspace(ws_dir.path());

    let store = Store::open(data_dir.path()).unwrap();
    let (backend, info) = OutlContentBackend::open(
        ws_dir.path().to_path_buf(),
        "ai-memory",
        store.writer.clone(),
        store.reader.clone(),
    )
    .unwrap();
    assert!(!info.ephemeral_actor, "first opener gets the config actor");

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
    Rig {
        _data_dir: data_dir,
        _ws_dir: ws_dir,
        store,
        backend,
        ws,
        proj,
    }
}

fn new_page(rig: &Rig, path: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: rig.ws,
        project_id: rig.proj,
        path: PagePath::new(path).unwrap(),
        title: "Test page".into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({"tier": "semantic"}),
        pinned: false,
        links: vec![],
        author_id: None,
    }
}

const BODY: &str = "# Decision\n\nWe picked outl as SoT.\nSecond line of paragraph.\n\n```rust\nfn main() {}\n```\n";

#[tokio::test]
async fn persist_page_lands_in_outl_and_index() {
    let rig = rig().await;
    let page = new_page(&rig, "notes/decision.md", BODY);
    rig.backend.persist_page(page, String::new()).await.unwrap();

    // Index row serves the ORIGINAL body.
    let stored = rig
        .store
        .reader
        .page_body_by_ids(rig.ws, rig.proj, "notes/decision.md")
        .await
        .unwrap()
        .expect("index row exists");
    assert_eq!(stored.body, BODY);

    // Outl page exists under the deterministic slug with the content
    // projection.
    let expected_slug = slug::encode(
        "ai-memory",
        rig.ws,
        rig.proj,
        &PagePath::new("notes/decision.md").unwrap(),
    );
    let owned = rig
        .backend
        .handle()
        .read_owned(expected_slug)
        .await
        .unwrap()
        .expect("outl page exists");
    assert!(owned.body.contains("We picked outl as SoT."));
    assert!(owned.body.contains("```rust\nfn main() {}\n```"));
    assert!(owned.stored_sha.is_some());

    // read_page (trait) returns the original document body.
    let content = rig
        .backend
        .read_page(
            rig.ws,
            rig.proj,
            &PagePath::new("notes/decision.md").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(content.body, BODY);
}

#[tokio::test]
async fn reconcile_skips_own_writes_and_absorbs_external_edits() {
    let rig = rig().await;
    let page = new_page(&rig, "notes/edited.md", BODY);
    rig.backend.persist_page(page, String::new()).await.unwrap();

    // Pass 1: nothing external happened — index body must stay the
    // ORIGINAL document (no dialect drift).
    let outcome = reconcile::run_once(&rig.backend).await.unwrap();
    assert_eq!(outcome.reindexed, 0, "own write must not reconcile");
    let stored = rig
        .store
        .reader
        .page_body_by_ids(rig.ws, rig.proj, "notes/edited.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.body, BODY);

    // Simulate an edit made inside the Outl app: append a block via the
    // actor without updating the sha stamp (upsert_page would restamp,
    // so we go through a raw handle write on a distinct page state —
    // easiest faithful simulation is editing through upsert with the
    // OLD sha kept, i.e. bypassing projected_sha; instead we edit via
    // a second in-process write that changes content but reuses the
    // stale stamp).
    let page_slug = slug::encode(
        "ai-memory",
        rig.ws,
        rig.proj,
        &PagePath::new("notes/edited.md").unwrap(),
    );
    let owned = rig
        .backend
        .handle()
        .read_owned(page_slug.clone())
        .await
        .unwrap()
        .unwrap();
    let stale_sha = owned.stored_sha.clone().unwrap();
    let edited_body = format!("{}\n\nuser addition from outl", owned.body.trim_end());
    let specs = ai_memory_store::adapters::outl::project::body_to_specs(&edited_body);
    rig.backend
        .handle()
        .upsert_page(
            page_slug.clone(),
            owned.title.clone(),
            "semantic".into(),
            specs,
            stale_sha, // stamp does NOT match the new content → "external edit"
        )
        .await
        .unwrap();

    let outcome = reconcile::run_once(&rig.backend).await.unwrap();
    assert_eq!(outcome.reindexed, 1, "external edit must reconcile");

    let stored = rig
        .store
        .reader
        .page_body_by_ids(rig.ws, rig.proj, "notes/edited.md")
        .await
        .unwrap()
        .unwrap();
    assert!(stored.body.contains("user addition from outl"));

    // Pass 3: stamp was refreshed — converged, nothing to do.
    let outcome = reconcile::run_once(&rig.backend).await.unwrap();
    assert_eq!(outcome.reindexed, 0, "reconcile must converge");
}

#[tokio::test]
async fn delete_page_trashes_outl_copy_and_index_rows() {
    let rig = rig().await;
    let page = new_page(&rig, "notes/tmp.md", BODY);
    rig.backend.persist_page(page, String::new()).await.unwrap();

    rig.backend
        .delete_page(
            rig.ws,
            rig.proj,
            PagePath::new("notes/tmp.md").unwrap(),
            None,
        )
        .await
        .unwrap();

    let page_slug = slug::encode(
        "ai-memory",
        rig.ws,
        rig.proj,
        &PagePath::new("notes/tmp.md").unwrap(),
    );
    assert!(
        rig.backend
            .handle()
            .read_owned(page_slug)
            .await
            .unwrap()
            .is_none(),
        "outl page must be trashed"
    );
    assert!(
        rig.store
            .reader
            .page_body_by_ids(rig.ws, rig.proj, "notes/tmp.md")
            .await
            .unwrap()
            .is_none(),
        "index rows must be gone"
    );

    // Idempotent: deleting again is fine.
    rig.backend
        .delete_page(
            rig.ws,
            rig.proj,
            PagePath::new("notes/tmp.md").unwrap(),
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn remove_project_only_trashes_that_projects_pages() {
    let rig = rig().await;
    let proj2 = rig
        .store
        .writer
        .get_or_create_project(rig.ws, "other", None)
        .await
        .unwrap();

    rig.backend
        .persist_page(new_page(&rig, "a.md", "keep me"), String::new())
        .await
        .unwrap();
    let mut other = new_page(&rig, "b.md", "other project");
    other.project_id = proj2;
    rig.backend
        .persist_page(other, String::new())
        .await
        .unwrap();

    rig.backend.remove_project(rig.ws, rig.proj).await.unwrap();

    let gone = slug::encode(
        "ai-memory",
        rig.ws,
        rig.proj,
        &PagePath::new("a.md").unwrap(),
    );
    let kept = slug::encode("ai-memory", rig.ws, proj2, &PagePath::new("b.md").unwrap());
    assert!(
        rig.backend
            .handle()
            .read_owned(gone)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        rig.backend
            .handle()
            .read_owned(kept)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn second_concurrent_opener_gets_ephemeral_actor() {
    let rig = rig().await;
    // While the first backend holds the config actor, a second opener
    // on the same workspace must fall back to an ephemeral actor (this
    // is the "Outl app is open while ai-memory runs" scenario).
    let data_dir2 = TempDir::new().unwrap();
    let store2 = Store::open(data_dir2.path()).unwrap();
    let (_backend2, info2) = OutlContentBackend::open(
        rig._ws_dir.path().to_path_buf(),
        "ai-memory",
        store2.writer.clone(),
        store2.reader.clone(),
    )
    .unwrap();
    assert!(
        info2.ephemeral_actor,
        "second opener must not steal the config actor"
    );
}
