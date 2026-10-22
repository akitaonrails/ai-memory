//! The provider's retry hint reaches `LlmError::Provider::retry_after` from a
//! real HTTP failure. Gemini sends no `Retry-After` on a quota 429: the delay
//! is a `google.rpc.RetryInfo` detail placed after the other details, more
//! than 1 KiB into the body, past the cut `provider_error_body` keeps. The
//! hint must therefore be read before that cut, which the oversized body
//! below would catch.

use std::time::Duration;

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{LlmError, LlmProvider, OpenAiCompatProvider};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The shape Gemini's OpenAI-compatible endpoint returns for a spent daily
/// quota: a one-element array, `RetryInfo` last, padded past 1 KiB the way
/// the real `QuotaFailure` and `Help` details pad it.
fn gemini_quota_body(retry_delay: &str) -> String {
    serde_json::json!([{
        "error": {
            "code": 429,
            "message": format!("You exceeded your current quota. {}", "x".repeat(1200)),
            "status": "RESOURCE_EXHAUSTED",
            "details": [
                { "@type": "type.googleapis.com/google.rpc.Help", "links": [] },
                { "@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [] },
                { "@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": retry_delay },
            ],
        }
    }])
    .to_string()
}

async fn failing_call(response: ResponseTemplate) -> LlmError {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(&server)
        .await;
    let provider = OpenAiCompatProvider::new(server.uri(), None, "gemini-3.8-flash")
        .expect("provider builds")
        .with_timeout_secs(5);
    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect_err("a 429 is an error")
}

#[tokio::test]
async fn a_retry_info_past_the_display_cut_still_reaches_the_error() {
    let body = gemini_quota_body("32993s");
    assert!(
        body.find("RetryInfo").unwrap() > 1024,
        "the fixture must place RetryInfo past the 1 KiB display cut"
    );

    let err = failing_call(ResponseTemplate::new(429).set_body_string(body)).await;

    let LlmError::Provider {
        status,
        body,
        retry_after,
    } = err
    else {
        panic!("expected a provider error, got {err}");
    };
    assert_eq!(status, 429);
    assert_eq!(retry_after, Some(Duration::from_secs(32_993)));
    assert!(
        !body.contains("RetryInfo"),
        "the carried body is still cut to the display size"
    );
}

#[tokio::test]
async fn a_retry_after_header_wins_over_the_body_hint() {
    let response = ResponseTemplate::new(429)
        .insert_header("Retry-After", "7")
        .set_body_string(gemini_quota_body("32993s"));

    let err = failing_call(response).await;

    assert!(matches!(
        err,
        LlmError::Provider { retry_after: Some(hint), .. } if hint == Duration::from_secs(7)
    ));
}

#[tokio::test]
async fn a_failure_without_a_hint_carries_none() {
    let err = failing_call(ResponseTemplate::new(503).set_body_string("overloaded")).await;

    assert!(matches!(
        err,
        LlmError::Provider {
            status: 503,
            retry_after: None,
            ..
        }
    ));
}
