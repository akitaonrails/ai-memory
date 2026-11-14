//! Public management assets disclose no data and cannot be framed.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

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
        "/memory/manage/static/ibm-plex-sans-latin-400-normal.woff2",
        "/memory/manage/static/chakra-petch-latin-600-normal.woff2",
        "/memory/manage/static/jetbrains-mono-latin-400-normal.woff2",
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
        "/memory/manage/static/instrument-sans-latin-wdth-normal.woff2",
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
