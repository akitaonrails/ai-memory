//! Integration coverage for `FallbackLlmProvider` against real network
//! failures: a genuine connection-refused and a genuine client-side
//! timeout, each produced by a real `reqwest::Error` rather than a
//! hand-built `LlmError`. `fallback.rs`'s own unit tests cover the pure
//! chain-order/circuit/redaction logic with fake providers; this file
//! proves `LlmError::is_transient()` sees those two real failure classes
//! the same way when they arrive through an actual `OpenAiCompatProvider`
//! HTTP call, matching `docs/llm-provider-fallback.md`'s delivery-sequence
//! scenario (a candidate that first fails transiently, then a working one).

use std::time::Duration;

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{Candidate, FallbackLlmProvider, LlmProvider, OpenAiCompatProvider};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn compat_provider(
    base_url: impl Into<String>,
    model: &str,
    timeout_secs: u64,
) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(base_url.into(), None, model)
        .expect("provider builds")
        .with_timeout_secs(timeout_secs)
}

fn ok_response_body(content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "id",
        "object": "chat.completion",
        "created": 0,
        "model": "second-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

/// A closed loopback port refuses the connection immediately, producing a
/// real `reqwest::Error` with `.is_connect() == true` — no wiremock or
/// external network access needed.
async fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener); // nothing listens on `addr` from here on
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn a_real_connection_error_advances_to_the_next_candidate() {
    let unreachable_base = closed_port_url().await;
    let working = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body("second answer")))
        .mount(&working)
        .await;

    let first = compat_provider(unreachable_base, "first-model", 5);
    let second = compat_provider(working.uri(), "second-model", 5);
    let chain = FallbackLlmProvider::new(vec![
        Candidate::new("openai-compat", "first-model", std::sync::Arc::new(first)),
        Candidate::new("openai-compat", "second-model", std::sync::Arc::new(second)),
    ]);

    let response = chain
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("a real connection error must advance to the next candidate");
    assert_eq!(response.text, "second answer");
}

#[tokio::test]
async fn a_real_client_timeout_advances_to_the_next_candidate() {
    let slow = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_response_body("too slow"))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&slow)
        .await;
    let working = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body("second answer")))
        .mount(&working)
        .await;

    // `with_timeout_secs` only accepts whole seconds, so the mock's
    // artificial delay is set well beyond the 1s client timeout — a real
    // `reqwest::Error::is_timeout() == true`, not a race.
    let first = compat_provider(slow.uri(), "first-model", 1);
    let second = compat_provider(working.uri(), "second-model", 5);
    let chain = FallbackLlmProvider::new(vec![
        Candidate::new("openai-compat", "first-model", std::sync::Arc::new(first)),
        Candidate::new("openai-compat", "second-model", std::sync::Arc::new(second)),
    ]);

    let response = chain
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("a real client timeout must advance to the next candidate");
    assert_eq!(response.text, "second answer");
}
