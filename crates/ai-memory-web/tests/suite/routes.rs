//! Smoke integration tests for the read-only web UI.
//!
//! Spins up a `Store` + `Wiki` in a tempdir, seeds two pages, builds
//! the router, and exercises each route via `tower::ServiceExt::oneshot`.

use ai_memory_core::{AgentKind, NewHandoff, NewPage, PagePath, Tier};
use ai_memory_store::Store;
use ai_memory_web::{api_router, router};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
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
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
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
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
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
async fn page_view_keeps_a_leading_h1_that_is_not_the_title() {
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
    // A frontmatter title wins the header, so an H1 that says something
    // else is the only place the page says it.
    let mut titled = wiki_req(
        ws,
        proj,
        "decisions/auth.md",
        "# Token refresh after sleep\n\nRefresh on wake.",
    );
    titled.frontmatter = serde_json::json!({"kind": "decision", "title": "Auth decisions"});
    wiki.write_page(titled).await.unwrap();
    // The title falls back to the path stem, since only an ATX `# ` line
    // names a page, so a setext H1 is not a repeat of it either.
    wiki.write_page(wiki_req(
        ws,
        proj,
        "notes/setext.md",
        "Cache warmup\n============\n\nWarm on boot.",
    ))
    .await
    .unwrap();
    // An H1 that is the title is still dropped, or the header repeats.
    wiki.write_page(wiki_req(ws, proj, "notes/same.md", "# Same title\n\nBody."))
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let get = |uri: &'static str| {
        let app = app.clone();
        async move {
            let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        }
    };

    let text = get("/w/default/scratch/p/decisions/auth.md").await;
    assert!(text.contains("Auth decisions"), "expected the title");
    assert!(
        text.contains("<h1>Token refresh after sleep</h1>"),
        "an H1 unlike the title was dropped: {text}"
    );
    let text = get("/w/default/scratch/p/notes/setext.md").await;
    assert!(
        text.contains("<h1>Cache warmup</h1>"),
        "a setext H1 unlike the title was dropped: {text}"
    );
    let text = get("/w/default/scratch/p/notes/same.md").await;
    assert!(
        !text.contains("<h1>Same title</h1>"),
        "an H1 that repeats the title should not render twice: {text}"
    );
}

// ── /web HTML chrome for multi-user attribution ──────────────────────

#[tokio::test]
async fn web_page_view_omits_author_chrome_for_anonymous_pages() {
    // Backward-compat gate: a page written without an actor (every
    // pre-v0.8 caller built this shape, and every internal caller
    // that isn't an HTTP request still does) must render with the
    // exact "Updated · Created" metadata chip layout it had before
    // multi-user landed — no "Last edited by …" chip, no email
    // link, nothing.
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
    wiki.write_page(wiki_req(ws, proj, "notes/anon.md", "anon body"))
        .await
        .unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/w/default/scratch/p/notes/anon.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        !text.contains("Last edited by"),
        "anonymous page must NOT render the author chip — backward compat"
    );
}

#[tokio::test]
async fn web_page_view_renders_author_chip_for_attributed_pages() {
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
    let mut new_user = ai_memory_core::NewUser {
        username: "alice".into(),
        name: Some("Alice Smith".into()),
        email: Some("alice@home".into()),
    };
    new_user.validate().unwrap();
    let user_id = store
        .writer
        .create_human_user(new_user, ai_memory_core::UserRole::User, None, false)
        .await
        .unwrap();

    let mut req = wiki_req(ws, proj, "notes/by-alice.md", "alice body");
    req.author_id = Some(user_id);
    req.actor = ai_memory_core::ActorContext {
        user: Some("alice".into()),
        name: Some("Alice Smith".into()),
        email: Some("alice@home".into()),
        ..ai_memory_core::ActorContext::default()
    };
    wiki.write_page(req).await.unwrap();

    let app = router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/w/default/scratch/p/notes/by-alice.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("Last edited by"),
        "attributed page must render the author chip"
    );
    assert!(text.contains("alice"), "username must appear in the chip");
    assert!(
        text.contains("Alice Smith"),
        "display name must appear when set"
    );
    assert!(
        text.contains("mailto:alice@home"),
        "email must be a mailto: link"
    );
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
    // Links are now relative (no leading `/web/`) so they resolve against the
    // `<base href>` the server injects per the configured prefix.
    assert!(
        text.contains("\"w/default/scratch%20%231/p/notes/a%20b%2525.md\""),
        "expected encoded relative href in project response: {text}"
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
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
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
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
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

/// Seed one session in `(ws, proj)` with `(kind, title, body)` rows, ended
/// when `completed`. Returns the session id.
async fn seed_session(
    store: &Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    completed: bool,
    rows: &[(ai_memory_core::ObservationKind, &str, &str)],
) -> ai_memory_core::SessionId {
    let session_id = ai_memory_core::SessionId::new();
    store
        .writer
        .begin_session(ai_memory_core::NewSession {
            occurred_at: None,
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::OpenCode,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();
    for (kind, title, body) in rows {
        store
            .writer
            .insert_observation(ai_memory_core::Sanitized::new(
                ai_memory_core::NewObservation {
                    occurred_at: None,
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: *kind,
                    extension: None,
                    source_event: None,
                    title: (*title).to_owned(),
                    body: (*body).to_owned(),
                    importance: 5,
                },
                &ai_memory_core::Sanitizer::builtin(),
            ))
            .await
            .unwrap();
    }
    if completed {
        store.writer.end_session(session_id, None).await.unwrap();
    }
    session_id
}

async fn json_body(resp: axum::response::Response) -> Value {
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn api_sessions_lists_completed_sessions_for_the_scope() {
    use ai_memory_core::ObservationKind;
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
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let completed = seed_session(
        &store,
        ws,
        proj,
        true,
        &[(ObservationKind::UserPrompt, "done", "completed work")],
    )
    .await;
    let open = seed_session(
        &store,
        ws,
        proj,
        false,
        &[(ObservationKind::UserPrompt, "live", "still running")],
    )
    .await;
    seed_session(
        &store,
        ws,
        other,
        true,
        &[(ObservationKind::UserPrompt, "elsewhere", "other project")],
    )
    .await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    let json = json_body(resp).await;
    let sessions = json["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "open and other-project sessions are hidden"
    );
    assert_eq!(sessions[0]["session_id"], completed.to_string());
    assert_eq!(sessions[0]["observation_count"], 1);
    assert!(sessions[0]["ended_at"].is_string());
    assert!(
        sessions[0]
            .get("consolidation")
            .is_some_and(serde_json::Value::is_null)
    );

    store
        .writer
        .enqueue_session_consolidation(ws, proj, completed)
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let with_job = json_body(resp).await;
    assert_eq!(
        with_job["sessions"][0]["consolidation"],
        serde_json::json!({"state":"pending","attempts":0})
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/sessions?include_open=true&limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    let sessions = json["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "limit=1 must cap the list");
    assert_eq!(
        sessions[0]["session_id"],
        open.to_string(),
        "newest first, and include_open surfaces the open one"
    );
}

#[tokio::test]
async fn api_session_observations_pages_orders_and_caps() {
    use ai_memory_core::ObservationKind;
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
    let long_body = "x".repeat(1_000);
    let session_id = seed_session(
        &store,
        ws,
        proj,
        true,
        &[
            (ObservationKind::UserPrompt, "first", "one quokka"),
            (ObservationKind::PostToolUse, "second", long_body.as_str()),
            (ObservationKind::Stop, "third", "three"),
        ],
    )
    .await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let base = format!("/workspaces/default/projects/scratch/sessions/{session_id}/observations");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{base}?limit=2"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    let json = json_body(resp).await;
    assert_eq!(json["session"]["session_id"], session_id.to_string());
    assert!(
        json["session"]
            .get("consolidation")
            .is_some_and(serde_json::Value::is_null)
    );
    assert_eq!(json["total"], 3);
    assert_eq!(json["limit"], 2);
    assert_eq!(json["offset"], 0);
    assert_eq!(json["order"], "asc");
    assert_eq!(json["elided_other_scope"], 0);
    assert_eq!(json["body_max_chars"], 4000);
    let titles: Vec<&str> = json["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["first", "second"]);
    assert_eq!(json["observations"][1]["body"], long_body);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "{base}?order=desc&kinds=post-tool-use,stop&body_max_chars=50"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    assert_eq!(json["order"], "desc");
    assert_eq!(json["total"], 2);
    assert_eq!(json["body_max_chars"], 200, "cap clamps up to the floor");
    let titles: Vec<&str> = json["observations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["third", "second"]);
    let body = json["observations"][1]["body"].as_str().unwrap();
    assert!(body.starts_with(&"x".repeat(200)));
    assert!(
        body.ends_with("[body truncated; 800 chars omitted]"),
        "got {body}"
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("{base}?q=quokka"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = json_body(resp).await;
    assert_eq!(json["total"], 1);
    assert_eq!(json["observations"][0]["title"], "first");
}

#[tokio::test]
async fn api_session_routes_return_404_for_missing_project_and_foreign_session() {
    use ai_memory_core::ObservationKind;
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
    let other = store
        .writer
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    let foreign = seed_session(
        &store,
        ws,
        other,
        true,
        &[(ObservationKind::UserPrompt, "hidden", "in other")],
    )
    .await;

    let app = api_router(store.reader.clone(), wiki.clone());
    for uri in [
        "/workspaces/default/projects/missing/sessions".to_owned(),
        format!("/workspaces/default/projects/missing/sessions/{foreign}/observations"),
        format!("/workspaces/default/projects/scratch/sessions/{foreign}/observations"),
        format!(
            "/workspaces/default/projects/scratch/sessions/{}/observations",
            ai_memory_core::SessionId::new()
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn api_session_observations_reject_bad_params() {
    use ai_memory_core::ObservationKind;
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
    let session_id = seed_session(
        &store,
        ws,
        proj,
        true,
        &[(ObservationKind::UserPrompt, "p", "b")],
    )
    .await;

    let app = api_router(store.reader.clone(), wiki.clone());
    let base = format!("/workspaces/default/projects/scratch/sessions/{session_id}/observations");
    for (uri, needle) in [
        (
            "/workspaces/default/projects/scratch/sessions/not-a-uuid/observations".to_owned(),
            "invalid session id",
        ),
        (
            format!("{base}?kinds=user-prompt,bogus"),
            "unknown observation kind",
        ),
        (format!("{base}?order=sideways"), "unknown order"),
    ] {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
        let json = json_body(resp).await;
        let msg = json["error"].as_str().unwrap();
        assert!(msg.contains(needle), "{uri}: got {msg}");
    }
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
            owner_user: None,
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
            owner_user: None,
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

/// Replay the extensions `require_bearer` injects, so a test can build the
/// request the way a real server hands it to the handler: `actor` is the
/// identity the middleware resolved (`None` = a caller the server cannot name)
/// and `auth` is the tier (`None` = the API mounted with no auth layer).
fn api_req(
    uri: &str,
    actor: Option<&str>,
    auth: Option<ai_memory_core::AuthLevel>,
) -> Request<Body> {
    api_req_actor(
        uri,
        actor.map(|user| ai_memory_core::ActorContext {
            user: Some(user.to_owned()),
            ..ai_memory_core::ActorContext::default()
        }),
        auth,
    )
}

/// Same, for a rung whose identity is not a plain username — the proxy that
/// asserts a complete OIDC issuer/subject pair.
fn api_req_actor(
    uri: &str,
    actor: Option<ai_memory_core::ActorContext>,
    auth: Option<ai_memory_core::AuthLevel>,
) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(actor) = actor {
        builder = builder.extension(actor);
    }
    if let Some(level) = auth {
        builder = builder.extension(level);
    }
    builder.body(Body::empty()).unwrap()
}

/// Owners are stored as the contract's qualified storage keys, so the fixtures
/// stamp them through `owner_stamp` rather than hand-writing the TEXT.
fn handoff_for(
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    summary: &str,
    owner: Option<&ai_memory_core::IdentityKey>,
) -> NewHandoff {
    NewHandoff {
        workspace_id: ws,
        project_id: proj,
        from_session_id: None,
        from_agent: AgentKind::ClaudeCode,
        to_agent: None,
        cwd: Some("/tmp/scratch".into()),
        summary: summary.into(),
        open_questions: vec![format!("{summary}_question")],
        next_steps: vec![format!("{summary}_step")],
        files_touched: vec!["alpha.md".into()],
        owner_user: ai_memory_core::owner_stamp(owner, true),
    }
}

fn user_key(name: &str) -> ai_memory_core::IdentityKey {
    ai_memory_core::IdentityKey::User(name.into())
}

fn sub_key(sub: &str) -> ai_memory_core::IdentityKey {
    ai_memory_core::IdentityKey::Subject {
        issuer: "https://idp.example".into(),
        subject: sub.into(),
    }
}

/// The OIDC proxy rung, end to end through the browser's own endpoint.
///
/// An ingress that terminates OIDC and forwards the issuer/subject pair resolves
/// to `AuthLevel::User` with `actor.user = None`. Keying ownership on `.user`
/// made that caller `OwnerFilter::Unattributed`, which drops their own owned
/// rows from the listing entirely AND trips the redaction gate on the shared
/// ones — so on an OIDC-proxy deployment every operator lost every handoff body
/// in the web UI, including their own.
#[tokio::test]
async fn api_handoff_listing_serves_an_oidc_proxy_caller_their_own_rows() {
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
    // Their own row, a colleague's, and a legacy unowned one.
    for (summary, owner) in [
        ("mine_handoff_marker", Some(sub_key("oidc-subject-alice"))),
        ("theirs_handoff_marker", Some(sub_key("oidc-subject-bob"))),
        ("shared_handoff_marker", None),
    ] {
        store
            .writer
            .insert_handoff(handoff_for(ws, proj, summary, owner.as_ref()))
            .await
            .unwrap();
    }

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(api_req_actor(
            "/workspaces/default/projects/scratch/handoffs",
            Some(ai_memory_core::ActorContext {
                issuer: Some("https://idp.example".into()),
                sub: Some("oidc-subject-alice".into()),
                ..ai_memory_core::ActorContext::default()
            }),
            Some(ai_memory_core::AuthLevel::User),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(raw.to_vec()).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();
    let entries = json["handoffs"].as_array().unwrap();

    let summaries: Vec<&str> = entries
        .iter()
        .filter_map(|e| e["summary"].as_str())
        .collect();
    assert!(
        summaries.contains(&"mine_handoff_marker"),
        "the proxied operator cannot read the body of their own handoff: {text}",
    );
    assert!(
        summaries.contains(&"shared_handoff_marker"),
        "a legacy unowned handoff must stay readable by everyone: {text}",
    );
    assert!(
        entries.iter().all(|e| e["redacted"] == false),
        "a caller the server can name must not be redacted: {text}",
    );
    // …and naming them did not widen anything: the colleague's row is neither
    // listed nor leaked.
    assert!(
        !text.contains("theirs_handoff_marker"),
        "another operator's handoff reached this caller: {text}",
    );
}

#[tokio::test]
async fn api_handoff_all_owners_is_root_only_and_returns_every_owner() {
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
    for (summary, owner) in [
        ("alice_all_marker", Some(user_key("alice"))),
        ("bob_all_marker", Some(user_key("bob"))),
        ("shared_all_marker", None),
    ] {
        store
            .writer
            .insert_handoff(handoff_for(ws, proj, summary, owner.as_ref()))
            .await
            .unwrap();
    }

    let app = api_router(store.reader.clone(), wiki.clone());
    for (actor, auth) in [
        (
            Some(ai_memory_core::ActorContext {
                user: Some("alice".into()),
                ..ai_memory_core::ActorContext::default()
            }),
            Some(ai_memory_core::AuthLevel::User),
        ),
        (None, Some(ai_memory_core::AuthLevel::Anonymous)),
    ] {
        let resp = app
            .clone()
            .oneshot(api_req_actor(
                "/workspaces/default/projects/scratch/handoffs?all_owners=true",
                actor,
                auth,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let text = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("all_owners requires root authorization"));
        assert!(!text.contains("alice_all_marker"));
        assert!(!text.contains("bob_all_marker"));
    }

    let resp = app
        .oneshot(api_req_actor(
            "/workspaces/default/projects/scratch/handoffs?all_owners=true",
            None,
            Some(ai_memory_core::AuthLevel::Root),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    for marker in ["alice_all_marker", "bob_all_marker", "shared_all_marker"] {
        assert!(
            text.contains(marker),
            "root listing omitted {marker}: {text}"
        );
    }
}

/// With `[auth].root_username` set every automatic handoff carries an owner, so
/// the card must use the caller's filter — otherwise it serialises `null` while
/// `pending_handoff_count` beside it, which does apply that filter, reports 1.
#[tokio::test]
async fn api_workspace_overview_card_agrees_with_pending_count_for_owner() {
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
        .insert_handoff(handoff_for(
            ws,
            proj,
            "owned_handoff_marker",
            Some(&user_key("alice")),
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(api_req(
            "/workspaces/default/overview",
            Some("alice"),
            Some(ai_memory_core::AuthLevel::Root),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["briefing"]["pending_handoff_count"], 1);
    assert_eq!(
        json["handoff"]["summary"], "owned_handoff_marker",
        "card must show the same handoff the count advertises: {json}"
    );
}

/// The other half of the same invariant: a browser the server cannot name still
/// sees neither the owned card nor a count promising one.
#[tokio::test]
async fn api_workspace_overview_hides_owned_handoff_from_unnamed_caller() {
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
        .insert_handoff(handoff_for(
            ws,
            proj,
            "owned_handoff_marker",
            Some(&user_key("alice")),
        ))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(api_req("/workspaces/default/overview", None, None))
        .await
        .unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert!(json["handoff"].is_null(), "expected no card: {json}");
    assert_eq!(json["briefing"]["pending_handoff_count"], 0);
}

/// Default config — no `[auth]` at all — behaves exactly as it did: the whole
/// wiki is open, so the listing carries its prompt-derived body, and a legacy
/// handoff with no owner stays visible.
#[tokio::test]
async fn api_handoff_listing_serves_body_when_auth_is_off() {
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
        .insert_handoff(handoff_for(ws, proj, "shared_handoff_marker", None))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(api_req(
            "/workspaces/default/projects/scratch/handoffs",
            None,
            Some(ai_memory_core::AuthLevel::Anonymous),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();

    let entry = &json["handoffs"][0];
    assert_eq!(entry["summary"], "shared_handoff_marker");
    assert_eq!(entry["open_questions"][0], "shared_handoff_marker_question");
    assert_eq!(entry["next_steps"][0], "shared_handoff_marker_step");
    assert_eq!(entry["redacted"], false);
}

/// On a server that DOES authenticate, a caller it can neither name nor place
/// as root gets the metadata — that is what makes the listing useful — but not
/// the text an automatic handoff synthesises from the operator's prompts.
///
/// No rung of `require_bearer` actually produces this pair: the DB-user rung
/// fills `user` from the row, and the proxy downgrade only reaches
/// `AuthLevel::User` when the proxy asserted `user` **or** `sub` — both of which
/// `ActorContext::identity_key` resolves, so those callers get `OwnerFilter::
/// User(_)` and the body. What this pins is the gate's own floor: whatever
/// hands the handler a tier below root without an identity — a future rung, or
/// a mount that injects a level and no actor — gets the fail-safe answer. Do
/// not weaken the arm on the grounds that nothing reaches it.
#[tokio::test]
async fn api_handoff_listing_withholds_body_from_unnamed_nonroot_caller() {
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
        .insert_handoff(handoff_for(ws, proj, "shared_handoff_marker", None))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(api_req(
            "/workspaces/default/projects/scratch/handoffs",
            None,
            Some(ai_memory_core::AuthLevel::User),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(raw.to_vec()).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();

    let entry = &json["handoffs"][0];
    // Metadata: still there, and the row itself is still listed — a legacy
    // unowned handoff stays visible to everyone.
    assert_eq!(entry["state"], "open");
    assert_eq!(entry["agent"], "claude-code");
    assert_eq!(entry["cwd"], "/tmp/scratch");
    assert_eq!(entry["files_touched"][0], "alpha.md");
    assert_eq!(entry["redacted"], true);
    // Body: gone, and nowhere else in the payload either.
    assert!(entry.get("summary").is_none(), "summary served: {text}");
    assert!(entry.get("open_questions").is_none());
    assert!(entry.get("next_steps").is_none());
    assert!(
        !text.contains("shared_handoff_marker"),
        "prompt-derived text leaked: {text}"
    );
}

/// The commonest authenticated deployment: `[auth].bearer_token` set,
/// `[auth].root_username` left out. Handoffs carry no owner and the operator's
/// own browser authenticates as root without a name, so a gate that asked only
/// for a name redacted the operator's data from the operator — while the
/// overview card next to it served the same three fields ungated.
#[tokio::test]
async fn api_handoff_listing_serves_body_to_root_without_root_username() {
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
        .insert_handoff(handoff_for(ws, proj, "shared_handoff_marker", None))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(api_req(
            "/workspaces/default/projects/scratch/handoffs",
            None,
            Some(ai_memory_core::AuthLevel::Root),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(raw.to_vec()).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();

    let entry = &json["handoffs"][0];
    assert_eq!(
        entry["redacted"], false,
        "root redacted from itself: {text}"
    );
    assert_eq!(entry["summary"], "shared_handoff_marker");
    assert_eq!(entry["open_questions"][0], "shared_handoff_marker_question");
    assert_eq!(entry["next_steps"][0], "shared_handoff_marker_step");

    // The overview card is the same operator's other view of the same row; the
    // two endpoints must not disagree about whether it may read it.
    let overview = api_router(store.reader.clone(), wiki.clone())
        .oneshot(api_req(
            "/workspaces/default/projects/scratch/overview",
            None,
            Some(ai_memory_core::AuthLevel::Root),
        ))
        .await
        .unwrap();
    let overview: Value = serde_json::from_slice(
        &axum::body::to_bytes(overview.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(overview["handoff"]["summary"], "shared_handoff_marker");
}

/// A named caller gets their own handoffs in full — and still never sees
/// somebody else's.
#[tokio::test]
async fn api_handoff_listing_serves_body_to_named_caller() {
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
    for handoff in [
        handoff_for(ws, proj, "alice_handoff_marker", Some(&user_key("alice"))),
        handoff_for(ws, proj, "bob_handoff_marker", Some(&user_key("bob"))),
        handoff_for(ws, proj, "shared_handoff_marker", None),
    ] {
        store.writer.insert_handoff(handoff).await.unwrap();
    }

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(api_req(
            "/workspaces/default/projects/scratch/handoffs",
            Some("alice"),
            Some(ai_memory_core::AuthLevel::User),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let raw = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(raw.to_vec()).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();

    let mut summaries: Vec<&str> = json["handoffs"]
        .as_array()
        .expect("handoffs")
        .iter()
        .map(|e| {
            assert_eq!(e["redacted"], false, "body withheld from its owner: {text}");
            e["summary"].as_str().expect("summary")
        })
        .collect();
    summaries.sort_unstable();
    // Own + shared (absent owner = shared), never bob's.
    assert_eq!(
        summaries,
        vec!["alice_handoff_marker", "shared_handoff_marker"],
        "{text}"
    );
    assert!(!text.contains("bob_handoff_marker"), "{text}");
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

// ── Part A: Cache-Control + ETag tests ─────────────────────────────────────

#[tokio::test]
async fn api_responses_set_cache_control_private() {
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

    let app = api_router(store.reader.clone(), wiki.clone());
    let req = Request::builder()
        .uri("/workspaces")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cc = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .expect("Cache-Control header must be present on /workspaces")
        .to_str()
        .unwrap();
    assert!(
        cc.contains("private"),
        "Cache-Control must be private: {cc}"
    );
    assert!(
        cc.contains("max-age=30"),
        "Cache-Control must be max-age=30 for /workspaces: {cc}"
    );
}

#[tokio::test]
async fn actor_scoped_api_responses_are_never_reused_across_credentials() {
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

    let app = api_router(store.reader.clone(), wiki.clone());
    for uri in [
        "/workspaces/default/projects/scratch/briefing",
        "/workspaces/default/overview",
        "/workspaces/default/projects/scratch/overview",
        "/workspaces/default/projects/scratch/handoffs",
    ] {
        let resp = app
            .clone()
            .oneshot(api_req_actor(
                uri,
                Some(ai_memory_core::ActorContext {
                    user: Some("alice".into()),
                    ..ai_memory_core::ActorContext::default()
                }),
                Some(ai_memory_core::AuthLevel::User),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let cache = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .expect("actor-scoped responses need Cache-Control")
            .to_str()
            .unwrap();
        assert_eq!(cache, "private, no-store", "{uri}");
    }
}

#[tokio::test]
async fn api_page_handler_emits_etag_and_supports_if_none_match() {
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
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("etag-test.md").unwrap(),
        frontmatter: serde_json::json!({"kind": "fact"}),
        body: "# ETag test\n\nSome content for hashing.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());

    // First request: must return 200 with ETag and Cache-Control.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/etag-test.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let etag = resp
        .headers()
        .get(header::ETAG)
        .expect("ETag must be present on single-page read")
        .to_str()
        .unwrap()
        .to_owned();
    let cc = resp
        .headers()
        .get(header::CACHE_CONTROL)
        .expect("Cache-Control must be present on single-page read")
        .to_str()
        .unwrap();
    assert!(cc.contains("max-age=300"), "page max-age must be 300: {cc}");

    // Second request with the returned ETag: must return 304 with empty body.
    let resp304 = app
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/etag-test.md")
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp304.status(),
        StatusCode::NOT_MODIFIED,
        "matching ETag must yield 304"
    );
    let body_bytes = axum::body::to_bytes(resp304.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(body_bytes.is_empty(), "304 body must be empty");
}

/// The ETag stands for the whole JSON page, not just its markdown. Pinning a
/// page, retitling it in frontmatter, or another page linking to it changes
/// what the route returns while the body stays byte-identical; a client that
/// revalidated with the old tag was told 304 and kept the stale page.
#[tokio::test]
async fn api_page_etag_changes_when_metadata_or_backlinks_change_but_body_does_not() {
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
    let body = "# Deploy\n\nTag before pushing.";
    wiki.write_page(wiki_req(ws, proj, "deploy.md", body))
        .await
        .unwrap();
    let app = api_router(store.reader.clone(), wiki.clone());
    let uri = "/workspaces/default/projects/scratch/pages/deploy.md";
    let fetch = |etag: Option<String>| {
        let app = app.clone();
        async move {
            let mut req = Request::builder().uri(uri);
            if let Some(etag) = etag {
                req = req.header(header::IF_NONE_MATCH, etag);
            }
            app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
        }
    };
    let first = fetch(None).await;
    let etag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(
        fetch(Some(etag.clone())).await.status(),
        StatusCode::NOT_MODIFIED,
        "an unchanged page still revalidates"
    );

    // Same body, now pinned.
    let mut pinned = wiki_req(ws, proj, "deploy.md", body);
    pinned.pinned = true;
    wiki.write_page(pinned).await.unwrap();
    let resp = fetch(Some(etag.clone())).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "pinning must invalidate the ETag"
    );
    assert_eq!(json_body(resp).await["pinned"], true);
    let etag = fetch(None).await.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();

    // Same body, but another page now links here.
    wiki.write_page(wiki_req(ws, proj, "release.md", "See [[deploy]] first."))
        .await
        .unwrap();
    let resp = fetch(Some(etag)).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a new backlink must invalidate the ETag"
    );
    assert_eq!(json_body(resp).await["backlinks"][0]["path"], "release.md");
}

#[tokio::test]
async fn api_page_handler_etag_differs_per_page() {
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
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("page-a.md").unwrap(),
        frontmatter: serde_json::json!({"kind": "fact"}),
        body: "Body for page A — unique content alpha.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("page-b.md").unwrap(),
        frontmatter: serde_json::json!({"kind": "fact"}),
        body: "Body for page B — unique content beta.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());

    let etag_a = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/page-a.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    let etag_b = app
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/page-b.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();

    assert_ne!(
        etag_a, etag_b,
        "pages with different bodies must produce different ETags"
    );
}

#[tokio::test]
async fn api_error_responses_do_not_set_cache_control() {
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

    let app = api_router(store.reader.clone(), wiki.clone());
    // Request a page that does not exist — handler returns 404.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/missing.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        resp.headers().get(header::CACHE_CONTROL).is_none(),
        "error responses must not carry a Cache-Control header"
    );
}

// ── P1.7: /api/v1 page response surfaces author + ETag invalidation ──

#[tokio::test]
async fn api_v1_page_omits_author_for_anonymous_writes() {
    // Backward-compat gate: writes built with the default anonymous
    // ActorContext (every pre-multi-user caller) MUST leave the
    // serialised ApiPage shape identical to v0.7.x — `author` is
    // omitted, not serialised as `null`. Catches a regression where
    // someone forgets the `skip_serializing_if` annotation.
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
    wiki.write_page(wiki_req(ws, proj, "notes/anon.md", "anonymous body"))
        .await
        .unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/notes/anon.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json.get("author").is_none(),
        "anonymous page response must omit `author` (not null) — backward-compat regression: {json}"
    );
}

#[tokio::test]
async fn api_v1_page_surfaces_author_for_db_user_writes() {
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

    // Seed a `users` row and write a page attributed to that id.
    let mut new_user = ai_memory_core::NewUser {
        username: "alice".into(),
        name: Some("Alice Smith".into()),
        email: Some("alice@home".into()),
    };
    new_user.validate().unwrap();
    let user_id = store
        .writer
        .create_human_user(new_user, ai_memory_core::UserRole::User, None, false)
        .await
        .unwrap();

    let mut req = wiki_req(ws, proj, "notes/by-alice.md", "alice wrote this");
    req.author_id = Some(user_id);
    req.actor = ai_memory_core::ActorContext {
        user: Some("alice".into()),
        name: Some("Alice Smith".into()),
        email: Some("alice@home".into()),
        ..ai_memory_core::ActorContext::default()
    };
    wiki.write_page(req).await.unwrap();

    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/notes/by-alice.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let author = json
        .get("author")
        .expect("identified write must surface `author`");
    assert_eq!(author["username"], "alice");
    assert_eq!(author["name"], "Alice Smith");
    assert_eq!(author["email"], "alice@home");
}

#[tokio::test]
async fn api_v1_etag_differs_between_anonymous_and_attributed_writes() {
    // Regression guard: the ETag must invalidate when author changes
    // even if the body stays the same. Otherwise a client cached the
    // "anonymous" view of a page wouldn't see the attribution flip
    // after the operator runs `rotate-token` + re-writes the page.
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
    let mut new_user = ai_memory_core::NewUser {
        username: "alice".into(),
        name: None,
        email: None,
    };
    new_user.validate().unwrap();
    let user_id = store
        .writer
        .create_human_user(new_user, ai_memory_core::UserRole::User, None, false)
        .await
        .unwrap();

    // First write: anonymous.
    wiki.write_page(wiki_req(ws, proj, "notes/etag.md", "shared body"))
        .await
        .unwrap();
    let app = api_router(store.reader.clone(), wiki.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/notes/etag.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag_anon = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Second write: same body, but attributed to alice. Same wiki path
    // → supersession; the new latest version carries author_id.
    let mut req = wiki_req(ws, proj, "notes/etag.md", "shared body");
    req.author_id = Some(user_id);
    req.actor = ai_memory_core::ActorContext {
        user: Some("alice".into()),
        ..ai_memory_core::ActorContext::default()
    };
    wiki.write_page(req).await.unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/workspaces/default/projects/scratch/pages/notes/etag.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let etag_with_author = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(
        etag_anon, etag_with_author,
        "ETag must invalidate when author changes — even when body is identical \
         (would otherwise let stale caches hide attribution flips)"
    );
}

// ── 2.0.1: knowledge pages stay in front, machinery collapses ──

#[tokio::test]
async fn project_view_separates_machinery_from_knowledge() {
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
    for (path, title) in [
        ("concepts/retrieval.md", "Retrieval Concept"),
        ("_rules/deploy-policy.md", "Deploy Policy Rule"),
        ("_lint/report.md", "Lint report 2026-09-02"),
        ("sessions/abc123.md", "Session abc123"),
        ("log-2026-09.md", "log-2026-09"),
        ("index.md", "Bundle index"),
        ("_meta.md", "meta"),
    ] {
        store
            .writer
            .upsert_page(new_page(ws, proj, path, title, "body text"))
            .await
            .unwrap();
    }

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

    // Recent Activity lists knowledge only. The list is the segment
    // after its heading; machinery must not appear there.
    let recent = &text[text.find("Recent Activity").expect("recent heading")..];
    assert!(recent.contains("Retrieval Concept"));
    assert!(
        recent.contains("Deploy Policy Rule"),
        "_rules are standing human-authored knowledge, not machinery"
    );
    for machinery in [
        "Lint report 2026-09-02",
        "Session abc123",
        "log-2026-09",
        "Bundle index",
    ] {
        assert!(
            !recent.contains(machinery),
            "machinery page {machinery:?} leaked into Recent Activity"
        );
    }

    // The sidebar still reaches everything: machinery sits in the
    // collapsed System section.
    let sidebar = &text[..text.find("Recent Activity").unwrap()];
    let system = &sidebar[sidebar.find("System").expect("system section")..];
    assert!(system.contains("Lint report 2026-09-02"));
    assert!(system.contains("Session abc123"));
    assert!(system.contains("log-2026-09"));
    // Knowledge renders BEFORE the System section.
    let knowledge = &sidebar[..sidebar.find("System").unwrap()];
    assert!(knowledge.contains("Retrieval Concept"));
    assert!(knowledge.contains("Deploy Policy Rule"));
    assert!(!knowledge.contains("Session abc123"));
}

#[tokio::test]
async fn homepage_llm_notice_is_dismissible_and_backup_banner_is_gone() {
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
        .upsert_page(new_page(ws, proj, "foo.md", "Foo Page", "Hello"))
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

    // The explainer ships hidden with a persistent dismissal control;
    // client script reveals it unless previously dismissed.
    assert!(text.contains(r#"id="llm-notice""#));
    assert!(text.contains(r#"id="llm-notice-close""#));
    assert!(text.contains("ai-memory-llm-notice-dismissed"));

    // The old always-on backup banner is gone (the migration dialog
    // carries that information; `status` keeps the durable reminder).
    assert!(
        !text.contains("pre-migration backup of your memory is still on disk"),
        "redundant backup banner must not render"
    );
}

// ── #603: namespace (directory) links list pages instead of 404 ──

#[tokio::test]
async fn namespace_path_lists_its_pages() {
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
    for (path, title) in [
        ("_lint/report.md", "Lint report 2026-09-03"),
        ("concepts/retrieval.md", "Retrieval"),
    ] {
        store
            .writer
            .upsert_page(new_page(ws, proj, path, title, "body"))
            .await
            .unwrap();
    }
    let app = router(store.reader.clone(), wiki.clone());

    // A directory path (the OKF bundle-index link shape) lists its pages.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/w/default/scratch/p/_lint/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = std::str::from_utf8(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
    .to_owned();
    assert!(
        text.contains("Lint report 2026-09-03"),
        "namespace listing must show its page"
    );
    assert!(
        !text.contains("Retrieval"),
        "must not list pages from other namespaces"
    );

    // A namespace with no pages still 404s.
    let empty = app
        .oneshot(
            Request::builder()
                .uri("/w/default/scratch/p/nope/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::NOT_FOUND);
}

/// #708 on the web surface. The page routes went straight from a URL to the
/// page body without resolving a scope, and global search had no scope to
/// resolve, so the guard never saw either: bob could open alice's page in a
/// browser and find it from the search box.
#[tokio::test]
async fn web_reads_honour_grants_in_a_restricted_project() {
    use ai_memory_core::{AuthorizedViewer, NewUser, UserId, UserRole};
    use ai_memory_store::GrantLevel;

    let (_tmp, store, wiki) = setup().await;
    // Grants only decide anything in a restricted project.
    store
        .writer
        .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let client = store
        .writer
        .get_or_create_project(ws, "alice-client-work", None)
        .await
        .unwrap();
    wiki.write_page(wiki_req(
        ws,
        client,
        "secrets/rates.md",
        "# Rates\n\nDay rate is confidential.",
    ))
    .await
    .unwrap();
    let human = |name: &'static str| {
        let writer = store.writer.clone();
        async move {
            writer
                .create_human_user(
                    NewUser {
                        username: name.into(),
                        name: None,
                        email: None,
                    },
                    UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap()
        }
    };
    let alice = human("alice").await;
    let bob = human("bob").await;
    store
        .writer
        .grant_memory(alice, client, GrantLevel::Read, None)
        .await
        .unwrap();

    let api = api_router(store.reader.clone(), wiki.clone());
    let web = router(store.reader.clone(), wiki.clone());
    // `viewer` is what the auth middleware stamps for a database user; `None`
    // is root, or an install with no database users.
    let get = |app: axum::Router, uri: &'static str, viewer: Option<UserId>| async move {
        let mut req = Request::builder().uri(uri);
        if let Some(viewer) = viewer {
            req = req.extension(AuthorizedViewer(viewer));
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    };

    // Opening the page, through the API and through the HTML wiki. Bob is
    // refused with a 403 that says so — not a 404 that sends him looking for
    // a typo, and not the 500 a refusal used to fall through to.
    for (app, uri) in [
        (
            &api,
            "/workspaces/default/projects/alice-client-work/pages/secrets/rates.md",
        ),
        (&web, "/w/default/alice-client-work/p/secrets/rates.md"),
        (&web, "/w/default/alice-client-work"),
    ] {
        let (status, body) = get(app.clone(), uri, Some(bob)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert!(
            body.contains("not authorized for alice-client-work"),
            "{uri}: {body}"
        );
        assert!(
            !body.contains("confidential"),
            "{uri} leaked the body: {body}"
        );

        assert_eq!(
            get(app.clone(), uri, Some(alice)).await.0,
            StatusCode::OK,
            "{uri}"
        );
        assert_eq!(get(app.clone(), uri, None).await.0, StatusCode::OK, "{uri}");
    }

    // Finding it, through the API's global search and the wiki search box.
    for (app, uri) in [
        (&api, "/search?q=confidential"),
        (&web, "/search?q=confidential"),
    ] {
        let (status, body) = get(app.clone(), uri, Some(bob)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(
            !body.contains("secrets/rates.md"),
            "{uri} surfaced it to bob: {body}"
        );

        let (_, body) = get(app.clone(), uri, Some(alice)).await;
        assert!(
            body.contains("secrets/rates.md"),
            "{uri} hid it from alice: {body}"
        );
        let (_, body) = get(app.clone(), uri, None).await;
        assert!(
            body.contains("secrets/rates.md"),
            "{uri} with no viewer: {body}"
        );
    }
}

/// Pins a documented, by-design boundary (see `docs/users.md`): under a
/// trusted-proxy deployment, a proxied non-root end-user is authenticated as
/// [`ai_memory_core::AuthLevel::User`] with an [`ai_memory_core::ActorContext`]
/// (`auth.rs::authenticate_token`'s proxy branch), but — unlike a database
/// user — is never stamped with an [`ai_memory_core::AuthorizedViewer`],
/// because grants are keyed on `UserId` and a proxied identity has none.
/// `viewer_from_parts` reads a missing `AuthorizedViewer` as "no per-project
/// check applies" (same as root, or an install with no database users), so
/// the per-project authorization gate is a pass-through for this actor: a
/// `restricted` project is readable with no grant at all. This is NOT an
/// endorsement of a gap to close — the proxy is the authorization boundary in
/// this deployment shape — it is a pin so a future change to this behavior is
/// an intentional decision, not a silent regression.
///
/// This mirrors `web_reads_honour_grants_in_a_restricted_project`'s harness:
/// that test's own `viewer: None` case already exercises the same code path
/// (a missing `AuthorizedViewer`), but doesn't carry the extensions a real
/// trusted-proxy request would, so it doesn't document *why* that is safe
/// here. The real middleware (`authenticate_token`) is not reachable from
/// this router-only harness, so this stamps the same extensions it would
/// have stamped, by hand.
#[tokio::test]
async fn a_trusted_proxy_user_is_not_subject_to_restricted_without_a_db_identity() {
    use ai_memory_core::{ActorContext, AuthLevel};

    let (_tmp, store, wiki) = setup().await;
    store
        .writer
        .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let client = store
        .writer
        .get_or_create_project(ws, "alice-client-work", None)
        .await
        .unwrap();
    wiki.write_page(wiki_req(
        ws,
        client,
        "secrets/rates.md",
        "# Rates\n\nDay rate is confidential.",
    ))
    .await
    .unwrap();
    // A real database user, with no grant, is correctly refused — the
    // control proving the project really is restricted.
    let carol = store
        .writer
        .create_human_user(
            ai_memory_core::NewUser {
                username: "carol".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    let api = api_router(store.reader.clone(), wiki.clone());
    let web = router(store.reader.clone(), wiki.clone());
    let routes = [
        (
            api.clone(),
            "/workspaces/default/projects/alice-client-work/pages/secrets/rates.md",
        ),
        (
            web.clone(),
            "/w/default/alice-client-work/p/secrets/rates.md",
        ),
    ];

    // Control: carol as a *database* user (AuthorizedViewer stamped, no
    // grant) is refused on both surfaces.
    for (app, uri) in &routes {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(*uri)
                    .extension(ai_memory_core::AuthorizedViewer(carol))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
    }

    // The pinned case: the same project, read by a request carrying the
    // extensions a trusted-proxy non-root user actually gets (`ActorContext`
    // + `AuthLevel::User`) and nothing else — no `AuthorizedViewer`. Today
    // this is admitted.
    for (app, uri) in &routes {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(*uri)
                    .extension(ActorContext {
                        user: Some("carol-proxied".into()),
                        ..ActorContext::default()
                    })
                    .extension(AuthLevel::User)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{uri}: trusted-proxy user without a DB identity must be admitted today (pinned boundary)"
        );
    }
}

/// #708, second half on the web: bob can no longer read or search alice's
/// repositories, but every listing, the graph, the workspace overview and a
/// page's link panel still told him they existed and roughly what was in
/// them — repository names, workspace names that are organisation names, page
/// titles and paths. Each surface now shows only what the viewer may read,
/// with the shared global scope visible to all and nothing filtered with no
/// viewer.
#[tokio::test]
async fn metadata_shows_only_what_the_viewer_may_read() {
    use ai_memory_core::{AuthorizedViewer, NewUser, UserId, UserRole};
    use ai_memory_store::GrantLevel;

    let (_tmp, store, wiki) = setup().await;
    // Grants only decide anything in a restricted project.
    store
        .writer
        .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let w = &store.writer;
    let acme = w.get_or_create_workspace("acme").await.unwrap();
    let shared = w.get_or_create_workspace("shared").await.unwrap();
    let default = w.get_or_create_workspace("default").await.unwrap();
    let client = w
        .get_or_create_project(acme, "client-work", None)
        .await
        .unwrap();
    let alpha = w
        .get_or_create_project(shared, "alpha", None)
        .await
        .unwrap();
    let beta = w.get_or_create_project(shared, "beta", None).await.unwrap();
    let global = w
        .get_or_create_project(default, ai_memory_core::GLOBAL_SCOPE_PROJECT, None)
        .await
        .unwrap();
    for (ws, proj, path, body) in [
        (
            acme,
            client,
            "secrets/rates.md",
            "# Rates\n\nDay rate is confidential.",
        ),
        (
            shared,
            alpha,
            "notes/alpha-plan.md",
            "# Alpha plan\n\nAlice's side.",
        ),
        // Written after its target so the cross-repository link resolves:
        // this is the edge bob must not see from alpha's side, nor alice from
        // beta's.
        (
            shared,
            beta,
            "notes/beta-plan.md",
            "# Beta plan\n\nDepends on [[shared/alpha:notes/alpha-plan]].",
        ),
        (
            default,
            global,
            "_rules/house-style.md",
            "# House style\n\nShared by all.",
        ),
    ] {
        wiki.write_page(wiki_req(ws, proj, path, body))
            .await
            .unwrap();
    }

    let human = |name: &'static str| {
        let writer = store.writer.clone();
        async move {
            writer
                .create_human_user(
                    NewUser {
                        username: name.into(),
                        name: None,
                        email: None,
                    },
                    UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap()
        }
    };
    let alice = human("alice").await;
    let bob = human("bob").await;
    let carol = human("carol").await;
    for (user, repo) in [
        (alice, client),
        (alice, alpha),
        (bob, beta),
        (carol, alpha),
        (carol, beta),
    ] {
        w.grant_memory(user, repo, GrantLevel::Read, None)
            .await
            .unwrap();
    }

    let api = api_router(store.reader.clone(), wiki.clone());
    let web = router(store.reader.clone(), wiki.clone());
    let get = |app: axum::Router, uri: &'static str, viewer: Option<UserId>| async move {
        let mut req = Request::builder().uri(uri);
        if let Some(viewer) = viewer {
            req = req.extension(AuthorizedViewer(viewer));
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    };
    let shows = |body: &str, needle: &str| body.contains(needle);

    // Workspaces: a workspace is listed only if it holds something readable.
    // `acme` is an organisation's name; bob holds nothing in it.
    let (_, body) = get(api.clone(), "/workspaces", Some(bob)).await;
    assert!(!shows(&body, "\"acme\""), "bob sees acme: {body}");
    assert!(shows(&body, "\"shared\""), "{body}");
    assert!(
        shows(&body, "\"default\""),
        "the global scope stays visible: {body}"
    );
    let (_, body) = get(api.clone(), "/workspaces", Some(alice)).await;
    assert!(
        shows(&body, "\"acme\"") && shows(&body, "\"shared\""),
        "{body}"
    );
    let (_, body) = get(api.clone(), "/workspaces", None).await;
    assert!(shows(&body, "\"acme\""), "no viewer lists all: {body}");

    // Project listings: the API, the API narrowed to a workspace, and the
    // wiki's front page.
    for (app, uri) in [(&api, "/projects"), (&web, "/")] {
        let (status, body) = get(app.clone(), uri, Some(bob)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(shows(&body, "beta"), "{uri}: {body}");
        assert!(
            shows(&body, "_global"),
            "{uri}: global stays visible: {body}"
        );
        assert!(
            !shows(&body, "client-work"),
            "{uri} leaked client-work: {body}"
        );
        assert!(!shows(&body, "alpha"), "{uri} leaked alpha: {body}");

        let (_, body) = get(app.clone(), uri, None).await;
        assert!(
            shows(&body, "client-work") && shows(&body, "alpha") && shows(&body, "beta"),
            "{uri} with no viewer: {body}"
        );
    }
    let (_, body) = get(api.clone(), "/projects?workspace=acme", Some(bob)).await;
    assert_eq!(body.trim(), "[]", "bob sees acme's projects: {body}");

    // The graph: an edge needs BOTH ends readable. Alice reads alpha only and
    // bob reads beta only, so neither sees beta -> alpha; carol reads both.
    for viewer in [Some(alice), Some(bob)] {
        let (_, body) = get(api.clone(), "/graph", viewer).await;
        assert!(
            !shows(&body, "alpha-plan"),
            "{viewer:?} sees the edge: {body}"
        );
        assert!(
            !shows(&body, "beta-plan"),
            "{viewer:?} sees the edge: {body}"
        );
    }
    for viewer in [Some(carol), None] {
        let (_, body) = get(api.clone(), "/graph", viewer).await;
        assert!(
            shows(&body, "beta-plan") && shows(&body, "alpha-plan"),
            "{viewer:?} lost the edge: {body}"
        );
    }

    // A page's link panel is the same edge seen from one page. The panel,
    // not the body: beta's own text names the link, and that text is bob's to
    // read. What must not appear is the resolved far end — alpha's title,
    // path and repository.
    let beta_page = "/workspaces/shared/projects/beta/pages/notes/beta-plan.md";
    let links = |body: &str| {
        let page: serde_json::Value = serde_json::from_str(body).unwrap();
        page["links"].to_string()
    };
    let (status, body) = get(api.clone(), beta_page, Some(bob)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(links(&body), "[]", "bob's link panel leaked alpha: {body}");
    let (_, body) = get(api.clone(), beta_page, Some(carol)).await;
    assert!(
        links(&body).contains("alpha-plan"),
        "carol lost a link she can read: {body}"
    );

    // Workspace overview. In `shared` bob sees beta and never alpha — titles,
    // paths and the health lists alike.
    let (status, body) = get(api.clone(), "/workspaces/shared/overview", Some(bob)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(shows(&body, "beta-plan"), "{body}");
    assert!(
        !shows(&body, "alpha-plan"),
        "bob's overview leaked alpha: {body}"
    );
    let (_, body) = get(api.clone(), "/workspaces/shared/overview", None).await;
    assert!(
        shows(&body, "beta-plan") && shows(&body, "alpha-plan"),
        "no viewer: {body}"
    );
    // A workspace holding nothing he may read is refused, not answered with an
    // overview of zeros that would read as "nothing is happening here".
    let (status, body) = get(api.clone(), "/workspaces/acme/overview", Some(bob)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(shows(&body, "not authorized for acme"), "{body}");
    assert!(!shows(&body, "rates"), "{body}");
    let (status, body) = get(api.clone(), "/workspaces/acme/overview", Some(alice)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(shows(&body, "secrets/rates.md"), "{body}");
}

#[tokio::test]
async fn api_recent_incremental_paginates_over_120_pages_and_binds_cursor() {
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
        .get_or_create_project(ws, "other", None)
        .await
        .unwrap();
    for n in 0..125 {
        store
            .writer
            .upsert_page(new_page(ws, proj, &format!("p{n:03}.md"), "Page", "body"))
            .await
            .unwrap();
    }
    let app = api_router(store.reader.clone(), wiki);
    let base = "/workspaces/default/projects/scratch/recent";
    let mut url = format!("{base}?updated_since=2000-01-01T00:00:00Z&limit=37");
    let mut paths = Vec::new();
    let mut first_cursor = None;
    loop {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let pages = json["pages"].as_array().unwrap();
        assert!(pages.len() <= 37);
        paths.extend(
            pages
                .iter()
                .map(|page| page["path"].as_str().unwrap().to_owned()),
        );
        let Some(cursor) = json["next_cursor"].as_str() else {
            break;
        };
        first_cursor.get_or_insert_with(|| cursor.to_owned());
        url = format!("{base}?cursor={cursor}&limit=37");
    }
    assert_eq!(paths.len(), 125);
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 125);
    let cursor = first_cursor.unwrap();
    let resume = format!("{base}?cursor={cursor}&limit=37");
    let mut replay_bodies = Vec::new();
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&resume).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        replay_bodies.push(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        replay_bodies[0], replay_bodies[1],
        "unchanged data gives a stable continuation"
    );
    use base64::Engine as _;
    let cursor_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&cursor)
        .unwrap();
    let cursor_json: Value = serde_json::from_slice(&cursor_bytes).unwrap();
    for (key, value) in [
        ("v", serde_json::json!(2)),
        ("path", serde_json::json!("../escape.md")),
        ("updated_at", cursor_json["since"].clone()),
        ("since", serde_json::json!("invalid")),
    ] {
        let mut malformed = cursor_json.clone();
        malformed[key] = value;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&malformed).unwrap());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{base}?cursor={raw}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    for url in [
        format!("/workspaces/default/projects/other/recent?cursor={cursor}"),
        format!("{base}?cursor={cursor}&updated_since=2001-01-01T00:00:00Z"),
        format!("{base}?cursor=zz"),
        format!("{base}?cursor={}", "a".repeat(8194)),
        format!("{base}?updated_since=yesterday"),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("{base}?limit=1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, max-age=30"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn api_recent_incremental_rechecks_restricted_scope_on_cursor() {
    use ai_memory_core::{AuthorizedViewer, NewUser, UserRole};
    use ai_memory_store::{AccessMode, GrantLevel};
    let (_tmp, store, wiki) = setup().await;
    store
        .writer
        .set_new_project_mode(AccessMode::Restricted)
        .await
        .unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "private-memory", None)
        .await
        .unwrap();
    for n in 0..2 {
        store
            .writer
            .upsert_page(new_page(ws, proj, &format!("p{n}.md"), "Page", "body"))
            .await
            .unwrap();
    }
    let mut users = Vec::new();
    for name in ["alice", "bob"] {
        users.push(
            store
                .writer
                .create_human_user(
                    NewUser {
                        username: name.into(),
                        name: None,
                        email: None,
                    },
                    UserRole::User,
                    None,
                    false,
                )
                .await
                .unwrap(),
        );
    }
    store
        .writer
        .grant_memory(users[0], proj, GrantLevel::Read, None)
        .await
        .unwrap();
    let app = api_router(store.reader.clone(), wiki);
    let request = |uri: String, user| {
        Request::builder()
            .uri(uri)
            .extension(AuthorizedViewer(user))
            .body(Body::empty())
            .unwrap()
    };
    let base = "/workspaces/default/projects/private-memory/recent";
    let first = app
        .clone()
        .oneshot(request(
            format!("{base}?updated_since=2000-01-01T00:00:00Z&limit=1"),
            users[0],
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    let url = format!("{base}?cursor={}", json["next_cursor"].as_str().unwrap());
    assert_eq!(
        app.clone()
            .oneshot(request(url.clone(), users[1]))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(request(url.clone(), users[0]))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert!(
        store
            .writer
            .revoke_memory(users[0], proj, None)
            .await
            .unwrap()
    );
    assert_eq!(
        app.oneshot(request(url, users[0])).await.unwrap().status(),
        StatusCode::FORBIDDEN,
        "a cursor cannot retain access after its user's grant is revoked"
    );
}
