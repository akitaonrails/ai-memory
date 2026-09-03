//! Integration tests asserting operator headers and the default user agent
//! actually reach the provider request, driving `build_provider` against a
//! real in-process HTTP mock (wiremock).
//!
//! Why not unit tests: `ExtraHeaders`' unit tests cover parsing, and the
//! factory's cover which defaults get layered. Neither proves the headers
//! survive `reqwest`'s request builder — and that is the whole point of the
//! feature. `reqwest::RequestBuilder::header` *appends* while `headers`
//! *replaces*, so "the value is on the struct" and "the right single value
//! is on the wire" are genuinely different claims.

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{
    DEFAULT_USER_AGENT, ExtraHeaders, ProviderAuth, ProviderChoice, ProviderConfig, build_provider,
};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn ok_body() -> serde_json::Value {
    json!({
        "id": "id",
        "object": "chat.completion",
        "created": 0,
        "model": "mistral-nemo",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

/// Drive one completion through `build_provider` against a fresh mock and
/// hand back the single request the mock received. Going through the factory
/// (rather than constructing a provider directly) is deliberate: the default
/// user agent is layered there, so this covers both halves at once.
async fn captured_request(headers: ExtraHeaders) -> Request {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    let provider = build_provider(ProviderConfig {
        provider: ProviderChoice::OpenAiCompat,
        model: "mistral-nemo".into(),
        auth: ProviderAuth::optional_api_key_from_env("LLM_API_KEY", None),
        base_url: Some(server.uri()),
        compat_strict: false,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: headers,
    })
    .expect("provider builds");

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("mock responds 200");

    let mut received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "expected exactly one upstream request");
    received.remove(0)
}

/// Every value sent for `name`. A `Vec` rather than a lookup so a duplicated
/// header fails the assertion instead of hiding behind the first value.
fn values(request: &Request, name: &str) -> Vec<String> {
    request
        .headers
        .get_all(name)
        .iter()
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .collect()
}

#[tokio::test]
async fn operator_headers_reach_the_provider_request() {
    let headers = ExtraHeaders::parse(["x-opencode-session: ses-1"]).expect("valid");
    let request = captured_request(headers).await;
    assert_eq!(values(&request, "x-opencode-session"), vec!["ses-1"]);
}

/// `reqwest` sends no user agent unless configured; `build_provider` layers
/// one in, and it must arrive as a single value.
#[tokio::test]
async fn the_default_user_agent_reaches_the_provider_request() {
    let request = captured_request(ExtraHeaders::default()).await;
    assert_eq!(values(&request, "user-agent"), vec![DEFAULT_USER_AGENT]);
}

/// Replace, not append: an operator user agent must not arrive alongside the
/// default. Two values would be worse than either one alone.
#[tokio::test]
async fn an_operator_user_agent_replaces_the_default_on_the_wire() {
    let headers = ExtraHeaders::parse(["user-agent: ai-memory-fork/9"]).expect("valid");
    let request = captured_request(headers).await;
    assert_eq!(values(&request, "user-agent"), vec!["ai-memory-fork/9"]);
}

/// The provider's own auth and wire-format headers must survive alongside
/// the operator's — `headers` replaces per name, so a regression here would
/// silently strip authentication rather than fail loudly.
#[tokio::test]
async fn provider_owned_headers_survive_alongside_operator_headers() {
    let headers = ExtraHeaders::parse(["x-tool: ai-memory"]).expect("valid");
    let request = captured_request(headers).await;
    assert_eq!(values(&request, "x-tool"), vec!["ai-memory"]);
    assert_eq!(values(&request, "content-type"), vec!["application/json"]);
    assert_eq!(
        request.headers.get_all("authorization").iter().count(),
        1,
        "provider auth must be sent exactly once"
    );
}

/// The `opencode` provider defaults to Go, so a base URL an operator sets
/// must actually redirect it — this is the regression the silently-dropped
/// `ProviderConfig::base_url` caused. Driving it against a mock is the only
/// way to see *where* the request went.
#[tokio::test]
async fn an_operator_base_url_redirects_the_opencode_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    let provider = build_provider(ProviderConfig {
        provider: ProviderChoice::OpenCode,
        model: "mistral-nemo".into(),
        auth: ProviderAuth::required_api_key_from_env(
            "OPENCODE_API_KEY",
            Some(SecretString::from("sk-test")),
        ),
        base_url: Some(server.uri()),
        compat_strict: false,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
    })
    .expect("provider builds");

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("the override must reach the mock, not opencode.ai");

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "request did not reach the override");

    // Redirecting must not cost the caller its identity: Zen and Go
    // correlate requests by the same header.
    let request = &received[0];
    assert!(
        values(request, "x-opencode-session")
            .first()
            .is_some_and(|v| v.starts_with("ai-memory-")),
        "session header lost when the base URL was overridden"
    );
    assert_eq!(values(request, "user-agent"), vec![DEFAULT_USER_AGENT]);
}
