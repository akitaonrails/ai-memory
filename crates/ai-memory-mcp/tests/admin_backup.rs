//! Integration tests for `POST /admin/backup`.
//!
//! Builds a real [`AdminState`] over a tmpdir-backed store + wiki,
//! seeds a page, POSTs to `/admin/backup`, and asserts:
//! - the response Content-Type is `application/gzip`,
//! - the body is a valid gzip/tar stream,
//! - the seeded wiki file appears in the tarball at the per-project path,
//! - the seeded file's content inside the tarball matches the original body.

use ai_memory_core::{PagePath, Tier};
use ai_memory_mcp::{AdminState, admin_router};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use flate2::read::GzDecoder;
use std::io::Read as _;
use tar::Archive;
use tempfile::TempDir;
use tower::ServiceExt;

async fn make_state(tmp: &TempDir) -> (AdminState, Store) {
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
        bind: "127.0.0.1:0".to_string(),
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    };
    (state, store)
}

/// Seed a page through [`Wiki::write_page`] so it lands in the correct
/// per-project directory (`<wiki>/<ws>/<proj>/<path>`), proving the
/// backup path picks up the production write layout.
async fn seed_page(state: &AdminState, store: &Store, path: &str, body: &str) {
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
    state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({ "title": "Test" }),
            body: body.to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: Some("Test".into()),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn backup_returns_application_gzip_content_type() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    seed_page(
        &state,
        &store,
        "concepts/karpathy.md",
        "compile not retrieve",
    )
    .await;

    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/admin/backup")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/gzip"),
        "expected application/gzip content-type, got: {ct}"
    );
}

#[tokio::test]
async fn backup_body_is_valid_gzip_and_contains_seeded_page() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_state(&tmp).await;
    let known_body = "compile-not-retrieve from Karpathy LLM wiki";
    seed_page(&state, &store, "concepts/karpathy.md", known_body).await;

    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/admin/backup")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!bytes.is_empty(), "backup body must not be empty");

    // Decompress, list entries, and check the seeded file's content.
    let decoder = GzDecoder::new(bytes.as_ref());
    let mut archive = Archive::new(decoder);
    let mut found_page = false;
    let mut found_db = false;
    let mut page_content = String::new();
    for entry in archive.entries().expect("tarball must be readable") {
        let mut entry = entry.expect("entry must be readable");
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        if path.contains("concepts/karpathy.md") {
            entry
                .read_to_string(&mut page_content)
                .expect("page entry must be readable");
            found_page = true;
        }
        if path.contains("memory.sqlite") {
            found_db = true;
        }
    }
    assert!(
        found_page,
        "tarball must contain the seeded page; no 'concepts/karpathy.md' entry found"
    );
    assert!(found_db, "tarball must contain the db snapshot");
    // Verify the page body matches what was written — confirms the backup
    // captured the per-project write-path content, not stale or wrong data.
    assert!(
        page_content.contains(known_body),
        "page content in tarball must contain the seeded body; got: {page_content:?}"
    );
}

#[tokio::test]
async fn backup_empty_store_still_succeeds() {
    // Even with no wiki pages, the backup must not return an error.
    let tmp = TempDir::new().unwrap();
    let (state, _store) = make_state(&tmp).await;

    let router = admin_router(state);
    let req = Request::builder()
        .method("POST")
        .uri("/admin/backup")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!bytes.is_empty());
}
