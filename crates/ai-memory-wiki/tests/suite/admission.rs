//! Integration tests for the admission webhook chain.
//!
//! Each test spins up a real `axum` server bound to a random loopback
//! port, points an [`AdmissionChain`] at it, and exercises the chain end
//! to end. We deliberately do NOT build a full `Wiki` here — those are
//! covered by the engine-level test suite. These tests pin down the
//! HTTP contract: payload shape, mutation semantics, failure policy,
//! and the loop-prevention skip list.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ai_memory_core::{ActorContext, PagePath};
use ai_memory_wiki::{
    AdmissionChain, AdmissionContext, AdmissionOp, FailurePolicy, Markdown, WebhookConfig,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

/// Spawn an axum app on a random loopback port. Returns the base URL.
async fn spawn_server(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // No settle needed: the socket is listening as soon as `bind` returns, so a
    // client connecting before the serve task is polled just queues in the
    // backlog.
    format!("http://{addr}")
}

fn page() -> (PagePath, Markdown) {
    (
        PagePath::new("gotchas/example.md").unwrap(),
        Markdown {
            frontmatter: json!({"title": "Example", "tags": ["t1"]}),
            body: "body content\n".to_string(),
        },
    )
}

#[tokio::test]
async fn webhook_mutating_frontmatter_is_applied() {
    let app = Router::new().route(
        "/enrich",
        post(|Json(payload): Json<Value>| async move {
            // Echo back a mutation: add `contributors: ["claude-code"]`.
            let mut fm = payload["page"]["frontmatter"].clone();
            let arr = fm
                .as_object_mut()
                .unwrap()
                .entry("contributors")
                .or_insert_with(|| json!([]));
            arr.as_array_mut().unwrap().push(json!("claude-code"));
            (
                StatusCode::OK,
                Json(json!({ "page": { "frontmatter": fm } })),
            )
        }),
    );
    let base = spawn_server(app).await;

    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "contributors".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await
        .unwrap();

    let contribs = md.frontmatter.get("contributors").expect("set by webhook");
    assert_eq!(contribs[0], json!("claude-code"));
    // Body untouched (webhook only returned frontmatter).
    assert_eq!(md.body, "body content\n");
}

#[tokio::test]
async fn webhook_204_is_noop() {
    let app = Router::new().route("/enrich", post(|| async { StatusCode::NO_CONTENT }));
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "noop".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let before_fm = md.frontmatter.clone();
    let before_body = md.body.clone();
    chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await
        .unwrap();
    assert_eq!(md.frontmatter, before_fm);
    assert_eq!(md.body, before_body);
}

#[tokio::test]
async fn failure_policy_ignore_swallows_error() {
    let app = Router::new().route(
        "/enrich",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "flaky".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    // Should NOT return Err — Ignore policy swallows the 500.
    let res = chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await;
    assert!(
        res.is_ok(),
        "ignore policy should not propagate error; got {res:?}"
    );
}

#[tokio::test]
async fn failure_policy_reject_aborts_write() {
    let app = Router::new().route(
        "/enrich",
        post(|| async { (StatusCode::FORBIDDEN, "no secrets") }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "validate-no-secrets".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let res = chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await;
    assert!(res.is_err(), "reject policy should propagate the error");
}

#[tokio::test]
async fn skip_webhooks_short_circuits_named_hook() {
    // Track whether the webhook was invoked. Asserted false at end —
    // proves the chain never POSTed (rather than panicking inside the
    // handler, which axum would convert into a 500 the chain might swallow).
    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_handler = fired.clone();
    let app = Router::new().route(
        "/enrich",
        post(move || {
            let fired = fired_in_handler.clone();
            async move {
                fired.store(true, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "contributors".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let ctx = AdmissionContext {
        skip_webhooks: vec!["contributors".to_string()],
        ..AdmissionContext::default()
    };
    chain.run(&path, &mut md, &ctx).await.unwrap();
    assert!(
        !fired.load(Ordering::SeqCst),
        "webhook must not have been called"
    );
}

#[tokio::test]
async fn op_filter_skips_webhook_not_subscribed_to_event() {
    // Webhook subscribes only to Consolidate; we call with WritePage.
    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_handler = fired.clone();
    let app = Router::new().route(
        "/enrich",
        post(move || {
            let fired = fired_in_handler.clone();
            async move {
                fired.store(true, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "only-consolidate".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::Consolidate],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let ctx = AdmissionContext {
        op: AdmissionOp::WritePage,
        ..AdmissionContext::default()
    };
    chain.run(&path, &mut md, &ctx).await.unwrap();
    assert!(
        !fired.load(Ordering::SeqCst),
        "webhook must not have been called"
    );
}

#[tokio::test]
async fn chain_runs_in_order_each_sees_previous_mutation() {
    // Two webhooks. First adds `step: 1` to frontmatter. Second adds
    // `step_after: <whatever step is>`. Order must be preserved.
    let recorder: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/first",
            post(|Json(payload): Json<Value>| async move {
                let mut fm = payload["page"]["frontmatter"].clone();
                fm["step"] = json!(1);
                Json(json!({ "page": { "frontmatter": fm } }))
            }),
        )
        .route(
            "/second",
            post({
                let recorder = recorder.clone();
                move |Json(payload): Json<Value>| {
                    let recorder = recorder.clone();
                    async move {
                        let step = payload["page"]["frontmatter"]["step"].clone();
                        recorder.lock().unwrap().push(format!("step_seen={step}"));
                        let mut fm = payload["page"]["frontmatter"].clone();
                        fm["step_after"] = step;
                        Json(json!({ "page": { "frontmatter": fm } }))
                    }
                }
            }),
        );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![
        WebhookConfig {
            name: "first".into(),
            url: format!("{base}/first"),
            timeout_ms: 1_000,
            failure_policy: FailurePolicy::Ignore,
            events: vec![AdmissionOp::WritePage],
            blocking: true,
        },
        WebhookConfig {
            name: "second".into(),
            url: format!("{base}/second"),
            timeout_ms: 1_000,
            failure_policy: FailurePolicy::Ignore,
            events: vec![AdmissionOp::WritePage],
            blocking: true,
        },
    ])
    .unwrap();

    let (path, mut md) = page();
    chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await
        .unwrap();

    assert_eq!(md.frontmatter["step"], json!(1));
    assert_eq!(md.frontmatter["step_after"], json!(1));
    let log = recorder.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0], "step_seen=1");
}

#[tokio::test]
async fn x_memory_op_header_is_sent() {
    // Track received header so we can assert on it.
    let recorder: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let recorder_clone = recorder.clone();
    let app = Router::new().route(
        "/enrich",
        post(move |headers: HeaderMap| {
            let recorder = recorder_clone.clone();
            async move {
                let v = headers
                    .get("X-Memory-Op")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                *recorder.lock().unwrap() = v;
                StatusCode::NO_CONTENT.into_response()
            }
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "any".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::Consolidate],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let ctx = AdmissionContext {
        op: AdmissionOp::Consolidate,
        ..AdmissionContext::default()
    };
    chain.run(&path, &mut md, &ctx).await.unwrap();

    assert_eq!(recorder.lock().unwrap().as_deref(), Some("consolidate"));
}

#[tokio::test]
async fn actor_context_is_propagated_in_payload() {
    let recorder: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let recorder_clone = recorder.clone();
    let app = Router::new().route(
        "/enrich",
        post(move |Json(payload): Json<Value>| {
            let recorder = recorder_clone.clone();
            async move {
                *recorder.lock().unwrap() = Some(payload);
                StatusCode::NO_CONTENT.into_response()
            }
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "echo".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let ctx = AdmissionContext {
        actor: ActorContext {
            agent: Some("claude-code".into()),
            user: Some("djalmajr".into()),
            sub: Some("8f3a-uuid".into()),
            client: Some("72836f52-uuid".into()),
            session_id: Some("019e6d-session".into()),
            ..ActorContext::default()
        },
        ..AdmissionContext::default()
    };
    chain.run(&path, &mut md, &ctx).await.unwrap();

    let payload = recorder.lock().unwrap().clone().expect("recorded");
    let actor = &payload["ctx"]["actor"];
    assert_eq!(actor["agent"], json!("claude-code"));
    assert_eq!(actor["user"], json!("djalmajr"));
    assert_eq!(actor["client"], json!("72836f52-uuid"));
    assert_eq!(payload["page"]["path"], json!("gotchas/example.md"));
}

#[tokio::test]
async fn webhook_can_mutate_body_too() {
    let app = Router::new().route(
        "/enrich",
        post(|Json(_payload): Json<Value>| async move {
            (
                StatusCode::OK,
                Json(json!({ "page": { "body": "REWRITTEN" } })),
            )
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "body-mutator".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let before_fm = md.frontmatter.clone();
    chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await
        .unwrap();
    assert_eq!(md.body, "REWRITTEN");
    assert_eq!(md.frontmatter, before_fm, "frontmatter untouched");
}

#[tokio::test]
async fn chain_rejects_more_than_max_admission_webhooks() {
    // Mis-templated configs (helm loops etc.) shouldn't be able to
    // serialise a 1000-webhook chain — sequential latency would
    // dominate the write_page critical path.
    let too_many: Vec<WebhookConfig> = (0..=ai_memory_wiki::MAX_ADMISSION_WEBHOOKS)
        .map(|i| WebhookConfig {
            name: format!("w{i}"),
            url: format!("http://127.0.0.1:1/{i}"),
            timeout_ms: 100,
            failure_policy: FailurePolicy::Ignore,
            events: vec![AdmissionOp::WritePage],
            blocking: true,
        })
        .collect();
    let err = AdmissionChain::new(too_many).expect_err("should refuse");
    assert!(
        err.to_string().contains("admission chain capped"),
        "unexpected error message: {err}"
    );
}

#[tokio::test]
async fn oversized_response_is_treated_as_noop() {
    // A webhook returning > MAX_RESPONSE_BYTES gets dropped — never
    // forces the engine to buffer the payload or attempt to apply it.
    let big_body = "x".repeat(ai_memory_wiki::MAX_RESPONSE_BYTES + 1);
    let body_arc = Arc::new(big_body);
    let body_clone = body_arc.clone();
    let app = Router::new().route(
        "/enrich",
        post(move || {
            let body = body_clone.clone();
            async move { (StatusCode::OK, Json(json!({ "page": { "body": &*body } }))) }
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "huge".into(),
        url: format!("{base}/enrich"),
        timeout_ms: 5_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let before_body = md.body.clone();
    chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await
        .unwrap();
    assert_eq!(md.body, before_body, "oversized response must not mutate");
}

#[tokio::test]
async fn workspace_and_project_names_propagate_in_payload() {
    // Webhooks need human-readable workspace/project to address pages by
    // the same names the engine and UI use — otherwise external mirrors
    // (git-mirror, etc.) have to fall back to header introspection or
    // `_unscoped` placeholders. The chain serialises both fields directly
    // from the AdmissionContext.
    let recorder: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let recorder_clone = recorder.clone();
    let app = Router::new().route(
        "/sync",
        post(move |Json(payload): Json<Value>| {
            let recorder = recorder_clone.clone();
            async move {
                *recorder.lock().unwrap() = Some(payload);
                StatusCode::NO_CONTENT.into_response()
            }
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "git-mirror".into(),
        url: format!("{base}/sync"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let (path, mut md) = page();
    let ctx = AdmissionContext {
        workspace: "default".into(),
        project: "ai-memory-ops".into(),
        ..AdmissionContext::default()
    };
    chain.run(&path, &mut md, &ctx).await.unwrap();

    let payload = recorder.lock().unwrap().clone().expect("recorded");
    assert_eq!(payload["ctx"]["workspace"], json!("default"));
    assert_eq!(payload["ctx"]["project"], json!("ai-memory-ops"));
}

/// Records `(op-header, page.path)` for each request the webhook receives.
fn recording_app(seen: Arc<Mutex<Vec<(String, String)>>>) -> Router {
    Router::new().route(
        "/hook",
        post(move |headers: HeaderMap, Json(payload): Json<Value>| {
            let seen = seen.clone();
            async move {
                let op = headers
                    .get("X-Memory-Op")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let path = payload["page"]["path"].as_str().unwrap_or("").to_string();
                seen.lock().unwrap().push((op, path));
                StatusCode::NO_CONTENT
            }
        }),
    )
}

#[tokio::test]
async fn notify_delete_sends_op_and_path() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_server(recording_app(seen.clone())).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "mirror".into(),
        url: format!("{base}/hook"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::Delete],
        blocking: true,
    }])
    .unwrap();

    let ctx = AdmissionContext {
        op: AdmissionOp::Delete,
        ..AdmissionContext::default()
    };
    chain.notify(Some("runbooks/02.md"), &ctx).await.unwrap();

    let rec = seen.lock().unwrap();
    assert_eq!(rec.len(), 1, "delete must fire one notify");
    assert_eq!(rec[0].0, "delete");
    assert_eq!(rec[0].1, "runbooks/02.md");
}

#[tokio::test]
async fn notify_purge_has_op_and_no_path() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_server(recording_app(seen.clone())).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "mirror".into(),
        url: format!("{base}/hook"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::PurgeProject],
        blocking: true,
    }])
    .unwrap();

    let ctx = AdmissionContext {
        op: AdmissionOp::PurgeProject,
        project: "atlas".into(),
        ..AdmissionContext::default()
    };
    chain.notify(None, &ctx).await.unwrap();

    let rec = seen.lock().unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].0, "purge_project");
    assert_eq!(rec[0].1, "", "purge carries no page path");
}

#[tokio::test]
async fn notify_respects_op_subscription() {
    // A webhook subscribed only to WritePage must not receive a Delete notify.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let base = spawn_server(recording_app(seen.clone())).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "writes-only".into(),
        url: format!("{base}/hook"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: true,
    }])
    .unwrap();

    let ctx = AdmissionContext {
        op: AdmissionOp::Delete,
        ..AdmissionContext::default()
    };
    chain.notify(Some("x.md"), &ctx).await.unwrap();

    assert!(
        seen.lock().unwrap().is_empty(),
        "delete must not reach a write-only webhook"
    );
}

#[tokio::test]
async fn non_blocking_webhook_skipped_by_run_but_dispatched_async() {
    // A `blocking: false` webhook must NOT run in the synchronous `run`
    // path (it can't mutate/reject and shouldn't add write latency), and
    // MUST fire via `dispatch_async` after the page has landed.
    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_handler = fired.clone();
    let app = Router::new().route(
        "/sync",
        post(move || {
            let fired = fired_in_handler.clone();
            async move {
                fired.store(true, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let base = spawn_server(app).await;
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "mirror".into(),
        url: format!("{base}/sync"),
        timeout_ms: 1_000,
        failure_policy: FailurePolicy::Ignore,
        events: vec![AdmissionOp::WritePage],
        blocking: false,
    }])
    .unwrap();

    let (path, mut md) = page();

    // run() must NOT fire a non-blocking webhook.
    chain
        .run(&path, &mut md, &AdmissionContext::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !fired.load(Ordering::SeqCst),
        "run() must skip a non-blocking webhook"
    );

    // dispatch_async fires it (fire-and-forget; poll for the spawned task).
    chain.dispatch_async(
        Some(path.as_str()),
        &md.frontmatter,
        &md.body,
        &AdmissionContext::default(),
    );
    for _ in 0..40 {
        if fired.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        fired.load(Ordering::SeqCst),
        "dispatch_async must fire the non-blocking webhook"
    );
}
