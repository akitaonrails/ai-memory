//! The raw-observation retention prune and its dry-run count, held to the
//! queued-consolidation exclusion.
//!
//! A session that resumed after its first summary gets a new
//! `session_consolidation_jobs` row (`pending` or `running`) while its old
//! summary page is still live. The prune predicate (live summary page + old
//! rows) matches such a session, and without the exclusion the prune would
//! delete the raw capture before the queued job reads it, leaving the job to
//! consolidate a truncated session. These tests pin both halves of the fix:
//! the delete leaves every observation of a session with a claimable job in
//! place while still pruning a sibling session in the same batch, and the
//! dry-run count — which shares the one SQL fragment
//! (`ops::OBSERVATION_PRUNE_SESSION_GUARD_SQL`) with the delete — equals what the
//! prune deletes in every job state.

use ai_memory_core::{ProjectId, SessionId, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::{Connection, params};

/// A fixed reference instant in microseconds. Every observation lands before
/// the cutoff used below, so age is never what separates the fixtures.
const NOW: i64 = 1_700_000_000_000_000;
/// Far past every `created_at` seeded below: all rows pass the age gate.
const CUTOFF: i64 = NOW + 100;
const OBSERVATIONS_PER_SESSION: i64 = 3;

fn id(n: u8) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[15] = n;
    b
}

fn ws() -> WorkspaceId {
    WorkspaceId::from_slice(&id(1)).unwrap()
}

fn proj() -> ProjectId {
    ProjectId::from_slice(&id(2)).unwrap()
}

/// Session 10 carries the consolidation job under test; session 11 is the
/// same-project control that never has a job.
fn s1() -> SessionId {
    SessionId::from_slice(&id(10)).unwrap()
}

fn s2() -> SessionId {
    SessionId::from_slice(&id(11)).unwrap()
}

/// Two ended sessions, each distilled into its own live summary page and
/// holding three old observations, with `ended_observation_count` recording
/// the three the way the session-end marker does.
fn seed(conn: &Connection) {
    let (ws, proj) = (id(1), id(2));

    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, 'w', ?2)",
        params![&ws[..], NOW],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![&proj[..], &ws[..], "proj", "/w/proj", NOW],
    )
    .unwrap();

    for (n, page_n) in [(10, 100), (11, 101)] {
        let path = format!("sessions/s{n}.md");
        conn.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?4, 'episodic', 'distilled', ?5, '{}', 1, ?6, ?6)",
            params![
                &id(page_n)[..],
                &ws[..],
                &proj[..],
                path,
                &[0u8; 32][..],
                NOW,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions \
             (id, workspace_id, project_id, agent_kind, cwd, started_at, ended_at, \
              summary_page_id, ended_observation_count) \
             VALUES (?1, ?2, ?3, 'claude-code', '/w/proj', ?4, ?5, ?6, 3)",
            params![
                &id(n)[..],
                &ws[..],
                &proj[..],
                NOW + i64::from(n),
                NOW + 50 + i64::from(n),
                &id(page_n)[..],
            ],
        )
        .unwrap();
        for i in 1..=OBSERVATIONS_PER_SESSION {
            let obs_n = 30u8 + (n - 10) * 3 + i as u8;
            conn.execute(
                "INSERT INTO observations \
                 (id, session_id, workspace_id, project_id, kind, title, body, importance, \
                  created_at) \
                 VALUES (?1, ?2, ?3, ?4, 'user-prompt', ?5, ?6, 7, ?7)",
                params![
                    &id(obs_n)[..],
                    &id(n)[..],
                    &ws[..],
                    &proj[..],
                    format!("obs {n}-{i}"),
                    "old capture",
                    NOW + i,
                ],
            )
            .unwrap();
        }
    }
}

/// Insert session 10's consolidation job in the given state, shaped the way
/// the queue writes it: `next_attempt_at` for `pending`, `started_at` and
/// `claim_id` for `running`.
fn queue_job(conn: &Connection, state: &str) {
    let (ws, proj) = (id(1), id(2));
    conn.execute(
        "INSERT INTO session_consolidation_jobs \
         (session_id, workspace_id, project_id, generation, state, requested_at, \
          next_attempt_at, started_at, claim_id) \
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5, \
                 CASE WHEN ?4 = 'running' THEN ?5 ELSE NULL END, \
                 CASE WHEN ?4 = 'running' THEN ?6 ELSE NULL END)",
        params![&id(10)[..], &ws[..], &proj[..], state, NOW, &[1u8; 16][..]],
    )
    .unwrap();
}

/// Everything the assertions need: the dry-run count before the prune, the
/// prune outcome, and the row counts and watermarks after it.
struct Measured {
    count_before: usize,
    count_after: usize,
    deleted: usize,
    touched: Vec<SessionId>,
    resynced: usize,
    s1_rows: i64,
    s2_rows: i64,
    s1_watermark: i64,
    s2_watermark: i64,
}

async fn measure(store: &Store) -> Measured {
    let count_before = store
        .reader
        .prunable_observation_count(ws(), proj(), CUTOFF)
        .await
        .unwrap();
    let outcome = store
        .writer
        .prune_consolidated_observations(ws(), proj(), CUTOFF, 100)
        .await
        .unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    let rows = |sid: SessionId| {
        conn.query_row(
            "SELECT COUNT(*) FROM observations WHERE session_id = ?1",
            params![sid.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    let watermark = |sid: SessionId| {
        conn.query_row(
            "SELECT ended_observation_count FROM sessions WHERE id = ?1",
            params![sid.as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    };
    let count_after = store
        .reader
        .prunable_observation_count(ws(), proj(), CUTOFF)
        .await
        .unwrap();
    Measured {
        count_before,
        count_after,
        deleted: outcome.deleted,
        touched: outcome.sessions_touched,
        resynced: outcome.sessions_resynced,
        s1_rows: rows(s1()),
        s2_rows: rows(s2()),
        s1_watermark: watermark(s1()),
        s2_watermark: watermark(s2()),
    }
}

fn open_seeded(state: Option<&str>) -> (tempfile::TempDir, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    seed(&conn);
    if let Some(state) = state {
        queue_job(&conn, state);
    }
    (tmp, store)
}

/// What each job state must yield. `pending` and `running` protect session 10
/// (the count and the delete both see only session 11's three rows); every
/// other state — and no job at all — prunes both sessions.
fn expect(state: Option<&str>) -> (bool, usize) {
    match state {
        Some("pending") | Some("running") => (true, 3),
        Some("completed") | Some("failed") | Some("superseded") | None => (false, 6),
        other => panic!("unexpected job state {other:?}"),
    }
}

fn assert_case(m: &Measured, state: Option<&str>) {
    let (protected, prunable) = expect(state);
    assert_eq!(m.count_before, prunable, "dry-run count for {state:?}");
    assert_eq!(m.deleted, prunable, "rows deleted for {state:?}");
    assert_eq!(
        m.s1_rows,
        if protected { 3 } else { 0 },
        "session 10 rows after the prune for {state:?}"
    );
    assert_eq!(
        m.s2_rows, 0,
        "the job-less session 11 is pruned in the same batch for {state:?}"
    );
    // The watermark resync covers exactly the sessions that lost rows.
    assert_eq!(
        m.s1_watermark,
        if protected { 3 } else { 0 },
        "session 10 watermark for {state:?}"
    );
    assert_eq!(m.s2_watermark, 0, "session 11 watermark for {state:?}");
    if protected {
        assert_eq!(
            m.touched,
            vec![s2()],
            "only the job-less session may be touched for {state:?}"
        );
        assert_eq!(
            m.resynced, 1,
            "only the job-less session resyncs for {state:?}"
        );
    } else {
        assert_eq!(m.touched, vec![s1(), s2()], "touched for {state:?}");
        assert_eq!(m.resynced, 2, "resynced for {state:?}");
    }
    assert_eq!(
        m.count_after, 0,
        "nothing left for the dry run to report after the prune for {state:?}"
    );
}

/// A session that still has a `pending` consolidation job keeps every
/// observation, however old, while its job-less same-project sibling is
/// pruned by the very same batch.
#[tokio::test]
async fn a_pending_job_protects_a_consolidated_sessions_observations() {
    let (_tmp, store) = open_seeded(Some("pending"));
    assert_case(&measure(&store).await, Some("pending"));
}

/// The same protection applies to a `running` job: the worker may still be
/// inside its lease (or about to reclaim a stale one) and read the raw
/// capture.
#[tokio::test]
async fn a_running_job_protects_a_consolidated_sessions_observations() {
    let (_tmp, store) = open_seeded(Some("running"));
    assert_case(&measure(&store).await, Some("running"));
}

/// Capture can resume after a session ended but before its next checkpoint or
/// SessionEnd enqueues a consolidation job. The persisted end generation then
/// trails the live observation count, which must protect the new raw rows.
#[tokio::test]
async fn resumed_capture_is_protected_before_its_next_job_exists() {
    let (_tmp, store) = open_seeded(None);
    let conn = Connection::open(store.db_path()).unwrap();
    conn.execute(
        "UPDATE sessions SET ended_observation_count = 2 WHERE id = ?1",
        params![s1().as_bytes()],
    )
    .unwrap();

    let m = measure(&store).await;
    assert_eq!(m.count_before, 3);
    assert_eq!(m.deleted, 3);
    assert_eq!(m.s1_rows, 3, "resumed capture must remain intact");
    assert_eq!(m.s1_watermark, 2, "the prior end generation is unchanged");
    assert_eq!(m.s2_rows, 0, "the fully ended control is still pruned");
    assert_eq!(m.s2_watermark, 0);
    assert_eq!(m.touched, vec![s2()]);
    assert_eq!(m.resynced, 1);
    assert_eq!(m.count_after, 0);
}

/// `completed`, `failed` and `superseded` are terminal for the automatic
/// worker: each control prunes both sessions exactly as before the fix.
#[tokio::test]
async fn a_terminal_job_state_does_not_protect_a_consolidated_session() {
    for state in ["completed", "failed", "superseded"] {
        let (_tmp, store) = open_seeded(Some(state));
        assert_case(&measure(&store).await, Some(state));
    }
}

/// The dry run and the real pass share one SQL fragment, so the count must
/// equal what the prune deletes — and only that — in every job state.
#[tokio::test]
async fn the_dry_run_count_equals_the_prune_deletes_in_every_job_state() {
    for state in [
        None,
        Some("pending"),
        Some("running"),
        Some("completed"),
        Some("failed"),
        Some("superseded"),
    ] {
        let (_tmp, store) = open_seeded(state);
        let m = measure(&store).await;
        assert_eq!(
            m.count_before, m.deleted,
            "dry-run count must equal the prune for {state:?}"
        );
        assert_case(&m, state);
    }
}

/// Start a durable processing job for session 10 and force it into `state`,
/// the way a worker's lifecycle would leave it.
async fn processing_job_in_state(store: &Store, state: &str) {
    use ai_memory_core::{ActorContext, ProcessingJobKind};
    use ai_memory_store::NewProcessingJob;

    store
        .writer
        .create_processing_job(NewProcessingJob {
            workspace_id: ws(),
            project_id: proj(),
            session_id: s1(),
            kind: ProcessingJobKind::Consolidate,
            actor: ActorContext::default(),
            author_id: None,
            viewer: None,
            owner: None,
            options_json: serde_json::json!({}),
            context_fingerprint: "ctx".into(),
            max_calls: 8,
        })
        .await
        .unwrap();
    let conn = Connection::open(store.db_path()).unwrap();
    conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
    conn.execute(
        "UPDATE processing_jobs SET state = ?1 WHERE session_id = ?2",
        params![state, s1().as_bytes()],
    )
    .unwrap();
}

/// A chunked job reads the whole stored session over many provider calls, so
/// every state that can still read it (including a paused or budget-limited
/// one that resumes later) keeps the raw capture, while the job-less sibling
/// is pruned in the same batch and the dry run agrees with the delete.
#[tokio::test]
async fn an_unfinished_processing_job_protects_the_session_it_will_read() {
    for state in [
        "queued",
        "running",
        "reducing",
        "publishing",
        "paused",
        "budget_exceeded",
    ] {
        let (_tmp, store) = open_seeded(None);
        processing_job_in_state(&store, state).await;
        let m = measure(&store).await;
        assert_eq!(
            m.count_before, m.deleted,
            "dry run equals prune for {state}"
        );
        assert_case(&m, Some("pending"));
    }
}

/// Legitimate control: a finished, failed or invalidated job reads nothing
/// any more, so it must not pin the raw capture forever.
#[tokio::test]
async fn a_finished_processing_job_does_not_pin_the_session() {
    for state in ["completed", "failed", "invalidated"] {
        let (_tmp, store) = open_seeded(None);
        processing_job_in_state(&store, state).await;
        let m = measure(&store).await;
        assert_eq!(
            m.count_before, m.deleted,
            "dry run equals prune for {state}"
        );
        assert_case(&m, None);
    }
}
