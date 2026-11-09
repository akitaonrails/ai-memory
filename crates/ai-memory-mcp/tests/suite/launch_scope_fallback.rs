//! The session-aware bridge's launch-directory fallback (#2005).
//!
//! Claude Code's `/resume` keeps the bridge's launch-time session id while the
//! hooks publish the active project under the resumed one, so the bridge's
//! keyed pointer lookup misses. The bridge sends its launch directory's scope
//! in two headers, and the server fills it into an unscoped call only on that
//! miss. These tests drive the real hook ingress and MCP HTTP service under
//! `per_actor` isolation and check that the fallback is strictly a fallback:
//! a pointer the hooks published wins, the filled scope is authorized like an
//! explicit one, and it never moves the pointer.

use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::{
    ActiveProject, ActiveProjectMode, ActorContext, AuthorizedViewer, NewPage, NewUser, PagePath,
    ProjectId, Tier, UserId, WorkspaceId,
};
use ai_memory_hooks::{HookState, ProjectCacheStore, SubagentSessionSet, hook_router};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_mcp::launch_scope::{
    FALLBACK_PROJECT_HEADER, FALLBACK_WORKSPACE_HEADER, encode_header_value,
};
use ai_memory_store::{AccessMode, Store};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

struct Harness {
    router: Router,
    store: Store,
    workspace: WorkspaceId,
    api: ProjectId,
    web: ProjectId,
    ingest: Arc<tokio::sync::Semaphore>,
    _tmp: TempDir,
}

async fn page(store: &Store, ws: WorkspaceId, project: ProjectId, body: &str) {
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: project,
            path: PagePath::new("notes/marker.md").unwrap(),
            title: "Marker".into(),
            body: body.into(),
            tier: Tier::Semantic,
            frontmatter_json: json!({}),
            pinned: false,
            links: vec![],
            author_id: None,
            expires_at: None,
            entities: vec![],
            evidence: vec![],
        })
        .await
        .unwrap();
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
    let api = store
        .writer
        .get_or_create_project(ws, "acme-api", None)
        .await
        .unwrap();
    let web = store
        .writer
        .get_or_create_project(ws, "acme-web", None)
        .await
        .unwrap();
    let secret = store
        .writer
        .get_or_create_project(ws, "secret", None)
        .await
        .unwrap();
    store
        .writer
        .set_access_mode(secret, AccessMode::Restricted)
        .await
        .unwrap();
    page(&store, ws, api, "apievidence").await;
    page(&store, ws, web, "webevidence").await;
    page(&store, ws, secret, "secretevidence").await;
    let alice: UserId = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();

    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let active_project = ActiveProject::with_config(
        ActiveProjectMode::PerActor,
        Duration::from_secs(60),
        ai_memory_core::DEFAULT_MAX_ENTRIES,
    );
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked)
        .with_wiki(wiki.clone())
        .with_active_project(active_project.clone());
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let ingest = Arc::new(tokio::sync::Semaphore::new(1));
    let hooks = hook_router(HookState {
        ingest_metrics: Arc::new(ai_memory_core::IngestMetrics::default()),
        workspace_id: ws,
        project_id: baked,
        writer: store.writer.clone(),
        reader: store.reader.clone(),
        wiki,
        consolidator: None,
        sanitizer: ai_memory_core::Sanitizer::default(),
        project_cache: Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default())),
        active_project,
        ingest_semaphore: ingest.clone(),
        ingest_gates: ai_memory_hooks::IngestGates::default(),
        session_agents: ai_memory_hooks::SessionAgents::default(),
        project_briefs: ai_memory_hooks::ProjectBriefs::default(),
        cwd_probes: ai_memory_hooks::CwdProbes::default(),
        consolidate_on_session_end: false,
        session_consolidation_notify: None,
        profile_notify: None,
        capture_assistant_enabled: false,
        assistant_excerpt_max_bytes: ai_memory_hooks::ASSISTANT_EXCERPT_MAX_BYTES,
        claim_handoff_on_session_start: true,
        create_handoff_on_session_end: true,
        per_user_slots: false,
        mid_session_routing: ai_memory_core::MidSessionRouting::default(),
        profile: ai_memory_core::profile::ProfileSettings::default(),
        ingest_drop_list: ai_memory_hooks::IngestDropList::default(),
        record_tool_names: false,
        skip_wiki_checkpoints: false,
        subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
        ingest_rate: Arc::new(tokio::sync::Mutex::new(
            ai_memory_hooks::IngestRateLimiter::disabled(),
        )),
        home_dir: None,
        trusted_proxy_identity: false,
        index_file_paths: false,
    });
    // Every request is alice, a database user under per-repository
    // authorization, so the restricted project refuses her.
    let router =
        Router::new()
            .nest_service("/mcp", mcp)
            .merge(hooks)
            .layer(axum::middleware::from_fn(
                move |mut request: Request<Body>, next: axum::middleware::Next| async move {
                    request.extensions_mut().insert(alice);
                    request.extensions_mut().insert(AuthorizedViewer(alice));
                    request.extensions_mut().insert(ActorContext {
                        user: Some("alice".into()),
                        ..ActorContext::default()
                    });
                    next.run(request).await
                },
            ));
    Harness {
        router,
        store,
        workspace: ws,
        api,
        web,
        ingest,
        _tmp: tmp,
    }
}

impl Harness {
    /// A Claude Code session start in `project`, as the hooks publish it.
    async fn session_start(&self, session: &str, project: &str) {
        let body = json!({"event": "session-start", "session_id": session,
            "cwd": format!("/tmp/aim-2005/{project}")});
        let request = Request::builder()
            .method("POST")
            .uri(format!(
                "/hook?event=session-start&agent=claude-code&project={project}"
            ))
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.ingest.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    /// One `tools/call` from a session-aware bridge with `session`, carrying
    /// the launch scope `fallback` when given. Returns the tool's text and
    /// whether it failed.
    async fn call(
        &self,
        session: &str,
        fallback: Option<(&str, &str)>,
        name: &str,
        arguments: Value,
    ) -> (String, bool) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("x-memory-actor-session-id", session);
        if let Some((workspace, project)) = fallback {
            request = request
                .header(FALLBACK_WORKSPACE_HEADER, encode_header_value(workspace))
                .header(FALLBACK_PROJECT_HEADER, encode_header_value(project));
        }
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}});
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
            .await
            .unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        if let Some(error) = envelope.get("error") {
            return (error.to_string(), true);
        }
        let text = envelope
            .pointer("/result/content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let failed = envelope.pointer("/result/isError") == Some(&json!(true));
        (text, failed)
    }

    async fn query(&self, session: &str, fallback: Option<(&str, &str)>, term: &str) -> String {
        let (text, failed) = self
            .call(session, fallback, "memory_query", json!({"query": term}))
            .await;
        assert!(!failed, "{text}");
        text
    }

    async fn page_in(&self, project: ProjectId, path: &str) -> bool {
        self.store
            .reader
            .latest_page_id_by_ids(self.workspace, project, path.to_owned())
            .await
            .unwrap()
            .is_some()
    }
}

/// The reported case: a session whose id has no published pointer (the
/// bridge's launch-time id after `/resume`) reads and writes its launch
/// directory's project instead of the server default, and a session whose
/// pointer the hooks published keeps it even when the hint names another
/// project.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pointer_miss_falls_back_to_the_launch_scope_and_a_live_pointer_wins() {
    let h = harness().await;
    // The resumed session publishes; the bridge still sends the old id.
    h.session_start("resumed-session", "acme-api").await;

    // Control: without the hint the old id misses and lands in scratch,
    // and an unscoped write is refused as a mismatch.
    assert!(
        !h.query("launch-session", None, "apievidence")
            .await
            .contains("notes/marker.md")
    );
    let (refused, failed) = h
        .call(
            "launch-session",
            None,
            "memory_write_page",
            json!({"path": "notes/fallback.md", "body": "x"}),
        )
        .await;
    assert!(failed, "{refused}");
    assert!(!h.page_in(h.api, "notes/fallback.md").await);

    // With the hint the miss resolves to the launch directory's project.
    assert!(
        h.query(
            "launch-session",
            Some(("default", "acme-api")),
            "apievidence"
        )
        .await
        .contains("notes/marker.md")
    );
    let (written, failed) = h
        .call(
            "launch-session",
            Some(("default", "acme-api")),
            "memory_write_page",
            json!({"path": "notes/fallback.md", "body": "x"}),
        )
        .await;
    assert!(!failed, "{written}");
    assert!(h.page_in(h.api, "notes/fallback.md").await);

    // A published pointer wins over a hint naming another project: reads
    // and writes stay in acme-api.
    let text = h
        .query(
            "resumed-session",
            Some(("default", "acme-web")),
            "webevidence",
        )
        .await;
    assert!(!text.contains("notes/marker.md"), "{text}");
    let (_, failed) = h
        .call(
            "resumed-session",
            Some(("default", "acme-web")),
            "memory_write_page",
            json!({"path": "notes/pointer.md", "body": "x"}),
        )
        .await;
    assert!(!failed);
    assert!(h.page_in(h.api, "notes/pointer.md").await);
    assert!(!h.page_in(h.web, "notes/pointer.md").await);

    // A scope the call names itself is kept as it is.
    assert!(
        h.query_named("launch-session", "acme-web", "webevidence")
            .await
            .contains("notes/marker.md")
    );

    // The hint never moved the pointer: the old id still misses without it.
    assert!(
        !h.query("launch-session", None, "apievidence")
            .await
            .contains("notes/marker.md")
    );
}

impl Harness {
    async fn query_named(&self, session: &str, project: &str, term: &str) -> String {
        let (text, failed) = self
            .call(
                session,
                Some(("default", "acme-api")),
                "memory_query",
                json!({"query": term, "workspace": "default", "project": project}),
            )
            .await;
        assert!(!failed, "{text}");
        text
    }
}

/// The hint is authorized like an explicit scope, because it becomes one: a
/// restricted project the caller has no grant on is refused rather than
/// read, and a hint naming a project that does not exist creates nothing on
/// a read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_launch_scope_never_reaches_a_project_the_caller_cannot_read() {
    let h = harness().await;
    h.session_start("other-session", "acme-api").await;

    let (text, failed) = h
        .call(
            "launch-session",
            Some(("default", "secret")),
            "memory_query",
            json!({"query": "secretevidence"}),
        )
        .await;
    assert!(failed, "the restricted project must refuse: {text}");
    assert!(!text.contains("notes/marker.md"), "{text}");

    let (text, failed) = h
        .call(
            "launch-session",
            Some(("default", "ghost")),
            "memory_query",
            json!({"query": "anything"}),
        )
        .await;
    assert!(failed, "{text}");
    assert!(
        h.store
            .reader
            .find_project(h.workspace, "ghost".into())
            .await
            .unwrap()
            .is_none()
    );
}
