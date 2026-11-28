//! `POST /admin/compact` with `background: true` and `GET /admin/compact`.
//!
//! The compaction's FTS rebuild writes, so it needs the database write lock.
//! A second connection that holds `BEGIN EXCLUSIVE` keeps the writer waiting
//! in its busy timeout for as long as a test needs: the job is then `running`
//! deterministically, and the writer is genuinely busy while the test asks
//! `GET` and sends conflicting `POST`s. Dropping the lock lets it finish. Held
//! past the writer's 5 s busy timeout, the job fails with SQLite's
//! "database is locked", which is the one failure seam that needs no
//! production hook.

use std::path::Path;
use std::time::Duration;

use ai_memory_consolidate::{DEFAULT_CONTRADICTION_SIM_HIGH, DEFAULT_CONTRADICTION_SIM_LOW};
use ai_memory_core::{ActorContext, AuthLevel};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        mcp_metrics: std::sync::Arc::new(ai_memory_mcp::McpMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm_providers: ai_memory_llm::LlmProviders::default(),
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
        compact_jobs: Default::default(),
    };
    (state, store)
}

/// Send one request through `router` and return its status and JSON body.
/// `level` is only read by the auth layer [`as_level`] adds in the adversarial
/// test; the plain tests leave it `None`.
async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    level: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(level) = level {
        req = req.header("x-test-auth-level", level);
    }
    let body = match body {
        Some(body) => {
            req = req.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body).unwrap())
        }
        None => Body::empty(),
    };
    let resp = router
        .clone()
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn start_background(router: &Router) -> (StatusCode, Value) {
    send(
        router,
        "POST",
        "/admin/compact",
        Some(json!({ "confirm": true, "background": true })),
        None,
    )
    .await
}

async fn status(router: &Router) -> Value {
    let (code, body) = send(router, "GET", "/admin/compact", None, None).await;
    assert_eq!(
        code,
        StatusCode::OK,
        "GET /admin/compact always answers 200"
    );
    body
}

/// Poll `GET /admin/compact` until it reports `wanted`. Bounded at ten seconds
/// so a stuck job fails the test instead of hanging it.
async fn wait_for_state(router: &Router, wanted: &str) -> Value {
    for _ in 0..500 {
        let body = status(router).await;
        if body["state"] == wanted {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the compaction did not reach `{wanted}` within ten seconds");
}

/// A second connection holding the database write lock. Dropping it rolls the
/// open transaction back and releases the lock.
struct WriteLockHeld {
    _conn: rusqlite::Connection,
}

impl WriteLockHeld {
    fn take(db_path: &Path) -> Self {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        Self { _conn: conn }
    }
}

#[tokio::test]
async fn background_post_answers_202_running_and_status_reaches_succeeded() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;
    let router = admin_router(state);

    let (code, started) = start_background(&router).await;
    assert_eq!(code, StatusCode::ACCEPTED);
    assert_eq!(started["state"], "running");
    let job_id = started["job_id"]
        .as_str()
        .expect("job_id is set")
        .to_string();

    let done = wait_for_state(&router, "succeeded").await;
    assert_eq!(done["job_id"], job_id);
    let report = &done["report"];
    let before = report["bytes_before"].as_u64().expect("bytes_before");
    let after = report["bytes_after"].as_u64().expect("bytes_after");
    assert_eq!(report["bytes_reclaimed"], before.saturating_sub(after));
    assert!(done["error"].is_null());
    assert!(done["started_at"].is_string());
    assert!(done["finished_at"].is_string());
    assert!(done["elapsed_ms"].is_u64());
    // Nothing writes between the start-time read and the compaction, so the
    // start figure is the same size the report measured.
    assert_eq!(done["database_bytes_at_start"], before);
    assert!(done["reclaimable_bytes_at_start"].is_u64());
    // The last success is this run, so the estimate is its own duration.
    assert_eq!(done["previous_duration_ms"], done["elapsed_ms"]);
}

/// The single-flight guard. Test 2 of the contract: the writer is held, so the
/// first job is still running when the conflicting requests arrive. The
/// synchronous and the background form are both refused with the running job.
#[tokio::test]
async fn a_second_request_while_a_compaction_runs_is_409_in_either_form() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let router = admin_router(state);
    let lock = WriteLockHeld::take(store.db_path());

    let (code, started) = start_background(&router).await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let job_id = started["job_id"].clone();

    let conflict_start = std::time::Instant::now();
    let (code, conflict) = send(
        &router,
        "POST",
        "/admin/compact",
        Some(json!({ "confirm": true })),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "synchronous form: {conflict}");
    assert_eq!(conflict["error"], "a compaction is already running");
    assert_eq!(conflict["job"]["state"], "running");
    assert_eq!(conflict["job"]["job_id"], job_id);

    let (code, conflict) = start_background(&router).await;
    assert_eq!(code, StatusCode::CONFLICT, "background form: {conflict}");
    assert_eq!(conflict["job"]["job_id"], job_id);
    assert!(
        conflict_start.elapsed() < Duration::from_secs(1),
        "conflicting POSTs must fail fast from memory instead of waiting on SQLite busy_timeout"
    );

    // The refused requests did not disturb the job they collided with.
    let still = status(&router).await;
    assert_eq!(still["state"], "running");
    assert_eq!(still["job_id"], job_id);

    drop(lock);
    let done = wait_for_state(&router, "succeeded").await;
    assert_eq!(done["job_id"], job_id);

    // The slot is released once the job ends: the next request is admitted.
    let (code, _) = send(
        &router,
        "POST",
        "/admin/compact",
        Some(json!({ "confirm": true })),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
}

/// Test 3 of the contract: `GET` answers while the writer is held by the job.
/// Before any run it is `idle`, with every other field null.
#[tokio::test]
async fn status_answers_while_the_writer_is_held() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let router = admin_router(state);

    let idle = status(&router).await;
    assert_eq!(
        idle,
        json!({
            "job_id": null,
            "state": "idle",
            "started_at": null,
            "finished_at": null,
            "elapsed_ms": null,
            "database_bytes_at_start": null,
            "reclaimable_bytes_at_start": null,
            "previous_duration_ms": null,
            "report": null,
            "error": null,
        })
    );

    let lock = WriteLockHeld::take(store.db_path());
    let (code, _) = start_background(&router).await;
    assert_eq!(code, StatusCode::ACCEPTED);

    let mid_run = status(&router).await;
    assert_eq!(mid_run["state"], "running");
    assert!(mid_run["elapsed_ms"].is_u64());
    assert!(mid_run["report"].is_null());
    assert!(mid_run["finished_at"].is_null());

    drop(lock);
    wait_for_state(&router, "succeeded").await;
}

mod slow {
    use super::*;

    /// Test 4 of the contract, through the writer's own busy timeout. The first
    /// run succeeds so there is a previous duration to keep; the second is held
    /// past the timeout and must end `failed` with the error and no report, while
    /// the previous duration is kept.
    #[tokio::test]
    async fn a_failed_compaction_ends_failed_and_keeps_the_last_success() {
        let tmp = TempDir::new().unwrap();
        let (state, store) = make_state(&tmp).await;
        let router = admin_router(state);

        let (code, _) = send(
            &router,
            "POST",
            "/admin/compact",
            Some(json!({ "confirm": true })),
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let first = status(&router).await;
        assert_eq!(first["state"], "succeeded");
        let first_ms = first["elapsed_ms"].clone();

        let lock = WriteLockHeld::take(store.db_path());
        let (code, _) = start_background(&router).await;
        assert_eq!(code, StatusCode::ACCEPTED);

        let failed = wait_for_state(&router, "failed").await;
        drop(lock);

        let error = failed["error"].as_str().expect("error is set");
        assert!(
            error.contains("locked"),
            "the failure should be the writer's busy timeout, got: {error}"
        );
        assert!(failed["report"].is_null());
        assert!(failed["finished_at"].is_string());
        assert_eq!(failed["previous_duration_ms"], first_ms);
    }
}

/// The estimate survives a newer job that has not finished yet.
#[tokio::test]
async fn the_last_success_duration_is_kept_while_a_newer_job_runs() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let router = admin_router(state);

    let (code, _) = send(
        &router,
        "POST",
        "/admin/compact",
        Some(json!({ "confirm": true })),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let first_ms = status(&router).await["elapsed_ms"].clone();

    let lock = WriteLockHeld::take(store.db_path());
    let (code, _) = start_background(&router).await;
    assert_eq!(code, StatusCode::ACCEPTED);

    let running = status(&router).await;
    assert_eq!(running["state"], "running");
    assert_eq!(running["previous_duration_ms"], first_ms);

    drop(lock);
    wait_for_state(&router, "succeeded").await;
}

/// The synchronous form keeps its exact answer, and its result is visible to
/// `GET` through the same job state.
#[tokio::test]
async fn the_synchronous_form_keeps_its_report_and_shows_in_status() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;
    let router = admin_router(state);

    let (code, body) = send(
        &router,
        "POST",
        "/admin/compact",
        Some(json!({ "confirm": true })),
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let mut keys: Vec<&str> = body
        .as_object()
        .expect("CompactReport object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["bytes_after", "bytes_before", "bytes_reclaimed"]);

    let shown = status(&router).await;
    assert_eq!(shown["state"], "succeeded");
    assert_eq!(shown["report"], body);
}

/// The contract's 400 row. Nothing starts, in either form, so `GET` stays idle.
#[tokio::test]
async fn confirm_false_is_refused_in_both_forms_and_starts_no_job() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;
    let router = admin_router(state);

    for body in [
        json!({ "confirm": false }),
        json!({ "confirm": false, "background": true }),
    ] {
        let (code, _) = send(&router, "POST", "/admin/compact", Some(body), None).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }
    assert_eq!(status(&router).await["state"], "idle");
}

/// Adds the auth level a test request declares, the way the server's auth
/// middleware would. `trusted_proxy_identity` is what makes the admin gate
/// refuse anonymous callers; without it a single-operator store admits them.
fn as_level(router: Router) -> Router {
    router.layer(axum::middleware::from_fn(
        |mut req: Request<Body>, next: axum::middleware::Next| async move {
            let level = match req
                .headers()
                .get("x-test-auth-level")
                .and_then(|v| v.to_str().ok())
            {
                Some("root") => AuthLevel::Root,
                Some("user") => AuthLevel::User,
                _ => AuthLevel::Anonymous,
            };
            req.extensions_mut().insert(level);
            req.extensions_mut().insert(ActorContext::anonymous());
            next.run(req).await
        },
    ))
}

/// Adversarial: the new `GET`, and both `POST` forms, sit behind the same
/// root-only gate as every other `/admin/*` route. The refused requests must
/// start nothing; the root control answers normally.
#[tokio::test]
async fn status_and_compaction_are_root_only_once_multiuser() {
    let tmp = TempDir::new().unwrap();
    let (mut state, _store) = make_state(&tmp).await;
    state.trusted_proxy_identity = true;
    let router = as_level(admin_router(state));

    let sync_body = json!({ "confirm": true });
    let bg_body = json!({ "confirm": true, "background": true });
    for (level, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("user"), StatusCode::FORBIDDEN),
    ] {
        let (code, _) = send(&router, "GET", "/admin/compact", None, level).await;
        assert_eq!(code, expected, "GET as {level:?}");
        let (code, _) = send(
            &router,
            "POST",
            "/admin/compact",
            Some(bg_body.clone()),
            level,
        )
        .await;
        assert_eq!(code, expected, "background POST as {level:?}");
        let (code, _) = send(
            &router,
            "POST",
            "/admin/compact",
            Some(sync_body.clone()),
            level,
        )
        .await;
        assert_eq!(code, expected, "synchronous POST as {level:?}");
    }
    let (_, idle) = send(&router, "GET", "/admin/compact", None, Some("root")).await;
    assert_eq!(
        idle["state"], "idle",
        "refused requests must not start a job"
    );

    let (code, _) = send(&router, "GET", "/admin/compact", None, Some("root")).await;
    assert_eq!(code, StatusCode::OK, "root control: GET");
    let (code, _) = send(
        &router,
        "POST",
        "/admin/compact",
        Some(bg_body),
        Some("root"),
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED, "root control: background POST");
    // Let the job end before the synchronous control, so it is admitted rather
    // than racing the job for the slot.
    for _ in 0..500 {
        let (_, body) = send(&router, "GET", "/admin/compact", None, Some("root")).await;
        if body["state"] == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (code, _) = send(
        &router,
        "POST",
        "/admin/compact",
        Some(sync_body),
        Some("root"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "root control: synchronous POST");
}
