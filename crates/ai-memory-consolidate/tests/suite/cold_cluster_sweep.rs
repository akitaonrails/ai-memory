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

/// `write`, plus a frontmatter `summary:` and `abstract:` — the shape a page
/// has when something upstream (or an operator) gave it a descriptor.
async fn write_with_summary(fx: &Fixture, path: &str, body: &str, summary: &str) -> PageId {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({
                "title": path,
                "summary": summary,
                "abstract": summary,
            }),
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

fn frontmatter_map(raw: &str) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::from_str(raw).unwrap() {
        serde_json::Value::Object(m) => m,
        other => panic!("frontmatter_json is not an object: {other}"),
    }
}

fn latest_frontmatter(fx: &Fixture, path: &str) -> serde_json::Map<String, serde_json::Value> {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    let raw: String = conn
        .query_row(
            "SELECT frontmatter_json FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![fx.ws.as_bytes(), fx.proj.as_bytes(), path],
            |row| row.get(0),
        )
        .unwrap();
    frontmatter_map(&raw)
}

fn superseded_frontmatter(
    fx: &Fixture,
    path: &str,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT frontmatter_json FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 0 \
             ORDER BY created_at",
        )
        .unwrap();
    let rows = stmt
        .query_map(params![fx.ws.as_bytes(), fx.proj.as_bytes(), path], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
    rows.map(|r| frontmatter_map(&r.unwrap())).collect()
}

/// What a non-FTS retrieval path says about a page: the recency stream's
/// descriptor, built by `page_descriptor_expr` like every other such stream.
async fn project_descriptor(fx: &Fixture, path: &str) -> String {
    fx.store
        .reader
        .recent_pages_for_project(fx.ws, fx.proj, 20)
        .await
        .unwrap()
        .into_iter()
        .find(|hit| hit.path.as_str() == path)
        .unwrap_or_else(|| panic!("{path} is not in the recency stream"))
        .snippet
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

async fn concurrent_member_write_skips_dedup_batch(edit_survivor: bool) {
    use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig};
    use std::io::{Read, Write};

    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    for path in ["sessions/a.md", "sessions/b.md"] {
        backdate_latest(&fx, path, COLD_DAYS);
    }
    let params = DecayParams::default();
    let plan = run_sweep_with_hygiene(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &params,
        0.0,
        ObservationRetention::default(),
        false,
        dedup_on(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(plan.merged.len(), 1);
    let cluster = &plan.merged[0];
    let (edited_path, peer_path) = if edit_survivor {
        (&cluster.survivor, &cluster.merged[0])
    } else {
        (&cluster.merged[0], &cluster.survivor)
    };
    let peer_body = latest_body(&fx, peer_path);
    let peer_id = fx
        .store
        .reader
        .latest_page_id_by_ids(fx.ws, fx.proj, peer_path.clone())
        .await
        .unwrap();

    // Both replacements have been derived when the first admission request
    // arrives. Land a real Wiki edit before the batch obtains any page lock.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut first = Some(started_tx);
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0_u8; 4096];
            let header_end = loop {
                if let Some(pos) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    break pos + 4;
                }
                let read = stream.read(&mut buf).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buf[..read]);
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            while bytes.len() < header_end + length {
                let read = stream.read(&mut buf).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&buf[..read]);
            }
            if let Some(started) = first.take() {
                started.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .unwrap();
        }
    });
    let sweep_wiki = fx.wiki.clone().with_admission_chain(
        AdmissionChain::new(vec![WebhookConfig {
            name: "pause-stale-dedup".into(),
            url,
            timeout_ms: 15_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::WritePage],
            blocking: true,
            projects: Vec::new(),
            headers: Vec::new(),
        }])
        .unwrap(),
    );
    let sweep = run_sweep_with_hygiene(
        &fx.store.reader,
        &fx.store.writer,
        Some(&sweep_wiki),
        fx.ws,
        fx.proj,
        &params,
        0.0,
        ObservationRetention::default(),
        false,
        dedup_on(),
        false,
    );
    let fresh_body = "Fresh human correction: this page is current, semantic and pinned.";
    let update = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), started_rx)
            .await
            .unwrap()
            .unwrap();
        let fresh_id = fx
            .wiki
            .write_page(WritePageRequest {
                workspace_id: fx.ws,
                project_id: fx.proj,
                path: PagePath::new(edited_path.clone()).unwrap(),
                frontmatter: serde_json::json!({"title": "Fresh correction"}),
                body: fresh_body.into(),
                tier: Tier::Semantic,
                pinned: true,
                title: Some("Fresh correction".into()),
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::anonymous(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
        release_tx.send(()).unwrap();
        fresh_id
    };
    let (report, fresh_id) = tokio::join!(sweep, update);
    server.join().unwrap();
    let report = report.unwrap();
    assert_eq!(report.merged.len(), 1);
    assert!(!report.merged[0].applied, "the stale batch must not land");
    assert_eq!(report.merged[0].new_survivor_id, None);
    assert!(
        report.evicted.is_empty(),
        "no fallback deletion of selected pages"
    );
    assert!(report.compacted.is_empty());

    for (path, expected_body, expected_id) in [
        (edited_path, fresh_body, Some(fresh_id)),
        (peer_path, peer_body.as_str(), peer_id),
    ] {
        assert_eq!(latest_body(&fx, path), expected_body);
        assert_eq!(
            fx.wiki
                .read_page(fx.ws, fx.proj, &PagePath::new(path.clone()).unwrap())
                .unwrap()
                .body,
            expected_body
        );
        assert_eq!(
            fx.store
                .reader
                .latest_page_id_by_ids(fx.ws, fx.proj, path.clone())
                .await
                .unwrap(),
            expected_id
        );
    }
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    let metadata: (String, bool) = conn
        .query_row(
            "SELECT tier, pinned FROM pages WHERE id = ?1",
            params![fresh_id.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(metadata, ("semantic".to_string(), true));
    assert!(superseded_bodies(&fx, peer_path).is_empty());
    assert_eq!(superseded_bodies(&fx, edited_path).len(), 1);
}

#[tokio::test]
async fn concurrent_survivor_write_skips_dedup_batch() {
    concurrent_member_write_skips_dedup_batch(true).await;
}

#[tokio::test]
async fn concurrent_loser_write_skips_dedup_batch() {
    concurrent_member_write_skips_dedup_batch(false).await;
}

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

/// The same pair with NO third page present. This is the shape the fixture
/// above hides: `adaptive_eps` used to return `None` for `points.len() <= k`,
/// so with the default `min_pts = 2` a cold set of exactly two near-duplicates
/// produced an empty plan, while adding any unrelated third page made the same
/// pair collapse.
#[tokio::test]
async fn flag_on_collapses_a_pair_with_no_other_cold_page() {
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
        dedup_on(),
        false,
    )
    .await
    .unwrap();

    assert_eq!(
        report.merged.len(),
        1,
        "a pair is the smallest collapsible set under min_pts = 2: {:?}",
        report.merged
    );
    let cluster = &report.merged[0];
    assert!(cluster.applied, "the collapse landed");
    assert_eq!(cluster.merged.len(), 1, "one member merged away");
    assert_ne!(
        cluster.survivor, cluster.merged[0],
        "survivor and loser are distinct"
    );
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

/// The summary a merged-away page carried described the body that moved to the
/// survivor. `page_descriptor_expr` prefers a non-blank frontmatter `summary:`
/// over the body on every non-FTS retrieval path, so leaving it on the stub
/// makes the brief and the recency / vector / entity / graph hit lists promise
/// content a reader will not find when they open the page.
#[tokio::test]
async fn merge_stub_drops_the_loser_summary_and_abstract() {
    let fx = seed_fixture(true).await;
    for path in ["sessions/a.md", "sessions/b.md"] {
        write_with_summary(
            &fx,
            path,
            if path.ends_with("a.md") { DUP_A } else { DUP_B },
            "Fixed the flaky sweep test by guarding the upsert in ops.rs.",
        )
        .await;
    }
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
    assert_eq!(report.merged.len(), 1, "one cluster: {:?}", report.merged);
    let cluster = &report.merged[0];
    assert!(cluster.applied, "the collapse landed");
    let loser_path = cluster.merged[0].clone();

    let fm = latest_frontmatter(&fx, &loser_path);
    assert!(
        !fm.contains_key("summary"),
        "the stub carries no summary of content that moved: {fm:?}"
    );
    assert!(
        !fm.contains_key("abstract"),
        "and no abstract, so it is not re-embedded into the abstract stream: {fm:?}"
    );
    // Control: the merge markers are still written, so the removal above did
    // not simply discard the stub's whole frontmatter.
    assert_eq!(fm.get("compacted"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(
        fm.get("merged_into").and_then(serde_json::Value::as_str),
        Some(cluster.survivor.as_str())
    );

    // Control: the pre-merge summary is still reachable through supersession,
    // so the test cannot pass because the summary was never written at all.
    let history = superseded_frontmatter(&fx, &loser_path);
    assert!(
        history.iter().any(|fm| fm
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| s.contains("guarding the upsert"))),
        "the pre-merge frontmatter stays in the chain: {history:?}"
    );

    // What retrieval now says about the stub: the merge note, not the fix.
    let descriptor = project_descriptor(&fx, &loser_path).await;
    assert!(
        descriptor.contains("merged into"),
        "the stub describes itself as a merge note: {descriptor}"
    );
    assert!(
        !descriptor.contains("guarding the upsert"),
        "and no longer promises the fix that moved to the survivor: {descriptor}"
    );
}
