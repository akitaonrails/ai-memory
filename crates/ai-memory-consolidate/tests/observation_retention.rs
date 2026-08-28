//! The opt-in observation prune, end to end.
//!
//! Raw observations are ~98% of a mature store's bytes, and their durable value
//! was already distilled into pages. Deleting them is safe exactly when that
//! distillation still exists — so these tests hold the pass to the only
//! discrimination that makes it safe: a session consolidated into a live page
//! loses its old raw capture; a session with equally old capture and no page,
//! or with a page decay already evicted, keeps every row.
//!
//! They also pin the two properties an operator has to be able to trust without
//! reading the SQL: nothing is deleted at the shipped defaults, and the FTS
//! index stops matching what the prune removed.

use ai_memory_consolidate::{ObservationRetention, run_sweep, run_sweep_with_options};
use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, ObservationKind, PageId, PagePath, ProjectId,
    Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_store::{DecayParams, Store, WriterHandle};
use rusqlite::{Connection, params};
use tempfile::TempDir;

const US_PER_DAY: i64 = 86_400_000_000;
/// Well past any retention age used below, so age is never what separates the
/// fixtures — only whether the session was consolidated.
const OBS_AGE_DAYS: i64 = 400;
const RETENTION_DAYS: i64 = 90;

/// A session whose observations the prune is allowed to delete: ended, with a
/// live summary page.
const CONSOLIDATED_BODY: &str = "consolidated capture zebrafinch";
/// A session with identical, equally old capture that was never consolidated.
const RAW_BODY: &str = "unconsolidated capture zebrafinch";
/// A session that WAS consolidated, but whose page decay has since evicted —
/// the raw rows are now the only surviving copy.
const EVICTED_BODY: &str = "evicted capture zebrafinch";

struct Fixture {
    ws: WorkspaceId,
    proj: ProjectId,
    consolidated: SessionId,
    unconsolidated: SessionId,
    evicted: SessionId,
}

/// Three sessions x three observations each, all backdated to the same age.
async fn seed(store: &Store) -> Fixture {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "retention".to_string(), None)
        .await
        .expect("proj");

    let consolidated = session(store, ws, proj, 1, CONSOLIDATED_BODY).await;
    let unconsolidated = session(store, ws, proj, 2, RAW_BODY).await;
    let evicted = session(store, ws, proj, 3, EVICTED_BODY).await;

    // Only the first two ends link a page; the third's page is evicted below.
    let page = write_page(&store.writer, ws, proj, "sessions/consolidated.md").await;
    store
        .writer
        .end_session(consolidated, Some(page))
        .await
        .expect("end consolidated");
    let evicted_page = write_page(&store.writer, ws, proj, "sessions/evicted.md").await;
    store
        .writer
        .end_session(evicted, Some(evicted_page))
        .await
        .expect("end evicted");

    let conn = aux(store);
    // Decay eviction, exactly as `soft_delete_for_decay_if_latest` writes it:
    // the row survives as a tombstone but the Markdown is gone.
    conn.execute(
        "UPDATE pages SET is_latest = 0, superseded_at = ?1 WHERE id = ?2",
        params![
            jiff::Timestamp::now().as_microsecond(),
            evicted_page.as_bytes()
        ],
    )
    .expect("evict page");
    let then_us = jiff::Timestamp::now().as_microsecond() - OBS_AGE_DAYS * US_PER_DAY;
    conn.execute("UPDATE observations SET created_at = ?1", params![then_us])
        .expect("backdate observations");

    Fixture {
        ws,
        proj,
        consolidated,
        unconsolidated,
        evicted,
    }
}

async fn session(store: &Store, ws: WorkspaceId, proj: ProjectId, n: u8, body: &str) -> SessionId {
    let mut raw = [0u8; 16];
    raw[15] = n;
    let id = SessionId::from_slice(&raw).expect("session id");
    store
        .writer
        .begin_session(NewSession {
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
        })
        .await
        .expect("begin session");
    let sanitizer = Sanitizer::builtin();
    for i in 0..3 {
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id: id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: format!("obs {n}-{i}"),
                    body: body.to_string(),
                    importance: 5,
                },
                &sanitizer,
            ))
            .await
            .expect("insert observation");
    }
    id
}

async fn write_page(writer: &WriterHandle, ws: WorkspaceId, proj: ProjectId, path: &str) -> PageId {
    writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path.to_string()).expect("path"),
            title: path.into(),
            body: "distilled".into(),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        })
        .await
        .expect("upsert page")
}

fn aux(store: &Store) -> Connection {
    let conn = Connection::open(store.db_path()).expect("aux conn");
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    conn
}

fn observations_for(conn: &Connection, session: SessionId) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
        params![session.as_bytes()],
        |row| row.get(0),
    )
    .expect("count observations")
}

fn fts_matches(conn: &Connection, needle: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM observations_fts WHERE observations_fts MATCH ?1",
        params![needle],
        |row| row.get(0),
    )
    .expect("fts match")
}

fn enabled() -> ObservationRetention {
    ObservationRetention {
        days: RETENTION_DAYS,
        batch: 5_000,
    }
}

/// The no-change proof. `run_sweep` is what every existing caller reaches, and
/// `ObservationRetention::default()` is what the shipped config produces. On a
/// store full of ancient consolidated capture, both must delete nothing at all.
#[tokio::test]
async fn the_default_configuration_prunes_no_observation() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;
    let params = DecayParams::default();

    let report = run_sweep(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &params,
        false,
    )
    .await
    .expect("sweep");
    assert_eq!(report.observations_pruned, 0);
    assert_eq!(
        report.observations_prunable, 0,
        "disabled must not even count"
    );
    assert_eq!(report.observation_prune_batches, 0);

    let report = run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &params,
        0.0,
        ObservationRetention::default(),
        false,
    )
    .await
    .expect("sweep");
    assert_eq!(report.observations_pruned, 0);

    let conn = aux(&store);
    for s in [f.consolidated, f.unconsolidated, f.evicted] {
        assert_eq!(
            observations_for(&conn, s),
            3,
            "the default configuration must leave every observation in place"
        );
    }
}

/// The discrimination the whole feature rests on. Same scope, same age, same
/// body length: the ONLY difference is whether the session's work still exists
/// as a page. Consolidated loses its rows; unconsolidated and decay-evicted
/// keep theirs.
#[tokio::test]
async fn only_a_consolidated_session_with_a_live_page_loses_its_observations() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;

    let report = run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        enabled(),
        false,
    )
    .await
    .expect("sweep");

    assert_eq!(report.observations_pruned, 3);
    assert_eq!(report.observations_prunable, 3);
    assert_eq!(report.observation_prune_sessions, 1);

    let conn = aux(&store);
    assert_eq!(
        observations_for(&conn, f.consolidated),
        0,
        "a consolidated session's old raw capture must be pruned"
    );
    assert_eq!(
        observations_for(&conn, f.unconsolidated),
        3,
        "a session never consolidated into a page must keep every row"
    );
    assert_eq!(
        observations_for(&conn, f.evicted),
        3,
        "raw capture whose summary page decay already evicted is the last copy"
    );
    assert_eq!(
        conn.query_row(
            "SELECT detail FROM audit_log WHERE op = 'prune_observations'",
            [],
            |r| r.get::<_, String>(0)
        )
        .expect("exactly one audit row for the single deleting batch"),
        "{\"deleted\":3}",
        "the audit row must record how many rows the batch deleted"
    );
}

/// `observations_fts` is `content='observations'`, so a stale index would keep
/// returning hits for rows that no longer exist. The `observations_fts_ad`
/// trigger is supposed to prevent that; assert it rather than trust it.
#[tokio::test]
async fn the_fts_index_stops_matching_pruned_bodies() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;
    let conn = aux(&store);

    assert_eq!(fts_matches(&conn, "consolidated"), 3);
    assert_eq!(fts_matches(&conn, "zebrafinch"), 9);

    run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        enabled(),
        false,
    )
    .await
    .expect("sweep");

    assert_eq!(
        fts_matches(&conn, "consolidated"),
        0,
        "the FTS index must no longer match a pruned body"
    );
    assert_eq!(
        fts_matches(&conn, "zebrafinch"),
        6,
        "only the pruned rows may leave the index"
    );
    // The drift check behind `fts_drift_status` compares these two directly.
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM observations", [], |r| r.get(0))
        .unwrap();
    let docsize: i64 = conn
        .query_row("SELECT COUNT(*) FROM observations_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, docsize, "FTS docsize must track the delete exactly");
}

/// A dry run must be able to answer "how much would this delete?" without
/// opening a write transaction. Counting is the whole point; deleting is not.
#[tokio::test]
async fn a_dry_run_counts_without_deleting() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;

    let report = run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        enabled(),
        true,
    )
    .await
    .expect("sweep");

    assert_eq!(report.observations_prunable, 3);
    assert_eq!(report.observations_pruned, 0);
    assert_eq!(report.observation_prune_batches, 0);
    let conn = aux(&store);
    assert_eq!(observations_for(&conn, f.consolidated), 3);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM audit_log WHERE op = 'prune_observations'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "a dry run must not write an audit row either"
    );
}

/// The bound that keeps a multi-million row delete off the write lock: one
/// transaction per batch, looping until a short batch. With `batch = 1` the
/// three eligible rows must cost four batches (three full, one short) and still
/// land completely.
#[tokio::test]
async fn the_prune_deletes_in_bounded_batches() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;

    let report = run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention {
            days: RETENTION_DAYS,
            batch: 1,
        },
        false,
    )
    .await
    .expect("sweep");

    assert_eq!(report.observations_pruned, 3);
    assert_eq!(
        report.observation_prune_batches, 4,
        "each row is its own transaction, plus the short batch that ends the loop"
    );
    assert_eq!(
        report.observation_prune_sessions, 1,
        "a session split across batches must be counted once"
    );
    let conn = aux(&store);
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM audit_log WHERE op = 'prune_observations'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3,
        "one audit row per batch that actually deleted, none for the empty one"
    );
}

/// `session_end_disposition` reads `sessions.ended_observation_count` against a
/// live `COUNT(*)`. Prune without repairing that watermark and a resumed
/// session's genuinely new work reads as `AlreadyEnded` forever.
#[tokio::test]
async fn the_session_end_watermark_follows_the_prune() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;
    let conn = aux(&store);

    let watermark = |conn: &Connection, s: SessionId| -> i64 {
        conn.query_row(
            "SELECT ended_observation_count FROM sessions WHERE id = ?1",
            params![s.as_bytes()],
            |row| row.get(0),
        )
        .expect("watermark")
    };
    assert_eq!(watermark(&conn, f.consolidated), 3);

    run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        enabled(),
        false,
    )
    .await
    .expect("sweep");

    assert_eq!(
        watermark(&conn, f.consolidated),
        0,
        "the watermark must come back down to the surviving row count"
    );
    // Idempotent and one-directional: a second sweep with nothing left to
    // delete must not touch it again.
    run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        enabled(),
        false,
    )
    .await
    .expect("second sweep");
    assert_eq!(watermark(&conn, f.consolidated), 0);
}

/// Observations younger than the retention age are never eligible, however
/// thoroughly their session was consolidated — the age gate is what keeps
/// consolidation, auto-improve review and the `raw_hits` fallback whole.
#[tokio::test]
async fn recent_observations_survive_a_consolidated_session() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;
    let conn = aux(&store);
    let recent = jiff::Timestamp::now().as_microsecond() - US_PER_DAY;
    conn.execute(
        "UPDATE observations SET created_at = ?1 WHERE session_id = ?2",
        params![recent, f.consolidated.as_bytes()],
    )
    .expect("freshen");

    let report = run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        enabled(),
        false,
    )
    .await
    .expect("sweep");

    assert_eq!(report.observations_pruned, 0);
    assert_eq!(observations_for(&conn, f.consolidated), 3);
}

/// A negative age is a nonsensical cutoff and must stop the sweep before it
/// mutates anything, exactly as an invalid breadth coefficient does.
#[tokio::test]
async fn a_negative_retention_age_stops_the_sweep() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let f = seed(&store).await;

    let error = run_sweep_with_options(
        &store.reader,
        &store.writer,
        None,
        f.ws,
        f.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention {
            days: -1,
            batch: 5_000,
        },
        false,
    )
    .await
    .expect_err("a negative retention age must fail closed");
    assert!(error.to_string().contains("observation_retention_days"));
}
