//! Durable session-processing jobs: manifest snapshots, owner/scope reads,
//! coalescing, fenced claims, restart-safe call reservation, and the
//! purge/move/pause lifecycle.

use std::time::Duration;

use ai_memory_core::{
    ActorContext, AgentKind, IdentityKey, NewObservation, NewPage, NewSession, ObservationKind,
    PagePath, ProcessingJobId, ProcessingJobKind, ProcessingJobState, ProjectId, Sanitized,
    Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_store::processing_jobs::{
    PROCESSING_MAX_UNFINISHED_JOBS, PROCESSING_MAX_UNFINISHED_JOBS_PER_OWNER,
};
use ai_memory_store::{
    AutoImproveProposalOperation, CallReservation, ClaimedProcessingJob, Compaction,
    NewAutoImproveProposal, NewProcessingJob, PagesMode, ProcessingCheckpoint, ProcessingNodeState,
    ProcessingNodeWrite, ProcessingOutcome, ProcessingOwnerFilter, PurgeMode, StageAutoImproveRun,
    Store, StoreError,
};
use rusqlite::{Connection, params};
use serde_json::json;

const LIVE: Duration = Duration::from_secs(60);

struct Fixture {
    _tmp: tempfile::TempDir,
    store: Store,
    ws: WorkspaceId,
    proj: ProjectId,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default".to_owned())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch".to_owned(), None)
        .await
        .unwrap();
    Fixture {
        _tmp: tmp,
        store,
        ws,
        proj,
    }
}

async fn session_with(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    bodies: &[&str],
) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::Codex,
            cwd: Some("/repo".into()),
            actor_user: None,
        })
        .await
        .unwrap();
    for body in bodies {
        append(store, ws, proj, id, body).await;
    }
    id
}

async fn append(store: &Store, ws: WorkspaceId, proj: ProjectId, id: SessionId, body: &str) {
    // Every row shares one timestamp: ordering must come from the snapshot,
    // not from distinct `created_at` values.
    let obs = NewObservation {
        occurred_at: Some(1_700_000_000_000_000),
        session_id: id,
        workspace_id: ws,
        project_id: proj,
        kind: ObservationKind::UserPrompt,
        extension: None,
        source_event: None,
        title: "prompt".into(),
        body: body.into(),
        importance: 5,
    };
    store
        .writer
        .insert_observation(Sanitized::new(obs, &Sanitizer::builtin()))
        .await
        .unwrap();
}

fn request(ws: WorkspaceId, proj: ProjectId, session: SessionId) -> NewProcessingJob {
    NewProcessingJob {
        workspace_id: ws,
        project_id: proj,
        session_id: session,
        kind: ProcessingJobKind::Consolidate,
        actor: ActorContext::default(),
        author_id: None,
        viewer: None,
        owner: None,
        options_json: json!({}),
        context_fingerprint: "ctx-1".into(),
        max_calls: 10,
    }
}

async fn claim_one(f: &Fixture, lease: Duration) -> ClaimedProcessingJob {
    f.store
        .writer
        .claim_next_processing_job(lease)
        .await
        .unwrap()
        .expect("a runnable job")
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_owned(),
        body: "published".into(),
        tier: Tier::Semantic,
        frontmatter_json: json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

async fn status_state(
    f: &Fixture,
    job: ProcessingJobId,
) -> Option<(ProcessingJobState, Option<String>)> {
    f.store
        .reader
        .processing_job_status(f.ws, f.proj, job, ProcessingOwnerFilter::Any)
        .await
        .unwrap()
        .map(|s| (s.state, s.last_error))
}

#[tokio::test]
async fn manifest_rebuilds_every_stored_body_in_order_and_rejects_tampering() {
    let f = fixture().await;
    // Multi-byte text, an empty body and equal timestamps must all survive.
    let bodies = ["alpha ✓ 日本語", "", "gamma 🙂 tail", "delta"];
    let session = session_with(&f.store, f.ws, f.proj, &bodies).await;
    let created = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();
    assert!(created.created);
    let job = created.status.job_id;

    let manifest = f
        .store
        .reader
        .processing_manifest_page(f.ws, f.proj, job, None, 100)
        .await
        .unwrap();
    assert_eq!(manifest.len(), bodies.len());
    assert!(
        manifest
            .iter()
            .enumerate()
            .all(|(i, m)| m.ordinal as usize == i)
    );

    // Page through with a tiny byte budget: still at least one row per call.
    let mut after = None;
    let mut rebuilt = Vec::new();
    loop {
        let batch = f
            .store
            .reader
            .read_processing_sources(f.ws, f.proj, job, after, 100, 1)
            .await
            .unwrap();
        if batch.is_empty() {
            break;
        }
        assert_eq!(batch.len(), 1, "max_bytes = 1 yields exactly one row");
        after = batch.last().map(|s| s.ordinal);
        rebuilt.extend(batch.into_iter().map(|s| s.body));
    }
    assert_eq!(rebuilt, bodies);

    // A body that changed after the snapshot is refused, never half-read.
    Connection::open(f.store.db_path())
        .unwrap()
        .execute(
            "UPDATE observations SET body = 'tampered' WHERE session_id = ?1 AND body = 'delta'",
            params![session.as_bytes()],
        )
        .unwrap();
    let err = f
        .store
        .reader
        .read_processing_sources(f.ws, f.proj, job, Some(2), 100, usize::MAX)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidState(_)), "{err:?}");
}

#[tokio::test]
async fn a_removed_source_observation_invalidates_the_job_instead_of_resuming() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["one", "two"]).await;
    let job = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap()
        .status
        .job_id;
    Connection::open(f.store.db_path())
        .unwrap()
        .execute(
            "DELETE FROM observations WHERE session_id = ?1 AND body = 'two'",
            params![session.as_bytes()],
        )
        .unwrap();
    assert!(
        f.store
            .writer
            .claim_next_processing_job(LIVE)
            .await
            .unwrap()
            .is_none()
    );
    let (state, error) = status_state(&f, job).await.unwrap();
    assert_eq!(state, ProcessingJobState::Invalidated);
    assert!(error.unwrap().contains("changed or were removed"));
    let err = f
        .store
        .reader
        .read_processing_sources(f.ws, f.proj, job, None, 10, 1024)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidState(_)), "{err:?}");
}

#[tokio::test]
async fn foreign_scope_and_owner_never_see_a_job_while_the_legitimate_caller_does() {
    let f = fixture().await;
    let other_proj = f
        .store
        .writer
        .get_or_create_project(f.ws, "other".to_owned(), None)
        .await
        .unwrap();
    let session = session_with(&f.store, f.ws, f.proj, &["secret plan"]).await;
    let alice = IdentityKey::User("alice".into());
    let mut req = request(f.ws, f.proj, session);
    req.owner = Some(alice.clone());
    let job = f
        .store
        .writer
        .create_processing_job(req)
        .await
        .unwrap()
        .status
        .job_id;

    let read = |proj: ProjectId, owner: ProcessingOwnerFilter| {
        f.store.reader.processing_job_status(f.ws, proj, job, owner)
    };
    // Legitimate controls.
    assert!(
        read(f.proj, ProcessingOwnerFilter::Owner(Some(alice.clone())))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        read(f.proj, ProcessingOwnerFilter::Any)
            .await
            .unwrap()
            .is_some()
    );
    // Foreign project, foreign operator, and the shared bucket all see nothing.
    assert!(
        read(other_proj, ProcessingOwnerFilter::Any)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        read(
            f.proj,
            ProcessingOwnerFilter::Owner(Some(IdentityKey::User("bob".into())))
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        read(f.proj, ProcessingOwnerFilter::Owner(None))
            .await
            .unwrap()
            .is_none()
    );
    // A foreign scope can neither claim the job, read its sources, nor start
    // work on the session.
    assert!(
        f.store
            .writer
            .claim_processing_job(f.ws, other_proj, job, LIVE)
            .await
            .unwrap()
            .is_none()
    );
    let err = f
        .store
        .reader
        .read_processing_sources(f.ws, other_proj, job, None, 10, 1024)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    let err = f
        .store
        .writer
        .create_processing_job(request(f.ws, other_proj, session))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    // The legitimate scope still claims it.
    assert!(
        f.store
            .writer
            .claim_processing_job(f.ws, f.proj, job, LIVE)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn equivalent_requests_coalesce_per_owner_and_new_observations_start_a_new_job() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["a", "b"]).await;
    let first = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();
    let mut retry = request(f.ws, f.proj, session);
    retry.max_calls = 99; // not part of equivalence
    let again = f.store.writer.create_processing_job(retry).await.unwrap();
    assert!(first.created && !again.created);
    assert_eq!(first.status.job_id, again.status.job_id);
    assert_eq!(
        again.status.max_calls, 10,
        "a coalesced start keeps its budget"
    );

    // Another operator's identical request must not receive this job.
    let mut bob = request(f.ws, f.proj, session);
    bob.owner = Some(IdentityKey::User("bob".into()));
    let bobs = f.store.writer.create_processing_job(bob).await.unwrap();
    assert!(bobs.created);
    assert_ne!(bobs.status.job_id, first.status.job_id);

    // A different context (model/prompt/policy) is different work.
    let mut other_ctx = request(f.ws, f.proj, session);
    other_ctx.context_fingerprint = "ctx-2".into();
    assert!(
        f.store
            .writer
            .create_processing_job(other_ctx)
            .await
            .unwrap()
            .created
    );

    // A later observation belongs to another generation of work.
    append(&f.store, f.ws, f.proj, session, "c").await;
    let next = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();
    assert!(next.created);
    assert_ne!(next.status.job_id, first.status.job_id);
}

#[tokio::test]
async fn an_empty_or_missing_session_is_refused_and_leaves_no_job() {
    let f = fixture().await;
    let empty = session_with(&f.store, f.ws, f.proj, &[]).await;
    let err = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, empty))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidState(_)), "{err:?}");
    let err = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, SessionId::new()))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    assert!(!f.store.reader.processing_jobs_exist().await.unwrap());
}

#[tokio::test]
async fn only_one_claim_owns_a_job_and_a_lost_lease_fences_the_old_worker() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    f.store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();

    let first = claim_one(&f, Duration::ZERO).await;
    assert_eq!(first.claim.fence, 1);
    // Zero lease: the next worker takes over; the old claim is dead.
    let second = claim_one(&f, LIVE).await;
    assert_eq!(second.claim.fence, 2);
    assert!(second.claim_count >= 2);
    assert!(
        f.store
            .writer
            .claim_next_processing_job(LIVE)
            .await
            .unwrap()
            .is_none(),
        "a live lease admits no second owner"
    );
    for result in [
        f.store
            .writer
            .checkpoint_processing_job(&first.claim, ProcessingCheckpoint::default())
            .await
            .map(|_| ()),
        f.store
            .writer
            .reserve_processing_call(&first.claim)
            .await
            .map(|_| ()),
        f.store
            .writer
            .finish_processing_job(
                &first.claim,
                ProcessingOutcome::Completed { result: json!({}) },
            )
            .await
            .map(|_| ()),
        f.store
            .writer
            .renew_processing_job(&first.claim, LIVE)
            .await,
    ] {
        assert!(result.unwrap_err().is_stale_processing_claim());
    }
    // The current owner is unaffected.
    f.store
        .writer
        .renew_processing_job(&second.claim, LIVE)
        .await
        .unwrap();
    // The stale worker's pages are rejected inside the page transaction; the
    // current owner's are not.
    let stale = f
        .store
        .writer
        .fenced(&first.claim)
        .upsert_page(page(f.ws, f.proj, "stale.md"))
        .await
        .unwrap_err();
    assert!(stale.is_stale_processing_claim(), "{stale:?}");
    f.store
        .writer
        .fenced(&second.claim)
        .upsert_page(page(f.ws, f.proj, "current.md"))
        .await
        .unwrap();
    let paths: Vec<String> = Connection::open(f.store.db_path())
        .unwrap()
        .prepare("SELECT path FROM pages ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(paths, ["current.md"]);
}

#[tokio::test]
async fn reserved_attempts_are_never_refunded_across_leases_and_restarts() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    let mut req = request(f.ws, f.proj, session);
    req.max_calls = 2;
    let job = f
        .store
        .writer
        .create_processing_job(req.clone())
        .await
        .unwrap()
        .status
        .job_id;

    let claim = claim_one(&f, Duration::ZERO).await;
    // Reserved, then the worker dies before any checkpoint: an uncertain send.
    assert_eq!(
        f.store
            .writer
            .reserve_processing_call(&claim.claim)
            .await
            .unwrap(),
        CallReservation::Reserved {
            attempt: 1,
            calls_used: 1,
            max_calls: 2
        }
    );
    let Fixture {
        _tmp,
        store,
        ws,
        proj,
    } = f;
    drop(store);
    let store = Store::open(_tmp.path()).unwrap();
    let f = Fixture {
        _tmp,
        store,
        ws,
        proj,
    };

    let status = f
        .store
        .reader
        .processing_job_status(ws, proj, job, ProcessingOwnerFilter::Any)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.calls_used, 1, "restart does not reset the budget");
    let claim = claim_one(&f, LIVE).await;
    assert_eq!(
        f.store
            .writer
            .reserve_processing_call(&claim.claim)
            .await
            .unwrap(),
        CallReservation::Reserved {
            attempt: 2,
            calls_used: 2,
            max_calls: 2
        }
    );
    assert_eq!(
        f.store
            .writer
            .reserve_processing_call(&claim.claim)
            .await
            .unwrap(),
        CallReservation::BudgetExhausted {
            calls_used: 2,
            max_calls: 2
        }
    );
    let (state, _) = status_state(&f, job).await.unwrap();
    assert_eq!(state, ProcessingJobState::BudgetExceeded);
    assert!(
        f.store
            .writer
            .claim_next_processing_job(LIVE)
            .await
            .unwrap()
            .is_none(),
        "an exhausted job is not claimable"
    );
    assert!(
        f.store
            .writer
            .reserve_processing_call(&claim.claim)
            .await
            .unwrap_err()
            .is_stale_processing_claim()
    );

    // Only an explicit, larger compatible budget resumes it, keeping the spend.
    let mut more = req;
    more.max_calls = 5;
    let resumed = f.store.writer.create_processing_job(more).await.unwrap();
    assert!(!resumed.created);
    assert_eq!(resumed.status.job_id, job);
    assert_eq!(resumed.status.state, ProcessingJobState::Queued);
    assert_eq!(
        (resumed.status.calls_used, resumed.status.max_calls),
        (2, 5)
    );
    let claim = claim_one(&f, LIVE).await;
    assert_eq!(
        f.store
            .writer
            .reserve_processing_call(&claim.claim)
            .await
            .unwrap(),
        CallReservation::Reserved {
            attempt: 3,
            calls_used: 3,
            max_calls: 5
        }
    );
}

#[tokio::test]
async fn call_timing_accumulates_across_restarts_saturates_and_is_claim_fenced() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    let job = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap()
        .status;
    assert_eq!((job.wait_ms, job.exec_ms), (0, 0));
    let job = job.job_id;

    let first = claim_one(&f, Duration::ZERO).await;
    f.store
        .writer
        .reserve_processing_call(&first.claim)
        .await
        .unwrap();
    f.store
        .writer
        .record_processing_call_timing(&first.claim, 100, 200)
        .await
        .unwrap();

    let Fixture {
        _tmp,
        store,
        ws,
        proj,
    } = f;
    drop(store);
    let store = Store::open(_tmp.path()).unwrap();
    let f = Fixture {
        _tmp,
        store,
        ws,
        proj,
    };
    let read = |f: &Fixture| {
        let reader = f.store.reader.clone();
        let (ws, proj) = (f.ws, f.proj);
        async move {
            reader
                .processing_job_status(ws, proj, job, ProcessingOwnerFilter::Any)
                .await
                .unwrap()
                .unwrap()
        }
    };
    let status = read(&f).await;
    assert_eq!(
        (status.wait_ms, status.exec_ms, status.calls_used),
        (100, 200, 1),
        "completed-call timing survives a restart"
    );

    let second = claim_one(&f, LIVE).await;
    f.store
        .writer
        .record_processing_call_timing(&second.claim, 5, 7)
        .await
        .unwrap();
    let status = read(&f).await;
    assert_eq!(
        (status.wait_ms, status.exec_ms, status.calls_used),
        (105, 207, 1)
    );

    f.store
        .writer
        .record_processing_call_timing(&second.claim, u64::MAX, 0)
        .await
        .unwrap();
    f.store
        .writer
        .record_processing_call_timing(&second.claim, 1, 0)
        .await
        .unwrap();
    let status = read(&f).await;
    assert_eq!(
        status.wait_ms,
        u64::try_from(i64::MAX).unwrap(),
        "sums saturate"
    );
    assert_eq!(status.exec_ms, 207);

    assert!(
        f.store
            .writer
            .record_processing_call_timing(&first.claim, 1, 1)
            .await
            .unwrap_err()
            .is_stale_processing_claim(),
        "the superseded claim cannot record"
    );
    assert_eq!(read(&f).await.exec_ms, 207);
}

#[tokio::test]
async fn purge_before_and_after_claim_removes_the_job_and_blocks_the_in_memory_response() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["private"]).await;
    let job = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap()
        .status
        .job_id;
    let claim = claim_one(&f, LIVE).await;
    f.store
        .writer
        .checkpoint_processing_job(
            &claim.claim,
            ProcessingCheckpoint {
                chunks_total: Some(1),
                upsert_nodes: vec![node("m:0", json!({"facts": ["private"]}))],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Control: before the purge the fenced write succeeds.
    f.store
        .writer
        .fenced(&claim.claim)
        .upsert_page(page(f.ws, f.proj, "before.md"))
        .await
        .unwrap();

    f.store
        .writer
        .purge_session(
            f.ws,
            f.proj,
            session,
            None,
            Compaction::Skip,
            PurgeMode::Commit,
        )
        .await
        .unwrap();

    assert!(status_state(&f, job).await.is_none());
    let conn = Connection::open(f.store.db_path()).unwrap();
    for table in [
        "processing_jobs",
        "processing_job_observations",
        "processing_job_nodes",
        "processing_job_effects",
    ] {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table} must not outlive the session");
    }
    // The worker still holds its response in memory: every effect is refused.
    let err = f
        .store
        .writer
        .fenced(&claim.claim)
        .upsert_page(page(f.ws, f.proj, "after.md"))
        .await
        .unwrap_err();
    assert!(err.is_stale_processing_claim(), "{err:?}");
    assert!(
        f.store
            .writer
            .finish_processing_job(
                &claim.claim,
                ProcessingOutcome::Completed { result: json!({}) }
            )
            .await
            .unwrap_err()
            .is_stale_processing_claim()
    );
    let titles: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE path = 'after.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(titles, 0);
}

fn node(key: &str, json: serde_json::Value) -> ProcessingNodeWrite {
    ProcessingNodeWrite {
        key: key.to_owned(),
        level: 0,
        state: ProcessingNodeState::Completed,
        input_fingerprint: "fp".into(),
        json,
    }
}

#[tokio::test]
async fn moving_a_session_invalidates_its_active_job_in_the_same_transaction() {
    let f = fixture().await;
    let target = f
        .store
        .writer
        .get_or_create_project(f.ws, "target".to_owned(), None)
        .await
        .unwrap();
    let active = session_with(&f.store, f.ws, f.proj, &["moving"]).await;
    let done = session_with(&f.store, f.ws, f.proj, &["finished"]).await;
    let active_job = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, active))
        .await
        .unwrap()
        .status
        .job_id;
    let done_job = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, done))
        .await
        .unwrap()
        .status
        .job_id;
    let finished = f
        .store
        .writer
        .claim_processing_job(f.ws, f.proj, done_job, LIVE)
        .await
        .unwrap()
        .unwrap();
    f.store
        .writer
        .finish_processing_job(
            &finished.claim,
            ProcessingOutcome::Completed {
                result: json!({"ok": true}),
            },
        )
        .await
        .unwrap();
    let claim = f
        .store
        .writer
        .claim_processing_job(f.ws, f.proj, active_job, LIVE)
        .await
        .unwrap()
        .unwrap();

    f.store
        .writer
        .move_session(active, f.ws, target, PagesMode::Move, None, true)
        .await
        .unwrap();
    f.store
        .writer
        .move_session(done, f.ws, target, PagesMode::Move, None, true)
        .await
        .unwrap();

    let old = |job| {
        f.store
            .reader
            .processing_job_status(f.ws, f.proj, job, ProcessingOwnerFilter::Any)
    };
    let new = |job| {
        f.store
            .reader
            .processing_job_status(f.ws, target, job, ProcessingOwnerFilter::Any)
    };
    assert!(old(active_job).await.unwrap().is_none());
    assert!(old(done_job).await.unwrap().is_none());
    let moved = new(active_job).await.unwrap().unwrap();
    assert_eq!(moved.state, ProcessingJobState::Invalidated);
    let kept = new(done_job).await.unwrap().unwrap();
    assert_eq!(kept.state, ProcessingJobState::Completed);
    assert_eq!(kept.result, Some(json!({"ok": true})));
    // The old claim cannot publish into the scope the session left.
    let err = f
        .store
        .writer
        .fenced(&claim.claim)
        .upsert_page(page(f.ws, f.proj, "late.md"))
        .await
        .unwrap_err();
    assert!(err.is_stale_processing_claim(), "{err:?}");
    assert!(
        f.store
            .writer
            .claim_next_processing_job(LIVE)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn checkpoints_publication_identity_and_effects_survive_a_lost_claim() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["a", "b"]).await;
    let job = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap()
        .status
        .job_id;
    let claim = claim_one(&f, Duration::ZERO).await;
    let status = f
        .store
        .writer
        .checkpoint_processing_job(
            &claim.claim,
            ProcessingCheckpoint {
                state: Some(ProcessingJobState::Reducing),
                chunks_total: Some(2),
                chunks_done: Some(1),
                upsert_nodes: vec![node("m:0", json!({"n": 1})), node("m:1", json!({"n": 2}))],
                delete_nodes: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (status.state, status.chunks_done, status.chunks_total),
        (ProcessingJobState::Reducing, 1, 2)
    );

    // A dependent node is invalidated by deleting it.
    f.store
        .writer
        .checkpoint_processing_job(
            &claim.claim,
            ProcessingCheckpoint {
                delete_nodes: vec!["m:1".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Invalid payloads are rejected as a whole, leaving nothing behind.
    for bad in [
        ProcessingCheckpoint {
            upsert_nodes: vec![node("bad key!", json!({}))],
            ..Default::default()
        },
        ProcessingCheckpoint {
            upsert_nodes: vec![node("big", json!({"x": "y".repeat(70_000)}))],
            ..Default::default()
        },
        ProcessingCheckpoint {
            chunks_done: Some(9),
            ..Default::default()
        },
        ProcessingCheckpoint {
            state: Some(ProcessingJobState::Completed),
            ..Default::default()
        },
    ] {
        let err = f
            .store
            .writer
            .checkpoint_processing_job(&claim.claim, bad)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidState(_)), "{err:?}");
    }
    let nodes = f
        .store
        .reader
        .processing_nodes(f.ws, f.proj, job, None, 10)
        .await
        .unwrap();
    assert_eq!(
        nodes.iter().map(|n| n.key.as_str()).collect::<Vec<_>>(),
        ["m:0"]
    );

    let publication = f
        .store
        .writer
        .begin_processing_publication(&claim.claim, "pub-1".into(), json!({"pages": ["a.md"]}))
        .await
        .unwrap();
    f.store
        .writer
        .record_processing_effect(&claim.claim, "page:a".into(), json!({"page_id": "p1"}))
        .await
        .unwrap();
    // A different identity cannot replace the stamped one.
    let same = f
        .store
        .writer
        .begin_processing_publication(&claim.claim, "pub-2".into(), json!({}))
        .await
        .unwrap();
    assert_eq!(same.identity, publication.identity);
    assert_eq!(same.identity, "pub-1");

    // The lease was zero: the next claimant inherits identity, effects, state.
    let next = claim_one(&f, LIVE).await;
    assert_eq!(next.status.state, ProcessingJobState::Publishing);
    assert_eq!(next.publication.as_ref().unwrap().identity, "pub-1");
    let effects = f
        .store
        .reader
        .processing_effects(f.ws, f.proj, job)
        .await
        .unwrap();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].json, json!({"page_id": "p1"}));
    // Publication never goes back to a map/reduce phase.
    assert!(matches!(
        f.store
            .writer
            .checkpoint_processing_job(
                &next.claim,
                ProcessingCheckpoint {
                    state: Some(ProcessingJobState::Running),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
        StoreError::InvalidState(_)
    ));

    let done = f
        .store
        .writer
        .finish_processing_job(
            &next.claim,
            ProcessingOutcome::Completed {
                result: json!({"outcomes": []}),
            },
        )
        .await
        .unwrap();
    assert_eq!(done.state, ProcessingJobState::Completed);
    assert!(
        f.store
            .reader
            .processing_nodes(f.ws, f.proj, job, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "intermediates are dropped at completion"
    );
    assert_eq!(
        f.store
            .reader
            .processing_effects(f.ws, f.proj, job)
            .await
            .unwrap()
            .len(),
        1,
        "confirmed effects outlive the intermediates"
    );
}

#[tokio::test]
async fn disabling_pauses_work_clears_claims_and_enabling_resumes_the_same_phase() {
    let f = fixture().await;
    let queued = session_with(&f.store, f.ws, f.proj, &["q"]).await;
    let running = session_with(&f.store, f.ws, f.proj, &["r"]).await;
    let q = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, queued))
        .await
        .unwrap()
        .status
        .job_id;
    let r = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, running))
        .await
        .unwrap()
        .status
        .job_id;
    let claim = f
        .store
        .writer
        .claim_processing_job(f.ws, f.proj, r, LIVE)
        .await
        .unwrap()
        .unwrap();
    f.store
        .writer
        .begin_processing_publication(&claim.claim, "pub".into(), json!({}))
        .await
        .unwrap();

    assert_eq!(f.store.writer.pause_processing_jobs().await.unwrap(), 2);
    assert_eq!(
        status_state(&f, q).await.unwrap().0,
        ProcessingJobState::Paused
    );
    assert!(
        f.store
            .writer
            .claim_next_processing_job(LIVE)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.store
            .writer
            .fenced(&claim.claim)
            .upsert_page(page(f.ws, f.proj, "paused.md"))
            .await
            .unwrap_err()
            .is_stale_processing_claim(),
        "a paused job accepts no in-flight publication"
    );

    assert_eq!(
        f.store
            .writer
            .resume_paused_processing_jobs()
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        status_state(&f, q).await.unwrap().0,
        ProcessingJobState::Queued
    );
    assert_eq!(
        status_state(&f, r).await.unwrap().0,
        ProcessingJobState::Publishing,
        "publication progress is kept, not demoted"
    );
}

#[tokio::test]
async fn cleanup_deletes_idle_jobs_but_never_one_under_a_live_lease() {
    let f = fixture().await;
    let live_session = session_with(&f.store, f.ws, f.proj, &["live"]).await;
    let idle_session = session_with(&f.store, f.ws, f.proj, &["idle"]).await;
    let live = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, live_session))
        .await
        .unwrap()
        .status
        .job_id;
    let idle = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, idle_session))
        .await
        .unwrap()
        .status
        .job_id;
    let claim = f
        .store
        .writer
        .claim_processing_job(f.ws, f.proj, live, LIVE)
        .await
        .unwrap()
        .unwrap();
    // Nothing is older than a week yet.
    assert_eq!(
        f.store
            .writer
            .cleanup_processing_jobs(Duration::from_secs(7 * 86_400))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.store
            .writer
            .cleanup_processing_jobs(Duration::ZERO)
            .await
            .unwrap(),
        1
    );
    assert!(status_state(&f, idle).await.is_none());
    assert!(status_state(&f, live).await.is_some());
    f.store
        .writer
        .renew_processing_job(&claim.claim, LIVE)
        .await
        .unwrap();
}

fn staging(
    ws: WorkspaceId,
    proj: ProjectId,
    session: SessionId,
    job: ProcessingJobId,
) -> StageAutoImproveRun {
    StageAutoImproveRun {
        workspace_id: ws,
        project_id: proj,
        session_id: Some(session),
        provider: None,
        model: None,
        summary: Some("review".into()),
        warnings_json: json!([]),
        rejected_candidates_json: json!([]),
        // The engine stamps this so a restart can find a stage that committed
        // before its effect was recorded.
        config_json: json!({"processing_job": job.to_string()}),
        proposal_actor: ActorContext::default(),
        proposals: vec![NewAutoImproveProposal {
            operation: AutoImproveProposalOperation::Create,
            target_path: PagePath::new("learned.md").unwrap(),
            kind: "learning".into(),
            title: "Learned".into(),
            confidence: 0.9,
            rationale: "seen twice".into(),
            evidence_json: json!([]),
            body_markdown: "body".into(),
            artifact_sha256: None,
            edit_mode: None,
            patch_json: None,
            expected_base_body_sha256: None,
        }],
    }
}

#[tokio::test]
async fn a_fenced_stage_commits_for_the_owner_and_is_refused_after_a_takeover() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    let mut req = request(f.ws, f.proj, session);
    req.kind = ProcessingJobKind::AutoImprove;
    let job = f
        .store
        .writer
        .create_processing_job(req)
        .await
        .unwrap()
        .status
        .job_id;
    let old = claim_one(&f, Duration::ZERO).await;
    let current = claim_one(&f, LIVE).await;
    let rows = |table: &str| -> i64 {
        Connection::open(f.store.db_path())
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };

    // Nothing is reconstructable before a stage committed.
    assert!(
        f.store
            .reader
            .auto_improve_run_for_processing_job(f.ws, f.proj, job)
            .await
            .unwrap()
            .is_none()
    );
    let err = f
        .store
        .writer
        .fenced(&old.claim)
        .stage_auto_improve_run_for_owner(staging(f.ws, f.proj, session, job), None)
        .await
        .unwrap_err();
    assert!(err.is_stale_processing_claim(), "{err:?}");
    assert_eq!(
        (rows("auto_improve_runs"), rows("auto_improve_proposals")),
        (0, 0)
    );

    let staged = f
        .store
        .writer
        .fenced(&current.claim)
        .stage_auto_improve_run_for_owner(staging(f.ws, f.proj, session, job), None)
        .await
        .unwrap();
    // Crash between the stage commit and the effect checkpoint: the run and
    // proposal ids are still recoverable from the job stamp alone.
    let (run, proposals) = f
        .store
        .reader
        .auto_improve_run_for_processing_job(f.ws, f.proj, job)
        .await
        .unwrap()
        .expect("staged run is found by its job stamp");
    assert_eq!(run, staged.run_id);
    assert_eq!(proposals, staged.proposal_ids);
    // A foreign scope cannot use the lookup to learn about the job.
    let other = f
        .store
        .writer
        .get_or_create_project(f.ws, "other".to_owned(), None)
        .await
        .unwrap();
    assert!(matches!(
        f.store
            .reader
            .auto_improve_run_for_processing_job(f.ws, other, job)
            .await
            .unwrap_err(),
        StoreError::NotFound(_)
    ));
}

#[tokio::test]
async fn a_store_holding_a_processing_job_is_never_a_clean_reindex_target() {
    // Jobs are derived state tied to sessions, which `reindex` does not
    // rebuild, and `reindex` only runs on an empty store. A job therefore can
    // never survive into, or be resumed after, a rebuild.
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    f.store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();
    let status = f.store.reader.reindex_target_status().await.unwrap();
    assert!(!status.is_clean());
    assert!(status.sessions >= 1 && status.observations >= 1);
}

#[tokio::test]
async fn a_stale_claim_cannot_re_cite_evidence_on_an_unchanged_page() {
    use ai_memory_core::{PageEvidence, PageEvidenceKind};

    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    f.store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();
    let old = claim_one(&f, Duration::ZERO).await;
    let current = claim_one(&f, LIVE).await;
    let cite = |source: &str| {
        let mut p = page(f.ws, f.proj, "cited.md");
        p.evidence = vec![PageEvidence {
            kind: PageEvidenceKind::Session,
            source_id: source.to_owned(),
        }];
        p
    };
    let evidence_rows = || -> i64 {
        Connection::open(f.store.db_path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM page_evidence", [], |r| r.get(0))
            .unwrap()
    };

    let owner = f.store.writer.fenced(&current.claim);
    owner.upsert_page(cite("first")).await.unwrap();
    assert_eq!(evidence_rows(), 1);
    // Identical body: no new pages row, only evidence would be written. The
    // stale worker must still be refused, and the owner still allowed.
    let err = f
        .store
        .writer
        .fenced(&old.claim)
        .upsert_page(cite("second"))
        .await
        .unwrap_err();
    assert!(err.is_stale_processing_claim(), "{err:?}");
    assert_eq!(evidence_rows(), 1, "stale re-cite must not insert evidence");
    owner.upsert_page(cite("second")).await.unwrap();
    assert_eq!(evidence_rows(), 2);
}

#[tokio::test]
async fn two_store_connections_cannot_borrow_each_others_fence() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    f.store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();
    let old = claim_one(&f, Duration::ZERO).await;
    let current = claim_one(&f, LIVE).await;
    // A second writer connection to the same database file, as a second
    // server process or Store instance would have.
    let other = Store::open(f._tmp.path()).unwrap();

    for round in 0..25 {
        let owner = other.writer.fenced(&current.claim);
        let stale = f.store.writer.fenced(&old.claim);
        let (ok, refused) = tokio::join!(
            owner.upsert_page(page(f.ws, f.proj, &format!("ok-{round}.md"))),
            stale.upsert_page(page(f.ws, f.proj, &format!("late-{round}.md"))),
        );
        ok.unwrap();
        assert!(refused.unwrap_err().is_stale_processing_claim());
        // And the other direction: the connection that owns the claim is
        // not disturbed by the other one's plain, unfenced writes.
        f.store
            .writer
            .fenced(&current.claim)
            .upsert_page(page(f.ws, f.proj, &format!("mine-{round}.md")))
            .await
            .unwrap();
        other
            .writer
            .upsert_page(page(f.ws, f.proj, &format!("plain-{round}.md")))
            .await
            .unwrap();
    }
    let late: i64 = Connection::open(f.store.db_path())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE path LIKE 'late-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(late, 0);
}

#[tokio::test]
async fn a_body_changed_after_a_checkpoint_invalidates_instead_of_reusing_cached_nodes() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["alpha", "beta"]).await;
    let job = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap()
        .status
        .job_id;
    let first = claim_one(&f, Duration::ZERO).await;
    f.store
        .writer
        .checkpoint_processing_job(
            &first.claim,
            ProcessingCheckpoint {
                chunks_total: Some(2),
                chunks_done: Some(1),
                upsert_nodes: vec![node("m:0", json!({"facts": ["alpha"]}))],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Same row count, same length: only the content differs.
    Connection::open(f.store.db_path())
        .unwrap()
        .execute(
            "UPDATE observations SET body = 'ALPHA' WHERE session_id = ?1 AND body = 'alpha'",
            params![session.as_bytes()],
        )
        .unwrap();

    assert!(
        f.store
            .writer
            .claim_next_processing_job(LIVE)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        status_state(&f, job).await.unwrap().0,
        ProcessingJobState::Invalidated
    );
    assert!(
        f.store
            .writer
            .fenced(&first.claim)
            .upsert_page(page(f.ws, f.proj, "cached-result.md"))
            .await
            .unwrap_err()
            .is_stale_processing_claim()
    );
    // A fresh job over the changed snapshot is new work with no cached nodes.
    let fresh = f
        .store
        .writer
        .create_processing_job(request(f.ws, f.proj, session))
        .await
        .unwrap();
    assert!(fresh.created);
    assert!(
        f.store
            .reader
            .processing_nodes(f.ws, f.proj, fresh.status.job_id, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn unfinished_job_lookup_matches_scope_session_kind_and_owner_bucket_only() {
    let f = fixture().await;
    let other_proj = f
        .store
        .writer
        .get_or_create_project(f.ws, "other".to_owned(), None)
        .await
        .unwrap();
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    let alice = IdentityKey::User("alice".into());
    let mut req = request(f.ws, f.proj, session);
    req.owner = Some(alice.clone());
    let job = f
        .store
        .writer
        .create_processing_job(req)
        .await
        .unwrap()
        .status
        .job_id;
    let has = |proj: ProjectId, kind: ProcessingJobKind, owner: Option<IdentityKey>| {
        f.store
            .reader
            .has_unfinished_processing_job(f.ws, proj, session, kind, owner)
    };
    let kind = ProcessingJobKind::Consolidate;
    // Legitimate control.
    assert!(has(f.proj, kind, Some(alice.clone())).await.unwrap());
    // Foreign scope, owner, shared bucket and kind never match.
    assert!(!has(other_proj, kind, Some(alice.clone())).await.unwrap());
    assert!(
        !has(f.proj, kind, Some(IdentityKey::User("bob".into())))
            .await
            .unwrap()
    );
    assert!(!has(f.proj, kind, None).await.unwrap());
    assert!(
        !has(f.proj, ProcessingJobKind::AutoImprove, Some(alice.clone()))
            .await
            .unwrap()
    );
    // Paused and budget-limited jobs still count; finished ones do not.
    f.store.writer.pause_processing_jobs().await.unwrap();
    assert!(has(f.proj, kind, Some(alice.clone())).await.unwrap());
    f.store
        .writer
        .resume_paused_processing_jobs()
        .await
        .unwrap();
    let claim = f
        .store
        .writer
        .claim_processing_job(f.ws, f.proj, job, LIVE)
        .await
        .unwrap()
        .unwrap();
    f.store
        .writer
        .finish_processing_job(
            &claim.claim,
            ProcessingOutcome::Completed { result: json!({}) },
        )
        .await
        .unwrap();
    assert!(!has(f.proj, kind, Some(alice)).await.unwrap());
}

fn owned(f: &Fixture, session: SessionId, owner: Option<&str>) -> NewProcessingJob {
    NewProcessingJob {
        owner: owner.map(|name| IdentityKey::User(name.into())),
        ..request(f.ws, f.proj, session)
    }
}

fn job_rows(f: &Fixture) -> i64 {
    Connection::open(f.store.db_path())
        .unwrap()
        .query_row("SELECT COUNT(*) FROM processing_jobs", [], |row| row.get(0))
        .unwrap()
}

async fn create(f: &Fixture, req: NewProcessingJob) -> Result<ProcessingJobId, StoreError> {
    f.store
        .writer
        .create_processing_job(req)
        .await
        .map(|created| created.status.job_id)
}

async fn complete(f: &Fixture, job: ProcessingJobId) {
    let claim = f
        .store
        .writer
        .claim_processing_job(f.ws, f.proj, job, LIVE)
        .await
        .unwrap()
        .expect("a runnable job");
    f.store
        .writer
        .finish_processing_job(
            &claim.claim,
            ProcessingOutcome::Completed { result: json!({}) },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_project_owner_bucket_admits_a_bounded_number_of_unfinished_jobs() {
    let f = fixture().await;
    let cap = PROCESSING_MAX_UNFINISHED_JOBS_PER_OWNER as usize;
    let mut accepted = Vec::new();
    for i in 0..cap {
        let session = session_with(&f.store, f.ws, f.proj, &[&format!("body {i}")]).await;
        let job = create(&f, owned(&f, session, Some("alice"))).await.unwrap();
        accepted.push((session, job));
    }

    // Boundary: the next distinct snapshot is refused with the typed,
    // retryable error, and nothing of it is stored.
    let extra = session_with(&f.store, f.ws, f.proj, &["extra"]).await;
    let err = create(&f, owned(&f, extra, Some("alice")))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ProcessingQueueFull(_)), "{err:?}");
    assert_eq!(job_rows(&f), cap as i64, "a refusal must leave no job row");

    // An equivalent request joins the accepted job even at capacity.
    let (first_session, first_job) = accepted[0];
    let joined = f
        .store
        .writer
        .create_processing_job(owned(&f, first_session, Some("alice")))
        .await
        .unwrap();
    assert!(!joined.created);
    assert_eq!(joined.status.job_id, first_job);

    // The legitimate other operator, the shared bucket and another project
    // are unaffected by alice's full bucket.
    create(&f, owned(&f, extra, Some("bob"))).await.unwrap();
    create(&f, owned(&f, extra, None)).await.unwrap();
    let other = f
        .store
        .writer
        .get_or_create_project(f.ws, "other".to_owned(), None)
        .await
        .unwrap();
    let elsewhere = session_with(&f.store, f.ws, other, &["elsewhere"]).await;
    create(
        &f,
        NewProcessingJob {
            owner: Some(IdentityKey::User("alice".into())),
            ..request(f.ws, other, elsewhere)
        },
    )
    .await
    .unwrap();

    // A paused job still holds its slot.
    f.store.writer.pause_processing_jobs().await.unwrap();
    let err = create(&f, owned(&f, extra, Some("alice")))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ProcessingQueueFull(_)), "{err:?}");
    f.store
        .writer
        .resume_paused_processing_jobs()
        .await
        .unwrap();

    // A terminal job releases its slot; the bucket is bounded again after.
    complete(&f, first_job).await;
    create(&f, owned(&f, extra, Some("alice"))).await.unwrap();
    let another = session_with(&f.store, f.ws, f.proj, &["another"]).await;
    let err = create(&f, owned(&f, another, Some("alice")))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ProcessingQueueFull(_)), "{err:?}");
}

#[tokio::test]
async fn the_server_admits_a_bounded_number_of_unfinished_jobs_across_owners() {
    let f = fixture().await;
    let per_owner = PROCESSING_MAX_UNFINISHED_JOBS_PER_OWNER as usize;
    let cap = PROCESSING_MAX_UNFINISHED_JOBS as usize;
    let mut first = None;
    for owner in 0..cap / per_owner {
        for i in 0..per_owner {
            let session =
                session_with(&f.store, f.ws, f.proj, &[&format!("body {owner}/{i}")]).await;
            let name = format!("user{owner}");
            let job = create(&f, owned(&f, session, Some(&name))).await.unwrap();
            first.get_or_insert((session, job, name));
        }
    }
    assert_eq!(job_rows(&f), cap as i64);

    // A fresh operator has an empty bucket but the server is full.
    let extra = session_with(&f.store, f.ws, f.proj, &["extra"]).await;
    let err = create(&f, owned(&f, extra, Some("newcomer")))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ProcessingQueueFull(_)), "{err:?}");
    assert!(err.to_string().contains("server-wide"), "{err}");
    assert_eq!(job_rows(&f), cap as i64, "a refusal must leave no job row");

    // Joining an equivalent job still works at capacity.
    let (session, job, name) = first.unwrap();
    let joined = f
        .store
        .writer
        .create_processing_job(owned(&f, session, Some(&name)))
        .await
        .unwrap();
    assert!(!joined.created);
    assert_eq!(joined.status.job_id, job);

    // Finishing one job frees exactly one server slot.
    complete(&f, job).await;
    create(&f, owned(&f, extra, Some("newcomer")))
        .await
        .unwrap();
    let another = session_with(&f.store, f.ws, f.proj, &["another"]).await;
    let err = create(&f, owned(&f, another, Some("newcomer")))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::ProcessingQueueFull(_)), "{err:?}");
}

#[tokio::test]
async fn a_request_never_joins_a_job_with_a_different_authorization_viewer() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    let alice = ai_memory_core::UserId::new();
    let mallory = ai_memory_core::UserId::new();
    let with = |viewer: Option<ai_memory_core::UserId>, author: Option<ai_memory_core::UserId>| {
        NewProcessingJob {
            viewer,
            author_id: author,
            owner: Some(IdentityKey::User("alice".into())),
            ..request(f.ws, f.proj, session)
        }
    };

    // A server-owned job (no viewer, no revocation check) exists first.
    let server_owned = f
        .store
        .writer
        .create_processing_job(with(None, None))
        .await
        .unwrap();
    assert!(server_owned.created);

    // The same operator bucket, but an authenticated viewer: must get its own
    // job, never the weaker viewer-less one.
    let alices = f
        .store
        .writer
        .create_processing_job(with(Some(alice), Some(alice)))
        .await
        .unwrap();
    assert!(
        alices.created,
        "a stricter viewer must not join a viewer-less job"
    );
    assert_ne!(alices.status.job_id, server_owned.status.job_id);

    // Attribution alone (author without viewer) joins the viewer-less job:
    // authorization, not attribution, decides equivalence.
    let attribution_only = f
        .store
        .writer
        .create_processing_job(with(None, Some(alice)))
        .await
        .unwrap();
    assert!(!attribution_only.created);
    assert_eq!(attribution_only.status.job_id, server_owned.status.job_id);

    // Control: the identical viewer joins, another user's viewer does not.
    let again = f
        .store
        .writer
        .create_processing_job(with(Some(alice), None))
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.status.job_id, alices.status.job_id);
    let foreign = f
        .store
        .writer
        .create_processing_job(with(Some(mallory), Some(alice)))
        .await
        .unwrap();
    assert!(foreign.created);
    assert_ne!(foreign.status.job_id, alices.status.job_id);
    assert_eq!(job_rows(&f), 3);
}

#[tokio::test]
async fn an_automatic_server_owned_job_is_isolated_from_authenticated_operator_jobs() {
    let f = fixture().await;
    let session = session_with(&f.store, f.ws, f.proj, &["x"]).await;
    let alice = ai_memory_core::UserId::new();
    // SessionEnd policy: shared owner bucket, no viewer, attribution only.
    let automatic = NewProcessingJob {
        author_id: None,
        viewer: None,
        owner: None,
        ..request(f.ws, f.proj, session)
    };
    let server_owned = create(&f, automatic.clone()).await.unwrap();

    // Control: another automatic request (e.g. a retry) joins it.
    let retry = f
        .store
        .writer
        .create_processing_job(automatic)
        .await
        .unwrap();
    assert!(!retry.created);
    assert_eq!(retry.status.job_id, server_owned);

    // An authenticated operator (own bucket and viewer) never adopts it, and
    // neither does a viewer in the shared bucket.
    for (owner, viewer) in [
        (Some(IdentityKey::User("alice".into())), Some(alice)),
        (Some(IdentityKey::User("alice".into())), None),
        (None, Some(alice)),
    ] {
        let created = f
            .store
            .writer
            .create_processing_job(NewProcessingJob {
                author_id: Some(alice),
                viewer,
                owner,
                ..request(f.ws, f.proj, session)
            })
            .await
            .unwrap();
        assert!(created.created);
        assert_ne!(created.status.job_id, server_owned);
    }
}

fn intermediate_rows(f: &Fixture, job: ProcessingJobId) -> (i64, i64) {
    let conn = Connection::open(f.store.db_path()).unwrap();
    let count = |table: &str| -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE job_id = ?1"),
            params![job.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    };
    (
        count("processing_job_nodes"),
        count("processing_job_observations"),
    )
}

async fn finish_with(f: &Fixture, body: &str, outcome: ProcessingOutcome) -> ProcessingJobId {
    let session = session_with(&f.store, f.ws, f.proj, &[body]).await;
    let job = create(f, request(f.ws, f.proj, session)).await.unwrap();
    let claim = claim_one(f, LIVE).await;
    f.store
        .writer
        .checkpoint_processing_job(
            &claim.claim,
            ProcessingCheckpoint {
                chunks_total: Some(1),
                upsert_nodes: vec![node("m:0", json!({"n": 1}))],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(intermediate_rows(f, job), (1, 1));
    f.store
        .writer
        .finish_processing_job(&claim.claim, outcome)
        .await
        .unwrap();
    job
}

#[tokio::test]
async fn a_failed_job_keeps_its_nodes_and_manifest_until_cleanup_but_an_invalidated_one_does_not() {
    let f = fixture().await;
    let failed = finish_with(
        &f,
        "failing",
        ProcessingOutcome::Failed {
            error: "provider rejected".into(),
        },
    )
    .await;
    let invalidated = finish_with(
        &f,
        "invalidated",
        ProcessingOutcome::Invalidated {
            reason: "source changed".into(),
        },
    )
    .await;
    let completed = finish_with(
        &f,
        "completed",
        ProcessingOutcome::Completed { result: json!({}) },
    )
    .await;

    assert_eq!(
        status_state(&f, failed).await.unwrap().0,
        ProcessingJobState::Failed
    );
    assert_eq!(
        intermediate_rows(&f, failed),
        (1, 1),
        "a failed job retains its derived nodes and manifest"
    );
    // Controls: invalidated and completed jobs discard theirs.
    assert_eq!(intermediate_rows(&f, invalidated), (0, 0));
    assert_eq!(intermediate_rows(&f, completed), (0, 0));

    // A terminal failed job still frees queue capacity (it is not counted).
    let rows: i64 = Connection::open(f.store.db_path())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM processing_jobs WHERE state IN \
             ('queued', 'running', 'reducing', 'publishing', 'paused', 'budget_exceeded')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0);

    // Normal retention cleanup removes the failed job with its intermediates.
    f.store
        .writer
        .cleanup_processing_jobs(Duration::ZERO)
        .await
        .unwrap();
    assert!(status_state(&f, failed).await.is_none());
    assert_eq!(intermediate_rows(&f, failed), (0, 0));
}
