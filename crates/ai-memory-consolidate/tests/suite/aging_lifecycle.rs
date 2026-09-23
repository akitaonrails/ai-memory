//! End-to-end 2.4 memory-aging lifecycle — the capstone use-case test.
//!
//! Where the sibling suites (`compaction_sweep`, `cold_cluster_sweep`,
//! `contradiction_lint`, `dream_pass`, `entropy_experience`, `access_breadth_sweep`)
//! each pin ONE aging feature in isolation, this file drives a realistic small
//! corpus through the WHOLE pipeline over *simulated* time and proves the pieces
//! documented in `docs/design-memory-aging.md` cooperate — and, just as
//! important, that every new knob is INERT until an operator turns it on.
//!
//! It reads as executable documentation of the intended aging behaviour:
//!
//! * A1 — per-tier decay curves (a short working half-life ages faster than a
//!   long episodic one).
//! * A2 — extractive tier-down of cold episodic pages (compaction) vs the
//!   historical eviction.
//! * A3 — cold-cluster dedup (near-duplicates collapse to one survivor).
//! * A5 — advisory contradiction lint (never destructive, invariant #16).
//! * C1 — access reinforcement (a re-read cold page survives the sweep).
//! * B2/B3/B4 — the opt-in LLM "dream" merge, driven by a FAKE provider.
//!
//! ## How simulated time is applied
//!
//! The aging passes compute a page's age as `now − updated_at` (and the access
//! term as `now − last_accessed_at`). There is no injectable clock: the
//! established pattern — shared by every suite above — is to **backdate** a
//! page's timestamps directly in SQLite so `now` lands the desired number of
//! days past them. `backdate_latest` does exactly that; the rest of the test is
//! deterministic (controlled/synthetic embeddings, a fake LLM, no wall-clock
//! sleeps, no live provider).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ai_memory_consolidate::{
    ColdClusterDedup, DreamCancel, DreamConfig, EmbeddingCoord, LintOptions, ObservationRetention,
    run_dream_pass, run_lint, run_sweep, run_sweep_with_compaction, run_sweep_with_hygiene,
};
use ai_memory_core::{ActorContext, PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{
    ChatRequest, ChatResponse, Embedder, LlmProvider, LlmResult, SyntheticEmbedder,
};
use ai_memory_store::{
    DecayParams, Store, TierLambdas, f32_vec_to_bytes, lambda_from_half_life_days, retention_score,
};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const DAY_US: i64 = 86_400_000_000;
/// Old enough that an unread episodic page decays well below the default
/// `cold_threshold` (0.20) under the scalar λ: `exp(−0.02·200) ≈ 0.018`.
const COLD_DAYS: i64 = 200;
/// Dimension of the synthetic bag-of-words embedder used for A3/dream clustering.
const DIM: u32 = 64;

// ---------------------------------------------------------------------------
// Fixture + helpers (the same DB-seam pattern the sibling suites use)
// ---------------------------------------------------------------------------

struct Fixture {
    tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
}

/// A real Store + Wiki over a temp dir. `with_embedder` decides whether written
/// pages get synthetic embedding rows — the clustering passes (A3, dream) need
/// them; the plain decay tests do not.
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

async fn write(fx: &Fixture, path: &str, tier: Tier, pinned: bool, body: &str) -> PageId {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": path}),
            body: body.to_string(),
            tier,
            pinned,
            title: Some(path.to_string()),
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap()
}

/// Advance simulated time: push the current `is_latest` version of `path` back
/// `days` so `now − updated_at` lands in the cold zone. Only touches the
/// time-since-update columns — access columns (C1's reinforcement seam) are left
/// alone so a later `bump_access` can layer a *recent* access on top of an old
/// page.
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

/// The `compacted_at` (V65) marker + body of the current latest version.
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

fn latest_body(fx: &Fixture, path: &str) -> String {
    latest_marker_and_body(fx, path).1
}

/// Every historical (superseded) body for `path`, oldest first — the
/// supersession chain the reversibility guarantee (invariant #16) rests on.
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

/// Insert a controlled embedding row under a chosen `(provider, model, dim)`
/// triple — used by A5, where the cosine similarity between two pages must be
/// pinned to an exact value the synthetic bag-of-words embedder cannot deliver.
fn seed_embedding(fx: &Fixture, page_id: PageId, provider: &str, model: &str, vector: &[f32]) {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    conn.execute(
        "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            page_id.as_bytes(),
            f32_vec_to_bytes(vector),
            provider,
            model,
            vector.len() as i64,
            jiff::Timestamp::now().as_microsecond(),
        ],
    )
    .unwrap();
}

fn latest_page_count(fx: &Fixture) -> i64 {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM pages \
         WHERE workspace_id = ?1 AND project_id = ?2 AND is_latest = 1",
        params![fx.ws.as_bytes(), fx.proj.as_bytes()],
        |row| row.get(0),
    )
    .unwrap()
}

/// A synthetic-embedder dedup/dream config, matching the sibling suites.
fn synthetic_coord() -> EmbeddingCoord {
    EmbeddingCoord {
        provider: "synthetic".into(),
        model: "bag-of-words-v1".into(),
        dim: DIM,
    }
}

fn dedup_on() -> ColdClusterDedup {
    ColdClusterDedup {
        enabled: true,
        embedding: Some(synthetic_coord()),
        min_pts: 0,
        max_eps: 0.0,
    }
}

fn dream_on() -> DreamConfig {
    DreamConfig {
        enabled: true,
        embedding: Some(synthetic_coord()),
        min_pts: 0,
        max_eps: 0.0,
        max_clusters_per_run: 0,
        min_cold_pages: 0,
        idle_window_secs: 0,
    }
}

// Two near-duplicate episodic bodies: near-identical prose (so the bag-of-words
// embedder places them close) but each carrying ONE unique durable fact (a
// distinct file path), so the survivor's keep-token union is testable.
const DUP_A: &str = "Investigated the flaky retention sweep test failure again and again during \
     the long debugging session that afternoon. The unique culprit turned out to be in the file \
     crates/ai-memory-store/src/ops.rs during the run.";
const DUP_B: &str = "Investigated the flaky retention sweep test failure again and again during \
     the long debugging session that afternoon. The unique culprit turned out to be in the file \
     crates/ai-memory-store/src/reader.rs during the run.";
const FAR: &str = "Kubernetes pod scheduling node selector affinity rules for the cluster \
     autoscaler deployment on the staging namespace.";

// ===========================================================================
// SCENARIO 1 — the zero-LLM lifecycle over simulated time (no provider)
// ===========================================================================

/// Stage 1. A freshly-written corpus is warm: an early sweep, at the shipped
/// defaults, evicts nothing and compacts nothing. Aging is a function of
/// elapsed time, and no time has elapsed yet.
#[tokio::test]
async fn s1_fresh_corpus_early_sweep_keeps_everything_warm() {
    let fx = seed_fixture(false).await;
    write(
        &fx,
        "sessions/a.md",
        Tier::Episodic,
        false,
        "a debugging note",
    )
    .await;
    write(&fx, "sessions/b.md", Tier::Episodic, false, "another note").await;
    write(
        &fx,
        "concepts/x.md",
        Tier::Semantic,
        false,
        "a durable fact",
    )
    .await;

    let report = run_sweep(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        false,
    )
    .await
    .unwrap();

    assert!(
        report.evicted.is_empty(),
        "fresh pages are warm: {:?}",
        report.evicted
    );
    assert!(report.compacted.is_empty(), "nothing compacts");
    assert!(report.merged.is_empty(), "nothing merges");
    assert_eq!(latest_page_count(&fx), 3, "all three pages remain live");
}

/// Stage 2. Advance time until an episodic page is cold, then sweep with A2
/// tier-down OFF (the default): today's behaviour holds — the page is EVICTED
/// (tombstoned), not compacted.
#[tokio::test]
async fn s1_cold_episodic_is_evicted_when_compaction_off() {
    let fx = seed_fixture(false).await;
    write(
        &fx,
        "sessions/old.md",
        Tier::Episodic,
        false,
        "prose nobody reopened",
    )
    .await;
    backdate_latest(&fx, "sessions/old.md", COLD_DAYS);

    let report = run_sweep(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
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
        "the cold page is evicted with the flag off"
    );
    assert!(report.evicted[0].deleted, "the eviction landed");
    assert!(
        report.compacted.is_empty(),
        "nothing compacts with the flag off"
    );
}

/// Stage 3. The SAME cold page, swept with A2 tier-down ON, is COMPACTED
/// instead: keep-tokens (a file path, an error code, a URL) survive, the
/// low-signal prose is dropped, the original stays reachable via the
/// supersession chain — and a second sweep does NOT re-compact it (the V65
/// marker makes a compacted page terminal for the decay pass).
#[tokio::test]
async fn s1_cold_episodic_is_compacted_when_compaction_on_and_terminal() {
    let fx = seed_fixture(false).await;
    let body = "Fixed the flaky sweep test after an afternoon of chasing it.\n\n\
         The root cause lived in crates/ai-memory-store/src/ops.rs and surfaced as \
         E0433 during the build; the server also returned HTTP 500. Details at \
         https://github.com/akitaonrails/ai-memory/issues/776 which everyone agreed \
         was worth writing down in careful, unnecessary, easily-forgotten prose.";
    write(&fx, "sessions/fix.md", Tier::Episodic, false, body).await;
    backdate_latest(&fx, "sessions/fix.md", COLD_DAYS);

    let first = run_sweep_with_compaction(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        true, // A2 tier-down ON
        false,
    )
    .await
    .unwrap();

    assert!(
        first.evicted.is_empty(),
        "compaction replaces eviction: {:?}",
        first.evicted
    );
    assert_eq!(
        first
            .compacted
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        vec!["sessions/fix.md"],
    );
    assert!(first.compacted[0].compacted, "the rewrite landed");

    let (marker, compacted_body) = latest_marker_and_body(&fx, "sessions/fix.md");
    assert!(marker.is_some(), "V65 compacted_at marker is set");
    // L2 keep-tokens survive.
    assert!(
        compacted_body.contains("crates/ai-memory-store/src/ops.rs"),
        "file path survives"
    );
    assert!(compacted_body.contains("E0433"), "error code survives");
    assert!(
        compacted_body.contains("github.com/akitaonrails/ai-memory"),
        "URL survives"
    );
    // The low-signal prose tail is dropped.
    assert!(
        !compacted_body.contains("easily-forgotten prose"),
        "prose dropped: {compacted_body}"
    );
    // Reversible: the full pre-compaction body stays reachable via supersession.
    assert!(
        superseded_bodies(&fx, "sessions/fix.md")
            .iter()
            .any(|b| b.contains("easily-forgotten prose")),
        "the pre-compaction body stays reachable via the supersession chain"
    );

    // Terminal: age the compacted version again and re-sweep — the marker blocks
    // both re-compaction and eviction.
    backdate_latest(&fx, "sessions/fix.md", COLD_DAYS);
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
        "a compacted page is terminal — never re-evicted"
    );
}

/// A1. Per-tier decay curves. With an aggressive working-tier half-life and a
/// long episodic one, two pages of the SAME age age differently: the working
/// page falls below `cold_threshold` while the episodic one does not.
///
/// Proven two ways. First at the formula level (`retention_score`, the exact
/// function the sweep scores with) so the tier-differentiation is explicit.
/// Then end-to-end through the sweep: the episodic page is cold under the
/// DEFAULT scalar λ (a dry-run sweep predicts its eviction) yet the long-half-
/// life episodic curve carries it over the threshold, so the very same page is
/// no longer an eviction candidate. (Only episodic pages are decay-evictable, so
/// the working page's cold score is asserted at the formula level.)
#[tokio::test]
async fn s1_per_tier_curves_differentiate_decay() {
    let params = DecayParams {
        tier_lambda: TierLambdas {
            working: Some(lambda_from_half_life_days(7.0)),
            episodic: Some(lambda_from_half_life_days(365.0)),
            ..TierLambdas::default()
        },
        ..DecayParams::default()
    };
    let age = COLD_DAYS as f64;

    // Formula level: same age, unused, feedback-free — only the per-tier time
    // term separates them.
    let working = retention_score(&params, Tier::Working, age, 0, None, None);
    let episodic = retention_score(&params, Tier::Episodic, age, 0, None, None);
    assert!(
        working < params.cold_threshold,
        "short-half-life working page is cold: {working}"
    );
    assert!(
        episodic > params.cold_threshold,
        "long-half-life episodic page survives: {episodic}"
    );

    // End-to-end: seed a cold episodic page.
    let fx = seed_fixture(false).await;
    write(
        &fx,
        "sessions/aged.md",
        Tier::Episodic,
        false,
        "an aged episodic note",
    )
    .await;
    backdate_latest(&fx, "sessions/aged.md", COLD_DAYS);

    // Under the DEFAULT scalar λ the page is cold — a dry-run sweep predicts its
    // eviction (dry_run mutates nothing, so we can then re-sweep the same page).
    let baseline = run_sweep(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        true, // dry run
    )
    .await
    .unwrap();
    assert_eq!(
        baseline
            .evicted
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec!["sessions/aged.md"],
        "under the default curve this episodic page is cold"
    );

    // Under the long episodic half-life the exact same page is no longer cold.
    let curved = run_sweep(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &params,
        true,
    )
    .await
    .unwrap();
    assert!(
        curved.evicted.is_empty(),
        "the A1 episodic curve carries the page over the threshold: {:?}",
        curved.evicted
    );
}

/// C1. Access reinforcement. A page that WOULD go cold is re-read: bumping its
/// access (via the store/writer path — the same seam the reinforcement uses)
/// raises `access_count` and sets a RECENT `last_accessed_at`, which lifts its
/// retention above `cold_threshold`, so it survives a sweep it would otherwise
/// fail. The reinforcement touches only the access columns, never `updated_at`,
/// so a genuinely old page stays old — it is the *reads* that keep it alive.
#[tokio::test]
async fn s1_access_reinforcement_saves_a_cold_page() {
    // Control: an identical un-reinforced page is evicted, proving the fixture
    // really is cold and the survival below is the reinforcement's doing.
    let control = seed_fixture(false).await;
    write(
        &control,
        "sessions/unread.md",
        Tier::Episodic,
        false,
        "cold and unread",
    )
    .await;
    backdate_latest(&control, "sessions/unread.md", COLD_DAYS);
    let control_report = run_sweep(
        &control.store.reader,
        &control.store.writer,
        Some(&control.wiki),
        control.ws,
        control.proj,
        &DecayParams::default(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        control_report
            .evicted
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec!["sessions/unread.md"],
        "an un-reinforced cold page is evicted"
    );

    // Reinforced: same age, but recently re-read a handful of times.
    let fx = seed_fixture(false).await;
    let id = write(
        &fx,
        "sessions/reread.md",
        Tier::Episodic,
        false,
        "cold but reopened",
    )
    .await;
    backdate_latest(&fx, "sessions/reread.md", COLD_DAYS);
    // Reinforce AFTER backdating so `last_accessed_at` is `now` while
    // `updated_at` stays old — the exact shape of a re-read stale page.
    for _ in 0..5 {
        fx.store.writer.bump_access(vec![id]).await.unwrap();
    }

    let report = run_sweep(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        false,
    )
    .await
    .unwrap();
    assert!(
        report.evicted.is_empty(),
        "reinforcement lifts the page above the cold threshold and it survives: {:?}",
        report.evicted
    );
    assert_eq!(
        latest_page_count(&fx),
        1,
        "the reinforced page is still live"
    );
}

/// A3. Cold-cluster dedup. Two near-duplicate cold episodic pages collapse to
/// one survivor; the far page is untouched. A fact that lived ONLY in the
/// merged-away duplicate is present in the survivor (recall preserved), and the
/// loser stays reachable via supersession (invariant #16, never a hard delete).
#[tokio::test]
async fn s1_dedup_collapses_near_duplicate_cold_pages() {
    let fx = seed_fixture(true).await; // synthetic embeddings on write
    write(&fx, "sessions/a.md", Tier::Episodic, false, DUP_A).await;
    write(&fx, "sessions/b.md", Tier::Episodic, false, DUP_B).await;
    write(&fx, "sessions/far.md", Tier::Episodic, false, FAR).await;
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
        false, // A2 off — dedup, not compaction, owns this cluster
        dedup_on(),
        false,
    )
    .await
    .unwrap();

    assert_eq!(
        report.merged.len(),
        1,
        "the two near-duplicates form one cluster: {:?}",
        report.merged
    );
    let cluster = &report.merged[0];
    assert!(cluster.applied, "the collapse landed");
    assert_eq!(cluster.merged.len(), 1, "one member merged away");
    let survivor = cluster.survivor.clone();
    let loser = cluster.merged[0].clone();
    assert_ne!(survivor, loser);
    assert!(
        !report.merged.iter().any(|m| m.survivor == "sessions/far.md"
            || m.merged.contains(&"sessions/far.md".to_string())),
        "the far page is never clustered"
    );

    // Recall preserved: the survivor carries BOTH unique facts.
    let survivor_body = latest_body(&fx, &survivor);
    assert!(
        survivor_body.contains("crates/ai-memory-store/src/ops.rs"),
        "own fact present"
    );
    assert!(
        survivor_body.contains("crates/ai-memory-store/src/reader.rs"),
        "the merged-away duplicate's fact survives in the survivor: {survivor_body}"
    );
    // The loser stays reachable via supersession.
    assert!(
        superseded_bodies(&fx, &loser)
            .iter()
            .any(|b| b.contains("unique culprit")),
        "the loser's pre-merge body stays reachable"
    );
    // The far page followed the normal cold path (evicted, not merged).
    assert!(
        report.evicted.iter().any(|e| e.path == "sessions/far.md"),
        "the far page is untouched by dedup and follows the normal cold path: {:?}",
        report.evicted
    );
}

/// A5. Contradiction lint. Two cold semantic pages whose embeddings sit in the
/// 0.4–0.75 cosine band produce ONE advisory `contradiction` finding naming
/// both — and nothing is deleted or superseded (advisory only, invariant #16).
/// Embeddings are seeded as exact 2-D unit vectors so the similarity is pinned
/// squarely inside the band.
#[tokio::test]
async fn s1_contradiction_lint_flags_band_pair_without_deleting() {
    const PROVIDER: &str = "test-embedder";
    const MODEL: &str = "unit-2d";
    let fx = seed_fixture(false).await;
    let old = write(
        &fx,
        "concepts/old-claim.md",
        Tier::Semantic,
        false,
        "The retry budget is 3.",
    )
    .await;
    let new = write(
        &fx,
        "concepts/new-claim.md",
        Tier::Semantic,
        false,
        "The retry budget is 5.",
    )
    .await;
    // dot([1,0],[0.6,0.8]) = 0.6 — squarely in the 0.4–0.75 band.
    seed_embedding(&fx, old, PROVIDER, MODEL, &[1.0, 0.0]);
    seed_embedding(&fx, new, PROVIDER, MODEL, &[0.6, 0.8]);
    for p in ["concepts/old-claim.md", "concepts/new-claim.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    let before = latest_page_count(&fx);
    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None, // no LLM — this is the zero-LLM band detector
        fx.ws,
        fx.proj,
        LintOptions {
            dry_run: true,
            use_llm: false,
            decay_lambda: 0.02,
            embedding: Some(EmbeddingCoord {
                provider: PROVIDER.into(),
                model: MODEL.into(),
                dim: 2,
            }),
        },
    )
    .await
    .unwrap();

    let contradictions: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "contradiction")
        .collect();
    assert_eq!(
        contradictions.len(),
        1,
        "one band pair ⇒ one finding: {:?}",
        report.findings
    );
    let f = contradictions[0];
    assert_eq!(f.severity, "info", "contradiction findings are advisory");
    assert!(f.pages.contains(&"concepts/old-claim.md".to_string()));
    assert!(f.pages.contains(&"concepts/new-claim.md".to_string()));
    assert_eq!(
        latest_page_count(&fx),
        before,
        "advisory only: the detector adds, deletes, and supersedes nothing"
    );
}

/// The documented default posture: with EVERY aging flag off (default config), a
/// sweep over a cold corpus does only the pre-existing evict/TTL work — it does
/// not compact and does not dedup. The new machinery is inert until enabled.
#[tokio::test]
async fn s1_all_flags_off_is_inert_on_a_cold_corpus() {
    let fx = seed_fixture(true).await; // embeddings exist, but dedup is off
    write(&fx, "sessions/a.md", Tier::Episodic, false, DUP_A).await;
    write(&fx, "sessions/b.md", Tier::Episodic, false, DUP_B).await;
    for p in ["sessions/a.md", "sessions/b.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    // `run_sweep` is the default surface: A2 off, A3 off, no observation prune.
    let report = run_sweep(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        false,
    )
    .await
    .unwrap();

    assert!(report.compacted.is_empty(), "A2 inert by default");
    assert!(
        report.merged.is_empty(),
        "A3 inert by default even with embeddings present"
    );
    assert_eq!(
        report.evicted.len(),
        2,
        "only the pre-existing eviction path runs"
    );
}

// ===========================================================================
// SCENARIO 2 — the LLM "dream" lifecycle (fake provider, zero live calls)
// ===========================================================================

/// Fake provider: returns a merged page whose body echoes the whole prompt (so
/// the survivor demonstrably keeps every source fact), records that it was
/// handed the `DreamMergedPage` JSON schema (invariant #7), and counts calls.
struct FakeMergeLlm {
    calls: Arc<AtomicUsize>,
    saw_schema: Arc<AtomicUsize>,
}

impl FakeMergeLlm {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            saw_schema: Arc::new(AtomicUsize::new(0)),
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
        let saw_schema = self.saw_schema.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if serde_json::to_string(&schema)
                .unwrap_or_default()
                .contains("body_markdown")
            {
                saw_schema.fetch_add(1, Ordering::SeqCst);
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

/// B2. Dream ON. A cold cluster of near-duplicates is merged/rewritten into one
/// LLM-authored survivor; sources are superseded (reachable, never deleted); the
/// `DreamReport` reports exactly one merge and the LLM was called once with the
/// structured schema.
#[tokio::test]
async fn s2_dream_merges_cold_cluster_via_llm() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", Tier::Episodic, false, DUP_A).await;
    write(&fx, "sessions/b.md", Tier::Episodic, false, DUP_B).await;
    // A far cold page: DBSCAN needs noise points around the cluster, exactly as
    // A3 does. It is noise, not a second cluster.
    write(&fx, "sessions/far.md", Tier::Episodic, false, FAR).await;
    for p in ["sessions/a.md", "sessions/b.md", "sessions/far.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

    let llm = FakeMergeLlm::new();
    let calls = llm.calls.clone();
    let saw_schema = llm.saw_schema.clone();
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
    assert_eq!(report.clusters_merged, 1, "the merge applied");
    assert_eq!(report.pages_rewritten, 1);
    assert_eq!(report.pages_superseded, 1);
    assert!(!report.cancelled);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "LLM called exactly once");
    assert_eq!(
        saw_schema.load(Ordering::SeqCst),
        1,
        "invariant #7: DreamMergedPage JSON schema"
    );

    let merge = &report.merges[0];
    let survivor = merge.survivor.clone();
    let loser = merge.merged[0].clone();
    assert_ne!(survivor, loser);
    // The LLM-authored survivor keeps every source fact (the fake echoes the
    // prompt, which carries both source bodies).
    let survivor_body = latest_body(&fx, &survivor);
    assert!(
        survivor_body.contains("crates/ai-memory-store/src/ops.rs")
            && survivor_body.contains("crates/ai-memory-store/src/reader.rs"),
        "survivor keeps every source fact: {survivor_body}"
    );
    // The loser is a merge note pointing at the survivor; its pre-merge body
    // stays reachable (invariant #16 — never a hard delete).
    let loser_body = latest_body(&fx, &loser);
    assert!(
        loser_body.contains("merged into") && loser_body.contains(&survivor),
        "loser points at survivor"
    );
    assert!(
        superseded_bodies(&fx, &loser)
            .iter()
            .any(|b| b.contains("unique culprit")),
        "the loser's pre-merge body stays reachable"
    );
}

/// B2. A dream dry run returns the plan, writes nothing, and never calls the LLM.
#[tokio::test]
async fn s2_dream_dry_run_plans_without_writing() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", Tier::Episodic, false, DUP_A).await;
    write(&fx, "sessions/b.md", Tier::Episodic, false, DUP_B).await;
    write(&fx, "sessions/far.md", Tier::Episodic, false, FAR).await;
    for p in ["sessions/a.md", "sessions/b.md", "sessions/far.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

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
        true, // dry run
    )
    .await
    .unwrap();

    assert!(report.dry_run);
    assert_eq!(
        report.clusters_considered, 1,
        "the plan still finds the cluster"
    );
    assert_eq!(report.clusters_merged, 0, "nothing applied on a dry run");
    assert_eq!(report.merges.len(), 1, "the plan is returned");
    assert!(!report.merges[0].applied);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no LLM call on a dry run");
    // Both source pages are untouched (still their original bodies).
    assert!(latest_body(&fx, "sessions/a.md").contains("unique culprit"));
    assert!(latest_body(&fx, "sessions/b.md").contains("unique culprit"));
}

/// B2. No provider ⇒ the dream pass is a clean no-op: the zero-LLM path is
/// entirely unaffected (invariants #13, #16).
#[tokio::test]
async fn s2_dream_no_provider_is_a_clean_no_op() {
    let fx = seed_fixture(true).await;
    write(&fx, "sessions/a.md", Tier::Episodic, false, DUP_A).await;
    write(&fx, "sessions/b.md", Tier::Episodic, false, DUP_B).await;
    for p in ["sessions/a.md", "sessions/b.md"] {
        backdate_latest(&fx, p, COLD_DAYS);
    }

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
    // The pages remain exactly as written — nothing was merged or rewritten.
    assert!(latest_body(&fx, "sessions/a.md").contains("unique culprit"));
    assert!(latest_body(&fx, "sessions/b.md").contains("unique culprit"));
}
