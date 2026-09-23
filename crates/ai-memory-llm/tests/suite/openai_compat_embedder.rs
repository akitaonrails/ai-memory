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
        copilot_auth: None,
        defaulted: false,
        query_prefix: String::new(),
        document_prefix: String::new(),
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
        copilot_auth: None,
        defaulted: false,
        query_prefix: String::new(),
        document_prefix: String::new(),
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
        copilot_auth: None,
        defaulted: false,
        query_prefix: String::new(),
        document_prefix: String::new(),
    });
    assert!(
        matches!(zero_dim, Err(LlmError::NotConfigured(ref msg)) if msg.contains("greater than zero")),
        "zero-dimensional vectors must fail at the factory boundary"
    );
}

#[tokio::test]
async fn query_prefix_is_sent_with_embed_query_not_embed_document() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
            assert_eq!(
                body["input"], "query: find the runbook",
                "embed_query must send the query prefix ahead of the text"
            );
            ResponseTemplate::new(200).set_body_json(embedding_body(8))
        })
        .expect(1)
        .mount(&server)
        .await;

    let e = OpenAiCompatEmbedder::new(format!("{}/v1", server.uri()), None, "nomic-embed-text", 8)
        .expect("embedder builds")
        .with_prefixes("query: ", "passage: ");

    e.embed_query("find the runbook")
        .await
        .expect("embed_query succeeds");
}

#[tokio::test]
async fn document_prefix_is_sent_with_embed_document_not_embed_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
            assert_eq!(
                body["input"], "passage: the runbook says...",
                "embed_document must send the document prefix ahead of the text"
            );
            ResponseTemplate::new(200).set_body_json(embedding_body(8))
        })
        .expect(1)
        .mount(&server)
        .await;

    let e = OpenAiCompatEmbedder::new(format!("{}/v1", server.uri()), None, "nomic-embed-text", 8)
        .expect("embedder builds")
        .with_prefixes("query: ", "passage: ");

    e.embed_document("the runbook says...")
        .await
        .expect("embed_document succeeds");
}

#[tokio::test]
async fn unset_prefixes_leave_embed_query_and_embed_document_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
            assert_eq!(
                body["input"], "plain text",
                "an unset prefix (the default) must not change the request body"
            );
            ResponseTemplate::new(200).set_body_json(embedding_body(8))
        })
        .expect(2)
        .mount(&server)
        .await;

    // No `with_prefixes` call at all — construction alone must be
    // byte-identical to before this feature existed.
    let e = OpenAiCompatEmbedder::new(format!("{}/v1", server.uri()), None, "nomic-embed-text", 8)
        .expect("embedder builds");

    e.embed_query("plain text")
        .await
        .expect("embed_query succeeds");
    e.embed_document("plain text")
        .await
        .expect("embed_document succeeds");
}

#[tokio::test]
async fn prefix_is_truncated_together_with_the_text_it_precedes() {
    // Truncation must bound prefix+text together; a prefix must never let
    // the combined request exceed the server's input cap.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
            let input = body["input"].as_str().expect("input is a string");
            assert!(
                input.starts_with("passage: "),
                "prefix must survive truncation: {input:.60}..."
            );
            assert!(
                input.len() <= 8_000,
                "prefix+text must stay within the same hard byte cap as before"
            );
            ResponseTemplate::new(200).set_body_json(embedding_body(8))
        })
        .expect(1)
        .mount(&server)
        .await;

    let e = OpenAiCompatEmbedder::new(format!("{}/v1", server.uri()), None, "nomic-embed-text", 8)
        .expect("embedder builds")
        .with_prefixes("query: ", "passage: ");

    let long_text = "x".repeat(50_000);
    e.embed_document(&long_text)
        .await
        .expect("embed_document succeeds");
}

#[tokio::test]
async fn factory_wires_configured_prefixes_into_the_compat_embedder() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(move |req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
            assert_eq!(body["input"], "query: via factory");
            ResponseTemplate::new(200).set_body_json(embedding_body(8))
        })
        .expect(1)
        .mount(&server)
        .await;

    let embedder = build_embedder(EmbedderConfig {
        provider: EmbedderChoice::OpenAiCompat,
        model: "nomic-embed-text".into(),
        dim: 8,
        api_key: SecretString::from(String::new()),
        base_url: Some(format!("{}/v1", server.uri())),
        models_dir: None,
        copilot_auth: None,
        defaulted: false,
        query_prefix: "query: ".into(),
        document_prefix: "passage: ".into(),
    })
    .expect("factory builds compat embedder with prefixes");

    embedder
        .embed_query("via factory")
        .await
        .expect("embed_query succeeds");
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
