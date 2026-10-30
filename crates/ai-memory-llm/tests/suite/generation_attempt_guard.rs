//! The generation-attempt guard fires before every actual generation send,
//! against real HTTP (wiremock): the openai-compat strict→tolerant re-send,
//! and a fallback chain whose first candidate fails before the second runs.
//! A refused send never reaches the wire, and the refusal is local
//! back-pressure: it neither marks the provider unhealthy nor advances the
//! chain. Per-provider send sites are covered by their own provider tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ai_memory_llm::types::ChatRequest;
use ai_memory_llm::{
    Candidate, FallbackLlmProvider, GenerationAttemptGuard, LlmError, LlmProvider, LlmResult,
    OpenAiCompatProvider, ProviderHealth, ProviderHealthStatus, before_generation_send,
    with_generation_attempt_guard,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Records every `prepaid` flag it is called with and lets `unprepaid_budget`
/// non-prepaid sends through before refusing with `CapacityBusy`, the way a
/// durable budget would.
pub(crate) struct RecordingGuard {
    state: Mutex<(Vec<bool>, usize)>,
}

impl RecordingGuard {
    pub(crate) fn new(unprepaid_budget: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((Vec::new(), unprepaid_budget)),
        })
    }

    pub(crate) fn flags(&self) -> Vec<bool> {
        self.state.lock().unwrap().0.clone()
    }
}

#[async_trait::async_trait]
impl GenerationAttemptGuard for RecordingGuard {
    async fn before_send(&self, prepaid: bool) -> LlmResult<()> {
        let mut state = self.state.lock().unwrap();
        state.0.push(prepaid);
        if prepaid {
            return Ok(());
        }
        if state.1 == 0 {
            return Err(LlmError::CapacityBusy("test attempt budget"));
        }
        state.1 -= 1;
        Ok(())
    }
}

fn prose_body() -> serde_json::Value {
    json!({
        "id": "id",
        "object": "chat.completion",
        "created": 0,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Sure! Here is the JSON: {\"ok\":true}",
            },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"],
    })
}

/// A server that answers every chat completion with prose around a JSON
/// object (strict parse fails, tolerant succeeds) and counts the requests.
async fn prose_server() -> (MockServer, Arc<AtomicUsize>) {
    let server = MockServer::start().await;
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &Request| {
            counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(prose_body())
        })
        .mount(&server)
        .await;
    (server, hits)
}

fn compat(base_url: String, strict: bool) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(base_url, None, "m")
        .expect("provider builds")
        .with_strict(strict)
}

/// Control: no scope, no guard, legacy behavior byte for byte — strict still
/// re-sends tolerantly (two requests) and the hook is a no-op.
#[tokio::test]
async fn without_a_scope_the_hook_is_a_no_op_and_behavior_is_unchanged() {
    assert!(before_generation_send().await.is_ok());
    let (server, hits) = prose_server().await;
    let value = compat(server.uri(), true)
        .complete_structured_raw(ChatRequest::user_prompt("go"), schema())
        .await
        .expect("tolerant fallback recovers");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

/// Strict then tolerant is two sends: the guard sees both, the first
/// prepaid, the second not, and a budget that covers it lets both through.
#[tokio::test]
async fn strict_to_tolerant_fires_the_guard_for_each_send() {
    let (server, hits) = prose_server().await;
    let guard = RecordingGuard::new(1);
    let value = with_generation_attempt_guard(
        guard.clone(),
        compat(server.uri(), true)
            .complete_structured_raw(ChatRequest::user_prompt("go"), schema()),
    )
    .await
    .expect("both sends reserved");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(guard.flags(), vec![true, false]);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

/// An unreserved second send is refused before it reaches the wire and the
/// local `CapacityBusy` propagates as is.
#[tokio::test]
async fn the_unreserved_tolerant_send_is_refused_before_the_wire() {
    let (server, hits) = prose_server().await;
    let guard = RecordingGuard::new(0);
    let err = with_generation_attempt_guard(
        guard.clone(),
        compat(server.uri(), true)
            .complete_structured_raw(ChatRequest::user_prompt("go"), schema()),
    )
    .await
    .expect_err("second send has no budget");
    assert!(matches!(err, LlmError::CapacityBusy(_)), "{err:?}");
    assert_eq!(guard.flags(), vec![true, false]);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "refused send never left");
}

/// A non-strict provider sends once: exactly one prepaid call, no extra.
#[tokio::test]
async fn a_single_send_is_only_the_prepaid_one() {
    let (server, hits) = prose_server().await;
    let guard = RecordingGuard::new(0);
    with_generation_attempt_guard(
        guard.clone(),
        compat(server.uri(), false)
            .complete_structured_raw(ChatRequest::user_prompt("go"), schema()),
    )
    .await
    .expect("one prepaid send");
    assert_eq!(guard.flags(), vec![true]);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// Each invocation scope starts over: the next scope's first send is
/// prepaid again.
#[tokio::test]
async fn every_scope_starts_with_a_prepaid_send() {
    let (server, _hits) = prose_server().await;
    let provider = compat(server.uri(), false);
    let guard = RecordingGuard::new(0);
    for _ in 0..2 {
        with_generation_attempt_guard(
            guard.clone(),
            provider.complete_structured_raw(ChatRequest::user_prompt("go"), schema()),
        )
        .await
        .unwrap();
    }
    assert_eq!(guard.flags(), vec![true, true]);
}

/// Candidate one fails to connect (a real send attempt), candidate two
/// answers: two guarded sends, the second reserved by the guard itself.
#[tokio::test]
async fn a_fallback_candidate_send_is_guarded_and_reserved() {
    let (server, hits) = prose_server().await;
    let chain = FallbackLlmProvider::new(vec![
        Candidate::new(
            "openai-compat",
            "m",
            Arc::new(compat("http://127.0.0.1:0/v1".into(), false)),
        ),
        Candidate::new("openai-compat", "m", Arc::new(compat(server.uri(), false))),
    ]);
    let guard = RecordingGuard::new(1);
    let value = with_generation_attempt_guard(
        guard.clone(),
        chain.complete_structured_raw(ChatRequest::user_prompt("go"), schema()),
    )
    .await
    .expect("second candidate answers");
    assert_eq!(value, json!({"ok": true}));
    assert_eq!(guard.flags(), vec![true, false]);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// A refused fallback send is local back-pressure: the chain stops with the
/// guard's `CapacityBusy` instead of `AllCandidatesFailed`, never calls the
/// next candidate, and leaves it (and the role health the engine reads)
/// untouched.
#[tokio::test]
async fn a_refused_fallback_send_stops_the_chain_and_is_not_a_health_failure() {
    let (server, hits) = prose_server().await;
    let chain: Arc<dyn LlmProvider> = Arc::new(FallbackLlmProvider::new(vec![
        Candidate::new(
            "openai-compat",
            "m",
            Arc::new(compat("http://127.0.0.1:0/v1".into(), false)),
        ),
        Candidate::new("openai-compat", "m", Arc::new(compat(server.uri(), false))),
    ]));
    let health = ProviderHealth::default();
    let provider = health.wrap_llm_provider(chain, "openai-compat", "m", None);
    let guard = RecordingGuard::new(0);
    let err = with_generation_attempt_guard(
        guard.clone(),
        provider.complete_structured_raw(ChatRequest::user_prompt("go"), schema()),
    )
    .await
    .expect_err("the second candidate send has no budget");
    assert!(matches!(err, LlmError::CapacityBusy(_)), "{err:?}");
    assert_eq!(guard.flags(), vec![true, false]);
    assert_eq!(hits.load(Ordering::SeqCst), 0, "refused send never left");

    let snapshot = health.snapshot();
    // The genuine connect failure of candidate one is recorded; the refusal
    // charged to candidate two is not.
    assert!(snapshot.llm_candidates[0].last_error_at.is_some());
    assert!(snapshot.llm_candidates[1].last_error_at.is_none());
    assert!(snapshot.llm_candidates[1].circuit_open_until.is_none());
    assert_eq!(snapshot.llm.status, ProviderHealthStatus::Unknown);
    assert!(snapshot.llm.last_error_at.is_none());
    assert!(snapshot.llm_workloads.iter().all(|w| w.calls == 0));
}
