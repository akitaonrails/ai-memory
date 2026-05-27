//! Smoke integration tests for the read-only web UI.
//!
//! Spins up a `Store` + `Wiki` in a tempdir, seeds two pages, builds
//! the router, and exercises each route via `tower::ServiceExt::oneshot`.

use ai_memory_core::{AgentKind, NewHandoff, NewPage, PagePath, Tier};
use ai_memory_store::Store;
use ai_memory_web::{api_router, router};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::Value;
use tempfile::TempDir;
use tower::ServiceExt;

async fn setup() -> (TempDir, Store, Wiki) {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    (tmp, store, wiki)
}

fn new_page(
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    path: &str,
    title: &str,
    body: &str,
) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.to_owned(),
        body: body.to_owned(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({"kind": "fact"}),
        pinned: false,
        links: Vec::new(),
    }
}

fn wiki_req(
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    path: &str,
    body: &str,
) -> WritePageRequest {
    WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        frontmatter: serde_json::json!({"kind": "fact"}),
        body: body.to_owned(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
    }
}

#[tokio::test]
async fn smoke_index_returns_200() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, proj, "foo.md", "Foo Page", "Hello world"))
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("scratch"),
        "expected project name in index response"
    );
}

#[tokio::test]
async fn smoke_project_page_returns_200() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            ws,
            proj,
            "notes/bar.md",
            "Bar Note",
            "A note about bar",
        ))
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/w/default/scratch")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("Bar Note"),
        "expected page title in project response"
    );
}

#[tokio::test]
async fn smoke_page_view_returns_200() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    // Use wiki.write_page so the file is written to disk (needed for read_page).
    wiki.write_page(wiki_req(ws, proj, "foo.md", "# Foo\n\nHello world"))
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/w/default/scratch/p/foo.md")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    // The title is derived from the H1 heading.
    assert!(text.contains("Foo"), "expected page title");
    assert!(text.contains("Hello world"), "expected rendered body");
}

#[tokio::test]
async fn smoke_search_returns_200() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            ws,
            proj,
            "foo.md",
            "Searchable Page",
            "unique_term_xyz_abc",
        ))
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=unique_term_xyz_abc")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("unique_term_xyz_abc"),
        "expected search term in results"
    );
}

#[tokio::test]
async fn web_links_percent_encode_route_segments() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch #1", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            ws,
            proj,
            "notes/a b%25.md",
            "Encoded Link",
            "route encoding check",
        ))
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/w/default/scratch%20%231")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("/web/w/default/scratch%20%231/p/notes/a%20b%2525.md"),
        "expected encoded href in project response: {text}"
    );
}

#[tokio::test]
async fn smoke_page_not_found_returns_404() {
    let (_tmp, store, wiki) = setup().await;
    let _ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/w/default/scratch/p/does-not-exist.md")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_projects_returns_project_stats() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, proj, "foo.md", "Foo Page", "Hello world"))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/projects")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json[0]["workspace_name"], "default");
    assert_eq!(json[0]["project_name"], "scratch");
    assert_eq!(json[0]["page_count"], 1);
}

#[tokio::test]
async fn api_workspaces_returns_workspace_stats() {
    let (_tmp, store, wiki) = setup().await;
    let default_ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let practice_ws = store
        .writer
        .get_or_create_workspace("practice")
        .await
        .unwrap();
    let scratch = store
        .writer
        .get_or_create_project(default_ws, "scratch", None)
        .await
        .unwrap();
    let testing = store
        .writer
        .get_or_create_project(practice_ws, "unit-testing", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            default_ws,
            scratch,
            "foo.md",
            "Foo Page",
            "Hello world",
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            practice_ws,
            testing,
            "patterns.md",
            "Testing Patterns",
            "Shared testing notes",
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 2);
    assert_eq!(json[0]["workspace_name"], "default");
    assert_eq!(json[0]["project_count"], 1);
    assert_eq!(json[0]["page_count"], 1);
    assert_eq!(json[1]["workspace_name"], "practice");
}

#[tokio::test]
async fn api_projects_can_filter_by_workspace() {
    let (_tmp, store, wiki) = setup().await;
    let default_ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let practice_ws = store
        .writer
        .get_or_create_workspace("practice")
        .await
        .unwrap();
    let scratch = store
        .writer
        .get_or_create_project(default_ws, "scratch", None)
        .await
        .unwrap();
    let testing = store
        .writer
        .get_or_create_project(practice_ws, "unit-testing", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            default_ws, scratch, "foo.md", "Foo Page", "default",
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            practice_ws,
            testing,
            "patterns.md",
            "Testing Patterns",
            "practice",
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/projects?workspace=practice")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    assert_eq!(json[0]["workspace_name"], "practice");
    assert_eq!(json[0]["project_name"], "unit-testing");
}

#[tokio::test]
async fn api_pages_returns_latest_pages_only() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    wiki.write_page(wiki_req(ws, proj, "foo.md", "# First\n\nOld"))
        .await
        .unwrap();
    wiki.write_page(wiki_req(ws, proj, "foo.md", "# Second\n\nNew"))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/projects/scratch/pages")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    assert_eq!(json[0]["path"], "foo.md");
    assert_eq!(json[0]["title"], "Second");
}

#[tokio::test]
async fn api_pages_derives_kind_from_path_when_frontmatter_absent() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    // Page WITHOUT a `kind` in its frontmatter, sitting under `decisions/`.
    // The reader must derive `kind = "decision"` from the path.
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("decisions/adr-x.md").unwrap(),
            title: "ADR X".to_owned(),
            body: "A decision".to_owned(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
        })
        .await
        .unwrap();

    // Page WITH an explicit `kind = "rule"` in its frontmatter, sitting at
    // a path that would otherwise derive `fact`. The explicit kind must win.
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/anything.md").unwrap(),
            title: "Explicit Rule".to_owned(),
            body: "An explicit rule".to_owned(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({"kind": "rule"}),
            pinned: false,
            links: Vec::new(),
        })
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/projects/scratch/pages")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let pages = json.as_array().unwrap();

    let decision = pages
        .iter()
        .find(|p| p["path"] == "decisions/adr-x.md")
        .expect("decisions/adr-x.md present");
    assert_eq!(
        decision["kind"], "decision",
        "kind derived from `decisions/` path when frontmatter has none"
    );

    let rule = pages
        .iter()
        .find(|p| p["path"] == "notes/anything.md")
        .expect("notes/anything.md present");
    assert_eq!(
        rule["kind"], "rule",
        "explicit frontmatter kind wins over path derivation"
    );
}

#[tokio::test]
async fn api_page_returns_markdown_and_metadata() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    wiki.write_page(wiki_req(ws, proj, "foo.md", "# Foo\n\nHello world"))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/projects/scratch/pages/foo.md")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["workspace"], "default");
    assert_eq!(json["project"], "scratch");
    assert_eq!(json["path"], "foo.md");
    assert_eq!(json["title"], "Foo");
    assert_eq!(json["frontmatter"]["kind"], "fact");
    assert!(
        json["body_markdown"]
            .as_str()
            .unwrap()
            .contains("Hello world")
    );
}

#[tokio::test]
async fn api_search_can_scope_to_project() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let scratch = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            ws,
            scratch,
            "foo.md",
            "Scratch Page",
            "shared_unique_term",
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            ws,
            other,
            "bar.md",
            "Other Page",
            "shared_unique_term",
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=shared_unique_term&workspace=default&project=scratch&limit=1")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    assert_eq!(json[0]["project"], "scratch");
    assert_eq!(json[0]["title"], "Scratch Page");
}

#[tokio::test]
async fn api_search_can_read_from_multiple_scopes() {
    let (_tmp, store, wiki) = setup().await;
    let client_ws = store
        .writer
        .get_or_create_workspace("client-a")
        .await
        .unwrap();
    let practice_ws = store
        .writer
        .get_or_create_workspace("practice")
        .await
        .unwrap();
    let product = store
        .writer
        .get_or_create_project(client_ws, "product", None)
        .await
        .unwrap();
    let unit_testing = store
        .writer
        .get_or_create_project(practice_ws, "unit-testing", None)
        .await
        .unwrap();
    let unrelated = store
        .writer
        .get_or_create_project(client_ws, "unrelated", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            client_ws,
            product,
            "product.md",
            "Product Rules",
            "shared_scope_token belongs to the product",
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            practice_ws,
            unit_testing,
            "patterns.md",
            "Testing Patterns",
            "shared_scope_token belongs to practice",
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            client_ws,
            unrelated,
            "hidden.md",
            "Hidden Page",
            "shared_scope_token must not appear",
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=shared_scope_token&scope=client-a/product&scope=practice/unit-testing")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let hits = json.as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().any(|hit| hit["project"] == "product"));
    assert!(hits.iter().any(|hit| hit["project"] == "unit-testing"));
    assert!(!hits.iter().any(|hit| hit["project"] == "unrelated"));
}

#[tokio::test]
async fn api_search_post_accepts_multi_scope_body() {
    let (_tmp, store, wiki) = setup().await;
    let client_ws = store
        .writer
        .get_or_create_workspace("client-a")
        .await
        .unwrap();
    let practice_ws = store
        .writer
        .get_or_create_workspace("practice")
        .await
        .unwrap();
    let product = store
        .writer
        .get_or_create_project(client_ws, "product", None)
        .await
        .unwrap();
    let unit_testing = store
        .writer
        .get_or_create_project(practice_ws, "unit-testing", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            client_ws,
            product,
            "product.md",
            "Product Rules",
            "post_scope_token belongs to the product",
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            practice_ws,
            unit_testing,
            "patterns.md",
            "Testing Patterns",
            "post_scope_token belongs to practice",
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let body = serde_json::json!({
        "q": "post_scope_token",
        "limit": 10,
        "scopes": [
            {"workspace": "client-a", "project": "product"},
            {"workspace": "practice", "project": "unit-testing"}
        ]
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/search")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn api_routes_do_not_accept_writes() {
    let (_tmp, store, wiki) = setup().await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/projects")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn api_search_rejects_partial_scope() {
    let (_tmp, store, wiki) = setup().await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=anything&workspace=default")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"],
        "workspace and project must be provided together"
    );
}

#[tokio::test]
async fn api_search_rejects_malformed_scope_param() {
    let (_tmp, store, wiki) = setup().await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=anything&scope=missing-project")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "scope must use the workspace/project format");
}

#[tokio::test]
async fn api_search_rejects_ambiguous_scope_inputs() {
    let (_tmp, store, wiki) = setup().await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=anything&workspace=default&project=scratch&scope=default/scratch")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"],
        "scopes cannot be combined with workspace/project"
    );
}

#[tokio::test]
async fn api_project_routes_return_404_for_missing_project() {
    let (_tmp, store, wiki) = setup().await;
    store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/projects/missing/pages")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_recent_and_briefing_return_project_data() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, proj, "foo.md", "Foo Page", "Hello world"))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let recent_req = Request::builder()
        .uri("/workspaces/default/projects/scratch/recent?limit=1")
        .body(Body::empty())
        .unwrap();
    let recent_resp = app.clone().oneshot(recent_req).await.unwrap();
    assert_eq!(recent_resp.status(), StatusCode::OK);

    let briefing_req = Request::builder()
        .uri("/workspaces/default/projects/scratch/briefing?limit=1")
        .body(Body::empty())
        .unwrap();
    let briefing_resp = app.oneshot(briefing_req).await.unwrap();
    assert_eq!(briefing_resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(briefing_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["counts"]["pages_latest"], 1);
    assert_eq!(json["recent_pages"][0]["path"], "foo.md");
}

#[tokio::test]
async fn api_workspace_overview_returns_aggregated_overview() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, proj, "foo.md", "Foo Page", "Hello world"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, proj, "bar.md", "Bar Page", "Second page"))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/overview?limit=10")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert!(json.get("handoff").is_some(), "missing handoff key");
    assert!(json["handoff"].is_null(), "expected null handoff");
    assert!(json.get("briefing").is_some(), "missing briefing key");
    assert_eq!(json["briefing"]["counts"]["pages_latest"], 2);

    let health = &json["health"];
    assert!(health.is_object(), "missing health object");
    assert!(health.get("stale").is_some(), "missing health.stale");
    assert!(
        health.get("duplicates").is_some(),
        "missing health.duplicates"
    );
    assert!(
        health.get("contradictions").is_some(),
        "missing health.contradictions"
    );
    assert!(health.get("orphans").is_some(), "missing health.orphans");
    assert_eq!(health["contradictions"], 0);
}

#[tokio::test]
async fn api_workspace_overview_aggregates_briefing_and_health() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let alpha = store
        .writer
        .get_or_create_project(ws, "alpha", None)
        .await
        .unwrap();
    let beta = store
        .writer
        .get_or_create_project(ws, "beta", None)
        .await
        .unwrap();

    // One normal page + one _rules/ page in each project, so we prove
    // the overview endpoint aggregates across the whole workspace.
    store
        .writer
        .upsert_page(new_page(ws, alpha, "intro.md", "Alpha Intro", "alpha body"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            ws,
            alpha,
            "_rules/style.md",
            "Alpha Style Rule",
            "always do X",
        ))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, beta, "intro.md", "Beta Intro", "beta body"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(
            ws,
            beta,
            "_rules/naming.md",
            "Beta Naming Rule",
            "name things well",
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/overview?limit=10")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    // 4 pages total across both projects in the workspace.
    assert_eq!(json["briefing"]["counts"]["pages_latest"], 4);

    // rules aggregates the _rules/ pages from BOTH projects.
    let rules = json["briefing"]["rules"].as_array().expect("rules array");
    assert_eq!(rules.len(), 2, "expected both _rules pages: {rules:?}");
    let rule_paths: Vec<&str> = rules.iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert!(rule_paths.contains(&"_rules/style.md"));
    assert!(rule_paths.contains(&"_rules/naming.md"));

    // Health: no contradictions, every page is an orphan (new_page uses
    // empty links), so orphans == total page count.
    assert_eq!(json["health"]["contradictions"], 0);
    assert_eq!(json["health"]["orphans"], 4);
}

#[tokio::test]
async fn api_workspace_overview_includes_open_handoff() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "handoff_summary_marker".into(),
            open_questions: vec!["open_question_marker".into()],
            next_steps: vec!["next_step_marker".into()],
            files_touched: vec![],
        })
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/overview")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    let handoff = &json["handoff"];
    assert!(!handoff.is_null(), "expected a non-null handoff: {json}");
    assert_eq!(handoff["summary"], "handoff_summary_marker");
    assert_eq!(handoff["open_questions"][0], "open_question_marker");
    assert_eq!(handoff["next_steps"][0], "next_step_marker");
    assert_eq!(handoff["project"], "scratch");
    assert_eq!(handoff["agent"], "claude-code");
}

#[tokio::test]
async fn api_project_overview_aggregates_handoff_briefing_health() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    // Another project that must NOT bleed into the scratch overview.
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();

    store
        .writer
        .upsert_page(new_page(ws, proj, "alpha.md", "Alpha", "body"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, other, "beta.md", "Beta", "body"))
        .await
        .unwrap();

    store
        .writer
        .insert_handoff(NewHandoff {
            workspace_id: ws,
            project_id: proj,
            from_session_id: None,
            from_agent: AgentKind::ClaudeCode,
            to_agent: None,
            cwd: None,
            summary: "scratch_handoff_marker".into(),
            open_questions: vec![],
            next_steps: vec![],
            files_touched: vec![],
        })
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/projects/scratch/overview")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    // Handoff is the scratch one, scoped to the project.
    assert_eq!(json["handoff"]["summary"], "scratch_handoff_marker");
    assert_eq!(json["handoff"]["project"], "scratch");

    // Briefing + health count only the scratch page, not other/beta.md.
    assert_eq!(json["briefing"]["counts"]["pages_latest"], 1);
    let orphans = json["health"]["orphan_pages"]
        .as_array()
        .expect("orphan_pages");
    let orphan_paths: Vec<&str> = orphans.iter().filter_map(|p| p["path"].as_str()).collect();
    assert_eq!(
        orphan_paths,
        vec!["alpha.md"],
        "scoped to scratch only: {json}"
    );
}

#[tokio::test]
async fn api_workspace_overview_health_detail_lists_pages() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    // All three pages are orphans (no links). Two share a title → duplicates.
    store
        .writer
        .upsert_page(new_page(ws, proj, "alpha.md", "Alpha", "body"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, proj, "dup-a.md", "SharedTitle", "body a"))
        .await
        .unwrap();
    store
        .writer
        .upsert_page(new_page(ws, proj, "dup-b.md", "SharedTitle", "body b"))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/overview")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let health = &json["health"];

    let orphans = health["orphan_pages"]
        .as_array()
        .expect("orphan_pages array");
    let orphan_paths: Vec<&str> = orphans.iter().filter_map(|p| p["path"].as_str()).collect();
    assert!(
        orphan_paths.contains(&"alpha.md"),
        "orphan list should include the unlinked page: {health}"
    );
    assert_eq!(orphans.len(), 3, "all three pages are orphans");

    let dups = health["duplicate_pages"]
        .as_array()
        .expect("duplicate_pages array");
    let dup_paths: Vec<&str> = dups.iter().filter_map(|p| p["path"].as_str()).collect();
    assert!(dup_paths.contains(&"dup-a.md") && dup_paths.contains(&"dup-b.md"));
    assert!(dups.iter().all(|p| p["title"] == "SharedTitle"));
    assert!(
        health["stale_pages"]
            .as_array()
            .expect("stale_pages array")
            .is_empty(),
        "freshly-written pages are not stale"
    );
}

#[tokio::test]
async fn api_page_returns_resolved_links_and_backlinks() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    // Target page first so the source's link resolves on write.
    wiki.write_page(wiki_req(
        ws,
        proj,
        "decisions/target.md",
        "# Target\n\nThe canonical decision.",
    ))
    .await
    .unwrap();
    // Source links to the target via a wikilink (resolves to decisions/target.md).
    wiki.write_page(wiki_req(
        ws,
        proj,
        "notes/source.md",
        "# Source\n\nSee [[decisions/target]] for the rationale.",
    ))
    .await
    .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());

    // Source page exposes the outgoing link, no back-links.
    let src_req = Request::builder()
        .uri("/workspaces/default/projects/scratch/pages/notes/source.md")
        .body(Body::empty())
        .unwrap();
    let src_resp = app.clone().oneshot(src_req).await.unwrap();
    assert_eq!(src_resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(src_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let src: Value = serde_json::from_slice(&body).unwrap();
    let links = src["links"].as_array().expect("links array");
    assert_eq!(links.len(), 1, "source has one outgoing link: {src}");
    assert_eq!(links[0]["path"], "decisions/target.md");
    // `wiki_req` writes an explicit `kind: fact` frontmatter, which the
    // resolver surfaces verbatim on the related-page row.
    assert_eq!(links[0]["kind"], "fact");
    assert!(
        src["backlinks"]
            .as_array()
            .expect("backlinks array")
            .is_empty(),
        "source has no back-links"
    );

    // Target page exposes the incoming back-link, no outgoing links.
    let tgt_req = Request::builder()
        .uri("/workspaces/default/projects/scratch/pages/decisions/target.md")
        .body(Body::empty())
        .unwrap();
    let tgt_resp = app.oneshot(tgt_req).await.unwrap();
    assert_eq!(tgt_resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(tgt_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let tgt: Value = serde_json::from_slice(&body).unwrap();
    let backlinks = tgt["backlinks"].as_array().expect("backlinks array");
    assert_eq!(backlinks.len(), 1, "target has one back-link: {tgt}");
    assert_eq!(backlinks[0]["path"], "notes/source.md");
    assert!(
        tgt["links"].as_array().expect("links array").is_empty(),
        "target has no outgoing links"
    );
}

#[tokio::test]
async fn api_page_returns_404_for_missing_page() {
    let (_tmp, store, wiki) = setup().await;
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    // workspace/project existem, mas a página não → 404 (não 500)
    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces/default/projects/scratch/pages/does/not/exist.md")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "page not found");
}

#[tokio::test]
async fn api_search_empty_query_returns_empty_array() {
    let (_tmp, store, wiki) = setup().await;

    // q só com espaços (%20) → termo vazio após trim → 200 com [] (sem tocar o FTS)
    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=%20%20")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.as_array().expect("array").is_empty(),
        "empty query yields no hits: {json}"
    );
}

#[tokio::test]
async fn api_search_rejects_non_integer_limit() {
    let (_tmp, store, wiki) = setup().await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=anything&limit=abc")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "limit must be an integer");
}

#[tokio::test]
async fn api_search_rejects_invalid_percent_encoding() {
    let (_tmp, store, wiki) = setup().await;

    // %zz não é hex válido → o decoder manual da querystring rejeita com 400
    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/search?q=%zz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "invalid percent-encoding in query");
}
