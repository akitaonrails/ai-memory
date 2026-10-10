//! Integration tests for the openai-compat guardrails, driving the real
//! HTTP client against an in-process wiremock server that synthesises
//! the engine responses (a vLLM-hosted Qwen3-class thinking model that
//! truncates the structured payload at the output budget).
//!
//! Two behaviours are proven end-to-end on the wire — no production
//! endpoint, no real key:
//!
//! 1. The opt-in `chat_template_kwargs: {"enable_thinking": false}`
//!    request payload is ABSENT by default and present verbatim when
//!    opted in, on the `openai-compat` dialect only.
//! 2. A structured response with `finish_reason = "length"` and a
//!    response with no usable `message.content` are terminal errors with
//!    clear classes (`truncated-response`, `empty-content`): no retry /
//!    tolerant fallback, no response content in the error message, and
//!    intact JSON responses still parse successfully.

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{LlmError, LlmProvider, OpenAiCompatProvider, OpenAiProvider};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const QWEN_MODEL: &str = "applianceai01/qwen3.8-27b";

/// Truncated mid-value: exactly what a `length` stop leaves behind.
/// Deliberately unbalanced so no extractor could "recover" an object.
const TRUNCATED_JSON: &str = "{ \"findings\": [\"kept the\" ";

fn tiny_request() -> ChatRequest {
    ChatRequest::user_prompt("emit JSON: { \"findings\": [] }")
}

fn tiny_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "findings": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["findings"]
    })
}

/// Synthesise an OpenAI-wire chat completion body. `content = None`
/// omits the key entirely (an engine that returns no content block).
fn qwen_body(
    content: Option<&str>,
    finish_reason: &str,
    completion_tokens: u32,
) -> serde_json::Value {
    let mut message = json!({ "role": "assistant" });
    if let Some(content) = content {
        message["content"] = json!(content);
    }
    json!({
        "id": "chatcmpl-qwen",
        "object": "chat.completion",
        "created": 0,
        "model": QWEN_MODEL,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": completion_tokens,
            "total_tokens": 12 + completion_tokens
        }
    })
}

fn body_mock(body: serde_json::Value) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
}

fn tolerant_provider(base_url: String) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(base_url, None, QWEN_MODEL).expect("provider builds")
}

/// Default request shape is byte-stable: no `chat_template_kwargs` key
/// at all (not even `false`), so unconfigured vLLM / Ollama / LM Studio
/// setups see exactly what they saw before.
#[tokio::test]
async fn request_omits_chat_template_kwargs_by_default() {
    let server = MockServer::start().await;
    body_mock(qwen_body(Some("prose"), "stop", 5))
        .mount(&server)
        .await;

    let provider = tolerant_provider(server.uri());
    provider
        .complete(tiny_request())
        .await
        .expect("plain completion succeeds");

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert!(
        body.get("chat_template_kwargs").is_none(),
        "no chat_template_kwargs without the opt-in, got {body}"
    );
}

/// Opted in, the exact vLLM / SGLang payload is on the wire.
#[tokio::test]
async fn request_sends_enable_thinking_false_when_opted_in() {
    let server = MockServer::start().await;
    body_mock(qwen_body(Some("prose"), "stop", 5))
        .mount(&server)
        .await;

    let provider = tolerant_provider(server.uri()).with_disable_thinking(true);
    provider
        .complete(tiny_request())
        .await
        .expect("plain completion succeeds");

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("request body is JSON");
    assert_eq!(
        body["chat_template_kwargs"],
        json!({ "enable_thinking": false }),
        "the opt-in payload must be sent verbatim, got {body}"
    );
}

/// Tolerant path: a `length`-truncated structured response is a
/// terminal `truncated-response` error — one upstream call (no retry),
/// the provider-reported completion token count is carried, and the
/// error message carries no response content.
#[tokio::test]
async fn tolerant_truncated_response_is_terminal_truncated_response() {
    let server = MockServer::start().await;
    body_mock(qwen_body(Some(TRUNCATED_JSON), "length", 4096))
        .mount(&server)
        .await;

    let provider = tolerant_provider(server.uri()).with_disable_thinking(true);
    let err = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect_err("a length stop must not parse");

    match &err {
        LlmError::TruncatedResponse {
            model,
            completion_tokens,
        } => {
            assert_eq!(model, QWEN_MODEL);
            assert_eq!(*completion_tokens, Some(4096));
        }
        other => panic!("expected TruncatedResponse, got {other:?}"),
    }
    assert_eq!(err.class(), "truncated-response");
    assert!(!err.is_transient(), "a length stop must not be retried");
    let rendered = err.to_string();
    assert!(
        !rendered.contains("kept the"),
        "the error message must not carry response content: {rendered}"
    );
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        requests.len(),
        1,
        "a length stop must not trigger a second upstream call"
    );
}

/// Strict path: the same `length` stop must NOT fall through to the
/// tolerant parser — that would be a second HTTP call re-truncating the
/// same output and doubling the spend.
#[tokio::test]
async fn strict_truncated_response_is_terminal_without_fallback() {
    let server = MockServer::start().await;
    body_mock(qwen_body(Some(TRUNCATED_JSON), "length", 4096))
        .mount(&server)
        .await;

    let provider = tolerant_provider(server.uri()).with_strict(true);
    let err = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect_err("a length stop must not fall back to the tolerant parser");

    assert_eq!(err.class(), "truncated-response");
    assert!(!err.is_transient());
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        requests.len(),
        1,
        "strict length stop must hit upstream exactly once (no tolerant fallback)"
    );
}

/// The guard lives in the shared OpenAI-wire structured path, so the
/// `Official` dialect (api.openai.com shape) classifies the same way.
#[tokio::test]
async fn official_truncated_structured_response_is_terminal() {
    let server = MockServer::start().await;
    body_mock(qwen_body(Some(TRUNCATED_JSON), "length", 4096))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(SecretString::new("dummy".into()), QWEN_MODEL)
        .expect("provider builds")
        .with_base_url(server.uri());
    let err = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect_err("a length stop must not parse");

    assert_eq!(err.class(), "truncated-response");
    assert!(!err.is_transient());
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 1);
}

/// Empty content is one class with hostile variants: an empty string,
/// whitespace only, and a `content` key missing entirely. Each must be
/// the terminal `empty-content` error — distinct from `serde` /
/// `unexpected-shape` — with a single upstream call.
#[tokio::test]
async fn tolerant_empty_content_variants_are_terminal_empty_content() {
    for (label, content) in [
        ("empty string", Some("")),
        ("whitespace only", Some("   \n\t  ")),
        ("missing content key", None),
    ] {
        let server = MockServer::start().await;
        body_mock(qwen_body(content, "stop", 0))
            .mount(&server)
            .await;

        let provider = tolerant_provider(server.uri());
        let err = provider
            .complete_structured_raw(tiny_request(), tiny_schema())
            .await
            .expect_err("empty content must surface a typed error");

        match &err {
            LlmError::EmptyContent { model } => assert_eq!(model, QWEN_MODEL),
            other => panic!("{label}: expected EmptyContent, got {other:?}"),
        }
        assert_eq!(err.class(), "empty-content");
        assert!(!err.is_transient(), "{label} must not be retried");
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1, "{label}: exactly one upstream call");
    }
}

/// Strict path, empty content: the tolerant fallback must NOT run —
/// there is nothing a second call can conjure.
#[tokio::test]
async fn strict_empty_content_is_terminal_without_fallback() {
    let server = MockServer::start().await;
    body_mock(qwen_body(Some(""), "stop", 0))
        .mount(&server)
        .await;

    let provider = tolerant_provider(server.uri()).with_strict(true);
    let err = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect_err("empty content must not fall back");
    assert_eq!(err.class(), "empty-content");
    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        requests.len(),
        1,
        "strict empty content must hit upstream exactly once (no tolerant fallback)"
    );
}

/// Control: an intact response — including a reasoning model's
/// think-block prefix with a complete JSON object after it, the
/// normal Qwen flow — still parses and succeeds.
#[tokio::test]
async fn tolerant_intact_json_response_succeeds() {
    let server = MockServer::start().await;
    // The reasoning tags are built from escapes so this source file
    // never contains a literal tag sequence (editing tooling mangles
    // one in transit).
    let open = "\u{3C}think";
    let close = "\u{3C}/think\u{3E}";
    let content =
        format!("{open}Let me emit the requested JSON.{close}\n{{\"findings\": [\"kept the\"]}}");
    body_mock(qwen_body(Some(content.as_str()), "stop", 321))
        .mount(&server)
        .await;

    let provider = tolerant_provider(server.uri()).with_disable_thinking(true);
    let value = provider
        .complete_structured_raw(tiny_request(), tiny_schema())
        .await
        .expect("an intact structured response must parse");
    assert_eq!(value, json!({ "findings": ["kept the"] }));
}

/// Plain (non-structured) completions keep working when the engine
/// stops at the budget: a prose answer cut at `max_tokens` is a normal
/// outcome, not an error.
#[tokio::test]
async fn plain_complete_with_length_finish_reason_still_succeeds() {
    let server = MockServer::start().await;
    let content = "partial answer that was cut at the token budget";
    body_mock(qwen_body(Some(content), "length", 4096))
        .mount(&server)
        .await;

    let provider = tolerant_provider(server.uri());
    let response = provider
        .complete(tiny_request())
        .await
        .expect("a prose completion cut at the budget is a normal outcome");
    assert_eq!(response.text, content);
    assert_eq!(response.usage.map(|u| u.output_tokens), Some(4096));
}
