//! The Vertex AI path of the Gemini provider: Application Default Credentials
//! become a cached bearer token, and the request goes to the project/location
//! URL instead of the AI Studio one.

use std::path::{Path, PathBuf};

use ai_memory_llm::{
    ChatRequest, GeminiProvider, LlmError, LlmProvider, VertexAuth, vertex_base_url,
};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const MODEL: &str = "gemini-3.5-flash";
const TEST_KEY_PEM: &str = include_str!("../support/vertex_test_service_account_key.pem");

fn generate_path(project: &str, location: &str) -> String {
    format!(
        "/v1/projects/{project}/locations/{location}/publishers/google/models/{MODEL}:generateContent"
    )
}

fn reply(text: &str) -> Value {
    json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1},
        "modelVersion": MODEL,
    })
}

fn token(access_token: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": access_token,
        "expires_in": 3599,
        "token_type": "Bearer",
    }))
}

fn write_credentials(dir: &Path, credentials: &Value) -> PathBuf {
    let file = dir.join("adc.json");
    std::fs::write(&file, credentials.to_string()).unwrap();
    file
}

fn authorized_user(server: &MockServer, quota_project: Option<&str>) -> Value {
    let mut file = json!({
        "type": "authorized_user",
        "client_id": "test-client.apps.googleusercontent.com",
        "client_secret": "test-client-secret",
        "refresh_token": "test-refresh-token",
        "token_uri": format!("{}/token", server.uri()),
    });
    if let Some(project) = quota_project {
        file["quota_project_id"] = json!(project);
    }
    file
}

fn provider(server: &MockServer, file: PathBuf, project: Option<&str>) -> GeminiProvider {
    GeminiProvider::new_vertex(
        VertexAuth {
            credentials_file: file,
            project: project.map(str::to_owned),
            location: "global".into(),
        },
        MODEL,
    )
    .unwrap()
    .with_base_url(server.uri())
}

fn form_field<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    std::str::from_utf8(&request.body)
        .ok()?
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

#[tokio::test]
async fn a_gcloud_login_is_refreshed_once_and_reused_as_a_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(token("vertex-access-1"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(generate_path("quota-project", "global")))
        .and(header("authorization", "Bearer vertex-access-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply("ok")))
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = write_credentials(dir.path(), &authorized_user(&server, Some("quota-project")));
    let provider = provider(&server, file, None);

    for _ in 0..2 {
        let response = provider
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect("vertex completion");
        assert_eq!(response.text, "ok");
    }

    assert_eq!(provider.name(), "vertex");
    let requests = server.received_requests().await.unwrap();
    let refresh = requests
        .iter()
        .find(|r| r.url.path() == "/token")
        .expect("token request");
    assert_eq!(
        form_field(refresh, "refresh_token"),
        Some("test-refresh-token")
    );
    assert_eq!(
        form_field(refresh, "client_secret"),
        Some("test-client-secret")
    );
    let generate = requests
        .iter()
        .find(|r| r.url.path() != "/token")
        .expect("generate request");
    assert!(
        generate.headers.get("x-goog-api-key").is_none(),
        "vertex must not send an AI Studio key header"
    );
}

#[tokio::test]
async fn an_explicit_project_wins_over_the_one_in_the_credentials_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(token("vertex-access-1"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(generate_path("billing-project", "global")))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply("ok")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = write_credentials(dir.path(), &authorized_user(&server, Some("quota-project")));

    provider(&server, file, Some("billing-project"))
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("vertex completion");
}

#[tokio::test]
async fn no_project_anywhere_is_a_configuration_error_naming_the_variable() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = write_credentials(dir.path(), &authorized_user(&server, None));

    let err = provider(&server, file, None)
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect_err("no project");

    assert!(
        matches!(err, LlmError::NotConfigured(ref msg) if msg.contains("GOOGLE_CLOUD_PROJECT")),
        "{err}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_credentials_file_points_at_gcloud_login() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();

    let err = provider(&server, dir.path().join("absent.json"), Some("p"))
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect_err("missing file");

    assert!(
        matches!(err, LlmError::Auth(ref msg) if msg.contains("gcloud auth application-default login")),
        "{err}"
    );
}

#[tokio::test]
async fn a_rejected_token_is_dropped_so_the_next_call_refreshes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(token("vertex-access-1"))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(generate_path("p", "global")))
        .respond_with(ResponseTemplate::new(401).set_body_string("token revoked"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(generate_path("p", "global")))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply("ok")))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = write_credentials(dir.path(), &authorized_user(&server, None));
    let provider = provider(&server, file, Some("p"));

    assert!(
        provider
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .is_err()
    );
    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("second call refreshes and succeeds");
}

#[tokio::test]
async fn a_refused_grant_is_an_auth_error_and_a_throttled_one_stays_transient() {
    for (status, transient) in [(400, false), (503, true)] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(status).set_body_string("{\"error\":\"x\"}"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let file = write_credentials(dir.path(), &authorized_user(&server, None));

        let err = provider(&server, file, Some("p"))
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("token exchange fails");

        assert_eq!(err.is_transient(), transient, "{status}: {err}");
        if !transient {
            assert!(matches!(err, LlmError::Auth(_)), "{err}");
        }
    }
}

#[tokio::test]
async fn a_service_account_key_signs_an_rs256_assertion_for_the_cloud_platform_scope() {
    let server = MockServer::start().await;
    let token_uri = format!("{}/token", server.uri());
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("jwt-bearer"))
        .respond_with(token("sa-access-1"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(generate_path("sa-project", "global")))
        .and(header("authorization", "Bearer sa-access-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply("ok")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let file = write_credentials(
        dir.path(),
        &json!({
            "type": "service_account",
            "project_id": "sa-project",
            "client_email": "ai-memory@sa-project.iam.gserviceaccount.com",
            "private_key": TEST_KEY_PEM,
            "token_uri": token_uri,
        }),
    );

    provider(&server, file, None)
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect("vertex completion");

    let requests = server.received_requests().await.unwrap();
    let exchange = requests
        .iter()
        .find(|r| r.url.path() == "/token")
        .expect("token request");
    let assertion = form_field(exchange, "assertion").expect("assertion field");
    let (signing_input, signature) = assertion.rsplit_once('.').expect("three JWT parts");
    let (header_part, claims_part) = signing_input.split_once('.').expect("header.claims");

    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header_part).unwrap()).unwrap();
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims_part).unwrap()).unwrap();
    assert_eq!(header["alg"], "RS256");
    assert_eq!(
        claims["iss"],
        "ai-memory@sa-project.iam.gserviceaccount.com"
    );
    assert_eq!(claims["aud"], token_uri);
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        3600
    );

    let der: String = TEST_KEY_PEM
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let key_pair = ring::signature::RsaKeyPair::from_pkcs8(&STANDARD.decode(der).unwrap()).unwrap();
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        key_pair.public().as_ref(),
    )
    .verify(
        signing_input.as_bytes(),
        &URL_SAFE_NO_PAD.decode(signature).unwrap(),
    )
    .expect("assertion is signed by the service-account key");
}

#[test]
fn global_has_no_regional_prefix_and_regions_do() {
    assert_eq!(
        vertex_base_url("global"),
        "https://aiplatform.googleapis.com"
    );
    assert_eq!(
        vertex_base_url("us-central1"),
        "https://us-central1-aiplatform.googleapis.com"
    );
}

/// The location is spliced into the host name and the project into the path;
/// a value that could move the bearer token to another host is refused before
/// any request, while real ids still build.
#[test]
fn a_location_or_project_that_could_redirect_the_token_is_refused() {
    let build = |project: Option<&str>, location: &str| {
        GeminiProvider::new_vertex(
            VertexAuth {
                credentials_file: PathBuf::from("/unused/adc.json"),
                project: project.map(str::to_owned),
                location: location.into(),
            },
            MODEL,
        )
    };
    for location in [
        "evil.example/x#",
        "us-central1.evil.example",
        "",
        "US-CENTRAL1",
    ] {
        let err = build(Some("p"), location).err().expect("location refused");
        assert!(
            matches!(err, LlmError::NotConfigured(ref msg) if msg.contains("GOOGLE_CLOUD_LOCATION")),
            "{location:?}: {err}"
        );
    }
    for project in ["p/../../other", "p?x=1", "p#frag", "evil.example/p"] {
        let err = build(Some(project), "global")
            .err()
            .expect("project refused");
        assert!(
            matches!(err, LlmError::NotConfigured(ref msg) if msg.contains("GOOGLE_CLOUD_PROJECT")),
            "{project:?}: {err}"
        );
    }
    for (project, location) in [
        (Some("my-project-123"), "global"),
        (Some("123456789012"), "us-central1"),
        (Some("example.com:legacy-project"), "europe-west4"),
        (None, "global"),
    ] {
        assert!(build(project, location).is_ok(), "{project:?} {location}");
    }
}

#[tokio::test]
async fn a_project_from_the_credentials_file_is_checked_like_an_explicit_one() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = write_credentials(dir.path(), &authorized_user(&server, Some("p/../other")));

    let err = provider(&server, file, None)
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .expect_err("malformed project");

    assert!(
        matches!(err, LlmError::NotConfigured(ref msg) if msg.contains("GOOGLE_CLOUD_PROJECT")),
        "{err}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}
