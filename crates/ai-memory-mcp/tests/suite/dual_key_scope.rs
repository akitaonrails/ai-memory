//! MCP coverage for identity-aware dual-key scope resolution (#1033).

use ai_memory_core::repository_identity::{IdentitySource, IdentityStyle, RepositoryIdentity};
use ai_memory_core::{ActorContext, AuthorizedViewer, NewPage, NewUser, PagePath, Tier, UserId};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::{AccessMode, GrantLevel, Store};
use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

struct Harness {
    router: Router,
    store: Store,
    project: ai_memory_core::ProjectId,
    writer: UserId,
    wiki_root: std::path::PathBuf,
    _tmp: TempDir,
}

async fn user(store: &Store, name: &str) -> UserId {
    store
        .writer
        .create_human_user(
            NewUser {
                username: name.into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap()
}

async fn harness() -> Harness {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let baked = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();
    let writer = user(&store, "writer").await;
    let reader = user(&store, "reader").await;
    let project = store
        .writer
        .resolve_project_by_identity(
            ws,
            RepositoryIdentity {
                identity: "github.com/acme/api".into(),
                source: IdentitySource::GitRemote,
            },
            IdentityStyle::HostPath,
            "api",
            None,
            None,
            Some(writer),
        )
        .await
        .unwrap()
        .0;
    store
        .writer
        .set_access_mode(project, AccessMode::Restricted)
        .await
        .unwrap();
    store
        .writer
        .grant_memory(reader, project, GrantLevel::Read, None)
        .await
        .unwrap();
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: project,
            path: PagePath::new("notes/existing.md").unwrap(),
            title: "Existing".into(),
            body: "dual key evidence".into(),
            tier: Tier::Semantic,
            frontmatter_json: json!({}),
            pinned: false,
            links: vec![],
            author_id: Some(writer),
            expires_at: None,
            entities: vec![],
            evidence: vec![],
        })
        .await
        .unwrap();
    let wiki_root = tmp.path().join("wiki");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    wiki.backfill_scope_manifests().await.unwrap();
    wiki.commit_all("fixture baseline").unwrap();
    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked).with_wiki(wiki);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let users = Arc::new(HashMap::from([
        ("writer".to_owned(), writer),
        ("reader".to_owned(), reader),
    ]));
    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(
            move |mut request: Request<Body>, next: axum::middleware::Next| {
                let users = users.clone();
                async move {
                    if let Some(name) = request
                        .headers()
                        .get("x-test-user")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned)
                        && let Some(user) = users.get(&name).copied()
                    {
                        request.extensions_mut().insert(user);
                        request.extensions_mut().insert(AuthorizedViewer(user));
                        request.extensions_mut().insert(ActorContext {
                            user: Some(name),

                            ..ActorContext::default()
                        });
                    }
                    next.run(request).await
                }
            },
        ));
    Harness {
        router,
        store,
        project,
        writer,
        wiki_root,
        _tmp: tmp,
    }
}

async fn rejecting_webhook() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/reject",
        axum::routing::post(|| async { (axum::http::StatusCode::CONFLICT, "rejected downstream") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, server)
}

fn rejecting_chain(address: std::net::SocketAddr, events: Vec<AdmissionOp>) -> AdmissionChain {
    AdmissionChain::new(vec![WebhookConfig {
        name: "reject".into(),
        url: format!("http://{address}/reject"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events,
        blocking: true,
        projects: Vec::new(),
        headers: Vec::new(),
    }])
    .unwrap()
}

async fn call(router: &Router, user: &str, tool: &str, arguments: Value) -> Value {
    let request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("x-test-user", user)
        .body(Body::from(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments }
            })
            .to_string(),
        ))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn tool_text(response: &Value) -> String {
    response
        .pointer("/result/content")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn project_name(h: &Harness) -> String {
    let conn = rusqlite::Connection::open(h.store.db_path()).unwrap();
    conn.query_row(
        "SELECT name FROM projects WHERE id = ?1",
        [h.project.as_bytes().to_vec()],
        |row| row.get(0),
    )
    .unwrap()
}

#[tokio::test]
async fn static_canonical_and_legacy_reads_do_not_promote_and_read_only_write_is_refused() {
    let h = harness().await;
    for name in ["api", "acme-api"] {
        let response = call(
            &h.router,
            "reader",
            "memory_read_page",
            json!({"workspace":"default", "project":name, "path":"notes/existing.md"}),
        )
        .await;
        assert!(
            tool_text(&response).contains("dual key evidence"),
            "{response}"
        );
    }
    let refused = call(
        &h.router,
        "reader",
        "memory_write_page",
        json!({
            "workspace":"default", "project":"acme-api", "path":"notes/denied.md",
            "body":"must not land"
        }),
    )
    .await;
    assert!(refused.get("error").is_some(), "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("write access")),
        "{refused}"
    );
    assert_eq!(project_name(&h), "api");
}

#[tokio::test]
async fn authorized_static_write_promotes_once_and_legacy_reads_still_work() {
    let h = harness().await;
    let response = call(
        &h.router,
        "writer",
        "memory_write_page",
        json!({
            "workspace":"default", "project":"acme-api", "path":"notes/promoted.md",
            "body":"promoted write"
        }),
    )
    .await;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response.pointer("/result/isError"),
        Some(&json!(false)),
        "{response}"
    );
    let result: Value = serde_json::from_str(&tool_text(&response)).unwrap();
    assert!(
        result.get("page_id").and_then(Value::as_str).is_some(),
        "{result}"
    );
    assert_eq!(project_name(&h), "acme-api");
    let manifest = std::fs::read_to_string(
        h.wiki_root
            .join(
                h.store
                    .writer
                    .get_or_create_workspace("default")
                    .await
                    .unwrap()
                    .to_string(),
            )
            .join(h.project.to_string())
            .join("_meta.md"),
    )
    .unwrap();
    assert!(manifest.contains("project: acme-api"), "{manifest}");
    assert!(h.wiki_root.join(".git").is_dir());
    let legacy = call(
        &h.router,
        "reader",
        "memory_read_page",
        json!({"workspace":"default", "project":"api", "path":"notes/promoted.md"}),
    )
    .await;
    assert!(tool_text(&legacy).contains("promoted write"), "{legacy}");
}

#[tokio::test]
async fn promotion_manifest_failure_is_disclosed_and_repaired_without_undoing_sql() {
    let h = harness().await;
    let ws = h
        .store
        .reader
        .find_workspace("default".into())
        .await
        .unwrap()
        .unwrap();
    let manifest = h
        .wiki_root
        .join(ws.to_string())
        .join(h.project.to_string())
        .join("_meta.md");
    assert!(
        std::fs::read_to_string(&manifest)
            .unwrap()
            .contains("project: api")
    );
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();
    let response = call(&h.router, "writer", "memory_write_page", json!({
        "workspace":"default", "project":"acme-api", "path":"notes/partial.md", "body":"surviving write"
    })).await;
    assert!(response.get("error").is_none(), "{response}");
    let result: Value = serde_json::from_str(&tool_text(&response)).unwrap();
    assert!(result["page_id"].as_str().is_some(), "{result}");
    assert!(
        result["manifest_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("committed")),
        "{result}"
    );
    assert_eq!(project_name(&h), "acme-api");
    assert!(manifest.is_dir());
    std::fs::remove_dir(&manifest).unwrap();
    let wiki = Wiki::new(h._tmp.path(), h.store.writer.clone())
        .unwrap()
        .with_store_reader(h.store.reader.clone());
    assert!(
        wiki.refresh_renamed_scope(ws, h.project)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        std::fs::read_to_string(&manifest)
            .unwrap()
            .contains("project: acme-api")
    );
    assert!(
        wiki.refresh_renamed_scope(ws, h.project)
            .await
            .unwrap()
            .is_none()
    );
    let legacy = call(
        &h.router,
        "reader",
        "memory_read_page",
        json!({
            "workspace":"default", "project":"api", "path":"notes/partial.md"
        }),
    )
    .await;
    assert!(tool_text(&legacy).contains("surviving write"), "{legacy}");
}

#[tokio::test]
async fn page_write_failure_after_promotion_preserves_manifest_warning() {
    let h = harness().await;
    let ws = h
        .store
        .reader
        .find_workspace("default".into())
        .await
        .unwrap()
        .unwrap();
    let manifest = h
        .wiki_root
        .join(ws.to_string())
        .join(h.project.to_string())
        .join("_meta.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();
    let (address, webhook) = rejecting_webhook().await;
    let wiki = Wiki::new(h._tmp.path(), h.store.writer.clone())
        .unwrap()
        .with_store_reader(h.store.reader.clone())
        .with_admission_chain(rejecting_chain(address, vec![AdmissionOp::WritePage]));
    let server = AiMemoryServer::new(
        h.store.reader.clone(),
        h.store.writer.clone(),
        ws,
        h.project,
    )
    .with_wiki(wiki);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let router = Router::new().nest_service("/mcp", service);

    let response = call(
        &router,
        "",
        "memory_write_page",
        json!({
            "workspace":"default", "project":"acme-api",
            "path":"notes/rejected.md", "body":"must not land"
        }),
    )
    .await;
    assert_eq!(response["error"]["code"], json!(-32603), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("admission webhook")),
        "{response}"
    );
    assert!(
        response["error"]["data"]["manifest_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("promotion committed")),
        "{response}"
    );
    assert_eq!(project_name(&h), "acme-api");
    assert!(
        h.store
            .reader
            .page_meta("default", "acme-api", "notes/rejected.md")
            .await
            .unwrap()
            .is_none()
    );
    webhook.abort();
}

#[tokio::test]
async fn handoff_failure_after_promotion_preserves_manifest_warning() {
    let h = harness().await;
    let ws = h
        .store
        .reader
        .find_workspace("default".into())
        .await
        .unwrap()
        .unwrap();
    let manifest = h
        .wiki_root
        .join(ws.to_string())
        .join(h.project.to_string())
        .join("_meta.md");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();
    let (address, webhook) = rejecting_webhook().await;
    let wiki = Wiki::new(h._tmp.path(), h.store.writer.clone())
        .unwrap()
        .with_store_reader(h.store.reader.clone())
        .with_admission_chain(rejecting_chain(address, vec![AdmissionOp::HandoffBegin]));
    let server = AiMemoryServer::new(
        h.store.reader.clone(),
        h.store.writer.clone(),
        ws,
        h.project,
    )
    .with_wiki(wiki);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let router = Router::new().nest_service("/mcp", service);

    let response = call(
        &router,
        "",
        "memory_handoff_begin",
        json!({
            "workspace":"default", "project":"acme-api",
            "summary":"must not land"
        }),
    )
    .await;
    assert_eq!(response["error"]["code"], json!(-32600), "{response}");
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("admission webhook")),
        "{response}"
    );
    assert!(
        response["error"]["data"]["manifest_warning"]
            .as_str()
            .is_some_and(|warning| warning.contains("promotion committed")),
        "{response}"
    );
    assert_eq!(project_name(&h), "acme-api");
    assert!(
        h.store
            .reader
            .open_handoffs_for_project(ws, h.project, 10)
            .await
            .unwrap()
            .is_empty()
    );
    webhook.abort();
}

#[tokio::test]
async fn foreign_workspace_and_cross_forge_keys_fail_closed() {
    let h = harness().await;
    let foreign_ws = h
        .store
        .writer
        .get_or_create_workspace("foreign")
        .await
        .unwrap();
    let foreign = h
        .store
        .writer
        .get_or_create_project(foreign_ws, "acme-api", None)
        .await
        .unwrap();
    h.store
        .writer
        .set_access_mode(foreign, AccessMode::Restricted)
        .await
        .unwrap();
    let foreign_response = call(
        &h.router,
        "reader",
        "memory_read_page",
        json!({"workspace":"foreign", "project":"acme-api", "path":"notes/existing.md"}),
    )
    .await;
    assert!(
        foreign_response.get("error").is_some(),
        "{foreign_response}"
    );

    let ws = h
        .store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    h.store
        .writer
        .resolve_project_by_identity(
            ws,
            RepositoryIdentity {
                identity: "gitlab.com/acme/api".into(),
                source: IdentitySource::GitRemote,
            },
            IdentityStyle::HostPath,
            "gitlab-api",
            None,
            None,
            Some(h.writer),
        )
        .await
        .unwrap();
    let ambiguous = call(
        &h.router,
        "reader",
        "memory_read_page",
        json!({"workspace":"default", "project":"acme-api", "path":"notes/existing.md"}),
    )
    .await;
    assert!(ambiguous.get("error").is_some(), "{ambiguous}");
}
