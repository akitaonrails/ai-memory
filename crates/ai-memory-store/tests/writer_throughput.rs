//! How much load the single writer actor absorbs before it becomes the
//! bottleneck — measured, not estimated.
//!
//! All writes funnel through one actor over a bounded `mpsc` channel, which is
//! the right design for SQLite but does make one question worth answering with
//! a number: at what point does a shared server stop keeping up?
//!
//! Run it on demand; it is `#[ignore]`d so a throughput measurement never
//! becomes a flaky CI gate on a loaded runner:
//!
//! ```text
//! cargo test -p ai-memory-store --test writer_throughput -- --ignored --nocapture
//! ```

use std::time::Instant;

use ai_memory_core::{
    AgentKind, NewObservation, NewSession, ObservationKind, ProjectId, Sanitized, Sanitizer,
    SessionId, WorkspaceId,
};
use ai_memory_store::Store;

async fn scope(store: &Store) -> (WorkspaceId, ProjectId, SessionId) {
    let ws = store
        .writer
        .get_or_create_workspace("bench".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "load".to_string(), None)
        .await
        .unwrap();
    let session_id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    (ws, proj, session_id)
}

fn observation(
    ws: WorkspaceId,
    proj: ProjectId,
    session_id: SessionId,
    n: usize,
) -> Sanitized<NewObservation> {
    Sanitized::new(
        NewObservation {
            session_id,
            workspace_id: ws,
            project_id: proj,
            kind: ObservationKind::PostToolUse,
            extension: None,
            source_event: None,
            title: format!("tool call {n}"),
            body: "ran a command and captured a bounded excerpt of its output".into(),
            importance: 5,
        },
        &Sanitizer::builtin(),
    )
}

/// Sustained throughput of the hot path — the write every tool call performs.
///
/// Reported as observations/second plus the implied budget in concurrent
/// agents, taking one tool call per agent-second as a deliberately pessimistic
/// stand-in for an actively working harness.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "throughput measurement; run explicitly"]
async fn writer_throughput_under_concurrent_load() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj, session_id) = scope(&store).await;

    for writers in [1usize, 8, 32, 128] {
        const PER_WRITER: usize = 200;
        let total = writers * PER_WRITER;

        let started = Instant::now();
        let mut tasks = Vec::with_capacity(writers);
        for _ in 0..writers {
            let writer = store.writer.clone();
            tasks.push(tokio::spawn(async move {
                for n in 0..PER_WRITER {
                    writer
                        .insert_observation(observation(ws, proj, session_id, n))
                        .await
                        .unwrap();
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let elapsed = started.elapsed();

        let per_sec = total as f64 / elapsed.as_secs_f64();
        let mean_ms = elapsed.as_secs_f64() * 1000.0 / total as f64;
        println!(
            "writers={writers:>4}  writes={total:>6}  {elapsed:>10.2?}  \
             {per_sec:>9.0}/s  mean {mean_ms:>6.3}ms  \
             ≈{per_sec:>7.0} concurrent agents at 1 tool-call/s"
        );
    }
}

/// The queue is bounded at 1024 with `send().await`, so a burst larger than the
/// queue applies backpressure rather than dropping work or growing without
/// limit. This asserts the property that matters operationally: every write in
/// an over-queue burst lands.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_larger_than_the_queue_applies_backpressure_and_loses_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj, session_id) = scope(&store).await;

    // Deliberately more than the 1024-deep channel.
    const BURST: usize = 1500;
    let mut tasks = Vec::with_capacity(BURST);
    for n in 0..BURST {
        let writer = store.writer.clone();
        tasks.push(tokio::spawn(async move {
            writer
                .insert_observation(observation(ws, proj, session_id, n))
                .await
                .unwrap()
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    let status = store.reader.derived_index_status().await.unwrap();
    assert_eq!(
        status.observations_rows, BURST as u64,
        "every write in a burst larger than the queue must land: backpressure, \
         never a silent drop"
    );
}
