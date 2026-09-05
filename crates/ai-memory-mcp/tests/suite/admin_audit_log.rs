//! Integration tests for `GET /admin/audit-log`.

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
        ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        llm: None,
        auto_improve_require_approval: false,
        auto_improve_review_config: Default::default(),
        embedder: None,
        provider_health: ai_memory_llm::ProviderHealth::default(),
        decay_params: DecayParams::default(),
        data_dir: tmp.path().to_path_buf(),
        db_path,
        bind: "127.0.0.1:49374".to_string(),
        home_dir: None,
        bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        token_pepper: None,
        active_project: ai_memory_core::ActiveProject::new(),
        scope_invalidator: None,
        trusted_proxy_identity: false,
    };
    (state, store)
}

async fn seed_page(store: &Store, workspace: &str, project: &str, path: &str, body: &str) {
    let ws = store
        .writer
        .get_or_create_workspace(workspace.to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, project.to_string(), None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: path.to_string(),
            body: body.to_string(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
        })
        .await
        .unwrap();
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    use axum::body::to_bytes;
    let bytes = to_bytes(resp.into_body(), 1_000_000).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn audit_log_returns_event_shape_and_honours_scope_filter() {
    let tmp = TempDir::new().unwrap();
    let (state, store) = make_admin_state(&tmp).await;
    seed_page(&store, "default", "scratch", "notes/a.md", "in default").await;
    seed_page(&store, "acme", "infra", "notes/b.md", "in acme").await;
    let app = admin_router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/audit-log")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let events = body["events"].as_array().expect("events array");
    assert!(events.len() >= 2, "seeded two page writes");
    let sample = &events[0];
    assert!(sample["id"].is_i64() || sample["id"].is_u64());
    assert!(sample["at"].is_i64() || sample["at"].is_u64());
    assert!(sample["op"].is_string());
    assert!(sample.get("workspace").is_some());
    assert!(sample.get("project").is_some());
    assert!(sample.get("page_path").is_some());
    assert!(sample.get("author_username").is_some());
    assert_eq!(sample["detail"], "{}");

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/audit-log?workspace=default&project=scratch")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let events = body["events"].as_array().expect("events array");
    assert!(!events.is_empty());
    assert!(events.iter().all(|e| {
        e["workspace"] == "default" && e["project"] == "scratch" && e["page_path"] != "notes/b.md"
    }));
}
