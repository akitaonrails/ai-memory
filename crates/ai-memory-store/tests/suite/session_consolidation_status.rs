//! The consolidation queue as `status` and the health panel read it (#1692).
//!
//! `SessionConsolidationQueue` stays counts-and-timestamps only — it feeds the
//! web health panel — while `failed_consolidation_jobs` is the operator-facing
//! companion on `/admin/status`: bounded, newest first, and carrying the
//! session id a re-consolidation needs.

use ai_memory_core::{AgentKind, NewSession, ProjectId, SessionId, WorkspaceId};
use ai_memory_store::Store;
use rusqlite::params;

fn workspace_and_project(store: &Store) -> (WorkspaceId, ProjectId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        (ws, proj)
    })
}

/// One ended session with a consolidation job in `state`.
fn seed_job(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    state: &str,
    attempts: i64,
    completed_at: Option<i64>,
    last_error: Option<&str>,
) -> SessionId {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let session = SessionId::new();
    rt.block_on(async {
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: session,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Codex,
                cwd: Some("/repo".into()),
                actor_user: None,
            })
            .await
            .unwrap();
    });
    seed_generation(
        store,
        session,
        (ws, proj),
        1,
        state,
        attempts,
        completed_at,
        last_error,
    );
    session
}

/// One more job row, `generation`, for an existing `session`.
#[allow(clippy::too_many_arguments)]
fn seed_generation(
    store: &Store,
    session: SessionId,
    (ws, proj): (WorkspaceId, ProjectId),
    generation: i64,
    state: &str,
    attempts: i64,
    completed_at: Option<i64>,
    last_error: Option<&str>,
) {
    rusqlite::Connection::open(store.db_path())
        .unwrap()
        .execute(
            "INSERT INTO session_consolidation_jobs \
             (session_id, workspace_id, project_id, generation, state, requested_at, \
              next_attempt_at, attempts, completed_at, last_error) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?7, ?8)",
            params![
                session.as_bytes().to_vec(),
                ws.as_bytes().to_vec(),
                proj.as_bytes().to_vec(),
                generation,
                state,
                attempts,
                completed_at,
                last_error
            ],
        )
        .unwrap();
}

#[test]
fn failed_jobs_are_newest_first_bounded_and_never_a_success() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = workspace_and_project(&store);

    let older = seed_job(
        &store,
        ws,
        proj,
        "failed",
        5,
        Some(1_000),
        Some("provider error 400: quota exceeded"),
    );
    let newer = seed_job(
        &store,
        ws,
        proj,
        "failed",
        5,
        Some(2_000),
        Some("provider error 429: rate limited"),
    );
    // Everything that is not a final failure stays out of the list.
    seed_job(&store, ws, proj, "completed", 1, Some(3_000), None);
    seed_job(&store, ws, proj, "pending", 0, None, None);
    seed_job(&store, ws, proj, "running", 1, None, None);
    seed_job(&store, ws, proj, "superseded", 0, None, None);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let jobs = rt
        .block_on(store.reader.failed_consolidation_jobs(20))
        .unwrap();

    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert_eq!(jobs[0].session_id, newer.to_string(), "newest first");
    assert_eq!(jobs[1].session_id, older.to_string());
    assert_eq!(jobs[0].attempts, 5);
    assert_eq!(jobs[0].failed_at_us, 2_000);
    assert!(jobs[0].last_error.contains("quota") || jobs[0].last_error.contains("rate limited"));

    // The cap is honoured and still answers the newest entries.
    let capped = rt
        .block_on(store.reader.failed_consolidation_jobs(1))
        .unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].session_id, newer.to_string());
}

/// The health panel's own view keeps the property the new list must not
/// erode: no session ids and no provider text.
#[test]
fn the_health_queue_stays_counts_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = workspace_and_project(&store);
    seed_job(
        &store,
        ws,
        proj,
        "failed",
        5,
        Some(1_000),
        Some("SECRET_SENTINEL provider body"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let queue = rt
        .block_on(store.reader.session_consolidation_queue())
        .unwrap();
    assert_eq!(queue.failed, 1);
    let rendered = serde_json::to_string(&queue).unwrap();
    assert!(!rendered.contains("SECRET_SENTINEL"), "{rendered}");
    assert!(!rendered.contains("session"), "{rendered}");
}

/// A failure a newer generation of the same session has since completed is
/// not a failure any more: the newer page covers the failed run's
/// observations. Neither the count nor the list may report it, or a #1692
/// watchdog re-consolidates a session that is already consolidated.
#[test]
fn a_failure_a_newer_generation_completed_is_not_reported() {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = workspace_and_project(&store);

    // Failed at generation 1, completed at generation 2: recovered.
    let recovered = seed_job(
        &store,
        ws,
        proj,
        "failed",
        1,
        Some(3_000),
        Some("all candidates failed"),
    );
    seed_generation(
        &store,
        recovered,
        (ws, proj),
        2,
        "completed",
        1,
        Some(4_000),
        None,
    );
    // Completed at generation 1, failed at generation 2: the newest
    // observations were never consolidated, so it is still a failure.
    let regressed = seed_job(&store, ws, proj, "completed", 1, Some(1_000), None);
    seed_generation(
        &store,
        regressed,
        (ws, proj),
        2,
        "failed",
        5,
        Some(2_000),
        Some("provider error 400: quota exceeded"),
    );
    // Failed at generation 1 with nothing after it.
    let unrecovered = seed_job(
        &store,
        ws,
        proj,
        "failed",
        5,
        Some(1_500),
        Some("provider error 429: rate limited"),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let jobs = rt
        .block_on(store.reader.failed_consolidation_jobs(20))
        .unwrap();
    let listed: Vec<&str> = jobs.iter().map(|job| job.session_id.as_str()).collect();
    assert_eq!(
        listed,
        [regressed.to_string(), unrecovered.to_string()],
        "recovered session {recovered} must not be listed"
    );

    let queue = rt
        .block_on(store.reader.session_consolidation_queue())
        .unwrap();
    assert_eq!(queue.failed, 2);
    assert_eq!(queue.completed, 2);
}
