//! Helpers shared by the admin route tests in this suite.

use ai_memory_mcp::{AdminState, admin_router};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post as route_post;
use axum::{Json, Router};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

/// POST a JSON body to `uri` through a fresh admin router built from `state`.
pub async fn post(
    state: AdminState,
    uri: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    router.oneshot(req).await.unwrap()
}

/// GET `uri` through a fresh admin router built from `state`.
pub async fn get(state: AdminState, uri: &str) -> axum::response::Response {
    let router = admin_router(state);
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    router.oneshot(req).await.unwrap()
}

/// Spawn a one-shot webhook sink on a free loopback port. Returns its URL
/// and a receiver that yields the first JSON payload POSTed to it, so a test
/// can observe what a non-blocking admission webhook was sent.
pub async fn spawn_capture_hook() -> (String, tokio::sync::oneshot::Receiver<serde_json::Value>) {
    let (tx, rx) = tokio::sync::oneshot::channel::<serde_json::Value>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let app = Router::new().route(
        "/hook",
        route_post(move |Json(payload): Json<serde_json::Value>| {
            let tx = tx.clone();
            async move {
                if let Some(tx) = tx.lock().unwrap().take() {
                    let _ = tx.send(payload);
                }
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, rx)
}
