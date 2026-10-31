//! `GET /admin/metrics` end to end, behind the production dual-auth
//! middleware, on one router that also serves `/mcp` and `/hook` with shared
//! counters, the way `serve` assembles them.
//!
//! The endpoint is content-free: counts and durations only, labels from a
//! closed set, scope never a label. The adversarial test plants
//! secret-shaped text in a query, a hook body and a project name and proves
//! none of it reaches the scrape, while the counters still move (control).
//! On a multi-user server the route is root-only.

use std::sync::Arc;

use ai_memory_core::{ActiveProject, IngestMetrics, Sanitizer};
use ai_memory_hooks::{HookState, ProjectCacheStore, SubagentSessionSet, hook_router};
use ai_memory_llm::ProviderHealth;
use ai_memory_mcp::auth::AuthState;
use ai_memory_mcp::{AdminState, AiMemoryServer, admin_router, require_dual_auth};
use ai_memory_store::{DecayParams, Store, TokenPepper};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const ROOT: &str = "root-token-for-metrics";
// Split with `concat!` so secret scanners do not flag the planted canaries.
const SECRET_QUERY: &str = concat!("sk-ant-", "api03-QUERYplanted0123456789abcdefXYZ");
const SECRET_HOOK: &str = concat!("ghp_", "HOOKplanted0123456789abcdefghijklmnop");
const SECRET_PROJECT: &str = "proj-AKIAPROJECTPLANTED7777";

struct Harness {
    _tmp: TempDir,
    router: Router,
    ingest_semaphore: Arc<tokio::sync::Semaphore>,
}

async fn harness() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, SECRET_PROJECT, None)
        .await
        .expect("proj");
    let pepper = TokenPepper::new("metrics-test-pepper");

    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki.clone());
    let mcp_metrics = server.mcp_metrics();
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );

    let ingest_metrics = Arc::new(IngestMetrics::default());
    let active_project = ActiveProject::new();
    let ingest_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let hooks = hook_router(HookState {
        ingest_metrics: ingest_metrics.clone(),
        workspace_id: ws,
        project_id: proj,
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki: wiki.clone(),
        consolidator: None,
        sanitizer: Sanitizer::default(),
        project_cache: Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default())),
        active_project: active_project.clone(),
        ingest_semaphore: ingest_semaphore.clone(),
        ingest_gates: ai_memory_hooks::IngestGates::default(),
        cwd_probes: ai_memory_hooks::CwdProbes::default(),
        consolidate_on_session_end: false,
        session_consolidation_notify: None,
        profile_notify: None,
        capture_assistant_enabled: false,
        claim_handoff_on_session_start: false,
        create_handoff_on_session_end: false,
        per_user_slots: false,
        mid_session_routing: ai_memory_core::MidSessionRouting::default(),
        profile: ai_memory_core::profile::ProfileSettings::default(),
        ingest_drop_list: ai_memory_hooks::IngestDropList::default(),
        record_tool_names: false,
        skip_wiki_checkpoints: false,
        subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
        ingest_rate: Arc::new(tokio::sync::Mutex::new(
            ai_memory_hooks::IngestRateLimiter::disabled(),
        )),
        home_dir: None,
        trusted_proxy_identity: false,
    });

    let admin = admin_router(AdminState {
        ingest_metrics,
        mcp_metrics,
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ProviderHealth::default(),
        decay_params: DecayParams::default(),
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        data_dir: tmp.path().to_path_buf(),
        db_path: store.db_path().to_path_buf(),
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_gate: ai_memory_mcp::BootstrapGate::default(),
        token_pepper: Some(pepper.clone()),
        active_project,
        scope_invalidator: None,
        trusted_proxy_identity: false,
    });

    let auth_state = Arc::new(AuthState::new(Some(ROOT.to_string())).with_multiuser(
        pepper,
        store.reader.clone(),
        store.writer.clone(),
    ));
    let router = Router::new()
        .nest_service("/mcp", mcp_service)
        .merge(hooks)
        .merge(admin)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_dual_auth,
        ));
    Harness {
        _tmp: tmp,
        router,
        ingest_semaphore,
    }
}

async fn send(router: &Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8_000_000)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn scrape(router: &Router, bearer: Option<&str>) -> (StatusCode, String) {
    let mut req = Request::builder().uri("/admin/metrics");
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    send(router, req.body(Body::empty()).unwrap()).await
}

/// The value of one exact series line, e.g. `name{label="x"}`.
fn sample(text: &str, series: &str) -> f64 {
    text.lines()
        .find_map(|line| {
            line.strip_prefix(series)
                .and_then(|rest| rest.strip_prefix(' '))
        })
        .unwrap_or_else(|| panic!("series {series} missing:\n{text}"))
        .parse()
        .expect("numeric sample")
}

async fn memory_query(router: &Router) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_query",
            "arguments": {
                "query": format!("where did we leave {SECRET_QUERY}"),
                "workspace": "default",
                "project": SECRET_PROJECT,
                "explain": true,
            },
        },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("authorization", format!("Bearer {ROOT}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text) = send(router, req).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let v: Value = serde_json::from_str(&text).expect("json-rpc");
    assert!(v.get("error").is_none(), "JSON-RPC error: {v}");
    assert_ne!(
        v.pointer("/result/isError"),
        Some(&json!(true)),
        "tool error: {v}"
    );
}

async fn hook(h: &Harness, event: &str, body: Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/hook?event={event}&agent=claude-code&workspace=default&project={SECRET_PROJECT}"
        ))
        .header("authorization", format!("Bearer {ROOT}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text) = send(&h.router, req).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{text}");
    // One permit: it is back once the spawned processing finished, so the
    // next event never races this one for capacity.
    drop(
        h.ingest_semaphore
            .acquire()
            .await
            .expect("semaphore stays open"),
    );
}

#[tokio::test]
async fn metrics_count_queries_and_hooks_without_leaking_any_content() {
    let h = harness().await;
    let (status, before) = scrape(&h.router, Some(ROOT)).await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(
        sample(&before, "ai_memory_query_duration_seconds_count"),
        0.0
    );
    assert_eq!(
        sample(&before, "ai_memory_hook_request_duration_seconds_count"),
        0.0
    );

    memory_query(&h.router).await;
    let session = "11111111-2222-7333-8444-555555555555";
    hook(
        &h,
        "session-start",
        json!({ "session_id": session, "cwd": format!("/work/{SECRET_PROJECT}") }),
    )
    .await;
    hook(
        &h,
        "user-prompt-submit",
        json!({
            "session_id": session,
            "cwd": format!("/work/{SECRET_PROJECT}"),
            "prompt": format!("deploy with token {SECRET_HOOK} and {SECRET_QUERY}"),
        }),
    )
    .await;

    let (status, after) = scrape(&h.router, Some(ROOT)).await;
    assert_eq!(status, StatusCode::OK, "{after}");
    // Control: the counters moved.
    assert_eq!(
        sample(&after, "ai_memory_query_duration_seconds_count"),
        1.0
    );
    assert_eq!(
        sample(
            &after,
            "ai_memory_query_duration_seconds_bucket{le=\"+Inf\"}"
        ),
        1.0
    );
    assert_eq!(
        sample(&after, "ai_memory_hook_request_duration_seconds_count"),
        2.0
    );
    assert_eq!(
        sample(
            &after,
            "ai_memory_hook_admissions_total{result=\"accepted\"}"
        ),
        2.0
    );
    assert_eq!(
        sample(&after, "ai_memory_mcp_tool_calls_total{kind=\"read\"}"),
        1.0
    );
    assert_eq!(
        sample(&after, "ai_memory_mcp_tool_calls_total{kind=\"write\"}"),
        0.0
    );

    // Adversarial: nothing the callers sent reaches the scrape.
    for planted in [
        SECRET_QUERY,
        SECRET_HOOK,
        SECRET_PROJECT,
        "QUERYplanted",
        "HOOKplanted",
        "PROJECTPLANTED",
        "sk-ant",
        "ghp_",
        "AKIA",
        "where did we leave",
        "deploy with token",
        "/work/",
        "default",
        session,
        ROOT,
    ] {
        assert!(
            !after.contains(planted),
            "/admin/metrics leaked {planted:?}:\n{after}"
        );
    }
    // Every label is one of the fixed, compile-time label names.
    for line in after.lines().filter(|l| !l.starts_with('#')) {
        if let Some((_, labels)) = line.split_once('{') {
            let key = labels.split('=').next().unwrap();
            assert!(
                ["result", "outcome", "kind", "le"].contains(&key),
                "unexpected label in {line}"
            );
        }
    }
}

#[tokio::test]
async fn metrics_are_root_only_on_a_multi_user_server() {
    let h = harness().await;
    let create = Request::builder()
        .method("POST")
        .uri("/admin/human-users")
        .header("authorization", format!("Bearer {ROOT}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "username": "alice" }).to_string()))
        .unwrap();
    let (status, created) = send(&h.router, create).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let issue = Request::builder()
        .method("POST")
        .uri("/admin/api-credentials")
        .header("authorization", format!("Bearer {ROOT}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "username": "alice", "label": "metrics" }).to_string(),
        ))
        .unwrap();
    let (status, issued) = send(&h.router, issue).await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    let issued: Value = serde_json::from_str(&issued).unwrap();
    let alice_token = issued["token"].as_str().expect("DB-user token").to_string();

    let (status, body) = scrape(&h.router, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "anonymous: {body}");
    assert!(!body.contains("ai_memory_"), "{body}");

    let (status, body) = scrape(&h.router, Some(&alice_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "DB user: {body}");
    assert!(!body.contains("ai_memory_"), "{body}");

    let (status, body) = scrape(&h.router, Some("not-a-real-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "unknown bearer: {body}");

    // Control: root still reads the metrics.
    let (status, body) = scrape(&h.router, Some(ROOT)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("# TYPE ai_memory_query_duration_seconds histogram"));
}
