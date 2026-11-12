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

/// Port 0 can never be listened on, so connecting to it fails inside
/// `connect()` on every platform, producing a real `reqwest::Error` with
/// `.is_connect() == true`. A bound-then-released ephemeral port is racy:
/// another test's mock server can claim it in between and answer.
async fn closed_port_url() -> String {
    "http://127.0.0.1:0/v1".to_owned()
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

// ---------------------------------------------------------------------------
// Credit / quota exhaustion through real providers built by `build_provider`,
// each answering with its provider's documented out-of-credit body.
// ---------------------------------------------------------------------------

mod quota {
    use std::time::Duration;

    use ai_memory_llm::types::ChatRequest;
    use ai_memory_llm::{
        Candidate, ExtraHeaders, FallbackLlmProvider, LlmError, LlmProvider, ProviderAuth,
        ProviderChoice, ProviderConfig, build_provider,
    };
    use secrecy::SecretString;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::ok_response_body;

    const ANTHROPIC_CREDIT: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."},"request_id":"req_test"}"#;
    const ANTHROPIC_BAD_KEY: &str =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    const OPENAI_INSUFFICIENT_QUOTA: &str = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#;
    const OPENROUTER_CREDITS: &str = r#"{"error":{"message":"Insufficient credits. Add more using https://openrouter.ai/settings/credits","code":402}}"#;
    const ANTHROPIC_SECRET: &str = "sk-ant-api03-quota-test-secret-0123456789";
    const OPENAI_SECRET: &str = "sk-proj-quota-test-secret-0123456789";

    async fn answering(status: u16, body: &'static str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(status).set_body_raw(body.as_bytes(), "application/json"),
            )
            .mount(&server)
            .await;
        server
    }

    async fn answering_ok(text: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body(text)))
            .mount(&server)
            .await;
        server
    }

    fn anthropic_ok(text: &str) -> serde_json::Value {
        serde_json::json!({
            "model": "claude-sonnet",
            "content": [{ "type": "text", "text": text }],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
    }

    fn candidate(
        choice: ProviderChoice,
        model: &str,
        auth: ProviderAuth,
        base: String,
    ) -> Candidate {
        let provider = build_provider(ProviderConfig {
            provider: choice,
            model: model.into(),
            auth,
            base_url: Some(base),
            compat_strict: false,
            compat_disable_thinking: false,
            compat_stream: None,
            request_timeout_secs: 5,
            reasoning_effort: None,
            extra_headers: ExtraHeaders::default(),
            request_metadata_headers: false,
        })
        .expect("provider builds");
        Candidate::new(choice.name(), model, provider)
    }

    fn anthropic(base: String) -> Candidate {
        candidate(
            ProviderChoice::Anthropic,
            "claude-sonnet",
            ProviderAuth::required_api_key_from_env(
                "ANTHROPIC_API_KEY",
                Some(SecretString::from(ANTHROPIC_SECRET.to_string())),
            ),
            base,
        )
    }

    fn openai(base: String) -> Candidate {
        candidate(
            ProviderChoice::OpenAi,
            "gpt-mini",
            ProviderAuth::required_api_key_from_env(
                "OPENAI_API_KEY",
                Some(SecretString::from(OPENAI_SECRET.to_string())),
            ),
            base,
        )
    }

    fn compat(base: String, model: &str) -> Candidate {
        candidate(
            ProviderChoice::OpenAiCompat,
            model,
            ProviderAuth::optional_api_key_from_env("LLM_API_KEY", None),
            base,
        )
    }

    async fn hits(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .expect("mock records")
            .len()
    }

    /// The reported scenario: Anthropic primary out of credit (HTTP 400),
    /// OpenAI fallback healthy. The fallback answers, the primary is not
    /// called again inside the quota window, it is probed once the window
    /// ends, and it takes back over once it answers.
    #[tokio::test]
    async fn anthropic_out_of_credit_fails_over_to_openai_then_recovers() {
        let primary = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_raw(ANTHROPIC_CREDIT.as_bytes(), "application/json"),
            )
            .up_to_n_times(1)
            .mount(&primary)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_ok("from anthropic")))
            .mount(&primary)
            .await;
        let fallback = answering_ok("from openai").await;
        let chain =
            FallbackLlmProvider::new(vec![anthropic(primary.uri()), openai(fallback.uri())])
                .with_quota_cooldown(Duration::from_secs(1));

        for _ in 0..2 {
            let response = chain
                .complete(ChatRequest::user_prompt("hi"))
                .await
                .unwrap();
            assert_eq!(response.text, "from openai");
        }
        assert_eq!(
            hits(&primary).await,
            1,
            "the exhausted primary is not hammered"
        );
        assert_eq!(hits(&fallback).await, 2);
        let health = chain.candidate_health();
        assert_eq!(health[0].last_error_status, Some(400));
        assert_eq!(health[0].circuit_reason.as_deref(), Some("quota-exhausted"));
        assert!(health[1].last_selected);

        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let response = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();
        assert_eq!(
            response.text, "from anthropic",
            "the topped-up primary takes back over"
        );
        assert_eq!(hits(&primary).await, 2);
        assert_eq!(hits(&fallback).await, 2);
        assert!(chain.candidate_health()[0].last_selected);
    }

    /// OpenAI's `429 insufficient_quota` and OpenRouter's `402` both advance,
    /// each provider is called exactly once (no in-place retry), and the
    /// third candidate answers.
    #[tokio::test]
    async fn openai_insufficient_quota_and_openrouter_402_advance_once_each() {
        let openai_server = answering(429, OPENAI_INSUFFICIENT_QUOTA).await;
        let router = answering(402, OPENROUTER_CREDITS).await;
        let working = answering_ok("third").await;
        let chain = FallbackLlmProvider::new(vec![
            openai(openai_server.uri()),
            compat(format!("{}/v1", router.uri()), "router-model"),
            compat(format!("{}/v1", working.uri()), "working-model"),
        ]);

        let response = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();
        assert_eq!(response.text, "third");
        assert_eq!(hits(&openai_server).await, 1);
        assert_eq!(hits(&router).await, 1);
        assert_eq!(hits(&working).await, 1);
        let health = chain.candidate_health();
        assert_eq!(
            health[0].last_error_class.as_deref(),
            Some("quota-exhausted")
        );
        assert_eq!(
            health[1].last_error_class.as_deref(),
            Some("quota-exhausted")
        );
        assert_eq!(health[1].last_error_status, Some(402));
    }

    /// A genuinely invalid key is the operator's to fix: no failover.
    #[tokio::test]
    async fn an_invalid_anthropic_key_does_not_fail_over() {
        let primary = answering(401, ANTHROPIC_BAD_KEY).await;
        let fallback = answering_ok("unreachable").await;
        let chain =
            FallbackLlmProvider::new(vec![anthropic(primary.uri()), openai(fallback.uri())]);
        let err = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("an invalid key stops the chain");
        assert_eq!(err.http_status(), Some(401));
        assert!(!err.should_fail_over());
        assert_eq!(hits(&fallback).await, 0);
    }

    /// Every candidate out of credit: one bounded, redacted aggregate that
    /// names each candidate's class and status, never a body or a key.
    #[tokio::test]
    async fn every_provider_out_of_credit_returns_a_redacted_aggregate() {
        let primary = answering(400, ANTHROPIC_CREDIT).await;
        let fallback = answering(429, OPENAI_INSUFFICIENT_QUOTA).await;
        let router = answering(402, OPENROUTER_CREDITS).await;
        let chain = FallbackLlmProvider::new(vec![
            anthropic(primary.uri()),
            openai(fallback.uri()),
            compat(format!("{}/v1", router.uri()), "router-model"),
        ]);
        let err = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("nothing answers");
        let rendered = err.to_string();
        assert!(
            matches!(err, LlmError::AllCandidatesFailed { attempted: 3, .. }),
            "{rendered}"
        );
        assert!(
            rendered.contains("anthropic/claude-sonnet: quota-exhausted 400"),
            "{rendered}"
        );
        assert!(
            rendered.contains("openai/gpt-mini: quota-exhausted 429"),
            "{rendered}"
        );
        assert!(
            rendered.contains("openai-compat/router-model: quota-exhausted 402"),
            "{rendered}"
        );
        for leak in [
            ANTHROPIC_SECRET,
            OPENAI_SECRET,
            "credit balance",
            "Insufficient credits",
        ] {
            assert!(!rendered.contains(leak), "{leak} leaked: {rendered}");
        }
        assert!(!format!("{:?}", chain.candidate_health()).contains("credit balance"));
    }
}
