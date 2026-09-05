//! Helpers shared by the admin route tests in this suite.

use ai_memory_mcp::{AdminState, admin_router};
use axum::body::Body;
use axum::http::Request;
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
