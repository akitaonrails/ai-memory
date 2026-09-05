//! The breadth term, end to end, for both readers of the retention formula.
//!
//! `pages.access_count` cannot tell "50 reads by one person" from "one read by
//! each of 50 people", although only the second says a page is load-bearing for
//! a team. `page_access` records that breakdown; these tests pin that the sweep
//! actually READS it — and that at the default `breadth_weight = 0.0` the
//! recorded breakdown moves neither the scores nor the eviction decisions.
//!
//! The curator is held to the same formula: its `cold_episodic` finding is a
//! prediction of what the sweep will evict, so a page the sweep keeps must not
//! be reported cold.

use ai_memory_consolidate::{
    CuratorParams, CuratorReport, run_curator_report_with_breadth, run_sweep,
    run_sweep_with_breadth,
};
use ai_memory_core::{IdentityKey, NewPage, PageId, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{DecayParams, Store, WriterHandle, retention_score};
use rusqlite::params;
use tempfile::TempDir;

const US_PER_DAY: i64 = 86_400_000_000;
/// Old enough and cold enough that the pair falls just under the default
/// `cold_threshold` — so any breadth bonus is what decides their fate.
const AGE_DAYS: i64 = 100;
const ACCESS_COUNT: u32 = 50;

/// Two otherwise identical episodic pages: `team.md` reinforced by five distinct
/// operators, `solo.md` by one. Same age, same total hit count, same last-access
/// timestamp, so the ONLY thing that can separate them is the breadth term.
async fn seed(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "breadth".to_string(), None)
        .await
        .expect("proj");

    let team = write_page(&store.writer, ws, proj, "sessions/team.md").await;
    let solo = write_page(&store.writer, ws, proj, "sessions/solo.md").await;

    // Actors are recorded under their qualified identity storage key, the same
    // TEXT `IdentityKey::storage_key()` produces on the read path.
    for actor in ["alice", "bob", "carol", "dave", "erin"] {
        store
            .writer
            .bump_access_for_actor(vec![team], Some(IdentityKey::User(actor.into())))
            .await
            .expect("bump team");
    }
    store
        .writer
        .bump_access_for_actor(vec![solo], Some(IdentityKey::User("alice".into())))
        .await
        .expect("bump solo");

    // Time-travel afterwards so both pages end up with byte-identical counters:
    // `updated_at == last_accessed_at` also makes `days_since_access` equal to
    // the sweep's reported `age_days`, which is what lets the assertions below
    // recompute the historical score exactly.
    let now_us = jiff::Timestamp::now().as_microsecond();
    let then_us = now_us - AGE_DAYS * US_PER_DAY;
    let conn = rusqlite::Connection::open(store.db_path()).expect("aux conn");
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    for id in [team, solo] {
        conn.execute(
            "UPDATE pages \
             SET created_at = ?1, updated_at = ?1, access_count = ?2, last_accessed_at = ?1 \
             WHERE id = ?3",
            params![then_us, i64::from(ACCESS_COUNT), id.as_bytes()],
        )
        .expect("backdate");
    }
    (ws, proj)
}

async fn write_page(writer: &WriterHandle, ws: WorkspaceId, proj: ProjectId, path: &str) -> PageId {
    writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path.to_string()).expect("path"),
            title: path.into(),
            body: "retention fixture".into(),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        })
        .await
        .expect("upsert")
}

/// The no-break proof: with the shipped defaults the sweep must score a page
/// read by five operators exactly as the pre-breadth formula scored it, and
/// evict it just the same. Recording `page_access` rows must not move a single
/// eviction decision on an existing database.
#[tokio::test]
async fn default_weight_scores_identically_however_many_operators_read_the_page() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let (ws, proj) = seed(&store).await;

    let params = DecayParams::default();
    let report = run_sweep(&store.reader, &store.writer, None, ws, proj, &params, true)
        .await
        .expect("sweep");

    let mut evicted: Vec<_> = report.evicted.iter().collect();
    evicted.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(
        evicted.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
        vec!["sessions/solo.md", "sessions/team.md"],
        "breadth must not save a page at the default weight",
    );
    for e in &evicted {
        assert_eq!(e.access_count, ACCESS_COUNT);
        assert_eq!(
            e.retention,
            retention_score(&params, e.age_days, e.access_count, Some(e.age_days), None),
            "{} scored differently from the pre-breadth formula",
            e.path,
        );
    }
    assert_eq!(
        evicted[0].retention, evicted[1].retention,
        "five readers and one reader must be indistinguishable at weight 0.0",
    );
}

/// And once an operator deliberately raises the weight, the breadth actually
/// reaches the score — which is the whole point of recording it.
#[tokio::test]
async fn a_page_many_operators_read_outlives_one_read_as_often_by_a_single_operator() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let (ws, proj) = seed(&store).await;

    let params = DecayParams::default();
    let report = run_sweep_with_breadth(
        &store.reader,
        &store.writer,
        None,
        ws,
        proj,
        &params,
        1.5,
        true,
    )
    .await
    .expect("sweep");

    assert_eq!(
        report
            .evicted
            .iter()
            .map(|e| e.path.as_str())
            .collect::<Vec<_>>(),
        vec!["sessions/solo.md"],
        "the team-read page should have been carried over the threshold",
    );
}

#[tokio::test]
async fn destructive_readers_reject_invalid_breadth_coefficients() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let (ws, proj) = seed(&store).await;
    let params = DecayParams::default();

    for weight in [-1.0, f64::NAN, f64::INFINITY] {
        let sweep_error = run_sweep_with_breadth(
            &store.reader,
            &store.writer,
            None,
            ws,
            proj,
            &params,
            weight,
            false,
        )
        .await
        .expect_err("an invalid coefficient must stop the sweep before mutation");
        assert!(sweep_error.to_string().contains("breadth_weight"));

        let curator_error = run_curator_report_with_breadth(
            &store.reader,
            ws,
            proj,
            "default",
            "breadth",
            CuratorParams {
                decay_params: params,
                ..CuratorParams::default()
            },
            weight,
        )
        .await
        .expect_err("the curator must reject the same invalid coefficient");
        assert!(curator_error.to_string().contains("breadth_weight"));
    }
}

/// `(path, score, age_days)` for every `cold_episodic` finding, sorted by path.
fn cold_findings(report: &CuratorReport) -> Vec<(String, f64, f64)> {
    let mut rows: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.kind == "cold_episodic")
        .map(|f| {
            let detail = f
                .detail
                .as_ref()
                .expect("cold findings carry a detail blob");
            (
                f.pages
                    .first()
                    .expect("cold finding names its page")
                    .clone(),
                detail["score"].as_f64().expect("score"),
                detail["age_days"].as_f64().expect("age_days"),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

async fn curate(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    decay: DecayParams,
    breadth_weight: f64,
) -> CuratorReport {
    run_curator_report_with_breadth(
        &store.reader,
        ws,
        proj,
        "default",
        "breadth",
        CuratorParams {
            decay_params: decay,
            ..CuratorParams::default()
        },
        breadth_weight,
    )
    .await
    .expect("curator")
}

/// The no-break proof for the curator: at the shipped default the recorded
/// `page_access` breakdown must not move a single reported score.
///
/// This one does not discriminate the fix — at `breadth_weight = 0.0` the
/// breadth-aware formula is the historical formula by construction — it exists
/// so a later change to the breadth term cannot silently alter default output.
#[tokio::test]
async fn curator_scores_match_the_pre_breadth_formula_at_the_default_weight() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let (ws, proj) = seed(&store).await;

    let params = DecayParams::default();
    let report = curate(&store, ws, proj, params, 0.0).await;

    let cold = cold_findings(&report);
    assert_eq!(
        cold.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        vec!["sessions/solo.md", "sessions/team.md"],
        "breadth must not spare a page at the default weight",
    );
    for (path, score, age_days) in &cold {
        assert_eq!(
            *score,
            retention_score(&params, *age_days, ACCESS_COUNT, Some(*age_days), None),
            "{path} scored differently from the pre-breadth formula",
        );
    }
    assert_eq!(
        cold[0].1, cold[1].1,
        "five readers and one reader must be indistinguishable at weight 0.0",
    );
}

/// The discriminating one. `cold_episodic` is a prediction of an eviction, so a
/// page the sweep carries over the threshold must not be reported cold: a
/// breadth-blind curator hands the operator a page that will never be evicted,
/// on a run of the very same `[decay]` parameters.
#[tokio::test]
async fn the_curator_reports_cold_exactly_when_the_sweep_would_evict() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let (ws, proj) = seed(&store).await;

    let params = DecayParams::default();
    let sweep = run_sweep_with_breadth(
        &store.reader,
        &store.writer,
        None,
        ws,
        proj,
        &params,
        1.5,
        true,
    )
    .await
    .expect("sweep");
    let cold = cold_findings(&curate(&store, ws, proj, params, 1.5).await);

    let mut evicted: Vec<&str> = sweep.evicted.iter().map(|e| e.path.as_str()).collect();
    evicted.sort_unstable();
    assert_eq!(
        evicted,
        vec!["sessions/solo.md"],
        "fixture must have the team page surviving, or there is nothing to disagree about",
    );
    assert_eq!(
        cold.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        evicted,
        "the curator flagged a page the sweep will never evict",
    );

    // Same page, same parameters, same score — the two must be reading the same
    // distinct-actor input, not merely reaching the same verdict by luck.
    let swept = &sweep.evicted[0];
    assert!(
        (cold[0].1 - swept.retention).abs() < 1e-6,
        "curator scored {} at {}, sweep at {}",
        cold[0].0,
        cold[0].1,
        swept.retention,
    );
}
