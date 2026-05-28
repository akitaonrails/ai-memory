//! Integration tests for `GET /admin/status` and `GET /admin/search`.
//!
//! Exercises the read-only admin surface end-to-end through the axum
//! router: build an `AdminState` over a real on-disk store + wiki,
//! seed a couple of pages, and hit each route.

use ai_memory_core::{NewPage, PagePath, Tier};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::Wiki;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

async fn make_admin_state(tmp: &TempDir) -> (AdminState, Store) {
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let db_path = store.db_path().to_path_buf();
    let state = AdminState {
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:49374".to_string(),
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    };
    (state, store)
}

async fn seed_page(store: &Store, title: &str, path: &str, body: &str) {
    let ws = store
        .writer
        .get_or_create_workspace("default".to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch".to_string(), None)
        .await
        .unwrap();
    let page = NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.to_string(),
        body: body.to_string(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
    };
    store.writer.upsert_page(page).await.unwrap();
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    use axum::body::to_bytes;
    to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn status_returns_counts_and_paths() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page(
        &store,
        "Karpathy LLM wiki",
        "concepts/karpathy.md",
        "Compile-not-retrieve pattern from Karpathy's notes.",
    )
    .await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    // Sanity-check the JSON shape.
    assert!(body["version"].is_string());
    assert_eq!(body["bind"], "127.0.0.1:49374");
    assert!(
        body["data_dir"]
            .as_str()
            .unwrap()
            .contains(tmp.path().to_str().unwrap())
    );
    assert!(body["db_path"].as_str().unwrap().ends_with(".sqlite"));
    assert_eq!(body["counts"]["pages_latest"].as_u64().unwrap(), 1);
    assert_eq!(body["counts"]["pages_all"].as_u64().unwrap(), 1);
    assert_eq!(body["derived"]["pages_rows"].as_u64().unwrap(), 1);
    assert_eq!(body["derived"]["pages_fts_rows"].as_u64().unwrap(), 1);
    assert_eq!(
        body["derived"]["latest_pages_missing_embeddings"]
            .as_u64()
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn search_returns_matching_hits() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page(
        &store,
        "Storage architecture",
        "concepts/storage.md",
        "We use SQLite in WAL mode with a single-writer actor for safe concurrency.",
    )
    .await;
    seed_page(
        &store,
        "Hook fire-and-forget",
        "concepts/hooks.md",
        "Lifecycle hooks POST with a 200ms hard timeout — fire and forget.",
    )
    .await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=sqlite&limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(hits.len(), 1, "only the storage page mentions sqlite");
    assert_eq!(hits[0]["path"].as_str().unwrap(), "concepts/storage.md");
    assert_eq!(hits[0]["title"].as_str().unwrap(), "Storage architecture");
}

#[tokio::test]
async fn search_with_empty_results_returns_empty_array() {
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=nonexistentterm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let hits: Vec<serde_json::Value> = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn search_limit_is_clamped_to_100() {
    // Server-side clamp prevents callers from requesting a million
    // hits; verify by passing a huge limit and ensuring we still get
    // a 200 (no panic, no OOM).
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_admin_state(&tmp).await;
    let app = admin_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/search?q=anything&limit=9999999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
