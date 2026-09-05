//! Ordered LLM provider fallback chain.
//!
//! See `docs/llm-provider-fallback.md` (issue #648) for the full design.
//! `FallbackLlmProvider` wraps an ordered list of already-constructed
//! providers behind the same [`LlmProvider`] trait: on a transient failure
//! (`LlmError::is_transient()`) it advances to the next eligible candidate,
//! preserving the original request, schema, and operation id; a
//! deterministic failure (bad request, auth, unsupported schema, malformed
//! response) returns immediately without trying the rest of the chain.
//! `Config::llm_provider_chain` in `ai-memory-cli` is the sole place that
//! builds one; `build_provider` remains the sole construction path for each
//! candidate.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jiff::Timestamp;

use crate::error::{LlmError, LlmResult};
use crate::health::CandidateHealth;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, LlmOperationId};

/// How long a candidate's circuit stays open after a transient failure
/// before it is eligible again. Restarting the process clears all circuit
/// state; there is no durable or cross-process cooldown.
pub const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(30);

/// One provider in an ordered fallback chain.
///
/// Carries only the diagnostic labels used by health reporting and circuit
/// tracking — never the raw configuration or credentials that built
/// `inner`. `build_provider` remains the sole construction path; this type
/// only wraps its result.
pub struct Candidate {
    provider: &'static str,
    model: String,
    inner: Arc<dyn LlmProvider>,
}

impl Candidate {
    /// Wrap an already-constructed provider for use in a fallback chain.
    #[must_use]
    pub fn new(
        provider: &'static str,
        model: impl Into<String>,
        inner: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            inner,
        }
    }
}

/// Per-candidate mutable state: circuit + last-call bookkeeping, shared
/// between the call path and health reporting.
#[derive(Default)]
struct CandidateState {
    last_success_at: Option<Timestamp>,
    last_error_at: Option<Timestamp>,
    last_error_status: Option<u16>,
    last_error_class: Option<&'static str>,
    circuit_open_until: Option<Instant>,
}

struct CandidateEntry {
    candidate: Candidate,
    state: Mutex<CandidateState>,
}

/// Ordered LLM provider fallback chain.
///
/// Tries each eligible candidate in declaration order, advancing only on a
/// transient failure; a deterministic failure returns immediately. See the
/// module docs and `docs/llm-provider-fallback.md`.
pub struct FallbackLlmProvider {
    entries: Vec<CandidateEntry>,
    /// Index of the candidate that answered the most recently completed
    /// call, for health reporting.
    last_selected: Mutex<Option<usize>>,
}

impl FallbackLlmProvider {
    /// Build a chain from an ordered list of candidates (primary first,
    /// then fallbacks in declaration order).
    #[must_use]
    pub fn new(candidates: Vec<Candidate>) -> Self {
        Self {
            entries: candidates
                .into_iter()
                .map(|candidate| CandidateEntry {
                    candidate,
                    state: Mutex::new(CandidateState::default()),
                })
                .collect(),
            last_selected: Mutex::new(None),
        }
    }

    fn circuit_open(&self, index: usize) -> bool {
        let state = self.entries[index]
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        matches!(state.circuit_open_until, Some(until) if Instant::now() < until)
    }

    fn record_success(&self, index: usize) {
        {
            let mut state = self.entries[index]
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.last_success_at = Some(Timestamp::now());
            state.circuit_open_until = None;
        }
        *self.last_selected.lock().unwrap_or_else(|e| e.into_inner()) = Some(index);
    }

    fn record_error(&self, index: usize, err: &LlmError) {
        let mut state = self.entries[index]
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.last_error_at = Some(Timestamp::now());
        state.last_error_status = err.http_status();
        state.last_error_class = Some(err.class());
        if err.is_transient() {
            state.circuit_open_until = Some(Instant::now() + CIRCUIT_COOLDOWN);
        }
    }

    fn describe_failure(&self, index: usize, err: &LlmError) -> String {
        let candidate = &self.entries[index].candidate;
        match err.http_status() {
            Some(status) => format!(
                "{}/{}: {} {status}",
                candidate.provider,
                candidate.model,
                err.class()
            ),
            None => format!(
                "{}/{}: {}",
                candidate.provider,
                candidate.model,
                err.class()
            ),
        }
    }

    fn exhausted(&self, attempted: usize, summary: Vec<String>) -> LlmError {
        LlmError::AllCandidatesFailed {
            attempted,
            summary: summary.join("; "),
        }
    }
}

#[async_trait]
impl LlmProvider for FallbackLlmProvider {
    fn name(&self) -> &'static str {
        self.entries
            .first()
            .map_or("fallback-chain-empty", |entry| entry.candidate.provider)
    }

    fn model(&self) -> &str {
        self.entries
            .first()
            .map_or("", |entry| entry.candidate.model.as_str())
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let mut attempted = 0usize;
        let mut summary = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if self.circuit_open(i) {
                continue;
            }
            attempted += 1;
            match entry.candidate.inner.complete(request.clone()).await {
                Ok(response) => {
                    self.record_success(i);
                    return Ok(response);
                }
                Err(err) => {
                    let transient = err.is_transient();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !transient {
                        return Err(err);
                    }
                }
            }
        }
        Err(self.exhausted(attempted, summary))
    }

    async fn complete_with_operation_id(
        &self,
        request: ChatRequest,
        operation_id: LlmOperationId,
    ) -> LlmResult<ChatResponse> {
        let mut attempted = 0usize;
        let mut summary = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if self.circuit_open(i) {
                continue;
            }
            attempted += 1;
            match entry
                .candidate
                .inner
                .complete_with_operation_id(request.clone(), operation_id)
                .await
            {
                Ok(response) => {
                    self.record_success(i);
                    return Ok(response);
                }
                Err(err) => {
                    let transient = err.is_transient();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !transient {
                        return Err(err);
                    }
                }
            }
        }
        Err(self.exhausted(attempted, summary))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let mut attempted = 0usize;
        let mut summary = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if self.circuit_open(i) {
                continue;
            }
            attempted += 1;
            match entry
                .candidate
                .inner
                .complete_structured_raw(request.clone(), schema.clone())
                .await
            {
                Ok(value) => {
                    self.record_success(i);
                    return Ok(value);
                }
                Err(err) => {
                    let transient = err.is_transient();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !transient {
                        return Err(err);
                    }
                }
            }
        }
        Err(self.exhausted(attempted, summary))
    }

    async fn complete_structured_raw_with_operation_id(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<serde_json::Value> {
        let mut attempted = 0usize;
        let mut summary = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            if self.circuit_open(i) {
                continue;
            }
            attempted += 1;
            match entry
                .candidate
                .inner
                .complete_structured_raw_with_operation_id(
                    request.clone(),
                    schema.clone(),
                    operation_id,
                )
                .await
            {
                Ok(value) => {
                    self.record_success(i);
                    return Ok(value);
                }
                Err(err) => {
                    let transient = err.is_transient();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !transient {
                        return Err(err);
                    }
                }
            }
        }
        Err(self.exhausted(attempted, summary))
    }

    fn candidate_health(&self) -> Vec<CandidateHealth> {
        let selected = *self.last_selected.lock().unwrap_or_else(|e| e.into_inner());
        self.entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                CandidateHealth {
                    provider: entry.candidate.provider.to_string(),
                    model: entry.candidate.model.clone(),
                    last_selected: selected == Some(i),
                    last_success_at: state.last_success_at,
                    last_error_at: state.last_error_at,
                    last_error_status: state.last_error_status,
                    last_error_class: state.last_error_class.map(str::to_string),
                    // Only report while the cooldown genuinely has not
                    // elapsed yet — a stale past instant (no later call
                    // resets it) would otherwise read as "still open".
                    circuit_open_until: state.circuit_open_until.and_then(|until| {
                        (until > now)
                            .then(|| Timestamp::now() + until.saturating_duration_since(now))
                    }),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::types::{ChatMessage, Usage};

    /// A scripted provider double: returns each configured outcome in
    /// order (repeating the last one once exhausted), and records every
    /// request/schema/operation id it was called with.
    struct ScriptedLlm {
        name: &'static str,
        model: String,
        outcomes: Mutex<Vec<LlmResult<ChatResponse>>>,
        calls: AtomicUsize,
        seen_requests: Mutex<Vec<ChatRequest>>,
        seen_schemas: Mutex<Vec<serde_json::Value>>,
        seen_operation_ids: Mutex<Vec<Option<LlmOperationId>>>,
    }

    impl ScriptedLlm {
        fn new(name: &'static str, model: &str, outcomes: Vec<LlmResult<ChatResponse>>) -> Self {
            Self {
                name,
                model: model.to_string(),
                outcomes: Mutex::new(outcomes),
                calls: AtomicUsize::new(0),
                seen_requests: Mutex::new(Vec::new()),
                seen_schemas: Mutex::new(Vec::new()),
                seen_operation_ids: Mutex::new(Vec::new()),
            }
        }

        fn ok(name: &'static str, model: &str, text: &str) -> Self {
            Self::new(
                name,
                model,
                vec![Ok(ChatResponse {
                    text: text.to_string(),
                    usage: Some(Usage::default()),
                    model: model.to_string(),
                })],
            )
        }

        fn err(name: &'static str, model: &str, err: LlmError) -> Self {
            Self::new(name, model, vec![Err(err)])
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn next_outcome(&self) -> LlmResult<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut outcomes = self.outcomes.lock().unwrap();
            if outcomes.len() > 1 {
                outcomes.remove(0)
            } else {
                clone_outcome(&outcomes[0])
            }
        }

        fn record(&self, request: &ChatRequest, operation_id: Option<LlmOperationId>) {
            self.seen_requests.lock().unwrap().push(request.clone());
            self.seen_operation_ids.lock().unwrap().push(operation_id);
        }
    }

    fn clone_outcome(outcome: &LlmResult<ChatResponse>) -> LlmResult<ChatResponse> {
        match outcome {
            Ok(response) => Ok(response.clone()),
            Err(err) => Err(clone_err(err)),
        }
    }

    fn clone_err(err: &LlmError) -> LlmError {
        match err {
            LlmError::Provider { status, body } => LlmError::Provider {
                status: *status,
                body: body.clone(),
            },
            LlmError::Serde(msg) => LlmError::Serde(msg.clone()),
            LlmError::UnexpectedShape(msg) => LlmError::UnexpectedShape(msg.clone()),
            LlmError::NotConfigured(msg) => LlmError::NotConfigured(msg.clone()),
            LlmError::Auth(msg) => LlmError::Auth(msg.clone()),
            LlmError::Schema(msg) => LlmError::Schema(msg.clone()),
            other => panic!("clone_err: unsupported variant in test double: {other}"),
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedLlm {
        fn name(&self) -> &'static str {
            self.name
        }

        fn model(&self) -> &str {
            &self.model
        }

        async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            self.record(&request, None);
            self.next_outcome()
        }

        async fn complete_with_operation_id(
            &self,
            request: ChatRequest,
            operation_id: LlmOperationId,
        ) -> LlmResult<ChatResponse> {
            self.record(&request, Some(operation_id));
            self.next_outcome()
        }

        async fn complete_structured_raw(
            &self,
            request: ChatRequest,
            schema: serde_json::Value,
        ) -> LlmResult<serde_json::Value> {
            self.record(&request, None);
            self.seen_schemas.lock().unwrap().push(schema);
            self.next_outcome()
                .map(|response| serde_json::json!({ "text": response.text }))
        }

        async fn complete_structured_raw_with_operation_id(
            &self,
            request: ChatRequest,
            schema: serde_json::Value,
            operation_id: LlmOperationId,
        ) -> LlmResult<serde_json::Value> {
            self.record(&request, Some(operation_id));
            self.seen_schemas.lock().unwrap().push(schema);
            self.next_outcome()
                .map(|response| serde_json::json!({ "text": response.text }))
        }
    }

    fn transient(status: u16) -> LlmError {
        LlmError::Provider {
            status,
            body: "upstream body that must never leak".to_string(),
        }
    }

    #[tokio::test]
    async fn chain_tries_candidates_in_order_and_stops_at_first_success() {
        let first = Arc::new(ScriptedLlm::err("anthropic", "claude", transient(503)));
        let second = Arc::new(ScriptedLlm::ok("gemini", "gemini-flash", "second answer"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("anthropic", "claude", first.clone()),
            Candidate::new("gemini", "gemini-flash", second.clone()),
        ]);

        let response = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();

        assert_eq!(response.text, "second answer");
        assert_eq!(first.calls(), 1);
        assert_eq!(second.calls(), 1);
    }

    #[tokio::test]
    async fn all_four_methods_preserve_request_schema_and_operation_id() {
        let first = Arc::new(ScriptedLlm::err("primary", "m1", transient(500)));
        let second = Arc::new(ScriptedLlm::ok("secondary", "m2", "ok"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("primary", "m1", first.clone()),
            Candidate::new("secondary", "m2", second.clone()),
        ]);
        let request = ChatRequest::user_prompt("preserve me").with_max_tokens(42);
        let schema = serde_json::json!({ "type": "object" });
        let op_id = LlmOperationId::new();

        chain.complete(request.clone()).await.unwrap();
        chain
            .complete_with_operation_id(request.clone(), op_id)
            .await
            .unwrap();
        chain
            .complete_structured_raw(request.clone(), schema.clone())
            .await
            .unwrap();
        chain
            .complete_structured_raw_with_operation_id(request.clone(), schema.clone(), op_id)
            .await
            .unwrap();

        // Every call advanced past `first` (transient) onto `second`, which
        // must have seen the exact same request/schema/operation id on
        // each of the four entry points.
        assert_eq!(second.calls(), 4);
        let seen_requests = second.seen_requests.lock().unwrap();
        assert!(seen_requests.iter().all(
            |r| matches!(&r.messages[..], [ChatMessage { content, .. }] if content == "preserve me")
                && r.max_tokens == 42
        ));
        let seen_schemas = second.seen_schemas.lock().unwrap();
        assert_eq!(seen_schemas.len(), 2);
        assert!(seen_schemas.iter().all(|s| *s == schema));
        let seen_ops = second.seen_operation_ids.lock().unwrap();
        // complete/complete_structured_raw pass None; the *_with_operation_id
        // calls pass the same op_id every time.
        assert_eq!(*seen_ops, vec![None, Some(op_id), None, Some(op_id)]);
    }

    #[tokio::test]
    async fn transient_statuses_advance_to_the_next_candidate() {
        for status in [429, 500, 502, 503, 504] {
            let first = Arc::new(ScriptedLlm::err("primary", "m1", transient(status)));
            let second = Arc::new(ScriptedLlm::ok("secondary", "m2", "ok"));
            let chain = FallbackLlmProvider::new(vec![
                Candidate::new("primary", "m1", first.clone()),
                Candidate::new("secondary", "m2", second.clone()),
            ]);

            let result = chain.complete(ChatRequest::user_prompt("hi")).await;
            assert!(result.is_ok(), "status {status} should have advanced");
            assert_eq!(second.calls(), 1, "status {status} should have advanced");
        }
    }

    #[tokio::test]
    async fn deterministic_errors_stop_on_the_first_candidate() {
        let deterministic = [
            LlmError::Provider {
                status: 400,
                body: String::new(),
            },
            LlmError::Provider {
                status: 401,
                body: String::new(),
            },
            LlmError::Provider {
                status: 403,
                body: String::new(),
            },
            LlmError::Provider {
                status: 404,
                body: String::new(),
            },
            LlmError::Provider {
                status: 422,
                body: String::new(),
            },
            LlmError::Schema("bad schema".into()),
            LlmError::Serde("deserialize failed".into()),
            LlmError::UnexpectedShape("no tool block".into()),
        ];
        for err in deterministic {
            let expected_class = err.class();
            let first = Arc::new(ScriptedLlm::err("primary", "m1", err));
            let second = Arc::new(ScriptedLlm::ok("secondary", "m2", "unreachable"));
            let chain = FallbackLlmProvider::new(vec![
                Candidate::new("primary", "m1", first.clone()),
                Candidate::new("secondary", "m2", second.clone()),
            ]);

            let result = chain.complete(ChatRequest::user_prompt("hi")).await;
            let returned = result.expect_err("deterministic error must propagate, not succeed");
            assert_eq!(returned.class(), expected_class);
            assert_eq!(first.calls(), 1);
            assert_eq!(
                second.calls(),
                0,
                "class {expected_class} must not try the next candidate"
            );
        }
    }

    #[tokio::test]
    async fn all_candidates_failing_returns_a_bounded_aggregate_error() {
        let first = Arc::new(ScriptedLlm::err(
            "primary",
            "m1",
            transient_with_body(500, "primary secret body"),
        ));
        let second = Arc::new(ScriptedLlm::err(
            "secondary",
            "m2",
            transient_with_body(503, "secondary secret body"),
        ));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("primary", "m1", first.clone()),
            Candidate::new("secondary", "m2", second.clone()),
        ]);

        let err = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("every candidate failed");
        match &err {
            LlmError::AllCandidatesFailed { attempted, summary } => {
                assert_eq!(*attempted, 2);
                assert!(summary.contains("primary/m1"));
                assert!(summary.contains("secondary/m2"));
                assert!(summary.contains("500"));
                assert!(summary.contains("503"));
                assert!(!summary.contains("secret body"));
            }
            other => panic!("expected AllCandidatesFailed, got {other}"),
        }
    }

    fn transient_with_body(status: u16, body: &str) -> LlmError {
        LlmError::Provider {
            status,
            body: body.to_string(),
        }
    }

    #[tokio::test]
    async fn an_open_circuit_skips_only_its_candidate() {
        let first = Arc::new(ScriptedLlm::err("primary", "m1", transient(500)));
        let second = Arc::new(ScriptedLlm::ok("secondary", "m2", "ok"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("primary", "m1", first.clone()),
            Candidate::new("secondary", "m2", second.clone()),
        ]);

        // First call: primary fails transiently and opens its circuit;
        // secondary answers.
        chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();
        assert_eq!(first.calls(), 1);
        assert_eq!(second.calls(), 1);

        // Second call, still inside the cooldown: primary must be skipped
        // outright (not attempted again), secondary answers again.
        chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();
        assert_eq!(first.calls(), 1, "an open circuit must not be attempted");
        assert_eq!(second.calls(), 2);
    }

    #[test]
    fn a_success_closes_an_open_circuit_immediately() {
        let chain = FallbackLlmProvider::new(vec![Candidate::new(
            "primary",
            "m1",
            Arc::new(ScriptedLlm::ok("primary", "m1", "ok")),
        )]);
        // Simulate an open circuit far in the future — a real cooldown, not
        // yet elapsed.
        {
            let mut state = chain.entries[0].state.lock().unwrap();
            state.circuit_open_until = Some(Instant::now() + Duration::from_secs(600));
        }
        assert!(chain.circuit_open(0));

        chain.record_success(0);

        assert!(
            !chain.circuit_open(0),
            "a success must close the circuit immediately, not wait out the cooldown"
        );
    }

    #[test]
    fn an_elapsed_cooldown_closes_the_circuit_without_an_explicit_success() {
        let chain = FallbackLlmProvider::new(vec![Candidate::new(
            "primary",
            "m1",
            Arc::new(ScriptedLlm::ok("primary", "m1", "ok")),
        )]);
        {
            let mut state = chain.entries[0].state.lock().unwrap();
            state.circuit_open_until = Some(Instant::now() - Duration::from_secs(1));
        }
        assert!(!chain.circuit_open(0));
    }

    #[tokio::test]
    async fn candidate_health_reports_labels_last_selected_and_redacted_errors() {
        let first = Arc::new(ScriptedLlm::err(
            "primary",
            "m1",
            transient_with_body(500, "must not leak"),
        ));
        let second = Arc::new(ScriptedLlm::ok("secondary", "m2", "ok"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("primary", "m1", first),
            Candidate::new("secondary", "m2", second),
        ]);

        chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();

        let health = chain.candidate_health();
        assert_eq!(health.len(), 2);
        assert_eq!(health[0].provider, "primary");
        assert_eq!(health[0].model, "m1");
        assert!(!health[0].last_selected);
        assert_eq!(health[0].last_error_status, Some(500));
        assert_eq!(health[0].last_error_class.as_deref(), Some("provider"));
        assert!(health[0].circuit_open_until.is_some());
        assert!(health[1].last_selected);
        assert!(health[1].last_success_at.is_some());
        assert!(health[1].circuit_open_until.is_none());

        let rendered = format!("{health:?}");
        assert!(!rendered.contains("must not leak"));
    }
}
