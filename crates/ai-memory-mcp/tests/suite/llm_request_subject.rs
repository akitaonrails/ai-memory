//! The opt-in `openai-compat` request metadata (`llm_request_metadata_headers`)
//! names what an MCP-driven LLM call reads and whom it acts for. The provider
//! turns the operation's `LlmSubject` into headers; these tests pin what the
//! two MCP call sites put in it:
//!
//! - `memory_explore` reads one project, so it names that scope and the
//!   authenticated caller;
//! - `memory_query(answer=true)` synthesizes from evidence that can span
//!   projects and `_global`, so it names only the caller, never a scope.

use std::sync::{Arc, Mutex};

use ai_memory_core::{ActorContext, NewPage, PagePath, Tier};
use ai_memory_llm::{
    ChatRequest, ChatResponse, LlmOperationId, LlmProvider, LlmResult, LlmSubject, LlmWorkload,
};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "acme";
const PROJ: &str = "billing-api";

/// Records the operation id of every call so a test can read its subject.
struct OperationCapturingLlm {
    seen: Arc<Mutex<Vec<LlmOperationId>>>,
}

#[async_trait::async_trait]
impl LlmProvider for OperationCapturingLlm {
    fn name(&self) -> &'static str {
        "capture"
    }

    fn model(&self) -> &str {
        "capture-1"
    }

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            text: "digest".into(),
            usage: None,
            model: "capture-1".into(),
        })
    }

    async fn complete_with_operation_id(
        &self,
        request: ChatRequest,
        operation_id: LlmOperationId,
    ) -> LlmResult<ChatResponse> {
        self.seen.lock().unwrap().push(operation_id);
        self.complete(request).await
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: Value,
    ) -> LlmResult<Value> {
        Ok(json!({
            "answer": "The needle lives in notes/topic.md.",
            "citations": ["notes/topic.md"],
        }))
    }

    async fn complete_structured_raw_with_operation_id(
        &self,
        request: ChatRequest,
        schema: Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<Value> {
        self.seen.lock().unwrap().push(operation_id);
        self.complete_structured_raw(request, schema).await
    }
}

struct Harness {
    router: Router,
    seen: Arc<Mutex<Vec<LlmOperationId>>>,
    _tmp: TempDir,
}

/// An MCP router whose requests carry `user` as the authenticated actor, the
/// way the auth middleware stamps it.
async fn harness(user: Option<&'static str>) -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    store
        .writer
        .upsert_page(NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/topic.md").unwrap(),
            title: "topic".into(),
            body: "the widget subsystem needle lives here".into(),
            tier: Tier::Semantic,
            frontmatter_json: json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(OperationCapturingLlm { seen: seen.clone() });
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_llm(provider);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(ActorContext {
                    user: user.map(str::to_owned),
                    ..ActorContext::anonymous()
                });
                next.run(req).await
            },
        ));
    Harness {
        router,
        seen,
        _tmp: tmp,
    }
}

async fn call(router: &Router, name: &str, arguments: Value) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .expect("mcp req");
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body");
    let resp: Value = serde_json::from_slice(&bytes).expect("JSON-RPC response");
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
}

fn only_operation(h: &Harness) -> LlmOperationId {
    let seen = h.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one LLM call per tool call: {seen:?}");
    seen[0].clone()
}

#[tokio::test]
async fn explore_names_its_scope_and_the_caller() {
    let h = harness(Some("alice")).await;
    call(
        &h.router,
        "memory_explore",
        json!({ "workspace": WS, "project": PROJ }),
    )
    .await;

    let op = only_operation(&h);
    assert_eq!(op.workload(), LlmWorkload::QueryAnswer);
    assert_eq!(
        op.subject(),
        Some(&LlmSubject {
            workspace: Some(WS.into()),
            project: Some(PROJ.into()),
            user: Some("alice".into()),
        })
    );
}

#[tokio::test]
async fn query_answer_names_the_caller_but_never_a_scope() {
    let h = harness(Some("alice")).await;
    call(
        &h.router,
        "memory_query",
        json!({ "query": "needle", "workspace": WS, "project": PROJ, "answer": true }),
    )
    .await;

    let op = only_operation(&h);
    assert_eq!(op.workload(), LlmWorkload::QueryAnswer);
    assert_eq!(
        op.subject(),
        Some(&LlmSubject {
            workspace: None,
            project: None,
            user: Some("alice".into()),
        }),
        "answer evidence can span projects: the scope must not be named"
    );
}

/// An anonymous caller is not named: the subject keeps only the scope.
#[tokio::test]
async fn an_anonymous_caller_names_no_user() {
    let h = harness(None).await;
    call(
        &h.router,
        "memory_explore",
        json!({ "workspace": WS, "project": PROJ }),
    )
    .await;

    let op = only_operation(&h);
    assert_eq!(op.subject().and_then(|s| s.user.as_deref()), None);
    assert_eq!(op.subject().and_then(|s| s.project.as_deref()), Some(PROJ));
}
