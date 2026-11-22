//! Startup selection of the Decisions `choice` reranker through normal configuration.

use super::*;
use crate::config::{Config, RuntimeEnv};
use ai_memory_llm::{ProviderHealth, RerankCandidate};
use axum::http::HeaderMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

type Seen = Arc<Mutex<Vec<(Option<String>, serde_json::Value)>>>;

const SENTINEL: &str = "SYNTHETIC_CREDENTIAL_SENTINEL";
const SELECTOR_ERROR: &str =
    "AI_MEMORY_RERANKER=decisions-choice requires OPENAI_API_KEY and llm_model";

#[derive(Clone)]
enum Reply {
    Prefer(Vec<(&'static str, f64)>),
    Status(u16),
    Refusal,
    SlowPrefer(Duration, Vec<(&'static str, f64)>),
}

fn preferred(order: &[(&'static str, f64)]) -> serde_json::Value {
    serde_json::json!({
        "model": "gpt-6-luna",
        "answers": [{
            "type": "choice",
            "name": "rerank",
            "choice": order[0].0,
            "confidence": order[0].1,
            "probabilities": order
                .iter()
                .map(|(value, probability)| serde_json::json!({
                    "value": value,
                    "probability": probability,
                }))
                .collect::<Vec<_>>(),
        }],
        "usage": {"input_tokens": 100, "output_tokens": 0, "total_tokens": 100},
    })
}

/// Loopback stand-in for `POST /v1/decisions`; records each request's
/// `authorization` header and JSON body before it answers.
async fn responder(mode: Reply) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let recorded = seen.clone();
    let app = axum::Router::new().route(
        "/v1/decisions",
        axum::routing::post(
            move |headers: HeaderMap, body: axum::Json<serde_json::Value>| {
                let recorded = recorded.clone();
                let mode = mode.clone();
                async move {
                    let authorization = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .map(|value| value.to_str().unwrap_or_default().to_owned());
                    recorded.lock().unwrap().push((authorization, body.0));
                    match mode {
                        Reply::Prefer(order) => {
                            (StatusCode::OK, axum::Json(preferred(&order))).into_response()
                        }
                        Reply::SlowPrefer(delay, order) => {
                            tokio::time::sleep(delay).await;
                            (StatusCode::OK, axum::Json(preferred(&order))).into_response()
                        }
                        Reply::Status(status) => (
                            StatusCode::from_u16(status).unwrap(),
                            axum::Json(serde_json::json!({})),
                        )
                            .into_response(),
                        Reply::Refusal => (
                            StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "model": "gpt-6-luna",
                                "answers": [{"type": "refusal", "name": "rerank"}],
                            })),
                        )
                            .into_response(),
                    }
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), seen)
}

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    server: AiMemoryServer,
    workspace: WorkspaceId,
    project: ProjectId,
}

async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let workspace = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project = store
        .writer
        .get_or_create_project(workspace, "scratch", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
    let server = AiMemoryServer::new(
        store.reader.clone(),
        store.writer.clone(),
        workspace,
        project,
    );
    Fixture {
        _tmp: tmp,
        store,
        wiki,
        server,
        workspace,
        project,
    }
}

fn choice_config(base_url: &str, timeout_secs: u64) -> Config {
    Config {
        reranker: Some("decisions-choice".into()),
        llm_model: Some("gpt-6-luna".into()),
        llm_base_url: Some(base_url.into()),
        rerank_timeout_secs: timeout_secs,
        runtime_env: RuntimeEnv::with_openai_api_key_for_tests(SENTINEL),
        ..Config::default()
    }
}

fn configure(
    config: &Config,
    fixture: Fixture,
    health: &ProviderHealth,
) -> Result<ConsolidatorSetup> {
    configure_consolidator(
        config,
        fixture.server,
        &fixture.store,
        &fixture.wiki,
        fixture.workspace,
        fixture.project,
        health,
    )
}

async fn rank(stage: &ai_memory_llm::RerankStage) -> Vec<String> {
    stage
        .apply(
            "which page answers the query",
            ["a", "b", "c"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<String>>(),
            3,
            stage.timeout(),
            |id: &String| RerankCandidate {
                id: id.clone(),
                title: format!("Title {id}"),
                snippet: format!("Text {id}"),
            },
            |_, _| {},
        )
        .await
}

async fn ranked_with(reply: Reply, timeout_secs: u64) -> (Vec<String>, usize) {
    let (url, seen) = responder(reply).await;
    let health = ProviderHealth::default();
    let setup = configure(&choice_config(&url, timeout_secs), fixture().await, &health)
        .expect("decisions-choice with a key and a model must configure");
    let stage = setup
        .server
        .rerank_stage()
        .expect("decisions-choice must install a rerank stage");
    let order = rank(&stage).await;
    let requests = seen.lock().unwrap().len();
    (order, requests)
}

#[tokio::test]
async fn decisions_choice_configured_server_reorders_through_loopback_decisions() {
    let (url, seen) = responder(Reply::Prefer(vec![("c", 0.7), ("b", 0.2), ("a", 0.1)])).await;
    let health = ProviderHealth::default();
    let setup = configure(&choice_config(&url, 20), fixture().await, &health)
        .expect("decisions-choice with a key and a model must configure");
    assert!(
        setup.llm_providers.default.is_none(),
        "decisions-choice must not need a generative provider"
    );
    let stage = setup
        .server
        .rerank_stage()
        .expect("decisions-choice must install a rerank stage");
    assert_eq!(stage.reranker().name(), "decisions");
    assert_eq!(stage.reranker().model(), "gpt-6-luna");
    assert_eq!(stage.timeout(), Duration::from_secs(20));

    assert_eq!(rank(&stage).await, ["c", "b", "a"]);

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "one Decisions request per eligible query");
    let (authorization, body) = &seen[0];
    assert!(
        authorization.is_none(),
        "a loopback destination must not receive the API key"
    );
    assert_eq!(body["model"], "gpt-6-luna");
    let questions = body["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0]["type"], "choice");
    let choices: Vec<&str> = questions[0]["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|choice| choice["value"].as_str().unwrap())
        .collect();
    assert_eq!(choices, ["a", "b", "c"]);

    let snapshot = health.snapshot();
    let row = snapshot
        .llm_workloads
        .iter()
        .find(|row| row.workload == LlmWorkload::Rerank)
        .expect("the rerank workload row must record the Decisions call");
    assert_eq!((row.calls, row.successes, row.errors), (1, 1, 0));
}

#[tokio::test]
async fn decisions_choice_failed_or_refused_answer_keeps_local_order_without_retry() {
    for reply in [Reply::Status(500), Reply::Status(429), Reply::Refusal] {
        let (order, requests) = ranked_with(reply, 20).await;
        assert_eq!(order, ["a", "b", "c"]);
        assert_eq!(requests, 1, "a failed answer must not be retried");
    }
}

#[tokio::test]
async fn decisions_choice_slow_answer_keeps_local_order() {
    let (order, requests) = ranked_with(
        Reply::SlowPrefer(
            Duration::from_secs(2),
            vec![("c", 0.7), ("b", 0.2), ("a", 0.1)],
        ),
        1,
    )
    .await;
    assert_eq!(order, ["a", "b", "c"]);
    assert_eq!(requests, 1);
}

#[tokio::test]
async fn decisions_choice_requires_key_and_model_before_any_send() {
    let (url, seen) = responder(Reply::Prefer(vec![("c", 0.7), ("b", 0.2), ("a", 0.1)])).await;
    let without_key = Config {
        runtime_env: RuntimeEnv::default(),
        ..choice_config(&url, 20)
    };
    let without_model = Config {
        llm_model: None,
        ..choice_config(&url, 20)
    };
    for config in [without_key, without_model] {
        let error = match configure(&config, fixture().await, &ProviderHealth::default()) {
            Ok(_) => panic!("decisions-choice without a key or a model must refuse startup"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains(SELECTOR_ERROR), "unexpected error: {error}");
        assert!(
            !error.contains(SENTINEL),
            "the error must not carry the key"
        );
    }
    assert_eq!(seen.lock().unwrap().len(), 0);
}

#[test]
fn decisions_choice_selector_is_explicit_and_case_insensitive() {
    for value in ["decisions-choice", "DECISIONS-CHOICE", " Decisions-Choice "] {
        let config = Config {
            reranker: Some(value.into()),
            ..Config::default()
        };
        assert!(config.reranker_choice().is_ok(), "{value}");
    }
    let config = Config {
        reranker: Some("decisions-score".into()),
        ..Config::default()
    };
    assert!(config.reranker_choice().is_err());
}
