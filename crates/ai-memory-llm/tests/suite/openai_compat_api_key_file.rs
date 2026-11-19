//! `llm_api_key_file` on the `openai-compat` provider, against a wiremock
//! gateway that accepts one bearer token at a time, the way a gateway with a
//! rotating token does.
//!
//! The key is read from the file on first use and cached. A `401` re-reads
//! the file once, and the request is retried once only when the key in it
//! changed. Every test counts the requests the gateway received, because the
//! property that matters is the bound: a file that still holds a rejected key
//! must cost one request per call, never a loop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{
    ExtraHeaders, LlmError, LlmProvider, ProviderAuth, ProviderChoice, ProviderConfig,
    build_provider,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn reply() -> serde_json::Value {
    json!({
        "id": "id",
        "object": "chat.completion",
        "created": 0,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

/// How the mock gateway treats a request whose bearer it does not accept.
struct Rejection {
    status: u16,
    /// Write this key to this file while rejecting, so the file changes
    /// between the rejected attempt and the provider's re-read.
    rotate_file_to: Option<(PathBuf, &'static str)>,
}

impl Rejection {
    fn unauthorized() -> Self {
        Self {
            status: 401,
            rotate_file_to: None,
        }
    }
}

/// A gateway that answers 200 only to `Bearer <accepted>`. The returned
/// handle changes which key it accepts, as the gateway does when a token
/// expires.
async fn gateway(accepted: &str, rejection: Rejection) -> (MockServer, Arc<Mutex<String>>) {
    let server = MockServer::start().await;
    let current = Arc::new(Mutex::new(accepted.to_owned()));
    let accepts = current.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            if bearer(request) == format!("Bearer {}", accepts.lock().unwrap()) {
                return ResponseTemplate::new(200).set_body_json(reply());
            }
            if let Some((file, key)) = &rejection.rotate_file_to {
                write_key(file, key);
            }
            ResponseTemplate::new(rejection.status).set_body_string("invalid credentials")
        })
        .mount(&server)
        .await;
    (server, current)
}

fn write_key(file: &Path, key: &str) {
    std::fs::write(file, format!("{key}\n")).unwrap();
}

fn key_file(dir: &tempfile::TempDir, key: &str) -> PathBuf {
    let file = dir.path().join("api-key");
    write_key(&file, key);
    file
}

fn provider(server: &MockServer, key_file: PathBuf) -> Arc<dyn LlmProvider> {
    build_provider(ProviderConfig {
        provider: ProviderChoice::OpenAiCompat,
        model: "m".into(),
        auth: ProviderAuth::optional_api_key_file("LLM_API_KEY", key_file),
        base_url: Some(format!("{}/v1", server.uri())),
        compat_strict: false,
        compat_disable_thinking: false,
        compat_stream: None,
        request_timeout_secs: 5,
        reasoning_effort: None,
        extra_headers: ExtraHeaders::default(),
        request_metadata_headers: false,
    })
    .expect("provider builds")
}

fn bearer(request: &Request) -> String {
    header(request, "authorization")
}

fn header(request: &Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

async fn received(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("mock records")
}

/// The bearer of every request the gateway received, in order.
async fn bearers(server: &MockServer) -> Vec<String> {
    received(server).await.iter().map(bearer).collect()
}

async fn call(provider: &Arc<dyn LlmProvider>) -> Result<String, LlmError> {
    provider
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .map(|response| response.text)
}

fn assert_status(result: Result<String, LlmError>, expected: u16) {
    match result {
        Err(LlmError::Provider { status, .. }) if status == expected => {}
        other => panic!("expected the gateway's {expected}, got {other:?}"),
    }
}

#[tokio::test]
async fn the_key_in_the_file_is_the_bearer_and_is_cached_while_it_works() {
    let (server, _) = gateway("key-a", Rejection::unauthorized()).await;
    let dir = tempfile::tempdir().unwrap();
    let file = key_file(&dir, "key-a");
    let provider = provider(&server, file.clone());

    assert_eq!(call(&provider).await.unwrap(), "ok");
    // A healthy key is not re-read: a new key in the file waits for a 401.
    write_key(&file, "key-b");
    assert_eq!(call(&provider).await.unwrap(), "ok");

    assert_eq!(bearers(&server).await, ["Bearer key-a", "Bearer key-a"]);
}

#[tokio::test]
async fn a_key_rotated_in_the_file_is_used_after_a_401_without_a_restart() {
    let (server, accepted) = gateway("key-a", Rejection::unauthorized()).await;
    let dir = tempfile::tempdir().unwrap();
    let file = key_file(&dir, "key-a");
    let provider = provider(&server, file.clone());
    assert_eq!(call(&provider).await.unwrap(), "ok");

    // The old token expires and the login CLI has written a new one.
    *accepted.lock().unwrap() = "key-b".into();
    write_key(&file, "key-b");

    assert_eq!(call(&provider).await.unwrap(), "ok");
    assert_eq!(
        bearers(&server).await,
        ["Bearer key-a", "Bearer key-a", "Bearer key-b"],
        "one rejected attempt, then exactly one retry with the new key"
    );

    // The new key is now the cached one: no second attempt on the next call.
    assert_eq!(call(&provider).await.unwrap(), "ok");
    assert_eq!(bearers(&server).await.len(), 4);
}

#[tokio::test]
async fn a_file_still_holding_the_rejected_key_costs_one_request_and_no_retry() {
    let (server, _) = gateway("key-b", Rejection::unauthorized()).await;
    let dir = tempfile::tempdir().unwrap();
    let provider = provider(&server, key_file(&dir, "key-a"));

    assert_status(call(&provider).await, 401);
    assert_eq!(bearers(&server).await, ["Bearer key-a"]);

    // Every later call stays bounded the same way until the file changes.
    assert_status(call(&provider).await, 401);
    assert_eq!(bearers(&server).await, ["Bearer key-a", "Bearer key-a"]);
}

#[tokio::test]
async fn a_new_key_that_is_also_rejected_is_retried_once_and_not_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = key_file(&dir, "key-a");
    // Accepts neither key; rewrites the file to key-c on every rejection.
    let rejection = Rejection {
        status: 401,
        rotate_file_to: Some((file.clone(), "key-c")),
    };
    let (server, _) = gateway("key-z", rejection).await;
    let provider = provider(&server, file);

    assert_status(call(&provider).await, 401);
    assert_eq!(bearers(&server).await, ["Bearer key-a", "Bearer key-c"]);

    // key-c is cached and the file still holds it: one request, no retry.
    assert_status(call(&provider).await, 401);
    assert_eq!(
        bearers(&server).await,
        ["Bearer key-a", "Bearer key-c", "Bearer key-c"]
    );
}

/// Only `401` says the key is wrong. A forbidden model, an exhausted quota
/// or a gateway fault would fail the same way with any key, so none of them
/// re-reads the file or spends a second request.
#[tokio::test]
async fn only_a_401_rereads_the_file() {
    for status in [403, 429, 500] {
        let dir = tempfile::tempdir().unwrap();
        let file = key_file(&dir, "key-a");
        let rejection = Rejection {
            status,
            rotate_file_to: Some((file.clone(), "key-b")),
        };
        let (server, _) = gateway("key-b", rejection).await;
        let provider = provider(&server, file);

        assert_status(call(&provider).await, status);
        assert_eq!(bearers(&server).await, ["Bearer key-a"], "status {status}");
    }
}

/// A login CLI that truncates the file before writing it can leave it empty
/// at the moment of the re-read. The gateway's 401 is still the failure to
/// report, and nothing is retried.
#[tokio::test]
async fn a_file_unreadable_at_the_reread_reports_the_401_without_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let file = key_file(&dir, "key-a");
    let rejection = Rejection {
        status: 401,
        rotate_file_to: Some((file.clone(), "")),
    };
    let (server, _) = gateway("key-b", rejection).await;
    let provider = provider(&server, file);

    assert_status(call(&provider).await, 401);
    assert_eq!(bearers(&server).await, ["Bearer key-a"]);
}

#[tokio::test]
async fn a_missing_key_file_fails_the_call_naming_the_file_without_a_request() {
    let (server, _) = gateway("key-a", Rejection::unauthorized()).await;
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-written-yet");
    let provider = provider(&server, missing.clone());

    let err = call(&provider).await.unwrap_err();
    assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
    assert!(err.to_string().contains(&missing.display().to_string()));
    assert!(received(&server).await.is_empty());

    // The server keeps running; the file appearing later is enough.
    write_key(&missing, "key-a");
    assert_eq!(call(&provider).await.unwrap(), "ok");
}
