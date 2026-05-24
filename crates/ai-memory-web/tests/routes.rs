//! Smoke integration tests for the read-only web UI.
//!
//! Spins up a `Store` + `Wiki` in a tempdir, seeds two pages, builds
//! the router, and exercises each route via `tower::ServiceExt::oneshot`.

use ai_memory_core::{NewPage, PagePath, Tier};
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
