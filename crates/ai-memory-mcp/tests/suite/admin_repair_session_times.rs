//! Integration tests for `POST /admin/repair-session-times`
//! (`ai-memory repair-backfill-timestamps`).
//!
//! Follows the same pattern as `admin_purge.rs`: build a real [`AdminState`]
//! over a tmpdir-backed store, drive the router with `tower::ServiceExt::oneshot`.

use super::common::post;
use ai_memory_core::{AgentKind, NewSession, SessionId};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt as _;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        bind: "127.0.0.1:0".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
        db_path,
    };
    (state, store)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn seed_session(store: &Store, workspace: &str, project: &str, ended: bool) -> SessionId {
    let ws = store
        .writer
        .get_or_create_workspace(workspace)
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project, None)
        .await
        .unwrap();
    let sid = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            id: sid,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: None,
            actor_user: None,
            occurred_at: None,
        })
        .await
        .unwrap();
    if ended {
        store.writer.end_session(sid, None).await.unwrap();
    }
    sid
}

async fn session_times(store: &Store, sid: SessionId) -> (i64, Option<i64>) {
    store
        .reader
        .with_conn(move |conn| {
            conn.query_row(
                "SELECT started_at, ended_at FROM sessions WHERE id = ?1",
                [&sid.as_bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap()
}

fn now_us() -> i64 {
    jiff::Timestamp::now().as_microsecond()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Without `confirm`, the endpoint is a real dry run: it reports the
/// would-be repair but the database keeps the original times.
#[tokio::test]
async fn dry_run_reports_without_writing() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let sid = seed_session(&store, "default", "scratch", true).await;
    let (original_started, original_ended) = session_times(&store, sid).await;
    let now = now_us();
    let new_started = now - 1_000_000_000;

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": [{"session_id": sid.to_string(), "started_at_us": new_started}],
            "confirm": false,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], json!(true));
    assert_eq!(body["repaired"].as_array().unwrap().len(), 1);

    assert_eq!(
        session_times(&store, sid).await,
        (original_started, original_ended),
        "dry run must not write anything"
    );
}

/// With `confirm: true` the write commits.
#[tokio::test]
async fn confirm_applies_the_repair() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let sid = seed_session(&store, "default", "scratch", true).await;
    let now = now_us();
    let new_started = now - 1_000_000_000;
    let new_ended = now - 999_000_000;

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": [{
                "session_id": sid.to_string(),
                "started_at_us": new_started,
                "ended_at_us": new_ended,
            }],
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["dry_run"], json!(false));
    assert_eq!(body["repaired"][0]["new_started_at_us"], json!(new_started));

    assert_eq!(
        session_times(&store, sid).await,
        (new_started, Some(new_ended))
    );
}

/// Adversarial: a session belonging to a different project must be reported
/// `not_found` and left untouched, even though it is a real session id.
/// Control: a sibling session actually inside the requested scope is
/// repaired in the same request.
#[tokio::test]
async fn cross_project_session_is_not_found_and_untouched() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let sid_in_scope = seed_session(&store, "default", "scratch", true).await;
    let sid_other = seed_session(&store, "default", "other-project", true).await;
    let original_other = session_times(&store, sid_other).await;
    let now = now_us();
    let new_started = now - 1_000_000_000;

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": [
                {"session_id": sid_in_scope.to_string(), "started_at_us": new_started},
                {"session_id": sid_other.to_string(), "started_at_us": new_started},
            ],
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["repaired"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(
        body["repaired"][0]["session_id"],
        json!(sid_in_scope.to_string())
    );
    assert_eq!(body["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["skipped"][0]["session_id"],
        json!(sid_other.to_string())
    );
    assert_eq!(body["skipped"][0]["reason"], json!("not_found"));

    assert_eq!(
        session_times(&store, sid_other).await,
        original_other,
        "a session outside the requested project must not be rewritten"
    );
}

/// Adversarial: a session belonging to a different WORKSPACE (same project
/// name, so same-named projects across workspaces do not collide) must also
/// be `not_found` and untouched. Control: a sibling session in the requested
/// workspace is repaired in the same request. Mirrors
/// `purge_session_refuses_a_session_from_another_workspace`.
#[tokio::test]
async fn cross_workspace_session_is_not_found_and_untouched() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let sid_in_scope = seed_session(&store, "default", "scratch", true).await;
    let sid_other = seed_session(&store, "other-workspace", "scratch", true).await;
    let original_other = session_times(&store, sid_other).await;
    let now = now_us();
    let new_started = now - 1_000_000_000;

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": [
                {"session_id": sid_in_scope.to_string(), "started_at_us": new_started},
                {"session_id": sid_other.to_string(), "started_at_us": new_started},
            ],
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["repaired"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(
        body["repaired"][0]["session_id"],
        json!(sid_in_scope.to_string())
    );
    assert_eq!(body["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["skipped"][0]["session_id"],
        json!(sid_other.to_string())
    );
    assert_eq!(body["skipped"][0]["reason"], json!("not_found"));

    assert_eq!(
        session_times(&store, sid_other).await,
        original_other,
        "a session in another workspace must not be rewritten"
    );
}

/// B1: a correctly hook-captured session (its stored `started_at` already
/// sits at or before the candidate's own end) must not be rewritten, even
/// though the candidate is otherwise well-formed — this endpoint targets the
/// backfill bug's signature, it is not a generic "set session times"
/// primitive. Control: a genuinely flattened sibling session is repaired in
/// the same request.
#[tokio::test]
async fn correctly_dated_session_is_not_flattened_and_untouched() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let correct_sid = seed_session(&store, "default", "scratch", true).await;
    let flattened_sid = seed_session(&store, "default", "scratch", true).await;
    let (correct_started, correct_ended) = session_times(&store, correct_sid).await;
    let now = now_us();

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": [
                // Repeats the session's own start and nearly (not exactly,
                // to keep this test distinct from a plain no-op) its own
                // end.
                {
                    "session_id": correct_sid.to_string(),
                    "started_at_us": correct_started,
                    "ended_at_us": correct_ended.map(|e| e - 1),
                },
                {
                    "session_id": flattened_sid.to_string(),
                    "started_at_us": now - 1_000_000_000,
                    "ended_at_us": now - 999_000_000,
                },
            ],
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["repaired"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(
        body["repaired"][0]["session_id"],
        json!(flattened_sid.to_string()),
        "control (flattened session) must be repaired"
    );
    assert_eq!(body["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["skipped"][0]["session_id"],
        json!(correct_sid.to_string())
    );
    assert_eq!(body["skipped"][0]["reason"], json!("not_flattened"));

    assert_eq!(
        session_times(&store, correct_sid).await,
        (correct_started, correct_ended),
        "a correctly-dated hook-captured session must not be touched"
    );
}

/// An open session (no `ended_at`) never has an end time imposed on it, even
/// when the candidate carries one; `started_at` is still applied.
#[tokio::test]
async fn open_session_keeps_null_end() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let sid = seed_session(&store, "default", "scratch", false).await;
    let now = now_us();
    let new_started = now - 1_000_000_000;

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": [{
                "session_id": sid.to_string(),
                "started_at_us": new_started,
                "ended_at_us": now - 999_000_000,
            }],
            "confirm": true,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["repaired"][0]["end_kept_open"], json!(true));

    let (started, ended) = session_times(&store, sid).await;
    assert_eq!(started, new_started);
    assert!(ended.is_none(), "an open session must stay open");
}

/// Future, negative, and inverted times are refused outright.
#[tokio::test]
async fn refuses_future_negative_and_inverted_times() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let router = admin_router(state);
    let now = now_us();

    for (label, item) in [
        (
            "future",
            json!({"started_at_us": now + 60 * 60 * 1_000_000}),
        ),
        ("negative", json!({"started_at_us": -1})),
        (
            "inverted",
            json!({"started_at_us": now - 1_000, "ended_at_us": now - 2_000}),
        ),
    ] {
        let sid = seed_session(&store, "default", "scratch", true).await;
        let mut item = item;
        item["session_id"] = json!(sid.to_string());
        let payload = json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": [item],
            "confirm": true,
        });
        let req = Request::builder()
            .method("POST")
            .uri("/admin/repair-session-times")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{label}");
        let body = body_json(resp).await;
        assert!(
            body["repaired"].as_array().unwrap().is_empty(),
            "{label}: {body}"
        );
        assert_eq!(body["skipped"].as_array().unwrap().len(), 1, "{label}");
    }
}

/// The request body is bounded: a batch over the limit is refused before any
/// scope lookup or write.
#[tokio::test]
async fn bounds_the_number_of_candidates() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;
    let sessions: Vec<_> = (0..2_001)
        .map(|_| json!({"session_id": SessionId::new().to_string(), "started_at_us": 1}))
        .collect();

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": sessions,
            "confirm": false,
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Exactly at the limit (2,000) is accepted — the bound is `> MAX`, not
/// `>= MAX`.
#[tokio::test]
async fn exact_limit_of_candidates_is_accepted() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    // Establish the scope so the request passes the scope lookup and reaches
    // per-candidate validation (all `not_found`, since these ids are random).
    store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let sessions: Vec<_> = (0..2_000)
        .map(|_| json!({"session_id": SessionId::new().to_string(), "started_at_us": 1}))
        .collect();

    let resp = post(
        state,
        "/admin/repair-session-times",
        json!({
            "workspace": "default",
            "project": "scratch",
            "sessions": sessions,
            "confirm": false,
        }),
    )
    .await;
    // Scope itself does not exist (only the workspace does), so this is a
    // 404 rather than 200 — the point is that it is NOT the 400 the
    // over-the-limit test gets, i.e. the bound check itself passed.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Mirrors `trusted_proxy_topology_gates_admin_without_db_users` in
/// `admin.rs` (the same gate `move-session`/`purge-session` sit behind, via
/// the shared `require_root_for_multiuser_admin` middleware): with
/// `trusted_proxy_identity: true`, only `AuthLevel::Root` may call this
/// endpoint. Control: root succeeds.
#[tokio::test]
async fn requires_root_once_multiuser() {
    use ai_memory_core::ActorContext;

    let tmp = TempDir::new().unwrap();
    let (mut state, store) = make_state(&tmp).await;
    let sid = seed_session(&store, "default", "scratch", true).await;
    state.trusted_proxy_identity = true;

    let router: Router = admin_router(state).layer(axum::middleware::from_fn(
        |mut req: Request<Body>, next: axum::middleware::Next| async move {
            let level = match req
                .headers()
                .get("x-test-auth-level")
                .and_then(|v| v.to_str().ok())
            {
                Some("root") => ai_memory_core::AuthLevel::Root,
                Some("user") => ai_memory_core::AuthLevel::User,
                _ => ai_memory_core::AuthLevel::Anonymous,
            };
            req.extensions_mut().insert(level);
            req.extensions_mut().insert(ActorContext::anonymous());
            next.run(req).await
        },
    ));

    let body = json!({
        "workspace": "default",
        "project": "scratch",
        "sessions": [{"session_id": sid.to_string(), "started_at_us": 1_700_000_000_000_000i64}],
        "confirm": false,
    });

    for (level, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("user"), StatusCode::FORBIDDEN),
        (Some("root"), StatusCode::OK),
    ] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/admin/repair-session-times")
            .header("content-type", "application/json");
        if let Some(level) = level {
            req = req.header("x-test-auth-level", level);
        }
        let resp = router
            .clone()
            .oneshot(
                req.body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), expected, "level={level:?}");
    }
}
