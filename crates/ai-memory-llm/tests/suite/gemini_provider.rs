//! Gemini's output-budget stop must reach callers that recover by splitting
//! structured batches; plain completions can still use partial prose.

use std::sync::Arc;

use ai_memory_llm::health::{ProviderHealth, ProviderHealthStatus};
use ai_memory_llm::{
    ChatRequest, GeminiProvider, LlmError, LlmOperationId, LlmProvider, LlmWorkload,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gemini-2.5-flash";

fn reply(text: Option<&str>, finish_reason: Option<&str>) -> Value {
    let mut candidate = json!({});
    if let Some(text) = text {
        candidate["content"] = json!({"role": "model", "parts": [{"text": text}]});
    }
    if let Some(finish_reason) = finish_reason {
        candidate["finishReason"] = json!(finish_reason);
    }
    json!({
        "candidates": [candidate],
        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2000},
        "modelVersion": MODEL,
    })
}

async fn provider(response: Value, requests: u64) -> (MockServer, GeminiProvider) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/v1beta/models/{MODEL}:generateContent")))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(requests)
        .mount(&server)
        .await;
    let provider = GeminiProvider::new(SecretString::from("synthetic-test-key"), MODEL)
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
#[case::absent_content(None)]
#[tokio::test]
async fn gemini_structured_max_tokens_is_truncated_before_parsing(#[case] text: Option<&str>) {
    let (_server, provider) = provider(reply(text, Some("MAX_TOKENS")), 2).await;
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
async fn gemini_budget_stop_without_usage_has_no_completion_token_count() {
    let mut response = reply(None, Some("MAX_TOKENS"));
    response.as_object_mut().unwrap().remove("usageMetadata");
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
#[case::normal_stop(Some("STOP"))]
#[case::legacy_missing_reason(None)]
#[tokio::test]
async fn gemini_complete_structured_json_still_succeeds(#[case] finish_reason: Option<&str>) {
    let (_server, provider) = provider(reply(Some(r#"{"items":["ok"]}"#), finish_reason), 1).await;
    let value = provider
        .complete_structured_raw(request("Classify"), schema())
        .await
        .unwrap();
    assert_eq!(value, json!({"items": ["ok"]}));
}

#[tokio::test]
async fn gemini_non_budget_parse_and_missing_content_errors_are_preserved() {
    let (_server, malformed) = provider(reply(Some("incomplete JSON"), Some("STOP")), 1).await;
    assert!(matches!(
        malformed
            .complete_structured_raw(request("Classify"), schema())
            .await,
        Err(LlmError::Serde(_))
    ));
    let (_server, blocked) = provider(reply(None, Some("SAFETY")), 1).await;
    assert!(matches!(
        blocked
            .complete_structured_raw(request("Classify"), schema())
            .await,
        Err(LlmError::UnexpectedShape(_))
    ));
}

#[tokio::test]
async fn gemini_plain_max_tokens_keeps_partial_text_and_usage() {
    let (_server, provider) = provider(reply(Some("partial prose"), Some("MAX_TOKENS")), 1).await;
    let response = provider.complete(request("Write prose")).await.unwrap();
    assert_eq!(response.text, "partial prose");
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 7);
    assert_eq!(usage.output_tokens, 2000);
}

#[tokio::test]
async fn gemini_budget_stop_keeps_profile_call_and_error_accounting() {
    let (_server, provider) = provider(reply(Some(r#"{"items":[]}"#), Some("MAX_TOKENS")), 1).await;
    let health = ProviderHealth::default();
    let wrapped = health.wrap_llm_provider(Arc::new(provider), "gemini", MODEL, None);
    let error = wrapped
        .complete_structured_raw_with_operation_id(
            request("Classify"),
            schema(),
            LlmOperationId::for_workload(LlmWorkload::Profile),
        )
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
}
