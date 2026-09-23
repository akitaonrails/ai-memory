//! 2.4 opt-in "dialectic answer" on `memory_query` (borrowed from Honcho's
//! dialectic endpoint): when the caller sets `answer=true` AND an LLM provider
//! is configured, the server synthesizes a natural-language, cited answer over
//! the top retrieved hits and attaches it as `answer: { text, citations }`.
//!
//! The invariants this proves through the real MCP tool over the JSON-RPC HTTP
//! transport:
//!
//! 1. `answer=true` + provider → the response carries the synthesized answer
//!    text + citations, and the provider WAS called.
//! 2. `answer=false` (the default) + provider wired → the provider is NEVER
//!    called (ZERO LLM calls) and the response has no `answer` field. This is
//!    the invariant-#13 guard: the zero-LLM default path is byte-identical.
//! 3. `answer=true` + NO provider → a graceful `answer_unavailable` note, the
//!    normal hits still present, and NO error / panic.
//! 4. (`#[ignore]`) a live smoke test that wires a real provider when a key is
//!    in the environment, and skips cleanly when it is not.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider, LlmResult};
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

const WS: &str = "default";
const PROJ: &str = "scratch";

/// A fake `LlmProvider` that returns a canned structured answer and records how
/// many times ANY completion method was invoked, so a test can assert both the
/// synthesized shape AND that the default path never touches the LLM.
struct FakeAnswerLlm {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmProvider for FakeAnswerLlm {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn model(&self) -> &str {
        "fake-answer-1"
    }

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            text: "unused".into(),
            usage: None,
            model: "fake-answer-1".into(),
        })
    }

    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Shape must deserialize into the server's answer-synthesis struct.
        Ok(json!({
            "answer": "The widget subsystem stores its needle in notes/topic.md.",
            "citations": ["notes/topic.md"],
        }))
    }
}

struct Harness {
    router: Router,
    store: Store,
    ws: WorkspaceId,
    proj: ProjectId,
    /// Call counter shared with the wired fake provider; `None` when no provider
    /// was attached.
    calls: Option<Arc<AtomicUsize>>,
    _tmp: TempDir,
}

fn mount(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

/// Build a harness; `with_provider` decides whether a fake LLM is attached.
async fn harness(with_provider: bool) -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let mut server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj).with_wiki(wiki);
    let calls = if with_provider {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(FakeAnswerLlm {
            calls: calls.clone(),
        });
        server = server.with_llm(provider);
        Some(calls)
    } else {
        None
    };

    Harness {
        router: mount(server),
        store,
        ws,
        proj,
        calls,
        _tmp: tmp,
    }
}

async fn call(router: &Router, name: &str, arguments: Value) -> Value {
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
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"));
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}\nfull: {text}");
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {text}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&joined).unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}"))
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

fn hit_paths(resp: &Value) -> Vec<String> {
    resp.get("hits")
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("no hits array: {resp}"))
        .iter()
        .filter_map(|h| h.get("path").and_then(|p| p.as_str()).map(str::to_owned))
        .collect()
}

fn query_args(query: &str, extra: Value) -> Value {
    let mut base = json!({
        "query": query,
        "workspace": WS,
        "project": PROJ,
        "limit": 10,
    });
    let obj = base.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    base
}

async fn seed_needle(h: &Harness) {
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "notes/topic.md",
            "the widget subsystem needle lives here",
        ))
        .await
        .unwrap();
}

/// 1. `answer=true` + provider → synthesized answer text + citations, and the
///    provider WAS invoked.
#[tokio::test]
async fn answer_true_with_provider_synthesizes_cited_answer() {
    let h = harness(true).await;
    seed_needle(&h).await;

    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({ "answer": true })),
    )
    .await;

    // Hits still present.
    assert!(
        hit_paths(&resp).contains(&"notes/topic.md".to_string()),
        "matching hit must still be present: {resp}"
    );

    // The synthesized answer is attached.
    let answer = resp
        .get("answer")
        .unwrap_or_else(|| panic!("answer=true must attach an answer: {resp}"));
    let text = answer
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or_else(|| panic!("answer must carry text: {answer}"));
    assert!(!text.trim().is_empty(), "answer text must be non-empty");
    let citations: Vec<&str> = answer
        .get("citations")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("answer must carry citations: {answer}"))
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    assert!(
        citations.contains(&"notes/topic.md"),
        "answer must cite the page it drew from: {citations:?}"
    );

    // The provider WAS called.
    assert_eq!(
        h.calls.as_ref().unwrap().load(Ordering::SeqCst),
        1,
        "answer=true must call the provider exactly once"
    );

    // No unavailable note when synthesis succeeded.
    assert!(
        resp.get("answer_unavailable").is_none(),
        "successful synthesis must not carry answer_unavailable: {resp}"
    );
}

/// 2. `answer=false` (the default) with a provider WIRED → ZERO LLM calls and no
///    `answer` field. This is the invariant-#13 guard.
#[tokio::test]
async fn answer_false_default_makes_zero_llm_calls() {
    let h = harness(true).await;
    seed_needle(&h).await;

    // Explicit answer=false.
    let explicit = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({ "answer": false })),
    )
    .await;
    assert!(
        explicit.get("answer").is_none(),
        "answer=false must not attach an answer: {explicit}"
    );
    assert!(
        explicit.get("answer_unavailable").is_none(),
        "answer=false must not attach an unavailable note: {explicit}"
    );

    // Default (answer omitted entirely).
    let default = call(&h.router, "memory_query", query_args("needle", json!({}))).await;
    assert!(
        default.get("answer").is_none(),
        "default query must not attach an answer: {default}"
    );

    // The critical guarantee: the provider was never touched.
    assert_eq!(
        h.calls.as_ref().unwrap().load(Ordering::SeqCst),
        0,
        "default / answer=false must make ZERO LLM calls (invariant #13)"
    );
}

/// 3. `answer=true` + NO provider → graceful `answer_unavailable`, hits present,
///    no error.
#[tokio::test]
async fn answer_true_without_provider_degrades_gracefully() {
    let h = harness(false).await;
    seed_needle(&h).await;

    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({ "answer": true })),
    )
    .await;

    // Hits are returned normally.
    assert!(
        hit_paths(&resp).contains(&"notes/topic.md".to_string()),
        "hits must be returned even without a provider: {resp}"
    );
    // No synthesized answer.
    assert!(
        resp.get("answer").is_none(),
        "no provider must not fabricate an answer: {resp}"
    );
    // An explicit, non-empty unavailable note (never an error).
    let note = resp
        .get("answer_unavailable")
        .and_then(|n| n.as_str())
        .unwrap_or_else(|| {
            panic!("answer=true without a provider must note answer_unavailable: {resp}")
        });
    assert!(
        !note.trim().is_empty(),
        "answer_unavailable note must explain why"
    );
}

/// 4. Live smoke test: wire a real provider only when a key is in the
///    environment, and assert a non-empty answer. Skips cleanly (and is
///    `#[ignore]`d) so it never runs in CI or reads a key that isn't set. Run
///    manually with `cargo test -p ai-memory-mcp -- --ignored query_answer`.
#[tokio::test]
#[ignore = "requires a live provider key (GEMINI_API_KEY or ANTHROPIC_API_KEY); run manually"]
async fn answer_live_provider_smoke() {
    use secrecy::SecretString;

    let provider: Arc<dyn LlmProvider> = if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        Arc::new(
            ai_memory_llm::GeminiProvider::new(SecretString::from(key), "gemini-2.5-flash")
                .expect("gemini provider"),
        )
    } else if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        Arc::new(
            ai_memory_llm::AnthropicProvider::new(
                SecretString::from(key),
                "claude-3-5-haiku-latest",
            )
            .expect("anthropic provider"),
        )
    } else {
        eprintln!("no live provider key set; skipping answer_live_provider_smoke");
        return;
    };

    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_llm(provider);
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "notes/deploy.md",
            "Deploys always use `compose pull`, never bin/deploy, which is single-arch.",
        ))
        .await
        .unwrap();
    let router = mount(server);

    let resp = call(
        &router,
        "memory_query",
        query_args("how do we deploy?", json!({ "answer": true })),
    )
    .await;
    let text = resp
        .get("answer")
        .and_then(|a| a.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_else(|| panic!("live provider must synthesize an answer: {resp}"));
    assert!(
        !text.trim().is_empty(),
        "live provider answer must be non-empty: {resp}"
    );
}
