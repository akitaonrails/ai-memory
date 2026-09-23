//! B2/B3/B4 — the opt-in LLM "dream" pass, end to end through a real
//! Store + Wiki + embeddings, with a FAKE `LlmProvider` (zero live calls).
//!
//! Invariants guarded (docs/design-memory-aging.md §B2–B4):
//!
//! B2:
//! 1. **Flag OFF / no provider = no-op.** No clustering, no merges, no writes.
//! 2. **Flag ON = LLM merge.** A cold cluster of near-duplicates collapses to one
//!    LLM-rewritten survivor; every merged-away member is superseded and stays
//!    reachable (invariant #16, never a hard delete); `page_evidence` records the
//!    `b2_dream:` sources (the hallucinated-merge guard).
//! 3. **`dry_run` returns a plan and writes nothing** — and never calls the LLM.
//! 4. **JSON-schema structured output only** (invariant #7): the fake asserts it
//!    was handed the `DreamMergedPage` schema.
//!
//! B3:
//! 5. **Default OFF ⇒ never runs.**
//! 6. **Cancel-on-activity** stops the pass at a cluster boundary: the in-flight
//!    cluster completes, every later cluster is left untouched.
//!
//! B4 surprisal ordering is unit-tested in `src/dream.rs` (pure function).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ai_memory_consolidate::{DreamCancel, DreamConfig, EmbeddingCoord, run_dream_pass};
use ai_memory_core::{ActorContext, PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{
    ChatRequest, ChatResponse, Embedder, LlmProvider, LlmResult, SyntheticEmbedder,
};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const DAY_US: i64 = 86_400_000_000;
const COLD_DAYS: i64 = 400;
const DIM: u32 = 64;

/// Fake provider: returns a merged page whose body echoes the whole prompt (so
/// the survivor demonstrably keeps every source fact). Records that it was
/// handed a schema (invariant #7), counts calls, and — if given a cancel handle
/// — flips it on its FIRST call to simulate the operator returning mid-pass.
struct FakeMergeLlm {
    calls: Arc<AtomicUsize>,
    saw_schema_field: Arc<AtomicUsize>,
    cancel_after_first: Option<DreamCancel>,
}

impl FakeMergeLlm {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_schema_field: Arc::new(AtomicUsize::new(0)),
            cancel_after_first: None,
        }
    }
}

impl LlmProvider for FakeMergeLlm {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn model(&self) -> &str {
        "dream-merge"
    }
    fn complete<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatRequest,
    ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(ChatResponse {
                text: "unused".into(),
                usage: None,
                model: "dream-merge".into(),
            })
        })
    }
    fn complete_structured_raw<'life0, 'async_trait>(
        &'life0 self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let calls = self.calls.clone();
        let saw_schema = self.saw_schema_field.clone();
        let cancel = self.cancel_after_first.clone();
        Box::pin(async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            // Invariant #7: the merge must be a JSON-schema structured call, and
            // the schema must be the DreamMergedPage contract.
            if serde_json::to_string(&schema)
                .unwrap_or_default()
                .contains("body_markdown")
            {
                saw_schema.fetch_add(1, Ordering::SeqCst);
            }
            // B3 cancel-on-activity: after the first cluster is merged, the
            // operator "returns" — the pass must stop before the next cluster.
            if n == 0
                && let Some(cancel) = cancel
            {
                cancel.cancel();
            }
            let prompt = request
                .messages
                .first()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            Ok(serde_json::json!({
                "title": "Dream Merge",
                "body_markdown": format!("Dream-merged coherent page.\n\n{prompt}"),
            }))
        })
    }
}

struct Fixture {
    tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
}

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

/// Total superseded (`is_latest = 0`) page-version rows for the project — the
/// blast radius of any dream write.
fn superseded_row_count(fx: &Fixture) -> i64 {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 0",
        params![fx.ws.as_bytes(), fx.proj.as_bytes()],
        |row| row.get(0),
    )
    .unwrap()
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

fn dream_on() -> DreamConfig {
    DreamConfig {
        enabled: true,
        embedding: Some(EmbeddingCoord {
            provider: "synthetic".into(),
            model: "bag-of-words-v1".into(),
            dim: DIM,
        }),
        min_pts: 0,
        max_eps: 0.0,
        max_clusters_per_run: 0,
        min_cold_pages: 0,
        idle_window_secs: 0,
    }
}

// Near-duplicate prose (so the bag-of-words embedder places them close), each
// carrying ONE unique durable fact (a distinct file path).
const DUP_A: &str = "Investigated the flaky retention sweep test failure again and again during \
     the long debugging session that afternoon. The unique culprit turned out to be in the file \
     crates/ai-memory-store/src/ops.rs during the run.";
const DUP_B: &str = "Investigated the flaky retention sweep test failure again and again during \
     the long debugging session that afternoon. The unique culprit turned out to be in the file \
     crates/ai-memory-store/src/reader.rs during the run.";
// A second, far-away near-duplicate pair (a distinct topic).
const K8S_A: &str = "Kubernetes pod scheduling node selector affinity rules for the cluster \
     autoscaler deployment on the staging namespace were tuned for the unique setting max-nodes-42.";
const K8S_B: &str = "Kubernetes pod scheduling node selector affinity rules for the cluster \
     autoscaler deployment on the staging namespace were tuned for the unique setting max-nodes-99.";

/// B2: flag ON — the near-duplicates collapse to one LLM-rewritten survivor; the
/// survivor keeps every source fact; the loser is superseded (reachable); the
/// merge is recorded in `page_evidence`; the LLM was called with the schema.
#[tokio::test]
async fn flag_on_merges_cluster_via_llm() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    // A third, far-away cold page: the adaptive k-distance elbow needs more than
    // `min_pts` points to form (exactly as A3 does). It is noise, not a cluster.
    write(&fx, "sessions/far.md", K8S_A).await;
    for p in ["sessions/a.md", "sessions/b.md", "sessions/far.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    let llm = FakeMergeLlm::new();
    let calls = llm.calls.clone();
    let saw_schema = llm.saw_schema_field.clone();
    let report = run_dream_pass(
        &fx.store.reader,
        &fx.wiki,
        Some(&llm),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        &dream_on(),
        &DreamCancel::new(),
        false,
    )
    .await
    .unwrap();

    assert_eq!(report.clusters_considered, 1, "one cluster: {report:?}");
    assert_eq!(report.clusters_merged, 1, "the merge applied: {report:?}");
    assert_eq!(report.pages_rewritten, 1);
    assert_eq!(report.pages_superseded, 1);
    assert_eq!(report.skipped, 0);
    assert!(!report.cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "LLM called exactly once");
    assert_eq!(
        saw_schema.load(Ordering::SeqCst),
        1,
        "invariant #7: the merge used the DreamMergedPage JSON schema"
    );

    let merge = &report.merges[0];
    let survivor_path = merge.survivor.clone();
    let loser_path = merge.merged[0].clone();
    assert_ne!(survivor_path, loser_path);

    // Recall preserved: the survivor body carries BOTH facts.
    let survivor_body = latest_body(&fx, &survivor_path);
    assert!(
        survivor_body.contains("crates/ai-memory-store/src/ops.rs")
            && survivor_body.contains("crates/ai-memory-store/src/reader.rs"),
        "survivor keeps every source fact: {survivor_body}"
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
    let new_survivor = merge.new_survivor_id.expect("new survivor id");
    let evidence = evidence_for(&fx, new_survivor);
    assert!(
        evidence
            .iter()
            .any(|(kind, src)| kind == "reconsolidation" && src.starts_with("b2_dream:")),
        "page_evidence records the merge sources: {evidence:?}"
    );
}

/// B2: a dry run returns the plan, writes nothing, and never calls the LLM.
#[tokio::test]
async fn dry_run_returns_plan_and_writes_nothing() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    write(&fx, "sessions/far.md", K8S_A).await;
    for p in ["sessions/a.md", "sessions/b.md", "sessions/far.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }
    let before = superseded_row_count(&fx);

    let llm = FakeMergeLlm::new();
    let calls = llm.calls.clone();
    let report = run_dream_pass(
        &fx.store.reader,
        &fx.wiki,
        Some(&llm),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        &dream_on(),
        &DreamCancel::new(),
        true, // dry_run
    )
    .await
    .unwrap();

    assert!(report.dry_run);
    assert_eq!(report.clusters_considered, 1);
    assert_eq!(report.clusters_merged, 0, "nothing applied on a dry run");
    assert_eq!(report.merges.len(), 1, "the plan is still returned");
    assert!(!report.merges[0].applied);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no LLM call on a dry run");
    assert_eq!(
        superseded_row_count(&fx),
        before,
        "a dry run writes nothing"
    );
    // Both cold pages are untouched (still their original bodies).
    assert!(latest_body(&fx, "sessions/a.md").contains("unique culprit"));
    assert!(latest_body(&fx, "sessions/b.md").contains("unique culprit"));
}

/// B2: no provider ⇒ a clean no-op (never an error, never a write). The zero-LLM
/// path (A3) owns a provider-less store (invariants #13, #16).
#[tokio::test]
async fn no_provider_is_a_clean_no_op() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    for p in ["sessions/a.md", "sessions/b.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }
    let before = superseded_row_count(&fx);

    let report = run_dream_pass(
        &fx.store.reader,
        &fx.wiki,
        None, // no provider
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        &dream_on(),
        &DreamCancel::new(),
        false,
    )
    .await
    .unwrap();

    assert!(report.disabled, "no provider ⇒ disabled no-op");
    assert_eq!(report.clusters_merged, 0);
    assert_eq!(superseded_row_count(&fx), before, "no writes");
}

/// B3: the default config is OFF, so the pass never runs even with a provider.
#[tokio::test]
async fn default_off_never_runs() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", DUP_A).await;
    write(&fx, "sessions/b.md", DUP_B).await;
    for p in ["sessions/a.md", "sessions/b.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }
    let before = superseded_row_count(&fx);

    let llm = FakeMergeLlm::new();
    let calls = llm.calls.clone();
    let report = run_dream_pass(
        &fx.store.reader,
        &fx.wiki,
        Some(&llm),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        &DreamConfig::default(), // OFF
        &DreamCancel::new(),
        false,
    )
    .await
    .unwrap();

    assert!(report.disabled, "default config is OFF");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no LLM call when OFF");
    assert_eq!(superseded_row_count(&fx), before, "no writes when OFF");
}

/// B3: cancel-on-activity. Two independent cold clusters; the operator "returns"
/// after the first is merged. The pass must stop at the cluster boundary — the
/// second cluster is left entirely untouched.
#[tokio::test]
async fn cancel_stops_before_next_cluster() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a1.md", DUP_A).await;
    write(&fx, "sessions/a2.md", DUP_B).await;
    write(&fx, "sessions/b1.md", K8S_A).await;
    write(&fx, "sessions/b2.md", K8S_B).await;
    for p in [
        "sessions/a1.md",
        "sessions/a2.md",
        "sessions/b1.md",
        "sessions/b2.md",
    ] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    let cancel = DreamCancel::new();
    let mut llm = FakeMergeLlm::new();
    llm.cancel_after_first = Some(cancel.clone());
    let calls = llm.calls.clone();

    let report = run_dream_pass(
        &fx.store.reader,
        &fx.wiki,
        Some(&llm),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        &dream_on(),
        &cancel,
        false,
    )
    .await
    .unwrap();

    assert_eq!(report.clusters_considered, 2, "two clusters found");
    assert_eq!(
        report.clusters_merged, 1,
        "exactly the in-flight cluster merged before the stop: {report:?}"
    );
    assert!(report.cancelled, "the pass reports it stopped early");
    assert_eq!(report.skipped, 0);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the LLM was called once only"
    );
    // Blast radius: exactly one cluster was written — its survivor rewrite and
    // its one loser stub each supersede a prior version (2 rows). Had the second
    // cluster also merged, there would be 4. The second cluster is untouched.
    assert_eq!(
        superseded_row_count(&fx),
        2,
        "only the first cluster was written (survivor + 1 loser); the rest is untouched"
    );
}
