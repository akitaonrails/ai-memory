//! Integration tests for `OpenAiCompatEmbedder` against an in-process
//! HTTP mock (wiremock).
//!
//! The load-bearing behaviours: (1) keyless requests carry NO
//! `Authorization` header — local engines such as Ollama and LM Studio
//! reject or ignore malformed auth, and sending a fabricated bearer
//! token would leak the assumption that a key always exists; (2) when
//! a gateway key IS configured it is sent as a bearer token; (3) the
//! embedder identifies as provider `openai-compat`, so stored
//! `{provider, model, dim}` triples are a distinct family from plain
//! `openai`; (4) the factory refuses to build without a base URL.

use ai_memory_llm::{
    Embedder, EmbedderChoice, EmbedderConfig, LlmError, OpenAiCompatEmbedder, build_embedder,
    default_embedding_dim, try_default_embedding_dim,
};
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn embedding_body(dim: usize) -> serde_json::Value {
    json!({
        "object": "list",
        "data": [{ "object": "embedding", "index": 0, "embedding": vec![0.5_f32; dim] }],
        "model": "nomic-embed-text",
        "usage": { "prompt_tokens": 1, "total_tokens": 1 },
    })
}

#[tokio::test]
async fn keyless_embed_sends_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(move |req: &Request| {
            assert!(
                req.headers.get("authorization").is_none(),
                "keyless compat embedder must not send an Authorization header"
            );
            ResponseTemplate::new(200).set_body_json(embedding_body(8))
        })
        .expect(1)
        .mount(&server)
        .await;

    let e = OpenAiCompatEmbedder::new(format!("{}/v1", server.uri()), None, "nomic-embed-text", 8)
        .expect("embedder builds");
    assert_eq!(e.provider(), "openai-compat");
    assert_eq!(e.provider(), EmbedderChoice::OpenAiCompat.name());

    let v = e.embed("hello world").await.expect("embed succeeds");
    assert_eq!(v.len(), 8);
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
}

#[tokio::test]
async fn configured_key_is_sent_as_bearer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("authorization", "Bearer sk-gateway"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_body(8)))
        .expect(1)
        .mount(&server)
        .await;

    let e = OpenAiCompatEmbedder::new(
        format!("{}/v1", server.uri()),
        Some(SecretString::from("sk-gateway")),
        "nomic-embed-text",
        8,
    )
    .expect("embedder builds");
    e.embed("hello").await.expect("embed succeeds");
}

#[tokio::test]
async fn factory_builds_compat_embedder_and_requires_base_url() {
    let ok = build_embedder(EmbedderConfig {
        provider: EmbedderChoice::OpenAiCompat,
        model: "nomic-embed-text".into(),
        dim: 768,
        api_key: SecretString::from(String::new()),
        base_url: Some("http://localhost:11434/v1".into()),
        models_dir: None,
        defaulted: false,
    })
    .expect("factory builds compat embedder");
    assert_eq!(ok.provider(), "openai-compat");
    assert_eq!(ok.dim(), 768);

    let err = match build_embedder(EmbedderConfig {
        provider: EmbedderChoice::OpenAiCompat,
        model: "nomic-embed-text".into(),
        dim: 768,
        api_key: SecretString::from(String::new()),
        base_url: None,
        models_dir: None,
        defaulted: false,
    }) {
        Ok(_) => panic!("compat embedder must not build without a base URL"),
        Err(err) => err,
    };
    assert!(
        matches!(err, LlmError::NotConfigured(ref msg) if msg.contains("AI_MEMORY_EMBEDDING_BASE_URL")),
        "expected NotConfigured for missing base URL, got {err:?}"
    );

    let zero_dim = build_embedder(EmbedderConfig {
        provider: EmbedderChoice::OpenAiCompat,
        model: "nomic-embed-text".into(),
        dim: 0,
        api_key: SecretString::from(String::new()),
        base_url: Some("http://localhost:11434/v1".into()),
        models_dir: None,
        defaulted: false,
    });
    assert!(
        matches!(zero_dim, Err(LlmError::NotConfigured(ref msg)) if msg.contains("greater than zero")),
        "zero-dimensional vectors must fail at the factory boundary"
    );
}

#[test]
fn existing_default_dimension_api_remains_source_compatible() {
    assert_eq!(
        default_embedding_dim(EmbedderChoice::OpenAi, "text-embedding-3-small"),
        1536
    );
    assert_eq!(
        try_default_embedding_dim(EmbedderChoice::OpenAiCompat, "nomic-embed-text"),
        None
    );
}
