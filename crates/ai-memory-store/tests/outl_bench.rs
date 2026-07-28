//! Ignored-by-default performance probes for the Outl content backend.
//! Run explicitly:
//!
//! ```sh
//! cargo test -p ai-memory-store --release --features adapter-outl \
//!     --test outl_bench -- --ignored --nocapture
//! ```
//!
//! Numbers are printed, not asserted — this is a measurement harness,
//! not a regression gate.

use std::time::Instant;

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::adapters::outl::{OutlContentBackend, reconcile};
use ai_memory_store::{ContentBackend, Store};
use tempfile::TempDir;

const BODY: &str = "# Bench page\n\nParagraph one with some text that resembles a real memory page.\nSecond line of the paragraph.\n\n## Details\n\n- not a list to outl, just text\n\n```rust\nfn bench() -> usize {\n    42\n}\n```\n\nClosing paragraph with a [[wikilink]] and #tag noise.\n";

struct Rig {
    _data_dir: TempDir,
    ws_dir: TempDir,
    store: Store,
    backend: std::sync::Arc<OutlContentBackend>,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
}

async fn rig() -> Rig {
    let data_dir = TempDir::new().unwrap();
    let ws_dir = TempDir::new().unwrap();
    let paths = outl_ws::layout::Paths::at(ws_dir.path().to_path_buf());
    outl_ws::layout::init(&paths).unwrap();

    let store = Store::open(data_dir.path()).unwrap();
    let (backend, _) = OutlContentBackend::open(
        ws_dir.path().to_path_buf(),
        "ai-memory",
        store.writer.clone(),
        store.reader.clone(),
    )
    .unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "bench", None)
        .await
        .unwrap();
    Rig {
        _data_dir: data_dir,
        ws_dir,
        store,
        backend,
        ws,
        proj,
    }
}

fn page(rig: &Rig, path: &str) -> NewPage {
    NewPage {
        workspace_id: rig.ws,
        project_id: rig.proj,
        path: PagePath::new(path).unwrap(),
        title: "Bench page".into(),
        body: BODY.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({"tier": "semantic"}),
        pinned: false,
        links: vec![],
        author_id: None,
    }
}

fn ops_bytes(rig: &Rig) -> u64 {
    std::fs::read_dir(rig.ws_dir.path().join("ops"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

#[tokio::test]
#[ignore = "perf probe; run with --ignored --nocapture"]
async fn bench_outl_backend() {
    let rig = rig().await;
    const PAGES: usize = 300;
    const UPDATES: usize = 200;

    // 1. Fresh writes: N distinct pages.
    let t = Instant::now();
    for i in 0..PAGES {
        rig.backend
            .persist_page(page(&rig, &format!("bench/page-{i:04}.md")), String::new())
            .await
            .unwrap();
    }
    let fresh = t.elapsed();
    let after_fresh = ops_bytes(&rig);
    println!(
        "[fresh-writes] {PAGES} pages in {:.2?} ({:.2} ms/page), op log {:.1} KiB ({:.0} B/page)",
        fresh,
        fresh.as_secs_f64() * 1000.0 / PAGES as f64,
        after_fresh as f64 / 1024.0,
        after_fresh as f64 / PAGES as f64,
    );

    // 2. Repeated updates of ONE page (worst case: every update trashes
    //    the old blocks and appends a fresh forest).
    let t = Instant::now();
    for i in 0..UPDATES {
        let mut p = page(&rig, "bench/hot.md");
        p.body = format!("{BODY}\n\nrevision {i}");
        rig.backend.persist_page(p, String::new()).await.unwrap();
    }
    let updates = t.elapsed();
    let after_updates = ops_bytes(&rig);
    println!(
        "[hot-updates] {UPDATES} updates of one page in {:.2?} ({:.2} ms/update), op log grew {:.1} KiB ({:.0} B/update)",
        updates,
        updates.as_secs_f64() * 1000.0 / UPDATES as f64,
        (after_updates - after_fresh) as f64 / 1024.0,
        (after_updates - after_fresh) as f64 / UPDATES as f64,
    );

    // 3. Steady-state reconcile pass (what runs every `reconcile_secs`
    //    even when nothing changed).
    let t = Instant::now();
    let outcome = reconcile::run_once(&rig.backend).await.unwrap();
    println!(
        "[reconcile-idle] scanned {} pages in {:.2?} (reindexed {})",
        outcome.scanned,
        t.elapsed(),
        outcome.reindexed,
    );
    let t = Instant::now();
    let outcome = reconcile::run_once(&rig.backend).await.unwrap();
    println!(
        "[reconcile-idle-2nd] scanned {} pages in {:.2?} (reindexed {})",
        outcome.scanned,
        t.elapsed(),
        outcome.reindexed,
    );

    // 4. read_page through the trait (served by the index).
    let t = Instant::now();
    for i in 0..100 {
        rig.backend
            .read_page(
                rig.ws,
                rig.proj,
                &PagePath::new(format!("bench/page-{:04}.md", i % PAGES)).unwrap(),
            )
            .await
            .unwrap();
    }
    println!(
        "[index-reads] 100 read_page in {:.2?} ({:.2} ms/read)",
        t.elapsed(),
        t.elapsed().as_secs_f64() * 10.0,
    );

    // 5. read_owned of the churned page (tree walk over live blocks —
    //    trashed nodes should not be visited, but prove it).
    let slug = ai_memory_store::adapters::outl::slug::encode(
        "ai-memory",
        rig.ws,
        rig.proj,
        &PagePath::new("bench/hot.md").unwrap(),
    );
    let t = Instant::now();
    let owned = rig
        .backend
        .handle()
        .read_owned(slug)
        .await
        .unwrap()
        .unwrap();
    println!(
        "[read-owned-hot] churned page read in {:.2?} (body {} B)",
        t.elapsed(),
        owned.body.len(),
    );

    // 6. Cold open over the accumulated op log (boot cost; also the
    //    cost of every RefreshExternal reopen).
    let data_dir2 = TempDir::new().unwrap();
    let store2 = Store::open(data_dir2.path()).unwrap();
    let t = Instant::now();
    let (_b2, info2) = OutlContentBackend::open(
        rig.ws_dir.path().to_path_buf(),
        "ai-memory",
        store2.writer.clone(),
        store2.reader.clone(),
    )
    .unwrap();
    println!(
        "[cold-open] workspace reopen over {:.1} KiB op log in {:.2?} (ephemeral={})",
        ops_bytes(&rig) as f64 / 1024.0,
        t.elapsed(),
        info2.ephemeral_actor,
    );

    // Keep the store alive to the end (drop order noise).
    drop(rig.store);
}
