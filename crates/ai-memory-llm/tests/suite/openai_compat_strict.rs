//! Integration tests for `OpenAiCompatProvider` strict mode, driving the
//! provider against a real in-process HTTP mock (wiremock).
//!
//! The strict path is the load-bearing addition from PR #70: when
//! `compat_strict=true`, the provider sends `response_format=json_schema`
//! first; on a *parse-shape* failure (HTTP 200 with bad JSON, or 200
//! with a non-object), or an explicit structured-output capability
//! rejection, it falls back to the tolerant prose-JSON parser. Other
//! transport / HTTP-status / auth failures propagate without retry.
//! These tests assert those outcomes
//! end-to-end against synthesised upstream responses.
//!
//! Why not unit-test in `openai_compat.rs`: the strict path delegates
//! to `OpenAiProvider::complete_structured_raw`, which speaks real
//! HTTP. The classification helper (`is_parse_shape_error`) is unit-
//! tested alongside its definition; here we cover what `is_parse_shape_error`
//! actually changes about end-to-end behaviour.

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{LlmProvider, OpenAiCompatProvider};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn strict_provider(base_url: String) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(base_url, None, "mistral-nemo")
        .expect("provider builds")
        .with_strict(true)
}

fn tiny_request() -> ChatRequest {
    ChatRequest::user_prompt("emit JSON: { \"ok\": true }")
}

fn tiny_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"],
    })
}

fn body_with_content(content: &str) -> serde_json::Value {
    json!({
        "id": "id",
        "object": "chat.completion",
        "created": 0,
        "model": "mistral-nemo",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

/// Counts every request hitting the mock server so a test can assert
/// "only one call was made" (strict succeeded) vs "two calls were made"
/// (strict raw, then tolerant fallback). The response itself is cloned
/// from a `(status, body)` pair on every call — `ResponseTemplate` doesn't
/// expose a clone, so we rebuild it each request.
#[derive(Clone)]
struct CountingResponder {
    count: Arc<AtomicUsize>,
    status: u16,
    body: Arc<serde_json::Value>,
}

impl CountingResponder {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(0)),
            status,
            body: Arc::new(body),
        }
    }
    fn raw(status: u16, body: &str) -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(0)),
            status,
            body: Arc::new(serde_json::Value::String(body.to_string())),
        }
    }
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl Respond for CountingResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.count.fetch_add(1, Ordering::SeqCst);
        match self.body.as_ref() {
            serde_json::Value::String(s) => ResponseTemplate::new(self.status).set_body_string(s),
            other => ResponseTemplate::new(self.status).set_body_json(other.clone()),
        }
    }
}

/// Strict path SUCCEEDS: model honoured `response_format` and returned
/// clean JSON. Provider must surface the parsed object directly with
/// exactly ONE upstream HTTP call (the tolerant fallback is NOT invoked).
#[tokio::test]
async fn strict_success_returns_directly_and_makes_one_call() {
    let server = MockServer::start().await;
    let responder = CountingResponder::json(200, body_with_content(r#"{"ok":true}"#));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let provider = strict_provider(server.uri());
    let value = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect("strict success path");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(
        responder.count(),
        1,
        "strict success must hit upstream exactly once (no fallback retry)"
    );
}

/// Strict path returns 200 with a NON-OBJECT body (e.g. plain prose).
/// The Compat wrapper detects the non-object response and falls through
/// to the tolerant path, which makes a SECOND HTTP call with no
/// `response_format` and extracts the first balanced `{…}` from the
/// resulting prose. End result: success after two HTTP hops.
#[tokio::test]
async fn strict_non_object_falls_back_to_tolerant_parser() {
    let server = MockServer::start().await;
    // First call (strict): returns prose with a JSON object embedded.
    // Because `OpenAiProvider::complete_structured_raw` calls
    // `serde_json::from_str` on `content` directly, prose that wraps a
    // JSON object will not parse — but the inner provider's parse-fail
    // path surfaces as `LlmError::Serde`, which `is_parse_shape_error`
    // catches and the wrapper retries via `inner.complete(...)`.
    // Both calls return the same prose body. The strict raw parse fails
    // because the model's `content` isn't itself valid JSON (just prose
    // wrapping a JSON object), surfaced as `LlmError::Serde`. The
    // wrapper's `is_parse_shape_error` catches it and the tolerant path
    // makes a second call whose response runs through `first_json_object`
    // to extract the embedded `{"ok":true}`.
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    let prose_body = body_with_content("Sure! Here is the JSON you asked for: {\"ok\":true}");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &Request| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(prose_body.clone())
        })
        .mount(&server)
        .await;

    let provider = strict_provider(server.uri());
    let value = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect("strict fallback to tolerant must recover the embedded object");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "fallback path must issue exactly two upstream calls (strict, then tolerant)"
    );
}

/// Strict path receives an HTTP 5xx (upstream outage). A tolerant
/// retry would hit the same outage and double the wall-clock /
/// token spend. Audit fix: provider must PROPAGATE the
/// `LlmError::Provider` instead of retrying, so the caller sees the
/// real cause and can back off.
#[tokio::test]
async fn strict_upstream_5xx_propagates_without_fallback_retry() {
    let server = MockServer::start().await;
    let responder = CountingResponder::raw(500, "boom");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let provider = strict_provider(server.uri());
    let err = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect_err("5xx must surface");
    assert!(
        err.to_string().contains("500"),
        "error must carry the upstream status, got: {err}"
    );
    assert_eq!(
        responder.count(),
        1,
        "5xx must NOT trigger the tolerant retry — that would double cost",
    );
}

/// Same propagation guarantee for HTTP 401 (auth) — the tolerant retry
/// would just hit the same 401. Distinct test so a future refactor
/// that special-cases auth errors fails loudly.
#[tokio::test]
async fn strict_upstream_401_propagates_without_fallback_retry() {
    let server = MockServer::start().await;
    let responder = CountingResponder::raw(401, "missing or invalid api key");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let provider = strict_provider(server.uri());
    let err = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect_err("401 must surface");
    assert!(
        err.to_string().contains("401"),
        "error must carry the auth status, got: {err}"
    );
    assert_eq!(
        responder.count(),
        1,
        "401 must NOT trigger the tolerant retry"
    );
}

/// Older compatibility endpoints may reject the structured-output field
/// before running the model. That specific capability failure is safe to
/// retry without `response_format`; a generic 400 remains non-retryable.
#[tokio::test]
async fn strict_response_format_rejection_falls_back_to_tolerant_parser() {
    let server = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            let request: serde_json::Value =
                serde_json::from_slice(&req.body).expect("request body is JSON");
            if request.get("response_format").is_some() {
                ResponseTemplate::new(400).set_body_string("unsupported parameter: response_format")
            } else {
                ResponseTemplate::new(200).set_body_json(body_with_content("result: {\"ok\":true}"))
            }
        })
        .mount(&server)
        .await;

    let provider = strict_provider(server.uri());
    let value = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect("capability rejection must use tolerant fallback");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn strict_generic_400_propagates_without_fallback_retry() {
    let server = MockServer::start().await;
    let responder = CountingResponder::raw(400, "unknown model");
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let provider = strict_provider(server.uri());
    let err = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect_err("generic 400 must surface");
    assert!(err.to_string().contains("400"));
    assert_eq!(responder.count(), 1);
}

/// Reasoning models (DeepSeek / Qwen3 / MiniMax M2.7) embed
/// `<think>…</think>` inside `content` before the JSON. Strict-mode
/// users on those models should keep the flag OFF; this test
/// documents that turning strict ON still WORKS — it just costs one
/// extra HTTP call. Strict-raw parses `content` directly with no
/// `<think>` strip and fails, fallback kicks in, tolerant path runs
/// `strip_reasoning_blocks` and recovers.
#[tokio::test]
async fn strict_falls_back_for_reasoning_model_with_think_in_content() {
    let server = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    let content = "<think>I should output JSON.</think>\n{\"ok\":true}";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &Request| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(body_with_content(content))
        })
        .mount(&server)
        .await;

    let provider = strict_provider(server.uri());
    let value = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect("fallback must strip <think> and recover");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "reasoning content costs the double-call fallback (operator should keep strict OFF)"
    );
}
