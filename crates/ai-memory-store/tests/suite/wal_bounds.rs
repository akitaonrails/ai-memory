//! The `-wal` file must not outlive the operation or the process that grew it.
//!
//! A `VACUUM` rewrites every page through the WAL, and SQLite's automatic
//! checkpoint never shrinks the file, so a reclaim on a large store left a
//! `-wal` as big as the database. Killed before a clean close, the server made
//! the next start replay all of it. Each test here runs against a real WAL-mode
//! [`Store`] and reads the `-wal` file's length; a fixture on the in-memory
//! journal cannot see the defect.

use std::path::PathBuf;

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_store::{Compaction, PurgeMode, Store, StoreError};

/// Enough ledger versions that a rewrite of the database is megabytes, not
/// bytes: the assertions compare the `-wal` against zero, and a database whose
/// whole rewrite is a few pages would hide a missing checkpoint in noise.
const LEDGER_VERSIONS: usize = 24;

fn wal_path(store: &Store) -> PathBuf {
    store.db_path().with_extension("sqlite-wal")
}

/// Length of the `-wal`, zero when SQLite has already deleted it.
fn wal_len(store: &Store) -> u64 {
    std::fs::metadata(wal_path(store)).map_or(0, |m| m.len())
}

/// A ledger body of about 30 KiB in the shape the hooks append, so the
/// reclaim's content gate accepts every version.
fn ledger_body(version: usize) -> String {
    (0..600)
        .map(|line| {
            format!(
                "## [2026-09-05T{version:02}:00:{:02}Z] tool Bash: cargo test\n",
                line % 60
            )
        })
        .collect()
}

/// A store with a ledger path carrying [`LEDGER_VERSIONS`] versions.
async fn store_with_ledger_residue(
    tmp: &tempfile::TempDir,
) -> (
    Store,
    ai_memory_core::WorkspaceId,
    ai_memory_core::ProjectId,
) {
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
    for version in 0..LEDGER_VERSIONS {
        store
            .writer
            .upsert_page(NewPage {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new("log-2026-09.md").unwrap(),
                title: "t".into(),
                body: ledger_body(version),
                tier: Tier::Episodic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
                trust_class: None,
            })
            .await
            .unwrap();
    }
    (store, ws, proj)
}

#[tokio::test]
async fn compact_truncates_the_wal_it_grew() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _, _) = store_with_ledger_residue(&tmp).await;
    assert!(
        wal_len(&store) > 0,
        "fixture: the writes must have left a WAL to truncate"
    );

    store.writer.compact().await.unwrap();

    assert_eq!(wal_len(&store), 0, "compact left its VACUUM in the -wal");
}

#[tokio::test]
async fn reclaiming_ledger_versions_truncates_the_wal_it_grew() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _, _) = store_with_ledger_residue(&tmp).await;

    let summary = store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Reclaim)
        .await
        .unwrap();

    assert!(summary.compacted, "fixture: the VACUUM must have run");
    assert_eq!(summary.pages_deleted, (LEDGER_VERSIONS - 1) as u64);
    assert_eq!(
        wal_len(&store),
        0,
        "the reclaim left its VACUUM in the -wal"
    );
}

/// Control: without `--compact` there is no `VACUUM`, so the reclaim must not
/// start checkpointing on its own. Keeps the assertion above from passing
/// merely because every call truncates.
#[tokio::test]
async fn reclaiming_ledger_versions_without_compaction_does_not_vacuum() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _, _) = store_with_ledger_residue(&tmp).await;

    let summary = store
        .writer
        .reclaim_ledger_versions(false, false, Compaction::Skip)
        .await
        .unwrap();

    assert!(!summary.compacted);
    assert!(wal_len(&store) > 0, "a delete without VACUUM keeps its WAL");
}

#[tokio::test]
async fn purging_a_project_with_compaction_truncates_the_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, ws, proj) = store_with_ledger_residue(&tmp).await;

    store
        .writer
        .purge_project(
            ws,
            proj,
            "default/app",
            None,
            true,
            Compaction::Reclaim,
            PurgeMode::Commit,
        )
        .await
        .unwrap();

    assert_eq!(
        wal_len(&store),
        0,
        "purge --compact left its VACUUM in the -wal"
    );
}

/// The shutdown path: `main` exits without joining the writer, so the
/// checkpoint has to happen before the process goes. A second connection stays
/// open, as the read pool's would, so SQLite's own "last connection closed"
/// checkpoint cannot be what empties the file.
#[tokio::test]
async fn close_checkpoints_the_wal_even_while_another_connection_is_open() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _, _) = store_with_ledger_residue(&tmp).await;
    let bystander = rusqlite::Connection::open(store.db_path()).unwrap();
    bystander
        .query_row("SELECT count(*) FROM pages", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert!(wal_len(&store) > 0, "fixture: pending WAL before close");

    store.close().await;

    assert_eq!(wal_len(&store), 0, "close left the WAL for the next start");
    drop(bystander);
}

/// Releasing the connections is observable: SQLite deletes the `-wal` only when
/// the last connection to the database closes. A writer or a pooled reader
/// that survives `close` keeps the file.
#[tokio::test]
async fn close_releases_the_writer_and_the_pooled_readers() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _, _) = store_with_ledger_residue(&tmp).await;
    // Park a read connection in the pool.
    store.reader.users_exist().await.unwrap();
    assert!(
        wal_path(&store).exists(),
        "fixture: a live WAL before close"
    );

    store.close().await;

    assert!(
        !wal_path(&store).exists(),
        "a connection survived close: SQLite keeps the -wal while one is open"
    );
}

#[tokio::test]
async fn the_writer_refuses_commands_after_close_and_close_is_repeatable() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _, _) = store_with_ledger_residue(&tmp).await;

    store.close().await;
    store.close().await;

    let err = store
        .writer
        .get_or_create_workspace("too-late".to_string())
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::WriterClosed), "got {err:?}");
}

/// A read that is running when `close` happens must neither be refused nor
/// revive the pool: it finishes on its connection, and that connection is
/// dropped on return. Deterministic: the read parks on a channel until the
/// test has closed the pool, so there is no sleep and no timing window.
#[tokio::test]
async fn a_read_in_flight_across_close_is_not_repooled_and_new_reads_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, _, _) = store_with_ledger_residue(&tmp).await;
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let reader = store.reader.clone();
    let held = tokio::spawn(async move {
        reader
            .with_conn(move |conn| {
                // Touch the database: a read-only connection that never read
                // has not joined the WAL, and so could not keep the file alive.
                conn.query_row("SELECT count(*) FROM pages", [], |r| r.get::<_, i64>(0))?;
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
    });
    // The connection is checked out and held.
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();

    store.reader.close();

    let refused = store.reader.users_exist().await.unwrap_err();
    assert!(
        matches!(refused, StoreError::ReaderClosed),
        "a read after close opened a connection: {refused:?}"
    );
    release_tx.send(()).unwrap();
    held.await
        .unwrap()
        .expect("the read already in flight finishes");

    // With the writer closed too, no connection may be left: SQLite deletes
    // the `-wal` only when the last one closes, so a returned connection that
    // went back into the pool would keep it.
    store.writer.checkpoint_and_close().await;
    assert!(
        !wal_path(&store).exists(),
        "the connection returned after close was pooled again"
    );
}

/// The pragma lives on the writer's own connection and nothing else sets it,
/// so only a read of that connection can notice it going missing. The fixtures
/// above are far below the limit and could not.
#[tokio::test]
async fn the_writer_connection_caps_the_wal_it_keeps_after_a_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    assert_eq!(
        store.writer.journal_size_limit().await.unwrap(),
        64 * 1024 * 1024
    );
}
