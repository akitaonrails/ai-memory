//! A5 zero-LLM contradiction detection, end to end through a real Store + Wiki.
//!
//! Invariants guarded (docs/design-memory-aging.md §A5):
//!
//! 1. **Band pair ⇒ finding.** Two cold pages whose embeddings sit in the
//!    0.4–0.75 cosine-similarity band produce one advisory `contradiction`
//!    finding naming both paths, with newer-wins advice.
//! 2. **Near-duplicate ⇒ NOT a contradiction.** A pair at ≥0.75 is A3
//!    dedup territory, never flagged here.
//! 3. **No embeddings ⇒ clean no-op** (never an error, never a provider call).
//! 4. **Advisory only (#16).** The detector writes no supersession and deletes
//!    nothing; the source pages are untouched.
//!
//! Embeddings are seeded directly into `page_embeddings` with controlled unit
//! vectors so the cosine similarity between two pages is exact and the band
//! boundaries are testable — the synthetic bag-of-words embedder cannot pin a
//! similarity to a chosen value.

use ai_memory_consolidate::{EmbeddingCoord, LintOptions, run_lint};
use ai_memory_core::{ActorContext, PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{Store, f32_vec_to_bytes};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const PROVIDER: &str = "test-embedder";
const MODEL: &str = "unit-2d";
const DIM: u32 = 2;

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

/// Write a semantic page (the tier the contradiction detector considers).
async fn write_semantic(fx: &Fixture, path: &str, body: &str) -> PageId {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": path}),
            body: body.to_string(),
            tier: Tier::Semantic,
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

/// Insert a controlled embedding row for a page under our test triple.
fn seed_embedding(fx: &Fixture, page_id: PageId, vector: &[f32]) {
    let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    conn.execute(
        "INSERT INTO page_embeddings (page_id, vector, provider, model, dim, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            page_id.as_bytes(),
            f32_vec_to_bytes(vector),
            PROVIDER,
            MODEL,
            DIM,
            jiff::Timestamp::now().as_microsecond(),
        ],
    )
    .unwrap();
}

fn coord() -> Option<EmbeddingCoord> {
    Some(EmbeddingCoord {
        provider: PROVIDER.into(),
        model: MODEL.into(),
        dim: DIM,
    })
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

fn options(embedding: Option<EmbeddingCoord>) -> LintOptions {
    LintOptions {
        // dry_run so the pass emits findings without writing a report page,
        // keeping the "nothing was mutated" assertion clean.
        dry_run: true,
        // No LLM in these tests — this is the zero-LLM detector.
        use_llm: false,
        decay_lambda: 0.02,
        embedding,
    }
}

/// Two cold pages in the band ⇒ one advisory contradiction finding naming
/// both, with newer-wins advice; nothing is deleted or rewritten.
#[tokio::test]
async fn band_pair_yields_advisory_contradiction() {
    let fx = seed_fixture().await;
    let old = write_semantic(&fx, "concepts/old-claim.md", "The retry budget is 3.").await;
    let new = write_semantic(&fx, "concepts/new-claim.md", "The retry budget is 5.").await;
    // dot([1,0],[0.6,0.8]) = 0.6 — squarely in the 0.4–0.75 band.
    seed_embedding(&fx, old, &[1.0, 0.0]);
    seed_embedding(&fx, new, &[0.6, 0.8]);
    // Make `new` the more recently updated page so it wins the timestamp advice.
    {
        let conn = rusqlite::Connection::open(fx.tmp.path().join("db/memory.sqlite")).unwrap();
        conn.execute(
            "UPDATE pages SET updated_at = updated_at + 1000000 \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                fx.ws.as_bytes(),
                fx.proj.as_bytes(),
                "concepts/new-claim.md"
            ],
        )
        .unwrap();
    }

    let before = latest_page_count(&fx);
    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
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
        "one band pair ⇒ one contradiction finding: {:?}",
        report.findings
    );
    let f = contradictions[0];
    assert_eq!(f.severity, "info", "detected contradictions are advisory");
    assert!(f.pages.contains(&"concepts/old-claim.md".to_string()));
    assert!(f.pages.contains(&"concepts/new-claim.md".to_string()));
    assert!(
        f.message.contains("concepts/new-claim.md") && f.message.contains("supersedes"),
        "newer page wins on a timestamp basis: {}",
        f.message
    );

    // Advisory (#16): nothing deleted or superseded.
    assert_eq!(
        latest_page_count(&fx),
        before,
        "the detector must not add, delete, or supersede any page"
    );
}

/// A near-duplicate pair (cosine ≈ 0.98) is A3's dedup job, not a
/// contradiction — the detector must stay silent.
#[tokio::test]
async fn near_duplicate_is_not_a_contradiction() {
    let fx = seed_fixture().await;
    let a = write_semantic(&fx, "concepts/a.md", "alpha").await;
    let b = write_semantic(&fx, "concepts/b.md", "beta").await;
    seed_embedding(&fx, a, &[1.0, 0.0]);
    // dot ≈ 0.98 — above the band.
    seed_embedding(&fx, b, &[0.98, 0.199]);

    let report = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
    )
    .await
    .unwrap();

    assert!(
        !report.findings.iter().any(|f| f.kind == "contradiction"),
        "a near-duplicate must not be flagged as a contradiction: {:?}",
        report.findings
    );
}

/// No embeddings for the configured triple ⇒ the detector is a clean no-op
/// (not an error, no provider call). Also covers `embedding: None`.
#[tokio::test]
async fn no_embeddings_is_a_clean_no_op() {
    let fx = seed_fixture().await;
    write_semantic(&fx, "concepts/a.md", "The retry budget is 3.").await;
    write_semantic(&fx, "concepts/b.md", "The retry budget is 5.").await;
    // No embedding rows seeded at all.

    // A triple is configured, but there are no matching vectors → no-op.
    let with_coord = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(coord()),
    )
    .await
    .unwrap();
    assert!(
        !with_coord
            .findings
            .iter()
            .any(|f| f.kind == "contradiction"),
        "no embeddings ⇒ no contradiction findings: {:?}",
        with_coord.findings
    );

    // No embedder configured at all (embedding: None) → also a no-op.
    let without_coord = run_lint(
        &fx.store.reader,
        &fx.wiki,
        None,
        fx.ws,
        fx.proj,
        options(None),
    )
    .await
    .unwrap();
    assert!(
        !without_coord
            .findings
            .iter()
            .any(|f| f.kind == "contradiction"),
        "embedding: None ⇒ no contradiction findings: {:?}",
        without_coord.findings
    );
}
