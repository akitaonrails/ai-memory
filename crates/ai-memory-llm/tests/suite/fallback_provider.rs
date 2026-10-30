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

fn chain(models: &[&str]) -> FallbackLlmProvider {
    FallbackLlmProvider::new(
        models
            .iter()
            .map(|&model| {
                Candidate::new(
                    "openai-compat",
                    model,
                    std::sync::Arc::new(compat_provider("http://localhost:9/v1", model, 5)),
                )
            })
            .collect(),
    )
}

/// The chain reports its primary's name and model, so those cannot tell two
/// chains apart. `cache_identity` must: a different fallback model, a
/// reordered chain and a dropped candidate each change it, while the
/// primary's public name/model stay exactly as before.
#[test]
fn cache_identity_changes_with_the_fallback_set_or_order_not_the_primary() {
    let base = chain(&["primary", "backup"]);
    let other_backup = chain(&["primary", "other-backup"]);
    let longer = chain(&["primary", "backup", "last"]);
    let alone = chain(&["primary"]);
    let reordered = chain(&["backup", "primary"]);

    for same_primary in [&other_backup, &longer, &alone] {
        assert_eq!(same_primary.name(), base.name());
        assert_eq!(same_primary.model(), base.model());
        assert_ne!(same_primary.cache_identity(), base.cache_identity());
    }
    assert_ne!(reordered.cache_identity(), base.cache_identity());
    assert_ne!(other_backup.cache_identity(), longer.cache_identity());
    // Deterministic: the same configuration always yields the same value.
    assert_eq!(
        base.cache_identity(),
        chain(&["primary", "backup"]).cache_identity()
    );
}

/// A plain provider keeps `name:model`; the health and admission wrappers
/// the server always installs forward the inner identity unchanged, so the
/// chain's identity survives them.
#[test]
fn cache_identity_survives_the_health_and_admission_wrappers() {
    use std::sync::Arc;

    use ai_memory_llm::{AdmittedLlmProvider, ProviderHealth};

    let plain = Arc::new(
        OpenAiCompatProvider::new("http://localhost:9/v1", None, "m").expect("provider builds"),
    );
    assert!(plain.cache_identity().starts_with("openai-compat:m|"));

    let inner: Arc<dyn LlmProvider> = Arc::new(chain(&["primary", "backup"]));
    let expected = inner.cache_identity();
    let health = ProviderHealth::default();
    let wrapped = health.wrap_llm_provider(Arc::clone(&inner), "openai-compat", "primary", None);
    assert_eq!(wrapped.cache_identity(), expected);
    assert_eq!(AdmittedLlmProvider::new(wrapped).cache_identity(), expected);
}

/// Backend settings that change the answer are part of the compat identity;
/// credentials, query strings and fragments never are.
#[test]
fn compat_cache_identity_tracks_backend_settings_and_never_leaks_secrets() {
    use secrecy::SecretString;

    let build = |url: &str, strict: bool, disable_thinking: bool| {
        OpenAiCompatProvider::new(url, Some(SecretString::from("sk-super-secret")), "m")
            .expect("provider builds")
            .with_strict(strict)
            .with_disable_thinking(disable_thinking)
    };
    let base = build("http://localhost:9/v1", false, false).cache_identity();
    assert_ne!(
        base,
        build("http://localhost:9/v1", true, false).cache_identity()
    );
    assert_ne!(
        base,
        build("http://localhost:9/v1", false, true).cache_identity()
    );
    assert_ne!(
        base,
        build("http://other-host:9/v1", false, false).cache_identity()
    );

    let credentialed = build(
        "http://user:hunter2@localhost:9/v1?api_key=leak#frag",
        false,
        false,
    )
    .cache_identity();
    for secret in [
        "sk-super-secret",
        "hunter2",
        "user",
        "api_key",
        "leak",
        "frag",
    ] {
        assert!(
            !credentialed.contains(secret),
            "{secret} leaked: {credentialed}"
        );
    }
    // Redaction keeps the identity of the same backend stable.
    assert_eq!(credentialed, base);
}

/// Official OpenAI, Anthropic, Gemini and OpenCode answer from whatever
/// endpoint they are pointed at, so the endpoint is part of their identity
/// (a gateway behind the same model name is a different backend), and a
/// credential in the URL never is.
#[test]
fn native_providers_cache_identity_includes_the_redacted_endpoint() {
    use ai_memory_llm::{
        AnthropicProvider, GeminiProvider, OPENCODE_GO_BASE_URL, OpenAiProvider, OpenCodeProvider,
    };
    use secrecy::SecretString;

    let key = || SecretString::from("sk-super-secret");
    let cred_url = "https://user:hunter2@gateway.example/v1?token=leak#frag";
    let identities = [
        (
            OpenAiProvider::new(key(), "m").unwrap().cache_identity(),
            OpenAiProvider::new(key(), "m")
                .unwrap()
                .with_base_url(cred_url)
                .cache_identity(),
        ),
        (
            AnthropicProvider::new(key(), "m").unwrap().cache_identity(),
            AnthropicProvider::new(key(), "m")
                .unwrap()
                .with_base_url(cred_url)
                .cache_identity(),
        ),
        (
            GeminiProvider::new(key(), "m").unwrap().cache_identity(),
            GeminiProvider::new(key(), "m")
                .unwrap()
                .with_base_url(cred_url)
                .cache_identity(),
        ),
        (
            OpenCodeProvider::new(key(), "m").unwrap().cache_identity(),
            OpenCodeProvider::new(key(), "m")
                .unwrap()
                .with_base_url(cred_url)
                .cache_identity(),
        ),
    ];
    for (default_endpoint, redirected) in identities {
        assert_ne!(default_endpoint, redirected);
        assert!(redirected.contains("gateway.example"), "{redirected}");
        for secret in ["sk-super-secret", "hunter2", "token", "leak", "frag"] {
            assert!(
                !redirected.contains(secret),
                "{secret} leaked: {redirected}"
            );
        }
    }

    // Same model name, different OpenCode catalogue: Go routes `minimax-m3`
    // over Anthropic Messages, Zen over Chat Completions.
    let go = OpenCodeProvider::new(key(), "minimax-m3").unwrap();
    let zen = OpenCodeProvider::new(key(), "minimax-m3")
        .unwrap()
        .with_base_url("https://opencode.ai/zen/v1");
    assert_eq!(go.model(), zen.model());
    assert_ne!(go.cache_identity(), zen.cache_identity());
    assert!(
        go.cache_identity()
            .contains(OPENCODE_GO_BASE_URL.trim_end_matches('/'))
    );
}
