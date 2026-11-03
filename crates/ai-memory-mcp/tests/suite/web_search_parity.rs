//! The web search ranks a project the way `memory_query` does.
//!
//! `memory_query` and the web search (`/web/search` with a project,
//! `/api/v1/search` scoped) both call `ai_memory_llm::embed_search_query`,
//! `ReaderPool::hybrid_search_scopes` and the shared `RerankStage`. These
//! tests seed one store, drive `memory_query` over the real JSON-RPC
//! transport and the web routers as `serve` mounts them, and require the same
//! top-k order from each, with a deterministic embedder and with none.
//!
//! The controls prove the comparison can fail: a web search built without the
//! embedder, or the old FTS-only scoped search, ranks differently from a
//! `memory_query` that has one.

use std::sync::Arc;

use ai_memory_core::{ActorContext, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{Embedder, SyntheticEmbedder};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_web::{Recall, WebMountSpec, split_web_routers};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const LIMIT: usize = 8;

/// Small enough that unrelated words share buckets, so the vector stream
/// reaches pages FTS does not and moves the fused order.
fn embedder() -> Arc<dyn Embedder> {
    Arc::new(SyntheticEmbedder::new(8))
}

struct Seeded {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    alpha: ProjectId,
}

const ALPHA_PAGES: &[(&str, &str)] = &[
    (
        "notes/token-rotation.md",
        "# Token rotation\n\nRotate the signing token every week. See [[notes/key-store]].",
    ),
    (
        "notes/key-store.md",
        "# Key store\n\nKeys live in the vault; rotation writes a new version.",
    ),
    (
        "notes/db-migration.md",
        "# Database migration\n\nMigrations run before deploy; rollback drops the new table.",
    ),
    (
        "notes/rollback.md",
        "# Rollback plan\n\nA failed deploy rolls back the release and the database.",
    ),
    (
        "notes/cache.md",
        "# Cache invalidation\n\nThe cache is keyed by page id and invalidated on write.",
    ),
    (
        "notes/deploy.md",
        "# Deploy pipeline\n\nThe pipeline builds, migrates the database, then swaps traffic. See [[notes/rollback]].",
    ),
    (
        "notes/oncall.md",
        "# On-call\n\nPage the owner when the pipeline or the vault is down.",
    ),
    (
        "notes/weekly.md",
        "# Weekly chores\n\nEvery week: rotate logs, prune the cache, review alerts.",
    ),
    (
        "decisions/vault.md",
        "# Use the vault for secrets\n\nDecision: signing keys and tokens stay in the vault.",
    ),
    (
        "gotchas/migration-lock.md",
        "# Migration lock\n\nTwo deploys at once deadlock on the migration lock table.",
    ),
];

const BETA_PAGES: &[(&str, &str)] = &[
    (
        "notes/beta-token.md",
        "# Beta token rotation\n\nBeta rotates its token and database keys every week.",
    ),
    (
        "notes/beta-deploy.md",
        "# Beta deploy\n\nBeta deploy pipeline with cache rollback and migration.",
    ),
];

fn write_req(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> WritePageRequest {
    WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        frontmatter: json!({"kind": "fact"}),
        body: body.to_owned(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::anonymous(),
        evidence: Vec::new(),
    }
}

/// Pages written through a `Wiki` that embeds on write, so every page has a
/// vector under the synthetic triple.
async fn seed() -> Seeded {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(embedder());
    for (path, body) in ALPHA_PAGES {
        wiki.write_page(write_req(ws, alpha, path, body))
            .await
            .unwrap();
    }
    for (path, body) in BETA_PAGES {
        wiki.write_page(write_req(ws, beta, path, body))
            .await
            .unwrap();
    }
    let status = store.reader.derived_index_status().await.unwrap();
    assert_eq!(
        status.latest_pages_missing_embeddings, 0,
        "every seeded page must carry a vector"
    );
    Seeded {
        _tmp: tmp,
        store,
        wiki: wiki.without_embedder(),
        ws,
        alpha,
    }
}

fn mcp_router(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

fn server(s: &Seeded, embedder: Option<Arc<dyn Embedder>>) -> Router {
    let mut server = AiMemoryServer::new(
        s.store.reader.clone(),
        s.store.writer.clone(),
        s.ws,
        s.alpha,
    )
    .with_wiki(s.wiki.clone());
    if let Some(embedder) = embedder {
        server = server.with_embedder(embedder);
    }
    mcp_router(server)
}

/// `/web` and `/api/v1`, mounted the way `serve` mounts them.
fn web(s: &Seeded, embedder: Option<Arc<dyn Embedder>>) -> Router {
    split_web_routers(
        true,
        s.store.reader.clone(),
        s.wiki.clone(),
        WebMountSpec {
            enable_api: true,
            web_ui_dir: None,
            cors_origins: &[],
            web_slug: "/web",
            base_href: "/web/",
            base_path: "",
            trusted_proxy_identity: false,
            recall: Recall {
                embedder,
                rerank: None,
            },
            runtime: None,
            health: None,
        },
    )
    .unwrap()
    .protected
}

async fn memory_query(router: &Router, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "memory_query", "arguments": arguments },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(v.get("error").is_none(), "JSON-RPC error: {v}");
    let text = v
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no tool text: {v}"));
    serde_json::from_str(text).unwrap()
}

fn paths(hits: &Value) -> Vec<String> {
    hits.as_array()
        .unwrap_or_else(|| panic!("not an array: {hits}"))
        .iter()
        .map(|h| h["path"].as_str().unwrap().to_owned())
        .collect()
}

async fn mcp_paths(router: &Router, query: &str, explain: bool) -> Vec<String> {
    let response = memory_query(
        router,
        json!({
            "query": query,
            "workspace": "default",
            "project": "alpha",
            "limit": LIMIT,
            "explain": explain,
        }),
    )
    .await;
    paths(&response["hits"])
}

async fn get(router: &Router, uri: &str) -> (u16, String) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn encode(query: &str) -> String {
    query.replace(' ', "%20")
}

async fn api_paths(router: &Router, query: &str) -> Vec<String> {
    let (status, body) = get(
        router,
        &format!(
            "/api/v1/search?q={}&workspace=default&project=alpha&limit={LIMIT}",
            encode(query)
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    paths(&serde_json::from_str(&body).unwrap())
}

/// Hit paths in page order, read from the `data-hit-path` attribute.
fn html_paths(body: &str) -> Vec<String> {
    body.split("data-hit-path=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().to_owned())
        .collect()
}

async fn web_paths(router: &Router, query: &str, explain: bool) -> Vec<String> {
    let (status, body) = get(
        router,
        &format!(
            "/web/search?q={}&workspace=default&project=alpha&limit={LIMIT}{}",
            encode(query),
            if explain { "&explain=1" } else { "" }
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    html_paths(&body)
}

const QUERIES: &[&str] = &[
    "token rotation",
    "database migration rollback",
    "cache invalidation",
    "deploy pipeline vault",
    "weekly",
];

#[tokio::test]
async fn web_search_returns_memory_query_order_with_an_embedder() {
    let s = seed().await;
    let mcp = server(&s, Some(embedder()));
    let web = web(&s, Some(embedder()));
    for query in QUERIES {
        let expected = mcp_paths(&mcp, query, false).await;
        assert!(!expected.is_empty(), "{query}: memory_query found nothing");
        assert!(
            expected.iter().all(|p| !p.contains("beta")),
            "{query}: another project leaked into memory_query: {expected:?}"
        );
        assert_eq!(api_paths(&web, query).await, expected, "{query}: /api/v1");
        assert_eq!(
            web_paths(&web, query, false).await,
            expected,
            "{query}: /web"
        );
        // Explain changes what is reported, never the order.
        assert_eq!(
            mcp_paths(&mcp, query, true).await,
            expected,
            "{query}: explain"
        );
        assert_eq!(
            web_paths(&web, query, true).await,
            expected,
            "{query}: /web explain"
        );
    }
}

#[tokio::test]
async fn web_search_returns_memory_query_order_without_an_embedder() {
    let s = seed().await;
    let mcp = server(&s, None);
    let web = web(&s, None);
    for query in QUERIES {
        let expected = mcp_paths(&mcp, query, false).await;
        assert_eq!(api_paths(&web, query).await, expected, "{query}: /api/v1");
        assert_eq!(
            web_paths(&web, query, true).await,
            expected,
            "{query}: /web"
        );
    }
}

#[tokio::test]
async fn scoped_api_search_merges_scopes_like_memory_query() {
    let s = seed().await;
    let mcp = server(&s, Some(embedder()));
    let web = web(&s, Some(embedder()));
    for query in QUERIES {
        let response = memory_query(
            &mcp,
            json!({
                "query": query,
                "scopes": [
                    {"workspace": "default", "project": "alpha"},
                    {"workspace": "default", "project": "beta"}
                ],
                "limit": LIMIT,
            }),
        )
        .await;
        let expected = paths(&response["hits"]);
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/search")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "q": query,
                    "scopes": [
                        {"workspace": "default", "project": "alpha"},
                        {"workspace": "default", "project": "beta"}
                    ],
                    "limit": LIMIT,
                })
                .to_string(),
            ))
            .unwrap();
        let resp = web.clone().oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
            .await
            .unwrap();
        let got = paths(&serde_json::from_slice(&bytes).unwrap());
        assert_eq!(got, expected, "{query}: scopes");
    }
}

/// The parity assertions above can fail: without the embedder (the web
/// search before it was injected) or with the old FTS-only scoped search,
/// at least one query ranks differently from a `memory_query` that embeds.
#[tokio::test]
async fn parity_controls_detect_a_web_search_that_skips_the_ranker() {
    let s = seed().await;
    let mcp = server(&s, Some(embedder()));
    let web_without_embedder = web(&s, None);
    let mut differs_without_embedder = false;
    let mut differs_from_fts_only = false;
    for query in QUERIES {
        let expected = mcp_paths(&mcp, query, false).await;
        differs_without_embedder |= api_paths(&web_without_embedder, query).await != expected;
        let fts_only: Vec<String> = s
            .store
            .reader
            .search_pages_for_project(s.ws, s.alpha, (*query).to_owned(), LIMIT, None)
            .await
            .unwrap()
            .into_iter()
            .map(|hit| hit.path.as_str().to_owned())
            .collect();
        differs_from_fts_only |= fts_only != expected;
    }
    assert!(
        differs_without_embedder,
        "the vector stream never moved the order; the embedder parity test proves nothing"
    );
    assert!(
        differs_from_fts_only,
        "FTS-only search matched the hybrid ranker everywhere; the parity test proves nothing"
    );
}
