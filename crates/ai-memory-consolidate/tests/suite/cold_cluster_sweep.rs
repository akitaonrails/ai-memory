//! A3 cold-cluster dedup, end to end through a real Store + Wiki + embeddings.
//!
//! Invariants guarded (docs/design-memory-aging.md §A3):
//!
//! 1. **Flag OFF = today's behaviour.** No clustering, no merges.
//! 2. **Flag ON = collapse.** Near-duplicate cold episodic pages collapse to one
//!    survivor; the other members are superseded with a merge note and stay
//!    reachable (invariant #16, never a hard delete).
//! 3. **Survivor keeps the union of keep-tokens** — a fact that lived only in a
//!    merged-away duplicate survives in the survivor (recall preserved).
//! 4. **Merge provenance** is recorded in `page_evidence`.
//! 5. **No embeddings ⇒ clean no-op** (never an error, never a provider call).

use std::sync::Arc;

use ai_memory_consolidate::{
    ColdClusterDedup, EmbeddingCoord, ObservationRetention, run_sweep_with_hygiene,
};
use ai_memory_core::{ActorContext, PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{Embedder, SyntheticEmbedder};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const DAY_US: i64 = 86_400_000_000;
const COLD_DAYS: i64 = 400;
const DIM: u32 = 64;

struct Fixture {
    tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
}

/// Build a fixture. `with_embedder` controls whether pages get embedding rows —
/// the no-embeddings no-op test wants a wiki without one.
async fn seed_fixture(with_embedder: bool) -> Fixture {
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
    let mut wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    if with_embedder {
        let embedder: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(DIM));
        wiki = wiki.with_embedder(embedder);
    }
    Fixture {
        tmp,
        store,
        wiki,
        ws,
        proj,
    }
}

async fn write(fx: &Fixture, path: &str, body: &str) -> PageId {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": path}),
            body: body.to_string(),
            tier: Tier::Episodic,
            pinned: false,
            title: Some(path.to_string()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap()
}

fn backdate_latest(fx: &Fixture, path: &str, days: i64) {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    let updated_at = jiff::Timestamp::now().as_microsecond() - days * DAY_US;
    conn.execute(
        "UPDATE pages SET created_at = ?1, updated_at = ?1 \
         WHERE workspace_id = ?2 AND project_id = ?3 AND path = ?4 AND is_latest = 1",
        params![updated_at, fx.ws.as_bytes(), fx.proj.as_bytes(), path],
    )
    .unwrap();
}

fn latest_body(fx: &Fixture, path: &str) -> String {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.query_row(
        "SELECT body FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
        params![fx.ws.as_bytes(), fx.proj.as_bytes(), path],
        |row| row.get(0),
    )
    .unwrap()
}

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

fn evidence_for(fx: &Fixture, page_id: PageId) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    let mut stmt = conn
        .prepare("SELECT source_kind, source_id FROM page_evidence WHERE page_id = ?1")
        .unwrap();
    let rows = stmt
        .query_map(params![page_id.as_bytes()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn dedup_on() -> ColdClusterDedup {
    ColdClusterDedup {
        enabled: true,
        embedding: Some(EmbeddingCoord {
            provider: "synthetic".into(),
            model: "bag-of-words-v1".into(),
            dim: DIM,
        }),
        min_pts: 0,
        max_eps: 0.0,
    }
}

// Two near-duplicate bodies: almost-identical prose (so the bag-of-words
// embedder places them close together) but each carrying ONE unique durable
// fact (a distinct file path), so the survivor's keep-token union is testable.
const DUP_A: &str = "Investigated the flaky retention sweep test failure again and again during \
     the long debugging session that afternoon. The unique culprit turned out to be in the file \
     crates/ai-memory-store/src/ops.rs during the run.";
const DUP_B: &str = "Investigated the flaky retention sweep test failure again and again during \
     the long debugging session that afternoon. The unique culprit turned out to be in the file \
     crates/ai-memory-store/src/reader.rs during the run.";
const FAR: &str = "Kubernetes pod scheduling node selector affinity rules for the cluster \
     autoscaler deployment on the staging namespace.";

/// Flag OFF: nothing clusters or merges — byte-identical to before A3.
#[tokio::test]
async fn flag_off_never_merges() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    for p in ["sessions/a.md", "sessions/b.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    let report = run_sweep_with_hygiene(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
        ColdClusterDedup::default(), // OFF
        false,
    )
    .await
    .unwrap();

    assert!(report.merged.is_empty(), "flag off must not merge");
    // With dedup off and compaction off, the cold pages are evicted as always.
    assert_eq!(report.evicted.len(), 2, "both cold pages evicted as before");
}

/// Flag ON: the two near-duplicates collapse to one survivor; the survivor keeps
/// BOTH facts; the loser is superseded (reachable); the merge is recorded in
/// `page_evidence`; the far page is untouched.
#[tokio::test]
async fn flag_on_collapses_cluster_and_preserves_recall() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    write(&fx, "sessions/far.md", FAR).await;
    for p in ["sessions/a.md", "sessions/b.md", "sessions/far.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    let report = run_sweep_with_hygiene(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
        dedup_on(),
        false,
    )
    .await
    .unwrap();

    assert_eq!(
        report.merged.len(),
        1,
        "the two near-duplicates form exactly one cluster: {:?}",
        report.merged
    );
    let cluster = &report.merged[0];
    assert!(cluster.applied, "the collapse landed");
    assert_eq!(cluster.merged.len(), 1, "one member merged away");
    let survivor_path = cluster.survivor.clone();
    let loser_path = cluster.merged[0].clone();
    assert!(
        survivor_path != loser_path,
        "survivor and loser are distinct"
    );
    assert!(
        survivor_path.starts_with("sessions/") && loser_path.starts_with("sessions/"),
        "far page is not part of the cluster"
    );
    assert!(
        !report.merged.iter().any(|m| m.survivor == "sessions/far.md"
            || m.merged.contains(&"sessions/far.md".to_string())),
        "the far page must never be clustered"
    );

    // Recall preserved: the survivor body carries BOTH facts — its own and the
    // one that lived only in the merged-away duplicate.
    let survivor_body = latest_body(&fx, &survivor_path);
    assert!(
        survivor_body.contains("crates/ai-memory-store/src/ops.rs"),
        "ops.rs fact present in survivor: {survivor_body}"
    );
    assert!(
        survivor_body.contains("crates/ai-memory-store/src/reader.rs"),
        "reader.rs fact (from the merged-away duplicate) survives in survivor: {survivor_body}"
    );

    // The loser's live version is a merge note pointing at the survivor…
    let loser_body = latest_body(&fx, &loser_path);
    assert!(
        loser_body.contains("merged into") && loser_body.contains(&survivor_path),
        "loser points at survivor: {loser_body}"
    );
    // …and its full pre-merge body stays reachable via supersession (reversible).
    let loser_history = superseded_bodies(&fx, &loser_path);
    assert!(
        loser_history.iter().any(|b| b.contains("unique culprit")),
        "the loser's pre-merge body stays reachable: {loser_history:?}"
    );

    // Merge provenance recorded on the new survivor version.
    let new_survivor = cluster.new_survivor_id.expect("new survivor id");
    let evidence = evidence_for(&fx, new_survivor);
    assert!(
        evidence
            .iter()
            .any(|(kind, src)| kind == "reconsolidation" && src.starts_with("a3_merge:")),
        "page_evidence records the merge: {evidence:?}"
    );

    // The far page was neither merged nor (being cold) skipped: it evicts.
    assert!(
        report.evicted.iter().any(|e| e.path == "sessions/far.md"),
        "the far page follows the normal cold path: {:?}",
        report.evicted
    );
}

/// No embeddings for the configured triple ⇒ A3 is a clean no-op, not an error.
#[tokio::test]
async fn no_embeddings_is_a_clean_no_op() {
    let fx = seed_fixture(false).await; // no embedder → no embedding rows
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    for p in ["sessions/a.md", "sessions/b.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    let report = run_sweep_with_hygiene(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
        dedup_on(), // ON, but there are no vectors to cluster
        false,
    )
    .await
    .unwrap();

    assert!(
        report.merged.is_empty(),
        "no embeddings ⇒ no merges, no error"
    );
    assert_eq!(report.evicted.len(), 2, "cold pages follow the normal path");
}
