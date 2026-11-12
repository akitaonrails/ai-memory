//! The writer connection's transactions must start IMMEDIATE.
//!
//! A deferred transaction that has already read (taken a WAL read
//! snapshot) cannot wait through the busy handler: when another process
//! holds the write lock, the read-to-write upgrade fails at once with
//! `SQLITE_BUSY` ("database is locked"). In production an external
//! `PRAGMA wal_checkpoint(TRUNCATE)` ran while the server was up, and
//! `/hook/batch` items — whose admission reads the session row before
//! its first write — failed immediately despite `busy_timeout = 5000`.
//!
//! `Store::open` sets the writer connection's transaction behavior to
//! `IMMEDIATE`, so `BEGIN` takes the write lock up front and the busy
//! handler waits out the external lock. This test holds the write lock
//! with an independent connection (the external process's shape),
//! releases it while the writer operation is in flight, and proves the
//! operation survived by waiting ~300 ms instead of failing.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use ai_memory_core::{
    AgentKind, NewObservation, NewSession, ObservationKind, OwnerFilter, Sanitized, Sanitizer,
    SessionId,
};
use ai_memory_store::{HookSessionAdmission, Store};
use rusqlite::Connection;

#[tokio::test]
async fn writer_hook_admission_survives_external_write_lock_held_during_read_then_write() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("lockwait")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "lockwait".to_string(), None)
        .await
        .unwrap();

    // The external process's lock: an active write transaction, the
    // shape of an external checkpoint or sqlite3 shell holding the write
    // lock while the server runs.
    let db_path = tmp.path().join("db").join("memory.sqlite");
    let external = Connection::open(&db_path).unwrap();
    external.execute_batch("BEGIN IMMEDIATE;").unwrap();

    // Hold the lock for ~300 ms — well inside the writer connection's
    // 5000 ms busy timeout — then release it while the writer
    // operation is in flight.
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        external.execute_batch("COMMIT;").unwrap();
    });

    let session_id = SessionId::new();
    let session = NewSession {
        occurred_at: None,
        id: session_id,
        workspace_id: ws,
        project_id: proj,
        agent_kind: AgentKind::Codex,
        cwd: Some(PathBuf::from("/repo")),
        actor_user: None,
        origin: None,
    };
    let raw_obs = NewObservation {
        occurred_at: None,
        session_id,
        workspace_id: ws,
        project_id: proj,
        kind: ObservationKind::UserPrompt,
        extension: None,
        source_event: None,
        title: "checkpoint lock wait".into(),
        body: "the writer must wait out an external checkpoint".into(),
        importance: 5,
        file_path: None,
    };

    let started = Instant::now();
    // The hook-batch admission reads the session row (ops.rs,
    // `SELECT ... FROM sessions WHERE id = ?1`) before its first write,
    // so a deferred transaction here has a read snapshot to invalidate
    // at the upgrade.
    let admission = store
        .writer
        .admit_hook_session_event(
            session,
            Sanitized::new(raw_obs, &Sanitizer::builtin()),
            OwnerFilter::Any,
            None,
            None,
        )
        .await
        .expect(
            "a deferred transaction that already read fails with bare SQLITE_BUSY at the \
             upgrade; an immediate transaction waits through the busy handler",
        );
    let elapsed = started.elapsed();
    release.join().unwrap();

    assert!(
        matches!(admission, HookSessionAdmission::Observation { .. }),
        "the hook event is admitted once the external lock clears: {admission:?}"
    );
    // It waited out the held lock (~300 ms) instead of racing it: a
    // write that did not wait would have returned near-instantly.
    assert!(
        elapsed >= Duration::from_millis(200),
        "returned after {elapsed:?} without waiting out the external lock"
    );
    // And the data is on disk once the external lock cleared.
    assert!(
        store
            .reader
            .session_has_substantive_observation(ws, proj, session_id)
            .await
            .unwrap(),
        "the observation is committed once the external lock clears"
    );
}
