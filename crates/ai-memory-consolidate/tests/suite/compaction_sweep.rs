//! A2 extractive tier-down, end to end through a real Store + Wiki.
//!
//! The invariants this file guards (docs/design-memory-aging.md §A2):
//!
//! 1. **Flag OFF = today's behaviour.** A cold episodic page is EVICTED
//!    (tombstoned), not compacted — an upgrade surprises no one.
//! 2. **Flag ON = compaction.** The same page is tiered down: the new latest
//!    version keeps its L0 abstract, an L1 summary and the L2 keep-tokens, drops
//!    the prose, sets the V65 `compacted_at` marker and the `compacted: true`
//!    frontmatter mirror, and is reported under `SweepReport.compacted`, not
//!    `evicted`.
//! 3. **Keep-tokens survive.** A file path + error code + URL seeded in the body
//!    are present in the compacted body.
//! 4. **Reversible.** The pre-compaction full body stays reachable via the
//!    supersession chain and is recoverable with `restore_page_from_checkpoint`.
//! 5. **No re-compaction.** A second sweep does not re-compact an
//!    already-compacted page, and the curator does not re-report it as cold.
//! 6. **Only episodic.** Pinned / semantic / procedural pages never compact.

use ai_memory_consolidate::ObservationRetention;
use ai_memory_consolidate::{
    CuratorParams, run_curator_report, run_sweep_with_compaction, run_sweep_with_options,
};
use ai_memory_core::{ActorContext, PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const DAY_US: i64 = 86_400_000_000;

struct Fixture {
    tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
}

async fn seed_fixture() -> Fixture {
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
    // The reader is required by the sweep's `evict_page_if_latest` guard; the
    // compaction path also reads the page body back through the wiki.
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    Fixture {
        tmp,
        store,
        wiki,
        ws,
        proj,
    }
}

async fn write(
    fx: &Fixture,
    path: &str,
    title: &str,
    tier: Tier,
    pinned: bool,
    body: &str,
) -> PageId {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": title}),
            body: body.to_string(),
            tier,
            pinned,
            title: Some(title.to_string()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap()
}

/// Push the current `is_latest` version of `path` back in time so it scores as
/// cold. Mirrors the aux-connection trick the other sweep/curator tests use.
fn backdate_latest(fx: &Fixture, path: &str, days: i64) {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    let updated_at = jiff::Timestamp::now().as_microsecond() - days * DAY_US;
    conn.execute(
        "UPDATE pages SET created_at = ?1, updated_at = ?1 \
         WHERE workspace_id = ?2 AND project_id = ?3 AND path = ?4 AND is_latest = 1",
        params![updated_at, fx.ws.as_bytes(), fx.proj.as_bytes(), path,],
    )
    .unwrap();
}

/// The `compacted_at` marker on the current latest version of `path`, and its
/// body, straight from SQLite.
fn latest_marker_and_body(fx: &Fixture, path: &str) -> (Option<i64>, String) {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.query_row(
        "SELECT compacted_at, body FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
        params![fx.ws.as_bytes(), fx.proj.as_bytes(), path],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

/// Every historical (superseded) body for `path`, oldest first — the
/// supersession chain the reversibility guarantee rests on.
fn superseded_bodies(fx: &Fixture, path: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT body FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 0 \
             ORDER BY created_at",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![fx.ws.as_bytes(), fx.proj.as_bytes(), path], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

const COLD_DAYS: i64 = 400;

/// Test 1: with the flag OFF the sweep evicts a cold episodic page exactly as it
/// always has — no compaction, no upgrade surprise.
#[tokio::test]
async fn flag_off_evicts_cold_episodic_page_unchanged() {
    let fx = seed_fixture().await;
    let id = write(
        &fx,
        "sessions/old.md",
        "Old Session",
        Tier::Episodic,
        false,
        "a long prose body about a debugging session that nobody reopened",
    )
    .await;
    backdate_latest(&fx, "sessions/old.md", COLD_DAYS);

    let report = run_sweep_with_options(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
    )
    .await
    .unwrap();

    assert_eq!(
        report
            .evicted
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec!["sessions/old.md"],
        "flag off must evict the cold page"
    );
    assert!(report.evicted[0].deleted, "eviction must have landed");
    assert!(
        report.compacted.is_empty(),
        "nothing is compacted with the flag off"
    );
    // The evicted page is a decay tombstone (not is_latest) — same as before A2.
    let _ = id;
    assert!(
        fx.store
            .reader
            .decay_candidates(fx.ws, fx.proj)
            .await
            .unwrap()
            .is_empty(),
        "the evicted page is gone from the live set"
    );
}

/// Test 2 + 3: with the flag ON the same page is compacted, not evicted; the
/// marker and frontmatter mirror are set; keep-tokens survive and prose drops.
#[tokio::test]
async fn flag_on_compacts_cold_episodic_and_keeps_durable_facts() {
    let fx = seed_fixture().await;
    // First paragraph → L1 summary; the second paragraph is droppable prose,
    // but its file path / error code / URL are mined as keep-tokens from the
    // full body and survive.
    let body = "Fixed the flaky sweep test after an afternoon of chasing it.\n\n\
         The root cause lived in crates/ai-memory-store/src/ops.rs and surfaced as \
         E0433 during the build; the server also returned HTTP 500. Details at \
         https://github.com/akitaonrails/ai-memory/issues/776 which everyone agreed \
         was worth writing down in careful, unnecessary, easily-forgotten prose.";
    write(
        &fx,
        "sessions/fix.md",
        "The Fix",
        Tier::Episodic,
        false,
        body,
    )
    .await;
    backdate_latest(&fx, "sessions/fix.md", COLD_DAYS);

    let report = run_sweep_with_compaction(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        true,
        false,
    )
    .await
    .unwrap();

    assert!(
        report.evicted.is_empty(),
        "the flag redirects the cold page from eviction to compaction: {:?}",
        report.evicted
    );
    assert_eq!(
        report
            .compacted
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        vec!["sessions/fix.md"],
        "the cold page is reported as compacted"
    );
    assert!(
        report.compacted[0].compacted,
        "the rewrite must have landed"
    );
    assert!(
        report.compacted[0].new_id.is_some(),
        "new version id reported"
    );

    let (marker, compacted_body) = latest_marker_and_body(&fx, "sessions/fix.md");
    assert!(marker.is_some(), "V65 compacted_at marker is set");

    // Keep-tokens (L2) survive.
    assert!(
        compacted_body.contains("crates/ai-memory-store/src/ops.rs"),
        "file path survives: {compacted_body}"
    );
    assert!(compacted_body.contains("E0433"), "error code survives");
    assert!(
        compacted_body.contains("github.com/akitaonrails/ai-memory"),
        "URL survives"
    );
    // Prose (the low-signal tail) is dropped.
    assert!(
        !compacted_body.contains("easily-forgotten prose"),
        "prose body dropped: {compacted_body}"
    );

    // Frontmatter mirror on disk.
    let md = fx
        .wiki
        .read_page(fx.ws, fx.proj, &PagePath::new("sessions/fix.md").unwrap())
        .unwrap();
    assert_eq!(
        md.frontmatter.get("compacted").and_then(|v| v.as_bool()),
        Some(true),
        "frontmatter carries the compacted: true mirror"
    );
}

/// Test 4: the pre-compaction full body stays reachable via the supersession
/// chain and is recoverable through `restore_page_from_checkpoint`.
#[tokio::test]
async fn compaction_is_reversible() {
    let fx = seed_fixture().await;
    let original_body = "The original full prose body with a keeper path lib.rs and \
         plenty of easily-forgotten narrative that compaction is meant to drop.";
    write(
        &fx,
        "sessions/reversible.md",
        "Reversible",
        Tier::Episodic,
        false,
        original_body,
    )
    .await;
    // Snapshot the original in git before compaction rewrites the working file.
    let rev = fx.wiki.commit_all("seed").unwrap().unwrap().to_string();
    backdate_latest(&fx, "sessions/reversible.md", COLD_DAYS);

    run_sweep_with_compaction(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        true,
        false,
    )
    .await
    .unwrap();

    // (a) The prior full body is still in the store's supersession chain.
    let prior = superseded_bodies(&fx, "sessions/reversible.md");
    assert!(
        prior
            .iter()
            .any(|b| b.contains("easily-forgotten narrative")),
        "the pre-compaction body stays reachable via supersession: {prior:?}"
    );

    // (b) And it is recoverable through the git checkpoint restore path.
    fx.wiki
        .restore_page_from_checkpoint(
            fx.ws,
            fx.proj,
            PagePath::new("sessions/reversible.md").unwrap(),
            &rev,
        )
        .await
        .unwrap();
    let (_marker, restored) = latest_marker_and_body(&fx, "sessions/reversible.md");
    assert!(
        restored.contains("easily-forgotten narrative"),
        "restore-page brings the original body back: {restored}"
    );
}

/// Test 5: a compacted page is terminal for the decay pass — a second sweep does
/// not re-compact it, and the curator does not re-report it as cold.
#[tokio::test]
async fn no_recompaction_and_curator_skips_compacted_pages() {
    let fx = seed_fixture().await;
    write(
        &fx,
        "sessions/twice.md",
        "Twice",
        Tier::Episodic,
        false,
        "a body mentioning ops.rs and E0001 among some throwaway prose",
    )
    .await;
    backdate_latest(&fx, "sessions/twice.md", COLD_DAYS);

    let first = run_sweep_with_compaction(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        true,
        false,
    )
    .await
    .unwrap();
    assert_eq!(first.compacted.len(), 1, "first sweep compacts once");

    // Age the compacted version so it would be cold again if the marker did not
    // block it.
    backdate_latest(&fx, "sessions/twice.md", COLD_DAYS);
    let second = run_sweep_with_compaction(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        true,
        false,
    )
    .await
    .unwrap();
    assert!(
        second.compacted.is_empty(),
        "the marker blocks re-compaction: {:?}",
        second.compacted
    );
    assert!(
        second.evicted.is_empty(),
        "a compacted page is terminal: it is neither re-compacted nor evicted"
    );

    // The curator (which predicts what the sweep would evict) must not report a
    // compacted page as cold.
    let report = run_curator_report(
        &fx.store.reader,
        fx.ws,
        fx.proj,
        "default",
        "scratch",
        CuratorParams::default(),
    )
    .await
    .unwrap();
    assert!(
        report.findings.iter().all(|f| f.kind != "cold_episodic"),
        "curator must skip compacted pages: {:?}",
        report.findings
    );
}

/// Test 6: only episodic pages compact. Pinned episodic, semantic and
/// procedural pages are never touched, even when cold, with the flag ON.
#[tokio::test]
async fn only_unpinned_episodic_pages_compact() {
    let fx = seed_fixture().await;
    write(
        &fx,
        "sessions/pinned.md",
        "Pinned",
        Tier::Episodic,
        true,
        "path a.rs prose",
    )
    .await;
    write(
        &fx,
        "concepts/sem.md",
        "Semantic",
        Tier::Semantic,
        false,
        "path b.rs prose",
    )
    .await;
    write(
        &fx,
        "runbooks/proc.md",
        "Procedural",
        Tier::Procedural,
        false,
        "path c.rs prose",
    )
    .await;
    for path in ["sessions/pinned.md", "concepts/sem.md", "runbooks/proc.md"] {
        backdate_latest(&fx, path, COLD_DAYS);
    }

    let report = run_sweep_with_compaction(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        true,
        false,
    )
    .await
    .unwrap();

    assert!(
        report.compacted.is_empty(),
        "pinned/semantic/procedural pages must never compact: {:?}",
        report.compacted
    );
    for path in ["sessions/pinned.md", "concepts/sem.md", "runbooks/proc.md"] {
        let (marker, _body) = latest_marker_and_body(&fx, path);
        assert!(marker.is_none(), "{path} must not be marked compacted");
    }
}
