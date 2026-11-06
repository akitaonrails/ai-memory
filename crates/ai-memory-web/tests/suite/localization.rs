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

/// Attributes whose value comes from the translation catalog. A script that
/// selects on one of them works only in the language it was written in (#2024).
const LOCALIZED_ATTRIBUTES: [&str; 7] = [
    "aria-label",
    "aria-description",
    "aria-roledescription",
    "aria-placeholder",
    "aria-valuetext",
    "title",
    "placeholder",
];

/// The string-literal arguments a script passes to the DOM's CSS selector
/// methods. Selectors built from variables are skipped: they are not literal
/// markup contracts.
fn selector_literals(script: &str) -> Vec<String> {
    let mut selectors = Vec::new();
    for method in [
        ".querySelector(",
        ".querySelectorAll(",
        ".closest(",
        ".matches(",
    ] {
        for (start, _) in script.match_indices(method) {
            let rest = &script[start + method.len()..];
            let Some(quote) = rest.chars().next().filter(|c| matches!(c, '\'' | '"')) else {
                continue;
            };
            if let Some(end) = rest[1..].find(quote) {
                selectors.push(rest[1..1 + end].to_owned());
            }
        }
    }
    selectors
}

/// The `[name]` and `[name="value"]` parts of a selector, as (name, value).
fn attribute_parts(selector: &str) -> Vec<(String, Option<String>)> {
    selector
        .split('[')
        .skip(1)
        .filter_map(|part| part.split_once(']').map(|(inner, _)| inner))
        .map(|inner| match inner.split_once('=') {
            Some((name, value)) => (
                name.trim_end_matches(['^', '$', '*', '~', '|'])
                    .trim()
                    .to_owned(),
                Some(value.trim_matches(['"', '\'']).to_owned()),
            ),
            None => (inner.trim().to_owned(), None),
        })
        .collect()
}

fn localized_selectors(source: &str) -> Vec<String> {
    selector_literals(source)
        .into_iter()
        .filter(|selector| {
            selector.contains("{{")
                || attribute_parts(selector)
                    .iter()
                    .any(|(name, _)| LOCALIZED_ATTRIBUTES.contains(&name.as_str()))
        })
        .collect()
}

#[test]
fn no_template_or_script_selects_elements_by_translated_text() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut pending = vec![root.join("templates"), root.join("static")];
    let mut scanned = 0;
    let mut offenders = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if !matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("html" | "js")
            ) {
                continue;
            }
            scanned += 1;
            let source = std::fs::read_to_string(&path).unwrap();
            for selector in localized_selectors(&source) {
                offenders.push(format!("{}: {selector}", path.display()));
            }
        }
    }
    assert!(scanned > 20, "the sweep found only {scanned} files");
    assert!(
        offenders.is_empty(),
        "scripts must select by id, class, role or data-* attributes, \
         never by translated text: {offenders:#?}"
    );
}

#[test]
fn the_selector_sweep_flags_localized_attributes() {
    let script = r#"document.querySelector('[role="tablist"][aria-label="Project sections"]');
        el.closest("[title^='Open']");el.matches('[data-x]');
        document.querySelectorAll('[aria-label="{{ i18n.text("x") }}"]');"#;
    assert_eq!(
        localized_selectors(script),
        vec![
            r#"[role="tablist"][aria-label="Project sections"]"#.to_owned(),
            r#"[aria-label="{{ i18n.text("x") }}"]"#.to_owned(),
            "[title^='Open']".to_owned(),
        ]
    );
}

#[tokio::test]
async fn project_tabs_script_finds_its_tablist_in_every_language() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let workspace = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(workspace, "scratch", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let router = crate::router(store.reader.clone(), wiki);
    for (language, label) in [("pt-BR", "Seções do projeto"), ("en", "Project sections")] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/w/default/scratch")
                    .header(header::ACCEPT_LANGUAGE, language)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LANGUAGE], language);
        let html = body(response).await;
        let tag_start = html.find("<nav class=\"project-tabs\"").unwrap();
        let tablist = &html[tag_start..tag_start + html[tag_start..].find('>').unwrap()];
        assert!(
            tablist.contains(&format!("aria-label=\"{label}\"")),
            "{tablist}"
        );
        let script_start = html.find("<script id=\"project-tabs-behavior\">").unwrap();
        let script =
            &html[script_start..script_start + html[script_start..].find("</script>").unwrap()];
        let selector = selector_literals(script)
            .into_iter()
            .find(|selector| selector.contains("tablist"))
            .expect("the tabs script selects its tablist with a literal selector");
        for (name, value) in attribute_parts(&selector) {
            let present = match value {
                Some(value) => tablist.contains(&format!("{name}=\"{value}\"")),
                None => tablist
                    .split_whitespace()
                    .any(|token| token == name || token.starts_with(&format!("{name}="))),
            };
            assert!(
                present,
                "{language}: `{selector}` does not match the rendered tablist {tablist}"
            );
        }
    }
}
