//! OpenCode's native Responses backend must distinguish an output-budget
//! stop from a parse failure or an ordinary empty answer.

use std::sync::Arc;

use ai_memory_llm::health::{ProviderHealth, ProviderHealthStatus};
use ai_memory_llm::opencode::OPENCODE_SESSION_HEADER;
use ai_memory_llm::types::ExtraHeaders;
use ai_memory_llm::{
    ChatRequest, LlmError, LlmOperationId, LlmProvider, LlmWorkload, OpenCodeProvider,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gpt-5.6-luna";

fn reply(text: Option<&str>, status: Option<&str>, reason: Option<&str>) -> Value {
    let output = match text {
        Some(text) => json!([{
            "type": "message",
            "content": [{"type": "output_text", "text": text}],
        }]),
        None => json!([{"type": "reasoning", "summary": []}]),
    };
    let mut response = json!({
        "model": MODEL,
        "output": output,
        "usage": {"input_tokens": 7, "output_tokens": 2000},
    });
    if let Some(status) = status {
        response["status"] = json!(status);
    }
    if let Some(reason) = reason {
        response["incomplete_details"] = json!({"reason": reason});
    }
    response
}

async fn provider(response: Value, requests: u64) -> (MockServer, OpenCodeProvider) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(requests)
        .mount(&server)
        .await;
    let provider = OpenCodeProvider::new(SecretString::from("synthetic-test-key"), MODEL)
        .unwrap()
        .with_base_url(server.uri());
    (server, provider)
}

fn schema() -> Value {
    json!({"type": "object", "properties": {"items": {"type": "array", "items": {"type": "string"}}}})
}

fn request(prompt: &str) -> ChatRequest {
    ChatRequest {
        max_tokens: 2000,
        ..ChatRequest::user_prompt(prompt)
    }
}

#[rstest::rstest]
#[case::incomplete_json(Some(r#"{"items":["partial"#))]
#[case::valid_json(Some(r#"{"items":[]}"#))]
#[case::empty_text(Some(""))]
#[case::no_visible_output(None)]
#[tokio::test]
async fn opencode_budget_stop_is_truncated_before_parsing(#[case] text: Option<&str>) {
    let (_server, provider) = provider(
        reply(text, Some("incomplete"), Some("max_output_tokens")),
        2,
    )
    .await;
    for operation_id in [
        None,
        Some(LlmOperationId::for_workload(LlmWorkload::Profile)),
    ] {
        let request = request("Classify these sentences");
        let result = match operation_id {
            Some(operation_id) => {
                provider
                    .complete_structured_raw_with_operation_id(request, schema(), operation_id)
                    .await
            }
            None => provider.complete_structured_raw(request, schema()).await,
        };
        let error = result.expect_err("a budget stop must not become a structured result");
        match &error {
            LlmError::TruncatedResponse {
                model,
                completion_tokens,
            } => {
                assert_eq!(model, MODEL);
                assert_eq!(*completion_tokens, Some(2000));
            }
            other => panic!("expected TruncatedResponse, got {other:?}"),
        }
        assert_eq!(error.class(), "truncated-response");
        assert!(!error.is_transient());
        assert!(!error.to_string().contains("partial"));
    }
}

#[tokio::test]
async fn opencode_budget_stop_without_usage_has_no_completion_token_count() {
    let mut response = reply(None, Some("incomplete"), Some("max_output_tokens"));
    response.as_object_mut().unwrap().remove("usage");
    let (_server, provider) = provider(response, 1).await;
    let error = provider
        .complete_structured_raw(request("Classify"), schema())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        LlmError::TruncatedResponse {
            completion_tokens: None,
            ..
        }
    ));
}

#[rstest::rstest]
#[case::completed(Some("completed"))]
#[case::legacy_missing_status(None)]
#[tokio::test]
async fn opencode_completed_json_still_succeeds(#[case] status: Option<&str>) {
    let (_server, provider) = provider(reply(Some(r#"{"items":["ok"]}"#), status, None), 1).await;
    let value = provider
        .complete_structured_raw(request("Classify"), schema())
        .await
        .unwrap();
    assert_eq!(value, json!({"items": ["ok"]}));
}

#[rstest::rstest]
#[case::completed(Some("completed"), None)]
#[case::legacy_missing_status(None, None)]
#[case::content_filter(Some("incomplete"), Some("content_filter"))]
#[tokio::test]
async fn opencode_non_budget_empty_content_is_preserved(
    #[case] status: Option<&str>,
    #[case] reason: Option<&str>,
) {
    let (_server, provider) = provider(reply(None, status, reason), 1).await;
    let error = provider
        .complete_structured_raw(request("Classify"), schema())
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::EmptyContent { .. }));
}

#[tokio::test]
async fn opencode_non_budget_json_error_is_preserved() {
    let (_server, provider) = provider(reply(Some("not JSON"), Some("completed"), None), 1).await;
    assert!(matches!(
        provider
            .complete_structured_raw(request("Classify"), schema())
            .await,
        Err(LlmError::Serde(_))
    ));
}

#[tokio::test]
async fn opencode_plain_budget_stop_keeps_partial_text_and_usage() {
    let (_server, provider) = provider(
        reply(
            Some("partial prose"),
            Some("incomplete"),
            Some("max_output_tokens"),
        ),
        1,
    )
    .await;
    let response = provider.complete(request("Write prose")).await.unwrap();
    assert_eq!(response.text, "partial prose");
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 2000);
}

#[tokio::test]
async fn opencode_budget_stop_keeps_headers_and_profile_error_accounting() {
    let (server, provider) = provider(
        reply(
            Some(r#"{"items":[]}"#),
            Some("incomplete"),
            Some("max_output_tokens"),
        ),
        1,
    )
    .await;
    let provider = provider.with_extra_headers(ExtraHeaders::parse(["x-test: retained"]).unwrap());
    let health = ProviderHealth::default();
    let wrapped = health.wrap_llm_provider(Arc::new(provider), "opencode", MODEL, None);
    let operation_id = LlmOperationId::for_workload(LlmWorkload::Profile);
    let error = wrapped
        .complete_structured_raw_with_operation_id(request("Classify"), schema(), operation_id)
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::TruncatedResponse { .. }));
    let snapshot = health.snapshot();
    let profile = snapshot
        .llm_workloads
        .iter()
        .find(|row| row.workload == LlmWorkload::Profile)
        .unwrap();
    assert_eq!(
        (profile.calls, profile.successes, profile.errors),
        (1, 0, 1)
    );
    assert!(profile.estimated_input_tokens > 0);
    assert_eq!(
        profile.last_error_class.as_deref(),
        Some("truncated-response")
    );
    assert_eq!(snapshot.llm.status, ProviderHealthStatus::Error);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers[OPENCODE_SESSION_HEADER],
        operation_id.to_string()
    );
    assert_eq!(requests[0].headers["x-test"], "retained");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["max_output_tokens"], 2000);
    assert_eq!(body["text"]["format"]["type"], "json_schema");
}
