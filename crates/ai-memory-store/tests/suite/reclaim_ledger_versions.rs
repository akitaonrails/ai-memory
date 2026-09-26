//! `reclaim_ledger_versions` must drop the superseded ledger versions the
//! pre-#660 indexer wrote, and must not touch anything else.
//!
//! Every test here is a fail-before/pass-after case for a destructive
//! operation, so each one states what must survive as loudly as what must go.
//! The content gate is the load-bearing part: a real page a human named
//! `log-2026-09.md` wears the ledger's filename shape, and deleting its version
//! chain would destroy a real page's history (invariant #16).

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::{Compaction, Store};

/// A hook event ledger body, exactly as the hooks append it.
fn ledger_body(stamp: &str) -> String {
    format!("## [{stamp}] tool Bash: cargo test\n")
}

/// The same ledger after the OKF v0.2 migration stamped frontmatter on it.
fn conformed_ledger_body(stamp: &str) -> String {
    format!("---\ntype: Note\ntitle: Log 2026-09\n---\n\n## [{stamp}] tool Bash: cargo test\n")
}

/// An ordinary page that merely wears the ledger's filename.
fn prose_body() -> String {
    "# Log 2026-09\n\nA hand-written page that happens to be named like the ledger.\n".to_string()
}

async fn write_page(
    store: &Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    path: &str,
    body: &str,
) {
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: "t".into(),
            body: body.into(),
            tier: Tier::Episodic,
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
}

/// Rows in `pages` for `path`, newest first, as `(is_latest, body)`.
fn versions(store: &Store, proj: ai_memory_core::ProjectId, path: &str) -> Vec<(i64, String)> {
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT is_latest, body FROM pages \
              WHERE project_id = ?1 AND path = ?2 ORDER BY created_at DESC, rowid DESC",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![proj.as_bytes(), path], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    rows.map(std::result::Result::unwrap).collect()
}

#[tokio::test]
async fn a_dry_run_reports_the_residue_and_deletes_nothing() {
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

    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-05T00:00:00Z"),
    )
    .await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-06T00:00:00Z"),
    )
    .await;
    assert_eq!(versions(&store, proj, "log-2026-09.md").len(), 2);

    let report = store
        .writer
        .reclaim_ledger_versions(true, false, Compaction::Skip)
        .await
        .unwrap();

    assert!(report.dry_run);
    assert_eq!(report.ledger_paths, 1);
    assert_eq!(report.pages_deleted, 1, "only the superseded version");
    assert!(report.bytes_deleted > 0);
    assert!(!report.dropped_latest);
    assert!(!report.compacted);
    assert_eq!(report.bytes_reclaimed(), 0, "a dry run reclaims nothing");
    assert_eq!(
        versions(&store, proj, "log-2026-09.md").len(),
        2,
        "a dry run changes nothing",
    );
}

#[tokio::test]
async fn the_confirmed_run_drops_only_the_superseded_ledger_version() {
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

    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-05T00:00:00Z"),
    )
    .await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-06T00:00:00Z"),
    )
    .await;

    let report = store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Skip)
        .await
        .unwrap();

    assert!(!report.dry_run);
    assert_eq!(report.pages_deleted, 1);

    let left = versions(&store, proj, "log-2026-09.md");
    assert_eq!(left.len(), 1, "the live ledger version survives");
    assert_eq!(left[0].0, 1, "and it is the latest one");
    assert!(
        left[0].1.contains("2026-09-06"),
        "the surviving version is the one the file holds now",
    );
}

#[tokio::test]
async fn a_prose_page_wearing_the_ledger_name_keeps_its_whole_version_chain() {
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

    // A divergent write: two versions of a real page at the ledger's path.
    write_page(&store, ws, proj, "log-2026-09.md", &prose_body()).await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        "# Log 2026-09\n\nSecond, hand-written revision.\n",
    )
    .await;

    let report = store
        .writer
        .reclaim_ledger_versions(false, true, Compaction::Reclaim)
        .await
        .unwrap();

    assert_eq!(
        report.pages_deleted, 0,
        "the filename alone is not a ledger"
    );
    assert_eq!(report.ledger_paths, 0);
    assert_eq!(
        versions(&store, proj, "log-2026-09.md").len(),
        2,
        "a real page's supersession chain is never a candidate",
    );
}

#[tokio::test]
async fn an_ordinary_page_chain_in_another_project_is_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "app".to_string(), None)
        .await
        .unwrap();
    let sidecar = store
        .writer
        .get_or_create_project(ws, "sidecar".to_string(), None)
        .await
        .unwrap();

    // The same ledger path, in two projects, one of them a real page. A
    // cleanup that keyed on the path alone would take the wrong rows.
    write_page(
        &store,
        ws,
        other,
        "log-2026-09.md",
        &ledger_body("2026-09-05T00:00:00Z"),
    )
    .await;
    write_page(
        &store,
        ws,
        other,
        "log-2026-09.md",
        &ledger_body("2026-09-06T00:00:00Z"),
    )
    .await;
    write_page(&store, ws, sidecar, "log-2026-09.md", &prose_body()).await;
    write_page(
        &store,
        ws,
        sidecar,
        "log-2026-09.md",
        "# Log 2026-09\n\nSecond revision.\n",
    )
    .await;
    write_page(&store, ws, sidecar, "notes/keep.md", "a real page").await;
    write_page(&store, ws, sidecar, "notes/keep.md", "a real page, revised").await;

    store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Skip)
        .await
        .unwrap();

    assert_eq!(
        versions(&store, sidecar, "log-2026-09.md").len(),
        2,
        "the other project's real page keeps its chain",
    );
    assert_eq!(
        versions(&store, sidecar, "notes/keep.md").len(),
        2,
        "an ordinary page is never a candidate at all",
    );
    assert_eq!(versions(&store, other, "log-2026-09.md").len(), 1);
}

#[tokio::test]
async fn drop_latest_removes_the_ledger_entirely() {
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

    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-05T00:00:00Z"),
    )
    .await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-06T00:00:00Z"),
    )
    .await;

    let report = store
        .writer
        .reclaim_ledger_versions(false, true, Compaction::Skip)
        .await
        .unwrap();

    assert!(report.dropped_latest);
    assert_eq!(report.pages_deleted, 2, "superseded version and live row");
    assert!(
        versions(&store, proj, "log-2026-09.md").is_empty(),
        "the ledger is gone, so the next hook append starts a fresh chain",
    );
}

#[tokio::test]
async fn a_migrated_ledger_is_still_a_ledger() {
    // #669 stamps OKF frontmatter on every `.md`, ledgers included. Stopping
    // at the fence would classify a migrated ledger as an ordinary page and
    // leave the residue behind — or worse, mislabel a real page as one.
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

    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-05T00:00:00Z"),
    )
    .await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &conformed_ledger_body("2026-09-06T00:00:00Z"),
    )
    .await;

    let report = store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Skip)
        .await
        .unwrap();

    assert_eq!(
        report.pages_deleted, 1,
        "frontmatter does not hide the ledger"
    );
    assert_eq!(versions(&store, proj, "log-2026-09.md").len(), 1);
}

#[tokio::test]
async fn the_fts_delete_trigger_survives_a_reclaim_byte_for_byte() {
    // The reclaim stands the trigger down for the bulk delete and puts the
    // DDL it read back from `sqlite_master` afterwards. If that read ever
    // stopped matching what is in the schema, the store would be left with no
    // delete propagation at all, and that is silent until the next delete.
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

    let trigger_sql = |store: &Store| -> Option<String> {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'pages_fts_ad'",
            [],
            |r| r.get(0),
        )
        .ok()
    };
    let before = trigger_sql(&store);
    assert!(before.is_some(), "the schema ships this trigger");

    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-05T00:00:00Z"),
    )
    .await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-06T00:00:00Z"),
    )
    .await;

    store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Skip)
        .await
        .unwrap();

    assert_eq!(
        trigger_sql(&store),
        before,
        "the trigger is restored exactly"
    );
}

#[tokio::test]
async fn a_reclaimed_ledger_row_leaves_no_searchable_text_behind() {
    // The trigger was stood down, so nothing propagated the delete into the
    // index. This is the check that the wholesale rebuild actually ran: the
    // superseded body must be unfindable, and the live one must still be
    // findable.
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

    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        "## [2026-09-05T00:00:00Z] tool Bash: zzzsupersededtokenzzz\n",
    )
    .await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        "## [2026-09-06T00:00:00Z] tool Bash: yyycurrenttokenyyy\n",
    )
    .await;

    store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Skip)
        .await
        .unwrap();

    let drift = store.reader.derived_index_status().await.unwrap();
    assert_eq!(
        drift.pages_fts_rows, drift.pages_rows,
        "the rebuilt index agrees with the content table again",
    );

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let hits = |needle: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM pages_fts WHERE pages_fts MATCH ?1",
            rusqlite::params![needle],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(hits("zzzsupersededtokenzzz"), 0, "the dropped body is gone");
    assert_eq!(
        hits("yyycurrenttokenyyy"),
        1,
        "the live body is still indexed"
    );
}

#[tokio::test]
async fn a_decay_tombstone_stays_with_the_sweep_that_owns_it() {
    // Only `decay` writes `superseded_at`, and `forget-sweep` hard-deletes
    // that ancestry. Taking those rows here would rob the sweep of the state
    // it reasons about, so the residue query excludes them.
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

    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-05T00:00:00Z"),
    )
    .await;
    write_page(
        &store,
        ws,
        proj,
        "log-2026-09.md",
        &ledger_body("2026-09-06T00:00:00Z"),
    )
    .await;
    {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute(
            "UPDATE pages SET superseded_at = created_at WHERE is_latest = 0",
            [],
        )
        .unwrap();
    }

    let report = store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Skip)
        .await
        .unwrap();

    assert_eq!(
        report.pages_deleted, 0,
        "decay's rows are not this command's"
    );
    assert_eq!(versions(&store, proj, "log-2026-09.md").len(), 2);
}

#[tokio::test]
async fn an_empty_store_reports_nothing_and_touches_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let _ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();

    let report = store
        .writer
        .reclaim_ledger_versions(false, true, Compaction::Reclaim)
        .await
        .unwrap();

    assert_eq!(report.ledger_paths, 0);
    assert_eq!(report.pages_deleted, 0);
    assert!(!report.dropped_latest);
    assert!(!report.compacted, "no rows went, so no VACUUM was needed");
}
