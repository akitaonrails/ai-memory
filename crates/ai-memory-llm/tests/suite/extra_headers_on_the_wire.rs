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
    DEFAULT_USER_AGENT, ExtraHeaders, LlmOperationId, LlmSubject, LlmWorkload,
    OPENROUTER_HTTP_REFERER, OPENROUTER_X_TITLE, ProviderAuth, ProviderChoice, ProviderConfig,
    build_provider,
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
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: headers,
        request_metadata_headers: false,
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

/// A base URL that is not OpenRouter must not receive OpenRouter's
/// attribution headers — a self-hosted Ollama/vLLM/LM Studio endpoint has no
/// leaderboard to attribute to.
#[tokio::test]
async fn a_non_openrouter_compat_endpoint_does_not_receive_openrouter_headers() {
    let request = captured_request(ExtraHeaders::default()).await;
    assert!(values(&request, "http-referer").is_empty());
    assert!(values(&request, "x-title").is_empty());
}

/// OpenRouter attributes usage on its app leaderboard by `HTTP-Referer` and
/// `X-Title`; without these on the wire, ai-memory's requests would arrive
/// unattributed.
#[tokio::test]
async fn openrouter_app_headers_reach_the_provider_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    // `is_openrouter_base` only checks for the substring `openrouter.ai`, so
    // a fake path segment on the local mock server exercises the same code
    // path a real `https://openrouter.ai/api/v1` base URL would.
    let base_url = format!("{}/openrouter.ai", server.uri());
    let provider = build_provider(ProviderConfig {
        provider: ProviderChoice::OpenAiCompat,
        model: "anthropic/claude-sonnet-4.6".into(),
        auth: ProviderAuth::optional_api_key_from_env("LLM_API_KEY", None),
        base_url: Some(base_url),
        compat_strict: false,
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
        request_metadata_headers: false,
    })
    .expect("provider builds");

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("mock responds 200");

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "expected exactly one upstream request");
    let request = &received[0];
    assert_eq!(
        values(request, "http-referer"),
        vec![OPENROUTER_HTTP_REFERER]
    );
    assert_eq!(values(request, "x-title"), vec![OPENROUTER_X_TITLE]);
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
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
        request_metadata_headers: false,
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
            .is_some_and(|v| !v.is_empty()),
        "session header lost when the base URL was overridden"
    );
    assert_eq!(values(request, "user-agent"), vec![DEFAULT_USER_AGENT]);
}

#[tokio::test]
async fn factory_opencode_chat_completions_sends_structured_response_format() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "id",
            "object": "chat.completion",
            "created": 0,
            "model": "mimo-v2.6-flash",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "{\"ok\":true}" },
                "finish_reason": "stop"
            }]
        })))
        .mount(&server)
        .await;

    let provider = build_provider(ProviderConfig {
        provider: ProviderChoice::OpenCode,
        model: "mimo-v2.6-flash".into(),
        auth: ProviderAuth::required_api_key_from_env(
            "OPENCODE_API_KEY",
            Some(SecretString::from("sk-test")),
        ),
        base_url: Some(server.uri()),
        compat_strict: false,
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
        request_metadata_headers: false,
    })
    .expect("provider builds");

    let value = provider
        .complete_structured_raw(
            ChatRequest::user_prompt("emit JSON"),
            json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"]
            }),
        )
        .await
        .expect("structured response parses");
    assert_eq!(value, json!({"ok": true}));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
}

/// An explicit `llm_base_url` configures the vendor-fixed providers too: it is
/// how an operator routes `anthropic` or `openai` through a gateway on
/// purpose. Dropping it sends the request, and the key, straight to the vendor
/// with nothing in the log.
#[tokio::test]
async fn an_operator_base_url_redirects_the_anthropic_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "claude-haiku-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "usage": {"input_tokens": 1, "output_tokens": 1},
        })))
        .mount(&server)
        .await;

    let provider = build_provider(ProviderConfig {
        provider: ProviderChoice::Anthropic,
        model: "claude-haiku-4-5".into(),
        auth: ProviderAuth::required_api_key_from_env(
            "ANTHROPIC_API_KEY",
            Some(SecretString::from("sk-ant-test")),
        ),
        base_url: Some(server.uri()),
        compat_strict: false,
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
        request_metadata_headers: false,
    })
    .expect("provider builds");

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("the override must reach the mock, not api.anthropic.com");

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "request did not reach the override");
}

/// Same contract for `openai`, whose default is api.openai.com.
#[tokio::test]
async fn an_operator_base_url_redirects_the_openai_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    let provider = build_provider(ProviderConfig {
        provider: ProviderChoice::OpenAi,
        model: "gpt-5.4-mini".into(),
        auth: ProviderAuth::required_api_key_from_env(
            "OPENAI_API_KEY",
            Some(SecretString::from("sk-test")),
        ),
        base_url: Some(server.uri()),
        compat_strict: false,
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
        request_metadata_headers: false,
    })
    .expect("provider builds");

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("the override must reach the mock, not api.openai.com");

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert_eq!(received.len(), 1, "request did not reach the override");
}

/// The load-bearing claim of layering operator headers over the provider's
/// own defaults: an explicit `AI_MEMORY_LLM_HEADERS` entry must beat the
/// per-operation id, and must arrive exactly once. Appending instead of
/// replacing would send both values, which is worse than either alone.
#[tokio::test]
async fn an_operator_session_header_beats_the_per_operation_default() {
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
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::parse([
            "x-opencode-session: homelab-01",
            "user-agent: ai-memory-fork/9",
        ])
        .expect("valid"),
        request_metadata_headers: false,
    })
    .expect("provider builds");

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("mock responds 200");

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    let request = &received[0];
    assert_eq!(values(request, "x-opencode-session"), vec!["homelab-01"]);
    assert_eq!(values(request, "user-agent"), vec!["ai-memory-fork/9"]);
}

const METADATA_HEADERS: [&str; 4] = [
    "x-ai-memory-workload",
    "x-ai-memory-workspace",
    "x-ai-memory-project",
    "x-ai-memory-user",
];

/// Drive one attributed completion through `build_provider` with the request
/// metadata turned on, and hand back every request the mock received. The
/// mock answers any path with an OpenAI-shaped body: providers that speak
/// another wire format may fail to parse it, which is irrelevant here, since
/// only the headers they sent are asserted.
async fn requests_with_metadata_on(
    provider: ProviderChoice,
    auth: ProviderAuth,
    model: &str,
) -> Vec<Request> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&server)
        .await;

    let llm = build_provider(ProviderConfig {
        provider,
        model: model.into(),
        auth,
        base_url: Some(server.uri()),
        compat_strict: false,
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 30,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
        request_metadata_headers: true,
    })
    .expect("provider builds");
    let operation_id = LlmOperationId::for_workload(LlmWorkload::Dream).with_subject(LlmSubject {
        workspace: Some("acme".into()),
        project: Some("billing-api".into()),
        user: Some("alice".into()),
    });
    let _ = llm
        .complete_with_operation_id(ChatRequest::user_prompt("hi"), operation_id)
        .await;

    let received = server
        .received_requests()
        .await
        .expect("mock recorded requests");
    assert!(!received.is_empty(), "the request reached the mock");
    received
}

/// The configured `openai-compat` provider is the one that sends the
/// metadata, each header exactly once.
#[tokio::test]
async fn the_compat_provider_sends_the_request_metadata_when_enabled() {
    let received = requests_with_metadata_on(
        ProviderChoice::OpenAiCompat,
        ProviderAuth::optional_api_key_from_env("LLM_API_KEY", None),
        "mistral-nemo",
    )
    .await;
    for request in &received {
        for (name, expected) in
            METADATA_HEADERS
                .into_iter()
                .zip(["dream", "acme", "billing-api", "alice"])
        {
            assert_eq!(values(request, name), vec![expected], "{name}");
        }
    }
}

/// The official providers and OpenCode never send it, even with the setting
/// on: it is an `openai-compat` gateway contract, not a vendor one.
#[tokio::test]
async fn other_providers_never_send_the_request_metadata() {
    let key = || Some(SecretString::from("sk-test"));
    for (provider, auth, model) in [
        (
            ProviderChoice::OpenAi,
            ProviderAuth::required_api_key_from_env("OPENAI_API_KEY", key()),
            "gpt-5.4-mini",
        ),
        (
            ProviderChoice::Anthropic,
            ProviderAuth::required_api_key_from_env("ANTHROPIC_API_KEY", key()),
            "claude-haiku-4-5",
        ),
        (
            ProviderChoice::Gemini,
            ProviderAuth::required_api_key_from_env("GEMINI_API_KEY", key()),
            "gemini-3.5-flash",
        ),
        (
            ProviderChoice::OpenCode,
            ProviderAuth::required_api_key_from_env("OPENCODE_API_KEY", key()),
            "mistral-nemo",
        ),
    ] {
        for request in requests_with_metadata_on(provider, auth, model).await {
            for name in METADATA_HEADERS {
                assert!(
                    values(&request, name).is_empty(),
                    "{provider:?} sent {name}"
                );
            }
        }
    }
}
