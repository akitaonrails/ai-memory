//! Public management assets disclose no data and cannot be framed, and the
//! plain form adapter refuses a cross-origin POST before it reaches the engine.
use ai_memory_web::management;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, header},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

/// A session id the form adapter accepts, so a refused request is refused for
/// its origin rather than for its payload.
const SESSION: &str = "0198f2c1-4a6e-7c31-9f2b-8d7e6a5b4c3d";
const HOST: &str = "memory.example";

/// Stand-in engine that records the one mutation the console can reach.
#[derive(Clone)]
struct Engine {
    purges: Arc<AtomicUsize>,
}

async fn engine_me() -> Json<Value> {
    Json(json!({
        "username": "root",
        "capabilities": {"user_management": true},
        "must_change_password": false,
    }))
}

async fn engine_purge(State(engine): State<Engine>) -> Json<Value> {
    engine.purges.fetch_add(1, Ordering::SeqCst);
    Json(json!({"files_failed": []}))
}

fn console(engine: Engine) -> Router {
    let backend = Router::new()
        .route("/auth/me", get(engine_me))
        .route(
            "/management/v1/sessions/{session}/purge",
            post(engine_purge),
        )
        .with_state(engine);
    Router::new().nest("/memory", management::router("/memory", backend))
}

/// `Origin` and `Sec-Fetch-Site` are what a browser puts on a form POST; the
/// harness sets exactly what each case needs and nothing else.
fn form_post(origin: Option<&str>, fetch_site: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/memory/manage/action")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::HOST, HOST);
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    if let Some(site) = fetch_site {
        builder = builder.header("sec-fetch-site", site);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 400_000)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn purge_form() -> String {
    format!("action=purge&session={SESSION}&workspace=default&project=scratch&confirm=true")
}

#[tokio::test]
async fn management_assets_work_under_a_prefix_and_deny_framing() {
    let router = axum::Router::new().nest(
        "/memory",
        ai_memory_web::management::router("/memory", axum::Router::new()),
    );
    for uri in [
        "/memory/manage/login",
        "/memory/manage/static/tailwind.css",
        "/memory/manage/static/logo.png",
        "/memory/manage/static/instrument-sans-latin-wdth-normal.woff2",
        "/memory/manage/static/bricolage-grotesque-latin-standard-normal.woff2",
        "/memory/manage/static/commit-mono-latin-400-normal.woff2",
        "/memory/manage/static/font-licenses.txt",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-frame-options"], "DENY");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let bytes = axum::body::to_bytes(response.into_body(), 400_000)
            .await
            .unwrap();
        if uri.ends_with("login") {
            let html = String::from_utf8(bytes.to_vec()).unwrap();
            assert!(html.contains("action=\"/memory/manage/action\""));
            assert!(html.contains("method=\"post\""));
            assert!(!html.contains("aim_"));
        }
    }
    // Only explicit vendored assets exist; no filesystem-backed wildcard.
    for uri in [
        "/memory/manage/static/app.js",
        "/memory/manage/static/config.toml",
        "/memory/manage/static/fonts.txt",
        "/memory/manage/static/%2e%2e%2fconfig.toml",
        "/memory/manage/static/../../db/memory.sqlite",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

/// A destructive form POST from a foreign page mutates nothing, while the same
/// POST with the console's own origin still reaches the engine. Without the
/// guard the first case answers 303 and the engine counts a purge; with a
/// blanket deny the control case never reaches it either.
#[tokio::test]
async fn management_form_refuses_a_foreign_origin_and_admits_its_own() {
    let engine = Engine {
        purges: Arc::new(AtomicUsize::new(0)),
    };
    let router = console(engine.clone());

    for (origin, fetch_site) in [
        (Some("https://evil.example"), Some("cross-site")),
        (Some("https://evil.example"), Some("same-origin")),
        (
            Some("https://memory.example.evil.example"),
            Some("same-origin"),
        ),
    ] {
        let response = router
            .clone()
            .oneshot(form_post(origin, fetch_site, &purge_form()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "origin {origin:?} with sec-fetch-site {fetch_site:?} must be refused"
        );
        assert!(
            body_text(response)
                .await
                .contains("Cross-origin form request refused")
        );
        assert_eq!(
            engine.purges.load(Ordering::SeqCst),
            0,
            "a refused form must not reach the engine"
        );
    }

    let response = router
        .clone()
        .oneshot(form_post(
            Some("https://memory.example"),
            Some("same-origin"),
            &purge_form(),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "the console's own origin must still mutate"
    );
    assert_eq!(engine.purges.load(Ordering::SeqCst), 1);
}

/// The same-origin check is a conjunction: a matching `Origin` is not enough
/// when the fetch metadata says cross-site, and an absent `Origin` is refused
/// rather than treated as same-origin.
#[tokio::test]
async fn management_form_refuses_cross_site_metadata_and_a_missing_origin() {
    let engine = Engine {
        purges: Arc::new(AtomicUsize::new(0)),
    };
    let router = console(engine.clone());

    for (origin, fetch_site) in [
        (Some("https://memory.example"), Some("cross-site")),
        (Some("https://memory.example"), Some("same-site")),
        (None, None),
        (None, Some("same-origin")),
    ] {
        let response = router
            .clone()
            .oneshot(form_post(origin, fetch_site, &purge_form()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "origin {origin:?} with sec-fetch-site {fetch_site:?} must be refused"
        );
    }
    assert_eq!(engine.purges.load(Ordering::SeqCst), 0);

    // A non-HTTP scheme and a non-root path in `Origin` are not same-origin.
    for origin in ["null", "https://memory.example/other"] {
        let response = router
            .clone()
            .oneshot(form_post(Some(origin), Some("same-origin"), &purge_form()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "origin {origin}");
    }
    assert_eq!(engine.purges.load(Ordering::SeqCst), 0);
}
