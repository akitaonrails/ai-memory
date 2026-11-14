//! What a provider request carries when its endpoint answers with a
//! cross-origin redirect. Two loopback mocks on different ports are two
//! origins: `first` answers 307 pointing at `second`, and the assertions
//! read what `second` received.
//!
//! reqwest strips `Authorization` (and cookies) on a cross-host redirect, but
//! nothing else, so a credential in a header of its own would reach whatever
//! host the redirect names. Provider clients therefore follow a redirect only
//! inside one origin.

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{
    Embedder, ExtraHeaders, GeminiProvider, GoogleEmbedder, LlmProvider, ProviderAuth,
    ProviderChoice, ProviderConfig, VertexAuth, build_provider,
};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A mock that answers every request with `status` and a `Location`
/// pointing at `target`.
async fn redirecting_to(target: &str, status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status).insert_header("location", target))
        .mount(&server)
        .await;
    server
}

/// A mock that records everything and answers 500, so a provider that does
/// reach it fails fast instead of retrying a parse.
async fn recording() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    server
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("mock records")
}

fn has_header(request: &Request, name: &str) -> bool {
    request.headers.get(name).is_some()
}

fn config(provider: ProviderChoice, auth: ProviderAuth, base_url: String) -> ProviderConfig {
    ProviderConfig {
        provider,
        model: "m".into(),
        auth,
        base_url: Some(base_url),
        compat_strict: false,
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 5,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::parse(["x-gateway-key: gateway-secret"]).expect("valid"),
        request_metadata_headers: false,
    }
}

/// Gemini authenticates with `x-goog-api-key`, which reqwest's redirect
/// guard does not know about.
#[tokio::test]
async fn gemini_api_key_does_not_follow_a_cross_origin_redirect() {
    let second = recording().await;
    let first = redirecting_to(&format!("{}/elsewhere", second.uri()), 307).await;
    let provider = build_provider(config(
        ProviderChoice::Gemini,
        ProviderAuth::required_api_key_from_env(
            "GEMINI_API_KEY",
            Some(SecretString::from("gemini-secret".to_string())),
        ),
        first.uri(),
    ))
    .expect("provider builds");

    let _ = provider.complete(ChatRequest::user_prompt("hi")).await;

    let leaked: Vec<_> = received(&second)
        .await
        .into_iter()
        .filter(|r| has_header(r, "x-goog-api-key") || has_header(r, "x-gateway-key"))
        .collect();
    assert!(
        leaked.is_empty(),
        "the redirect target received the API key or an operator header ({} request(s))",
        leaked.len()
    );
}

/// Anthropic sends `x-api-key`, and honours `base_url` for a gateway.
#[tokio::test]
async fn anthropic_api_key_does_not_follow_a_cross_origin_redirect() {
    let second = recording().await;
    let first = redirecting_to(&format!("{}/elsewhere", second.uri()), 307).await;
    let provider = build_provider(config(
        ProviderChoice::Anthropic,
        ProviderAuth::required_api_key_from_env(
            "ANTHROPIC_API_KEY",
            Some(SecretString::from("anthropic-secret".to_string())),
        ),
        first.uri(),
    ))
    .expect("provider builds");

    let _ = provider.complete(ChatRequest::user_prompt("hi")).await;

    let leaked: Vec<_> = received(&second)
        .await
        .into_iter()
        .filter(|r| has_header(r, "x-api-key") || has_header(r, "x-gateway-key"))
        .collect();
    assert!(
        leaked.is_empty(),
        "the redirect target received the API key or an operator header ({} request(s))",
        leaked.len()
    );
}

/// The gateway case: `openai-compat` with a key and an operator header.
/// `Authorization` is the control — reqwest already strips it — and the
/// operator header is the one that survives.
#[tokio::test]
async fn operator_headers_do_not_follow_a_cross_origin_redirect() {
    let second = recording().await;
    let first = redirecting_to(&format!("{}/elsewhere", second.uri()), 307).await;
    let provider = build_provider(config(
        ProviderChoice::OpenAiCompat,
        ProviderAuth::optional_api_key_from_env(
            "LLM_API_KEY",
            Some(SecretString::from("bearer-secret".to_string())),
        ),
        first.uri(),
    ))
    .expect("provider builds");

    let _ = provider.complete(ChatRequest::user_prompt("hi")).await;

    let at_second = received(&second).await;
    assert!(
        at_second.iter().all(|r| !has_header(r, "authorization")),
        "control: reqwest strips Authorization on a cross-host redirect"
    );
    assert!(
        at_second.iter().all(|r| !has_header(r, "x-gateway-key")),
        "the redirect target received the operator header ({} request(s))",
        at_second.len()
    );
}

/// Control: a redirect inside the same origin keeps working, so a fix that
/// only refuses origin changes does not break a gateway that rewrites paths.
#[tokio::test]
async fn a_same_origin_redirect_still_reaches_the_provider() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::path("/landing"))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/landing", server.uri())),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    let provider = build_provider(config(
        ProviderChoice::Gemini,
        ProviderAuth::required_api_key_from_env(
            "GEMINI_API_KEY",
            Some(SecretString::from("gemini-secret".to_string())),
        ),
        server.uri(),
    ))
    .expect("provider builds");

    let _ = provider.complete(ChatRequest::user_prompt("hi")).await;

    let landed: Vec<_> = received(&server)
        .await
        .into_iter()
        .filter(|r| r.url.path() == "/landing")
        .collect();
    assert!(
        !landed.is_empty(),
        "the same-origin redirect was not followed"
    );
    assert!(
        landed.iter().all(|r| has_header(r, "x-goog-api-key")),
        "a same-origin redirect keeps the key"
    );
}

/// The Google embedder sends the same `x-goog-api-key` from its own client.
#[tokio::test]
async fn google_embedder_key_does_not_follow_a_cross_origin_redirect() {
    let second = recording().await;
    let first = redirecting_to(&format!("{}/elsewhere", second.uri()), 307).await;
    let embedder = GoogleEmbedder::new(
        SecretString::from("google-secret".to_string()),
        "gemini-embedding-001",
        768,
    )
    .expect("embedder builds")
    .with_base_url(first.uri());

    let _ = embedder.embed("hi").await;

    let leaked: Vec<_> = received(&second)
        .await
        .into_iter()
        .filter(|r| has_header(r, "x-goog-api-key"))
        .collect();
    assert!(
        leaked.is_empty(),
        "the redirect target received the embedder key ({} request(s))",
        leaked.len()
    );
}

/// The refusal names the origin it would have gone to, so an operator can
/// point the endpoint at its final URL.
#[tokio::test]
async fn a_refused_redirect_says_where_it_pointed() {
    let second = recording().await;
    let first = redirecting_to(&format!("{}/elsewhere", second.uri()), 308).await;
    let provider = build_provider(config(
        ProviderChoice::Gemini,
        ProviderAuth::required_api_key_from_env(
            "GEMINI_API_KEY",
            Some(SecretString::from("gemini-secret".to_string())),
        ),
        first.uri(),
    ))
    .expect("provider builds");

    let err = provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect_err("a cross-origin redirect fails the request");

    assert!(
        received(&second).await.is_empty(),
        "nothing reaches the other origin"
    );
    let mut message = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    assert!(
        message.contains("another origin") && message.contains(&second.uri()),
        "the error explains the refusal: {message}"
    );
}

/// Writes an Application Default Credentials file whose token endpoint is
/// `token_uri`, so a Vertex provider built from it fetches its bearer there.
fn adc_credentials(dir: &std::path::Path, token_uri: &str) -> std::path::PathBuf {
    let file = dir.join("adc.json");
    std::fs::write(
        &file,
        json!({
            "type": "authorized_user",
            "client_id": "test-client.apps.googleusercontent.com",
            "client_secret": "test-client-secret",
            "refresh_token": "test-refresh-token",
            "token_uri": token_uri,
        })
        .to_string(),
    )
    .expect("write adc");
    file
}

fn vertex_provider(
    credentials_file: std::path::PathBuf,
    content_base_url: String,
) -> GeminiProvider {
    GeminiProvider::new_vertex(
        VertexAuth {
            credentials_file,
            project: Some("quota-project".into()),
            location: "global".into(),
        },
        "gemini-3.5-flash",
    )
    .expect("vertex provider builds")
    .with_base_url(content_base_url)
}

/// Vertex AI exchanges Application Default Credentials for a bearer by POSTing
/// the refresh secret (or a signed assertion) to the credentials' `token_uri`.
/// reqwest replays a POST body across a 307, so an attacker who could answer
/// the token endpoint with a cross-origin redirect would otherwise receive the
/// secret. The token client refuses the redirect, so it never leaves the origin.
#[tokio::test]
async fn vertex_token_exchange_does_not_follow_a_cross_origin_redirect() {
    let second = recording().await;
    let first = redirecting_to(&second.uri(), 307).await;
    // Never reached: the token exchange is refused before any content call.
    let content = recording().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let file = adc_credentials(dir.path(), &format!("{}/token", first.uri()));
    let provider = vertex_provider(file, content.uri());

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect_err("a cross-origin redirect off the token endpoint fails the request");

    let leaked = received(&second).await;
    assert!(
        leaked.is_empty(),
        "the refresh secret must not follow a cross-origin redirect off the Vertex token endpoint"
    );
}

/// The Vertex content call carries the bearer as `Authorization`. reqwest
/// strips `Authorization` across hosts on its own (the control), but the
/// content client additionally refuses the cross-origin redirect, so nothing —
/// bearer or otherwise — reaches the host the redirect names.
#[tokio::test]
async fn vertex_bearer_does_not_follow_a_cross_origin_redirect() {
    let second = recording().await;
    let first = redirecting_to(&second.uri(), 307).await;

    // A same-origin token endpoint so the provider actually obtains a bearer
    // before it makes the content call that gets redirected.
    let tokens = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "vertex-access-1",
            "expires_in": 3599,
            "token_type": "Bearer",
        })))
        .mount(&tokens)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let file = adc_credentials(dir.path(), &format!("{}/token", tokens.uri()));
    let provider = vertex_provider(file, first.uri());

    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect_err("a cross-origin redirect off the content endpoint fails the request");

    let leaked = received(&second).await;
    assert!(
        leaked.is_empty(),
        "the Vertex bearer must not follow a cross-origin redirect off the content endpoint"
    );
}
