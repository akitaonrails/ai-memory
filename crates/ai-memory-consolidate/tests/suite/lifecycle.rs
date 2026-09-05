//! End-to-end retention-lifecycle integration test.
//!
//! Demonstrates that M8's hybrid policy actually does what the docs
//! claim: episodic pages decay over time unless they get queried;
//! semantic concept pages compound forever; pinned anything survives;
//! and after sweep, the FTS5 index actually loses the evicted content
//! while keeping everything else searchable.
//!
//! Time travel is simulated by backdating the `updated_at` /
//! `access_count` / `last_accessed_at` columns via a secondary
//! `rusqlite::Connection`. WAL mode + a busy_timeout means we can
//! safely write while the writer actor is idle between operations.

use std::collections::HashSet;

use ai_memory_consolidate::{run_lint, run_sweep};
use ai_memory_core::{PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use rusqlite::params;
use tempfile::TempDir;

const US_PER_DAY: i64 = 86_400_000_000;

/// One page in the lifecycle fixture.
struct Fixture {
    /// Wiki-relative path.
    path: &'static str,
    /// Body — chosen to give FTS5 a distinct keyword per page.
    body: &'static str,
    /// Tier.
    tier: Tier,
    /// `true` if frontmatter should carry `pinned: true`.
    pinned: bool,
    /// Days since `updated_at`.
    age_days: i64,
    /// Total accesses simulated.
    access_count: u32,
    /// Days since `last_accessed_at`, or `None` to leave NULL.
    days_since_access: Option<i64>,
    /// Whether the sweep should evict this page.
    expected_evicted: bool,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        path: "sessions/fresh.md",
        body: "Started exploring rmcp tool routing patterns today",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 2,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    // Mid-term untouched: 60 days old, never queried. The point of
    // including this is to demonstrate the formula isn't too eager:
    // an unused episodic page should still survive in the 2-month
    // window when default params are in play. With lambda=0.02 the
    // time-term at 60d is exp(-1.2) = 0.30, still well above the
    // cold_threshold of 0.20.
    Fixture {
        path: "sessions/midterm-untouched.md",
        body: "Notes on dyn dispatch trade-offs and downcast overhead, midterm reference",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 60,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    Fixture {
        path: "sessions/hot-old.md",
        body: "Investigation of writer-actor backpressure design and tokio mpsc bounded channels",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 120,
        access_count: 50,
        days_since_access: Some(2),
        expected_evicted: false,
    },
    Fixture {
        path: "sessions/cold-old.md",
        body: "Quick spike on swapping jiff for an older datetime crate, abandoned mid-experiment",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 120,
        access_count: 0,
        days_since_access: None,
        expected_evicted: true,
    },
    Fixture {
        path: "sessions/very-cold.md",
        body: "Tried bolting cognee's pipeline shape onto the rust workspace, didn't pan out",
        tier: Tier::Episodic,
        pinned: false,
        age_days: 200,
        access_count: 0,
        days_since_access: None,
        expected_evicted: true,
    },
    Fixture {
        path: "sessions/pinned-ancient.md",
        body: "Decision log: never re-add the iii-engine sidecar dependency",
        tier: Tier::Episodic,
        pinned: true,
        age_days: 300,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    Fixture {
        path: "concepts/karpathy-wiki.md",
        body: "Karpathy LLM Wiki principle: compile knowledge into the artifact, do not re-retrieve",
        tier: Tier::Semantic,
        pinned: false,
        age_days: 300,
        access_count: 5,
        days_since_access: Some(60),
        expected_evicted: false,
    },
    Fixture {
        path: "concepts/single-writer.md",
        body: "All SQLite mutations flow through one writer actor backed by mpsc",
        tier: Tier::Semantic,
        pinned: false,
        age_days: 200,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
    Fixture {
        path: "concepts/wiki-conventions.md",
        body: "Wiki path conventions: sessions/, concepts/, decisions/, gotchas/",
        tier: Tier::Semantic,
        pinned: false,
        age_days: 30,
        access_count: 0,
        days_since_access: None,
        expected_evicted: false,
    },
];

/// Backdate one page's timestamps + access counters via a secondary
/// SQLite connection. WAL mode lets us write while the writer actor
/// is idle between operations.
fn backdate(db_path: &std::path::Path, now_us: i64, fixture: &Fixture, id: PageId) {
    let conn = rusqlite::Connection::open(db_path).expect("open aux conn");
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    let updated_at = now_us - fixture.age_days * US_PER_DAY;
    let last_access_us = fixture.days_since_access.map(|d| now_us - d * US_PER_DAY);
    conn.execute(
        "UPDATE pages \
         SET created_at = ?1, updated_at = ?1, access_count = ?2, last_accessed_at = ?3 \
         WHERE id = ?4",
        params![
            updated_at,
            i64::from(fixture.access_count),
            last_access_us,
            id.as_bytes(),
        ],
    )
    .expect("backdate update");
}

#[tokio::test]
async fn m8_retention_lifecycle_end_to_end() {
    // ── Phase 1 — bootstrap a fresh wiki + store ─────────────────
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "lifecycle-test", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_store_reader(store.reader.clone());

    // ── Phase 2 — seed the 8 fixtures through the normal write path ─
    let mut ids: Vec<(&'static str, PageId)> = Vec::new();
    for fx in FIXTURES {
        let title = format!("Page {}", fx.path);
        let mut frontmatter = serde_json::Map::new();
        frontmatter.insert("title".into(), serde_json::Value::String(title.clone()));
        if fx.pinned {
            frontmatter.insert("pinned".into(), serde_json::Value::Bool(true));
        }
        let id = wiki
            .write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new(fx.path.to_string()).expect("page path"),
                frontmatter: serde_json::Value::Object(frontmatter),
                body: fx.body.to_string(),
                tier: fx.tier,
                pinned: false,
                title: Some(title),
                admission_ctx: None,
                author_id: None,
                actor: ai_memory_core::ActorContext::anonymous(),
            })
            .await
            .expect("write page");
        ids.push((fx.path, id));
    }

    // ── Phase 3 — time-travel via direct SQL ────────────────────
    let now_us = jiff::Timestamp::now().as_microsecond();
    let db_path = tmp.path().join("db/memory.sqlite");
    for (fx, (_, id)) in FIXTURES.iter().zip(&ids) {
        backdate(&db_path, now_us, fx, *id);
    }

    // ── Phase 4 — dry-run sweep; verify the formula's verdict ───
    let params = DecayParams::default();
    let dry = run_sweep(
        &store.reader,
        &store.writer,
        None,
        ws,
        proj,
        &params,
        /* dry_run */ true,
    )
    .await
    .expect("dry sweep");
    assert!(dry.dry_run);
    assert_eq!(
        dry.candidates_evaluated,
        FIXTURES.len(),
        "all {} pages should be considered as candidates",
        FIXTURES.len(),
    );

    let evicted: HashSet<&str> = dry.evicted.iter().map(|e| e.path.as_str()).collect();
    for fx in FIXTURES {
        let got = evicted.contains(fx.path);
        assert_eq!(
            got,
            fx.expected_evicted,
            "page {} expected_evicted={} but sweep said {} \
             (tier={:?}, pinned={}, age={}, access={}, days_since_access={:?})",
            fx.path,
            fx.expected_evicted,
            got,
            fx.tier,
            fx.pinned,
            fx.age_days,
            fx.access_count,
            fx.days_since_access,
        );
    }

    // ── Phase 5 — real sweep; verify row counts ─────────────────
    let counts_before = store.reader.status_counts().await.expect("counts before");
    assert_eq!(counts_before.pages_latest as usize, FIXTURES.len());
    assert_eq!(counts_before.pages_all as usize, FIXTURES.len());

    // Destructive decay without the authoritative Wiki handle must fail
    // closed. Mutating only SQLite would leave the files for reconciliation
    // to recreate as live pages.
    let no_wiki = run_sweep(
        &store.reader,
        &store.writer,
        None,
        ws,
        proj,
        &params,
        /* dry_run */ false,
    )
    .await
    .expect("store-only sweep");
    assert!(no_wiki.evicted.iter().all(|page| !page.deleted));
    let counts_after_refusal = store
        .reader
        .status_counts()
        .await
        .expect("counts after refusal");
    assert_eq!(
        counts_after_refusal.pages_latest,
        counts_before.pages_latest
    );
    assert_eq!(counts_after_refusal.pages_all, counts_before.pages_all);

    let real = run_sweep(
        &store.reader,
        &store.writer,
        Some(&wiki),
        ws,
        proj,
        &params,
        /* dry_run */ false,
    )
    .await
    .expect("real sweep");
    assert!(!real.dry_run);
    let expected_evicted_count = FIXTURES.iter().filter(|f| f.expected_evicted).count();
    assert_eq!(real.evicted.len(), expected_evicted_count);
    assert!(
        real.evicted.iter().all(|page| page.deleted),
        "every selected page should complete its wiki-backed eviction: {:?}",
        real.evicted,
    );

    let counts_after = store.reader.status_counts().await.expect("counts after");
    assert_eq!(
        counts_after.pages_latest as usize,
        FIXTURES.len() - expected_evicted_count,
        "is_latest=1 should drop by exactly the evicted count",
    );
    assert_eq!(
        counts_after.pages_all as usize,
        FIXTURES.len(),
        "eviction preserves a tombstone for the hard-delete grace period",
    );
    let project_root = wiki.project_root(ws, proj);
    for fixture in FIXTURES {
        assert_eq!(
            project_root.join(fixture.path).exists(),
            !fixture.expected_evicted,
            "the Markdown source must be removed exactly for evicted pages: {}",
            fixture.path,
        );
    }

    // ── Phase 6 — FTS5 invariants ───────────────────────────────
    // Keywords unique to evicted pages should disappear from search.
    let cognee_hits = store
        .reader
        .search_pages("cognee".into(), 5)
        .await
        .expect("search cognee");
    assert!(
        cognee_hits.is_empty(),
        "evicted page 'very-cold' mentioned cognee; should be unsearchable now, got: {cognee_hits:?}",
    );

    let jiff_hits = store
        .reader
        .search_pages("jiff".into(), 5)
        .await
        .expect("search jiff");
    assert!(
        jiff_hits.is_empty(),
        "evicted page 'cold-old' mentioned jiff; should be unsearchable",
    );

    // Hot + semantic + pinned pages should still be searchable.
    // Note on FTS5: our tokenizer is `unicode61 tokenchars '/_-'`, so
    // `writer-actor` and `single-writer` are *single* tokens. We pick
    // distinct standalone keywords to test each page independently.
    let backpressure_hits = store
        .reader
        .search_pages("backpressure".into(), 5)
        .await
        .expect("search backpressure");
    let bp_paths: HashSet<&str> = backpressure_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        bp_paths.contains("sessions/hot-old.md"),
        "hot reinforced page (with 'backpressure') should remain searchable: {bp_paths:?}",
    );

    let mutations_hits = store
        .reader
        .search_pages("mutations".into(), 5)
        .await
        .expect("search mutations");
    let mut_paths: HashSet<&str> = mutations_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        mut_paths.contains("concepts/single-writer.md"),
        "semantic concept page (with 'mutations') should remain searchable: {mut_paths:?}",
    );

    let karpathy_hits = store
        .reader
        .search_pages("karpathy".into(), 5)
        .await
        .expect("search karpathy");
    let karpathy_paths: HashSet<&str> = karpathy_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        karpathy_paths.contains("concepts/karpathy-wiki.md"),
        "300-day-old semantic page survives forever (no tier decay)",
    );

    // Note: our FTS5 tokenizer keeps `-` inside tokens, but the FTS5
    // *query parser* still treats a bare `-` as the NOT prefix. Search
    // for a standalone token from the body instead of `iii-engine`.
    let sidecar_hits = store
        .reader
        .search_pages("sidecar".into(), 5)
        .await
        .expect("search sidecar");
    let sidecar_paths: HashSet<&str> = sidecar_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        sidecar_paths.contains("sessions/pinned-ancient.md"),
        "pinned ancient page survives regardless of age",
    );

    // The mid-term untouched page deserves its own assertion to make
    // the "don't forget too fast" property explicit — searching for a
    // term unique to that page should still return it.
    let midterm_hits = store
        .reader
        .search_pages("dyn".into(), 5)
        .await
        .expect("search dyn");
    let midterm_paths: HashSet<&str> = midterm_hits.iter().map(|h| h.path.as_str()).collect();
    assert!(
        midterm_paths.contains("sessions/midterm-untouched.md"),
        "60-day-old untouched episodic page should still be discoverable \
         (the formula isn't supposed to forget mid-term knowledge): {midterm_paths:?}",
    );

    // ── Phase 7 — lint catches the residual stale + duplicate signals ─
    let lint_report = run_lint(
        &store.reader,
        &wiki,
        None,
        ws,
        proj,
        ai_memory_consolidate::LintOptions {
            dry_run: true,
            use_llm: true,
            decay_lambda: ai_memory_store::DecayParams::default().lambda,
        },
    )
    .await
    .expect("lint dry-run");
    // We added rule-based 'stale' detection for episodic pages >30d
    // with zero accesses. After sweep, the cold pages are no longer
    // is_latest=1 so they don't appear; but lint is a safety net for
    // anything that slipped through (e.g. user disabled sweep). Just
    // confirm the report shape is well-formed.
    for f in &lint_report.findings {
        assert!(
            !f.message.is_empty(),
            "every finding must have a human-readable message",
        );
        assert!(
            ["info", "warning", "error"].contains(&f.severity.as_str()),
            "severity must be one of info/warning/error, got {}",
            f.severity,
        );
    }

    // ── Phase 8 — retention scores ordering sanity check ────────
    // The dry-run report carries the actual scores. Verify they're
    // ordered the way the docs imply: hot pages > pinned-skipped
    // (which is absent) > cold pages (which got evicted).
    if let (Some(very_cold), Some(cold_old)) = (
        dry.evicted
            .iter()
            .find(|e| e.path == "sessions/very-cold.md"),
        dry.evicted
            .iter()
            .find(|e| e.path == "sessions/cold-old.md"),
    ) {
        assert!(
            very_cold.retention < cold_old.retention,
            "200d cold should score below 120d cold: very_cold={} cold_old={}",
            very_cold.retention,
            cold_old.retention,
        );
    }
}

/// TTL lifecycle: pages whose frontmatter `expires_at:` has passed are
/// hidden from retrieval, listed by the sweep, and hard-deleted (file +
/// rows) by a real sweep — including pinned pages, since an explicit
/// expiry beats a pin. Future-dated TTLs change nothing.
#[tokio::test]
async fn ttl_expiry_lifecycle_end_to_end() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "ttl-test", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_store_reader(store.reader.clone());

    let write = |path: &str, body: &str, expires_at: Option<&str>, pinned: bool| {
        let mut frontmatter = serde_json::Map::new();
        if let Some(ts) = expires_at {
            frontmatter.insert("expires_at".into(), serde_json::Value::String(ts.into()));
        }
        if pinned {
            frontmatter.insert("pinned".into(), serde_json::Value::Bool(true));
        }
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path.to_string()).expect("page path"),
            frontmatter: serde_json::Value::Object(frontmatter),
            body: body.to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
    };

    write(
        "notes/expired.md",
        "# Expired\nttlmarker expired sprint context",
        Some("2020-01-01"),
        false,
    )
    .await
    .expect("write expired page");
    write(
        "notes/expired-pinned.md",
        "# Expired pinned\nttlmarker pinned but past its expiry",
        Some("2020-06-01T00:00:00Z"),
        true,
    )
    .await
    .expect("write expired pinned page");
    write(
        "notes/future.md",
        "# Future\nttlmarker still valid for decades",
        Some("2099-01-01"),
        false,
    )
    .await
    .expect("write future page");
    write(
        "notes/forever.md",
        "# Forever\nttlmarker no ttl at all",
        None,
        false,
    )
    .await
    .expect("write forever page");

    let stale_expired_id = write(
        "notes/refreshed.md",
        "# Refreshed\nttlmarker stale expired version",
        Some("2020-01-01"),
        false,
    )
    .await
    .expect("write stale expired version");
    let refreshed_id = write(
        "notes/refreshed.md",
        "# Refreshed\nttlmarker current non-expiring version",
        None,
        false,
    )
    .await
    .expect("refresh expired page");
    assert_ne!(stale_expired_id, refreshed_id);
    assert!(
        !wiki
            .delete_page_if_latest(
                ws,
                proj,
                &PagePath::new("notes/refreshed.md").unwrap(),
                stale_expired_id,
                None
            )
            .await
            .expect("stale conditional delete"),
        "a stale expiry candidate must not delete a refreshed page"
    );
    assert!(
        ws_dir_for(&tmp, ws, proj)
            .join("notes/refreshed.md")
            .exists()
    );

    // Invalid expires_at fails closed instead of meaning "never".
    let invalid = write("notes/bad.md", "# Bad\nbody", Some("soonish"), false).await;
    assert!(invalid.is_err(), "invalid expires_at must be rejected");

    // Retrieval hides expired pages by default…
    let hits = store
        .reader
        .search_pages_for_project(ws, proj, "ttlmarker".into(), 10, None)
        .await
        .expect("search");
    let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    assert!(paths.contains(&"notes/future.md"));
    assert!(paths.contains(&"notes/forever.md"));
    assert!(
        !paths.contains(&"notes/expired.md"),
        "expired hidden: {paths:?}"
    );
    assert!(!paths.contains(&"notes/expired-pinned.md"));
    let recent = store
        .reader
        .recent_pages_for_project(ws, proj, 10)
        .await
        .expect("recent");
    assert!(!recent.iter().any(|h| h.path.as_str() == "notes/expired.md"));

    // …but an i64::MIN cutoff (memory_query include_expired) shows them.
    let all_hits = store
        .reader
        .search_pages_for_project(ws, proj, "ttlmarker".into(), 10, Some(i64::MIN))
        .await
        .expect("search all");
    assert!(
        all_hits
            .iter()
            .any(|h| h.path.as_str() == "notes/expired.md"),
        "include_expired must surface expired pages",
    );

    // Dry-run sweep lists the expired pages without touching disk.
    let params = DecayParams::default();
    let dry = run_sweep(
        &store.reader,
        &store.writer,
        Some(&wiki),
        ws,
        proj,
        &params,
        true,
    )
    .await
    .expect("dry sweep");
    let dry_expired: HashSet<&str> = dry.expired.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        dry_expired,
        HashSet::from(["notes/expired.md", "notes/expired-pinned.md"]),
        "dry sweep lists exactly the expired pages",
    );
    assert!(dry.expired.iter().all(|e| !e.deleted));
    let ws_dir = tmp
        .path()
        .join("wiki")
        .join(ws.to_string())
        .join(proj.to_string());
    assert!(ws_dir.join("notes/expired.md").exists());

    // A caller without the canonical wiki handle must fail closed. Deleting
    // only the SQLite rows would let the watcher resurrect the file.
    let no_wiki = run_sweep(&store.reader, &store.writer, None, ws, proj, &params, false)
        .await
        .expect("sweep without wiki");
    assert!(no_wiki.expired.iter().all(|e| !e.deleted));
    assert!(ws_dir.join("notes/expired.md").exists());
    let still_indexed = store
        .reader
        .search_pages_for_project(ws, proj, "ttlmarker".into(), 10, Some(i64::MIN))
        .await
        .expect("search after refused store-only delete");
    assert!(
        still_indexed
            .iter()
            .any(|h| h.path.as_str() == "notes/expired.md")
    );

    // Real sweep hard-deletes them through the wiki layer.
    let real = run_sweep(
        &store.reader,
        &store.writer,
        Some(&wiki),
        ws,
        proj,
        &params,
        false,
    )
    .await
    .expect("real sweep");
    assert!(real.expired.iter().all(|e| e.deleted), "{:?}", real.expired);
    assert!(
        !ws_dir.join("notes/expired.md").exists(),
        "markdown file must be removed, not just the rows",
    );
    assert!(
        !ws_dir.join("notes/expired-pinned.md").exists(),
        "explicit expiry beats pin",
    );
    assert!(ws_dir.join("notes/future.md").exists());
    assert!(ws_dir.join("notes/refreshed.md").exists());

    // Rows are gone too — even an include-everything search misses them.
    let after = store
        .reader
        .search_pages_for_project(ws, proj, "ttlmarker".into(), 10, Some(i64::MIN))
        .await
        .expect("search after sweep");
    let after_paths: Vec<&str> = after.iter().map(|h| h.path.as_str()).collect();
    assert!(!after_paths.contains(&"notes/expired.md"));
    assert!(after_paths.contains(&"notes/future.md"));
    assert!(after_paths.contains(&"notes/forever.md"));
    assert!(after_paths.contains(&"notes/refreshed.md"));
}

#[tokio::test]
async fn aged_decay_cleanup_stays_within_the_requested_scope() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("workspace");
    let target = store
        .writer
        .get_or_create_project(ws, "target", None)
        .await
        .expect("target project");
    let sibling = store
        .writer
        .get_or_create_project(ws, "sibling", None)
        .await
        .expect("sibling project");
    let other_ws = store
        .writer
        .get_or_create_workspace("other")
        .await
        .expect("other workspace");
    let other_project = store
        .writer
        .get_or_create_project(other_ws, "target", None)
        .await
        .expect("other workspace project");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_store_reader(store.reader.clone());

    for (workspace_id, project_id, path, entity) in [
        (ws, target, "sessions/target.md", "target-entity"),
        (ws, sibling, "sessions/sibling.md", "sibling-entity"),
        (
            other_ws,
            other_project,
            "sessions/other-workspace.md",
            "other-workspace-entity",
        ),
    ] {
        wiki.write_page(WritePageRequest {
            workspace_id,
            project_id,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"entities": [entity]}),
            body: format!("# Tombstone\n{path}"),
            tier: Tier::Episodic,
            pinned: false,
            title: Some("Tombstone".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .expect("write tombstone fixture");
    }

    // All three rows represent sweep-evicted pages old enough for immediate
    // cleanup. The sweep below targets only `(default, target)`.
    let conn = rusqlite::Connection::open(store.db_path()).expect("open aux conn");
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    conn.execute(
        "UPDATE pages SET is_latest = 0, superseded_at = 1, access_count = 7",
        [],
    )
    .expect("age tombstone fixtures");
    drop(conn);
    std::fs::remove_file(wiki.abs_path(ws, target, &PagePath::new("sessions/target.md").unwrap()))
        .expect("simulate wiki-backed target eviction");

    let params = DecayParams {
        hard_delete_after_days: 0,
        ..DecayParams::default()
    };
    let report = run_sweep(
        &store.reader,
        &store.writer,
        Some(&wiki),
        ws,
        target,
        &params,
        false,
    )
    .await
    .expect("targeted sweep");
    assert_eq!(
        report.hard_deleted, 1,
        "the report must count only tombstones deleted from the target scope"
    );

    let conn = rusqlite::Connection::open(store.db_path()).expect("reopen aux conn");
    let remaining = |workspace_id: WorkspaceId, project_id: ProjectId| -> i64 {
        conn.query_row(
            "SELECT count(*) FROM pages WHERE workspace_id = ?1 AND project_id = ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(remaining(ws, target), 0, "target tombstone is deleted");
    assert_eq!(remaining(ws, sibling), 1, "sibling project is preserved");
    assert_eq!(
        remaining(other_ws, other_project),
        1,
        "same-named project in another workspace is preserved"
    );
    let remaining_entities = |workspace_id: WorkspaceId, project_id: ProjectId| -> i64 {
        conn.query_row(
            "SELECT count(*) FROM entities WHERE workspace_id = ?1 AND project_id = ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        remaining_entities(ws, target),
        0,
        "the target entity orphan is removed in the page-delete transaction"
    );
    assert_eq!(
        remaining_entities(ws, sibling),
        1,
        "sibling entity index is preserved"
    );
    assert_eq!(
        remaining_entities(other_ws, other_project),
        1,
        "other workspace entity index is preserved"
    );
}

#[tokio::test]
async fn rewritten_decay_eviction_removes_the_file_and_entire_version_chain() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("workspace");
    let proj = store
        .writer
        .get_or_create_project(ws, "rewritten", None)
        .await
        .expect("project");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_store_reader(store.reader.clone());
    let path = PagePath::new("sessions/rewritten.md").unwrap();

    let write = |body: &str, entity: &str| WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: path.clone(),
        frontmatter: serde_json::json!({"entities": [entity]}),
        body: body.to_string(),
        tier: Tier::Episodic,
        pinned: false,
        title: Some("Rewritten".into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    };
    let first = wiki
        .write_page(write("# Rewritten\nfirst-version-marker", "first-entity"))
        .await
        .unwrap();
    let second = wiki
        .write_page(write("# Rewritten\nsecond-version-marker", "second-entity"))
        .await
        .unwrap();

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE pages SET updated_at = 1, access_count = 7 WHERE id = ?1",
        params![second.as_bytes()],
    )
    .unwrap();
    drop(conn);

    let report = run_sweep(
        &store.reader,
        &store.writer,
        Some(&wiki),
        ws,
        proj,
        &DecayParams {
            hard_delete_after_days: 0,
            ..DecayParams::default()
        },
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.evicted.len(), 1);
    assert!(report.evicted[0].deleted);
    assert_eq!(
        report.hard_deleted, 2,
        "the evicted head and its superseded ancestor are both purged"
    );
    assert!(!wiki.abs_path(ws, proj, &path).exists());

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT count(*) FROM pages WHERE id IN (?1, ?2)",
            params![first.as_bytes(), second.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    let entities: i64 = conn
        .query_row(
            "SELECT count(*) FROM entities WHERE workspace_id = ?1 AND project_id = ?2",
            params![ws.as_bytes(), proj.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    assert_eq!(
        entities, 0,
        "orphaned entity rows are cleaned with the chain"
    );
    assert!(
        store
            .reader
            .search_pages_for_project(ws, proj, "second-version-marker".into(), 5, None)
            .await
            .unwrap()
            .is_empty(),
        "the deleted chain must leave no searchable FTS entry"
    );
}

#[tokio::test]
async fn hard_delete_preserves_a_page_recreated_at_the_same_path() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("workspace");
    let proj = store
        .writer
        .get_or_create_project(ws, "recreated", None)
        .await
        .expect("project");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_store_reader(store.reader.clone());
    let path = PagePath::new("sessions/recreated.md").unwrap();

    let write = |body: &str| WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: path.clone(),
        frontmatter: serde_json::json!({}),
        body: body.to_string(),
        tier: Tier::Episodic,
        pinned: false,
        title: Some("Recreated".into()),
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    };
    let first = wiki
        .write_page(write("# Recreated\nold one"))
        .await
        .unwrap();
    let second = wiki
        .write_page(write("# Recreated\nold two"))
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE pages SET updated_at = 1 WHERE id = ?1",
        params![second.as_bytes()],
    )
    .unwrap();
    drop(conn);

    let grace = run_sweep(
        &store.reader,
        &store.writer,
        Some(&wiki),
        ws,
        proj,
        &DecayParams {
            hard_delete_after_days: 30,
            ..DecayParams::default()
        },
        false,
    )
    .await
    .unwrap();
    assert!(grace.evicted[0].deleted);
    assert_eq!(grace.hard_deleted, 0);
    assert!(!wiki.abs_path(ws, proj, &path).exists());

    // Simulate an external editor recreating the authoritative file before
    // the watcher gets a chance to index it. Cleanup must notice and reindex
    // this file rather than treating the absent latest row as permission to
    // delete it.
    std::fs::write(
        wiki.abs_path(ws, proj, &path),
        "# Recreated\nnew-live-marker",
    )
    .unwrap();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE pages SET superseded_at = 1 WHERE id = ?1",
        params![second.as_bytes()],
    )
    .unwrap();
    drop(conn);

    let cleanup = run_sweep(
        &store.reader,
        &store.writer,
        Some(&wiki),
        ws,
        proj,
        &DecayParams {
            cold_threshold: 0.0,
            hard_delete_after_days: 0,
            ..DecayParams::default()
        },
        false,
    )
    .await
    .unwrap();
    assert!(cleanup.evicted.is_empty());
    assert_eq!(cleanup.hard_deleted, 2);
    assert!(wiki.abs_path(ws, proj, &path).exists());
    let recreated = store
        .reader
        .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
        .await
        .unwrap()
        .expect("the external recreation is indexed before cleanup");
    assert_eq!(
        store
            .reader
            .latest_page_id_by_ids(ws, proj, path.as_str().to_string())
            .await
            .unwrap(),
        Some(recreated),
    );

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let old_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM pages WHERE id IN (?1, ?2)",
            params![first.as_bytes(), second.as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_rows, 0);
    assert!(
        store
            .reader
            .search_pages_for_project(ws, proj, "new-live-marker".into(), 5, None)
            .await
            .unwrap()
            .iter()
            .any(|hit| hit.id == recreated),
        "the recreated page and its FTS entry must survive old-chain cleanup"
    );
}

fn ws_dir_for(tmp: &TempDir, ws: WorkspaceId, proj: ProjectId) -> std::path::PathBuf {
    tmp.path()
        .join("wiki")
        .join(ws.to_string())
        .join(proj.to_string())
}
