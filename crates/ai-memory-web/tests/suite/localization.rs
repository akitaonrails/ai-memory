//! Language is presentation state, independent of authorization and protocol data.

use ai_memory_core::AuthLevel;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use tempfile::TempDir;
use tower::ServiceExt;

async fn body(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn concurrent_languages_are_isolated_and_cookie_overrides_negotiation() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let router = crate::router(store.reader.clone(), wiki);
    let request = |language| {
        Request::builder()
            .uri("/")
            .header(header::ACCEPT_LANGUAGE, language)
            .body(Body::empty())
            .unwrap()
    };
    let (pt, en) = tokio::join!(
        router.clone().oneshot(request("pt-BR, en;q=0.5")),
        router.clone().oneshot(request("en, pt-BR;q=0.1"))
    );
    let pt = pt.unwrap();
    let en = en.unwrap();
    assert_eq!(pt.headers()[header::CONTENT_LANGUAGE], "pt-BR");
    assert_eq!(en.headers()[header::CONTENT_LANGUAGE], "en");
    assert!(
        pt.headers()
            .get_all(header::VARY)
            .iter()
            .any(|value| value.to_str().unwrap().contains("Accept-Language"))
    );
    let pt = body(pt).await;
    let en = body(en).await;
    assert!(pt.contains("<html lang=\"pt-BR\""));
    assert!(pt.contains("Painel de memória"));
    assert!(!pt.contains("Memory dashboard"));
    assert!(en.contains("Memory dashboard"));
    assert!(pt.contains("Português (Brasil)"));
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::ACCEPT_LANGUAGE, "pt-BR")
                .header(header::COOKIE, "ai_memory_language=en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_LANGUAGE], "en");
    let response = router.oneshot(request("fr-CA")).await.unwrap();
    assert_eq!(response.headers()[header::CONTENT_LANGUAGE], "en");
}

#[tokio::test]
async fn localized_root_page_keeps_the_existing_admin_gate() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    // A trusted identity proxy makes this a multi-operator deployment without
    // requiring test-only database authentication shortcuts.
    let split = crate::split_web_routers(
        true,
        store.reader.clone(),
        wiki,
        crate::WebMountSpec {
            enable_api: false,
            web_ui_dir: None,
            cors_origins: &[],
            web_slug: "",
            base_href: "/",
            base_path: "",
            trusted_proxy_identity: true,
            recall: crate::Recall::default(),
            runtime: None,
            health: None,
        },
    )
    .unwrap();
    for (level, expected) in [
        (AuthLevel::User, StatusCode::FORBIDDEN),
        (AuthLevel::Anonymous, StatusCode::UNAUTHORIZED),
        (AuthLevel::Root, StatusCode::OK),
    ] {
        let response = split
            .protected
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/pending")
                    .header(header::ACCEPT_LANGUAGE, "pt-BR")
                    .extension(level)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let text = body(response).await;
        if level == AuthLevel::User {
            assert!(text.contains("Acesso root necessário"), "{text}");
        }
        if level == AuthLevel::Root {
            assert!(text.contains("Gravações pendentes"), "{text}");
        }
    }
    let login = split
        .public
        .oneshot(
            Request::builder()
                .uri("/login")
                .header(header::ACCEPT_LANGUAGE, "pt-BR")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);
    let text = body(login).await;
    assert!(text.contains("Nome de usuário"));
    assert!(text.contains("Senha"));
}

#[tokio::test]
async fn api_response_is_identical_under_different_language_headers() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let router = crate::api_router(store.reader.clone(), wiki);
    let request = |language| {
        Request::builder()
            .uri("/projects")
            .header(header::ACCEPT_LANGUAGE, language)
            .body(Body::empty())
            .unwrap()
    };
    let en = router.clone().oneshot(request("en")).await.unwrap();
    let pt = router.oneshot(request("pt-BR")).await.unwrap();
    assert!(pt.headers().get(header::CONTENT_LANGUAGE).is_none());
    assert_eq!(body(en).await, body(pt).await);
}
