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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
    response: Value,
    requests: Arc<Mutex<Vec<(ChatRequest, Value)>>>,
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
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push((request, schema));
        // Shape must deserialize into the server's answer-synthesis struct.
        Ok(self.response.clone())
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
    requests: Arc<Mutex<Vec<(ChatRequest, Value)>>>,
    _tmp: TempDir,
}

fn mount(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

/// Build a harness; `with_provider` decides whether a fake LLM is attached.
async fn harness(with_provider: bool) -> Harness {
    harness_with_response(
        with_provider,
        json!({
            "answer": "The widget subsystem stores its needle in notes/topic.md.",
            "citations": ["notes/topic.md"],
        }),
    )
    .await
}

async fn harness_with_response(with_provider: bool, response: Value) -> Harness {
    harness_with_budget(with_provider, response, None).await
}

async fn harness_with_budget(
    with_provider: bool,
    response: Value,
    budget: Option<usize>,
) -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let mut server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_answer_context_budget(budget);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let calls = if with_provider {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn LlmProvider> = Arc::new(FakeAnswerLlm {
            calls: calls.clone(),
            response,
            requests: requests.clone(),
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
        requests,
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

#[tokio::test]
async fn answer_rejects_a_citation_not_in_the_request() {
    let h = harness_with_response(
        true,
        json!({
            "answer": "Invented answer must not escape.",
            "citations": ["notes/topic.md", "notes/fabricated.md"],
        }),
    )
    .await;
    seed_needle(&h).await;
    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({"answer": true})),
    )
    .await;
    assert_eq!(hit_paths(&resp), ["notes/topic.md"]);
    assert!(resp.get("answer").is_none(), "unsupported answer: {resp}");
    assert!(
        resp.get("answer_unavailable").is_some(),
        "missing reason: {resp}"
    );
    assert!(!resp.to_string().contains("Invented answer"));
    assert!(!resp.to_string().contains("fabricated.md"));
    assert_eq!(h.calls.as_ref().unwrap().load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn answer_rejects_a_citation_from_an_unqueried_project() {
    let h = harness_with_response(
        true,
        json!({
            "answer": "Foreign answer must not escape.",
            "citations": ["notes/foreign.md"],
        }),
    )
    .await;
    seed_needle(&h).await;
    let foreign = h
        .store
        .writer
        .get_or_create_project(h.ws, "foreign", None)
        .await
        .unwrap();
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            foreign,
            "notes/foreign.md",
            "needle foreign secret",
        ))
        .await
        .unwrap();
    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({"answer": true})),
    )
    .await;
    assert_eq!(hit_paths(&resp), ["notes/topic.md"]);
    assert!(resp.get("answer").is_none(), "foreign citation: {resp}");
    assert!(resp.get("answer_unavailable").is_some());
    assert!(!resp.to_string().contains("foreign"));
    assert_eq!(h.calls.as_ref().unwrap().load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn answer_rejects_same_path_citations_across_scopes() {
    let h = harness(true).await;
    seed_needle(&h).await;
    let foreign_ws = h
        .store
        .writer
        .get_or_create_workspace("other")
        .await
        .unwrap();
    let foreign = h
        .store
        .writer
        .get_or_create_project(foreign_ws, PROJ, None)
        .await
        .unwrap();
    h.store
        .writer
        .upsert_page(page(
            foreign_ws,
            foreign,
            "notes/topic.md",
            "needle from another workspace",
        ))
        .await
        .unwrap();
    let resp = call(
        &h.router,
        "memory_query",
        json!({
            "query": "needle", "answer": true,
            "scopes": [{"workspace": WS, "project": PROJ}, {"workspace": "other", "project": PROJ}],
        }),
    )
    .await;
    assert_eq!(hit_paths(&resp).len(), 2, "both scopes must match: {resp}");
    assert!(resp.get("answer").is_none(), "ambiguous citation: {resp}");
    assert!(resp.get("answer_unavailable").is_some());
    assert_eq!(h.calls.as_ref().unwrap().load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn answer_deduplicates_citations_in_model_order_and_preserves_unicode() {
    let paths = ["notes/decisão.md", "notes/topic.md", "notes/決定.md"];
    let h = harness_with_response(
        true,
        json!({
            "answer": "All three snippets mention needle.",
            "citations": [paths[0], paths[1], paths[0], paths[2], paths[1]],
        }),
    )
    .await;
    for path in paths {
        h.store
            .writer
            .upsert_page(page(h.ws, h.proj, path, "needle appears here"))
            .await
            .unwrap();
    }
    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({"answer": true})),
    )
    .await;
    assert_eq!(resp.pointer("/answer/citations"), Some(&json!(paths)));
    assert!(resp.get("answer_unavailable").is_none());
    assert_eq!(h.calls.as_ref().unwrap().load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn answer_global_preferences_accept_unique_paths_and_refuse_collisions() {
    for (path, accepted) in [("notes/global.md", true), ("notes/topic.md", false)] {
        let h = harness_with_response(
            true,
            json!({
                "answer": "The global preference mentions needle.",
                "citations": [path],
            }),
        )
        .await;
        seed_needle(&h).await;
        let global = ai_memory_store::create_global_scope(&h.store.writer)
            .await
            .unwrap();
        h.store
            .writer
            .upsert_page(page(
                global.workspace_id,
                global.project_id,
                path,
                "needle global preference",
            ))
            .await
            .unwrap();
        let resp = call(
            &h.router,
            "memory_query",
            query_args("needle", json!({"answer": true})),
        )
        .await;
        assert_eq!(resp["global_scope_hits"].as_array().unwrap().len(), 1);
        assert_eq!(resp.get("answer").is_some(), accepted, "{resp}");
        assert_eq!(
            resp.get("answer_unavailable").is_some(),
            !accepted,
            "{resp}"
        );
        assert_eq!(h.calls.as_ref().unwrap().load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn answer_preserves_empty_citations_for_abstention() {
    let h = harness_with_response(
        true,
        json!({
            "answer": "The snippets do not contain enough information.",
            "citations": [],
        }),
    )
    .await;
    seed_needle(&h).await;
    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({"answer": true})),
    )
    .await;
    assert_eq!(resp.pointer("/answer/citations"), Some(&json!([])));
    assert!(resp.get("answer_unavailable").is_none());
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

#[tokio::test]
async fn answer_budget_never_calls_provider_without_evidence_and_keeps_hits() {
    let h = harness_with_budget(
        true,
        json!({"answer":"unreachable", "citations":[]}),
        Some(1),
    )
    .await;
    seed_needle(&h).await;
    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({"answer":true})),
    )
    .await;
    assert_eq!(hit_paths(&resp), ["notes/topic.md"]);
    assert!(resp.get("answer").is_none());
    assert!(
        resp["answer_unavailable"]
            .as_str()
            .unwrap()
            .contains("no meaningful evidence")
    );
    assert_eq!(resp["answer_context"]["omitted"], 1);
    assert_eq!(h.calls.unwrap().load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn answer_budget_is_inert_on_default_and_no_provider_queries() {
    for with_provider in [true, false] {
        let h = harness_with_budget(
            with_provider,
            json!({"answer":"unused", "citations":[]}),
            Some(1),
        )
        .await;
        seed_needle(&h).await;
        let default = call(&h.router, "memory_query", query_args("needle", json!({}))).await;
        assert!(default.get("answer_context").is_none());
        assert!(default.get("answer_unavailable").is_none());
        if !with_provider {
            let answer = call(
                &h.router,
                "memory_query",
                query_args("needle", json!({"answer":true})),
            )
            .await;
            assert!(answer.get("answer_context").is_none());
            assert!(
                answer["answer_unavailable"]
                    .as_str()
                    .unwrap()
                    .contains("no LLM")
            );
        }
    }
}

async fn seed_budget_pages(h: &Harness, second: bool) {
    let mut first = page(
        h.ws,
        h.proj,
        "notes/topic.md",
        "needle: current deployment uses PostgreSQL.",
    );
    first.pinned = true;
    h.store.writer.upsert_page(first).await.unwrap();
    if second {
        h.store
            .writer
            .upsert_page(page(
                h.ws,
                h.proj,
                "notes/omitted.md",
                "needle: unrelated procedure for the build system.",
            ))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn answer_budget_transport_accepts_admitted_and_rejects_omitted_sources() {
    let response = |path: &str| json!({"answer":"PostgreSQL", "citations":[path]});
    let full = harness_with_budget(true, response("notes/topic.md"), Some(100_000)).await;
    seed_budget_pages(&full, false).await;
    let arguments = || query_args("needle", json!({"answer":true, "pin_first":true}));
    let resp = call(&full.router, "memory_query", arguments()).await;
    assert_eq!(resp["answer"]["citations"], json!(["notes/topic.md"]));
    let limit = resp["answer_context"]["estimated_input_tokens"]
        .as_u64()
        .unwrap()
        + resp["answer_context"]["reserved_output_tokens"]
            .as_u64()
            .unwrap();
    for path in ["notes/topic.md", "notes/omitted.md"] {
        let h = harness_with_budget(true, response(path), Some(limit as usize)).await;
        seed_budget_pages(&h, true).await;
        let resp = call(&h.router, "memory_query", arguments()).await;
        assert_eq!(hit_paths(&resp), ["notes/topic.md", "notes/omitted.md"]);
        assert_eq!(resp["answer_context"]["admitted"], 1);
        assert_eq!(resp["answer_context"]["omitted"], 1);
        assert_eq!(h.calls.as_ref().unwrap().load(Ordering::SeqCst), 1);
        if path == "notes/topic.md" {
            assert_eq!(resp["answer"]["citations"], json!([path]));
            assert!(resp.get("answer_unavailable").is_none());
        } else {
            assert!(resp.get("answer").is_none());
            assert!(
                resp["answer_unavailable"]
                    .as_str()
                    .unwrap()
                    .contains("citations")
            );
        }
    }
}

#[tokio::test]
async fn answer_budget_receipt_matches_request_at_provider_boundary() {
    // Inspect the actual structured call, including its schema. A canned answer
    // alone cannot detect a handler that accounts one prompt and sends another.
    for (tier, output) in [
        ("minimal", 2_000_u64),
        ("low", 3_000),
        ("medium", 4_000),
        ("high", 6_000),
        ("max", 8_000),
    ] {
        let h = harness_with_budget(
            true,
            json!({"answer":"PostgreSQL", "citations":["notes/topic.md"]}),
            Some(16_000),
        )
        .await;
        seed_budget_pages(&h, true).await;
        let resp = call(
            &h.router,
            "memory_query",
            query_args("needle", json!({"answer":true, "reasoning":tier})),
        )
        .await;
        let requests = h.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "{tier}");
        let (request, schema) = &requests[0];
        let actual_input = request.system.as_ref().unwrap().len()
            + request
                .messages
                .iter()
                .map(|m| m.content.len())
                .sum::<usize>()
            + serde_json::to_vec(schema).unwrap().len()
            + 1_024;
        assert_eq!(request.max_tokens as u64, output, "{tier}");
        assert_eq!(resp["answer_context"]["reserved_output_tokens"], output);
        assert_eq!(
            resp["answer_context"]["estimated_input_tokens"],
            actual_input
        );
        assert!(actual_input + request.max_tokens as usize <= 16_000);
        assert_eq!(resp["answer_context"]["admitted"], 2);
        assert_eq!(resp["answer_context"]["omitted"], 0);
        assert_eq!(resp["answer_context"]["clipped"], 0);
        assert_eq!(hit_paths(&resp).len(), 2);
        assert_eq!(h.calls.as_ref().unwrap().load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn answer_budget_uses_store_recency_without_reordering_retrieval_hits() {
    let h = harness_with_budget(
        true,
        json!({"answer":"PostgreSQL", "citations":["notes/current.md"]}),
        Some(100_000),
    )
    .await;
    let mut old = page(
        h.ws,
        h.proj,
        "notes/old.md",
        "needle: historical deployment uses SQLite.",
    );
    old.title = "Deployment decision".into();
    old.pinned = true;
    h.store.writer.upsert_page(old).await.unwrap();
    // The two writes need distinct stored modification times for this fixture.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let mut current = page(
        h.ws,
        h.proj,
        "notes/current.md",
        "needle: current deployment uses PostgreSQL.",
    );
    current.title = "Deployment decision".into();
    h.store.writer.upsert_page(current).await.unwrap();
    let response = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({"answer":true,"pin_first":true})),
    )
    .await;
    assert_eq!(hit_paths(&response), ["notes/old.md", "notes/current.md"]);
    let requests = h.requests.lock().unwrap();
    let body = &requests[0].0.messages[0].content;
    assert!(body.find("notes/current.md").unwrap() < body.find("notes/old.md").unwrap());
    assert_eq!(response["answer"]["citations"], json!(["notes/current.md"]));
}
